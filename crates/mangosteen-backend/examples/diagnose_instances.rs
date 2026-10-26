//! 优云智算实例/规格排查工具：打印厂商**原始响应**并逐可用区扫描。
//!
//! 用途：当 `DescribeCompShareInstance` 返回 `TotalCount: 0` 时，
//! 判断是「账号下确实没有实例」还是「查询参数把结果过滤掉了」。
//!
//! 与 `describe_instances` 的区别：那个是**适配验证**（看归一化结果），
//! 这个是**故障排查**（看厂商原文 + 全可用区覆盖）。
//!
//! 用法：
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! cargo run -p mangosteen-backend --example diagnose_instances
//! ```

use mangosteen_backend::client::Scope;
use mangosteen_backend::{CompshareClient, Credential, CurlTransport, Params};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let public_key = env("COMPSHARE_PUBLIC_KEY").ok_or("缺少 COMPSHARE_PUBLIC_KEY")?;
    let private_key = env("COMPSHARE_PRIVATE_KEY").ok_or("缺少 COMPSHARE_PRIVATE_KEY")?;
    let credential = Credential::new(public_key, private_key)?;

    // 不带任何地域过滤的客户端（Scope::None 时不注入 Region/Zone）
    let client = CompshareClient::new(
        mangosteen_backend::DEFAULT_ENDPOINT,
        credential,
        CurlTransport::new(30),
        "",
        "",
    )?;

    // ---------- 1. 全地域查询，打印完整原始 JSON ----------
    println!("========== [1] DescribeCompShareInstance（全地域，原始响应） ==========");
    let mut p = Params::new();
    p.set_i64("Limit", 100).set_i64("Offset", 0);
    match client.call("DescribeCompShareInstance", p, Scope::None) {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            );
        }
        Err(e) => println!("失败：{e}"),
    }
    println!();

    // ---------- 2. 逐个可用区查询（排除地域过滤导致漏报） ----------
    println!("========== [2] 逐个可用区查询实例 ==========");
    let zones = match client.zones() {
        Ok(r) => r.zones,
        Err(e) => {
            println!("查询可用区失败：{e}");
            return Ok(());
        }
    };

    let mut total_across_zones = 0usize;
    for z in &zones {
        let mut p = Params::new();
        p.set("Region", z.region.clone())
            .set("Zone", z.zone.clone())
            .set_i64("Limit", 100)
            .set_i64("Offset", 0);
        match client.call("DescribeCompShareInstance", p, Scope::None) {
            Ok(v) => {
                let total = v.get("TotalCount").and_then(|x| x.as_i64()).unwrap_or(0);
                let n = v
                    .get("UHostSet")
                    .and_then(|x| x.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                total_across_zones += n;
                println!(
                    "  {:<12} {:<12} {:<10} TotalCount={:<4} 本页={}",
                    z.region,
                    z.zone,
                    z.describe,
                    total,
                    n
                );
                // 有数据就把 ID 列出来
                if let Some(arr) = v.get("UHostSet").and_then(|x| x.as_array()) {
                    for u in arr {
                        println!(
                            "      → {} 「{}」 state={}",
                            u.get("UHostId").and_then(|x| x.as_str()).unwrap_or("?"),
                            u.get("Name").and_then(|x| x.as_str()).unwrap_or("?"),
                            u.get("State").and_then(|x| x.as_str()).unwrap_or("?")
                        );
                    }
                }
            }
            Err(e) => println!("  {:<12} {:<12} 失败：{e}", z.region, z.zone),
        }
    }
    println!();
    println!("全部可用区实例总数：{total_across_zones}");
    println!();

    // ---------- 3. 各可用区的可用 GPU 机型（用户常说的「GPU 列表」） ----------
    println!("========== [3] 各可用区可用 GPU 机型 ==========");
    for z in &zones {
        let zc = CompshareClient::new(
            mangosteen_backend::DEFAULT_ENDPOINT,
            Credential::new(
                env("COMPSHARE_PUBLIC_KEY").unwrap_or_default(),
                env("COMPSHARE_PRIVATE_KEY").unwrap_or_default(),
            )?,
            CurlTransport::new(30),
            &z.region,
            &z.zone,
        )?;
        match zc.available_types() {
            Ok(r) => {
                let on_sale: Vec<_> = r.types.iter().filter(|t| t.is_on_sale()).collect();
                println!(
                    "  {:<12} {:<12} {:<10} {} 个机型（可售 {}）",
                    z.region,
                    z.zone,
                    z.describe,
                    r.types.len(),
                    on_sale.len()
                );
                for t in on_sale.iter().take(12) {
                    let vram = t
                        .graphics_memory
                        .as_ref()
                        .map(|g| format!("{}GB", g.value_gb))
                        .unwrap_or_else(|| "?".to_string());
                    let max_gpu = t
                        .machine_sizes
                        .iter()
                        .map(|s| s.gpu)
                        .max()
                        .unwrap_or(0);
                    println!(
                        "      {:<10} 显存{:<6} 最多{}卡  {}",
                        t.name, vram, max_gpu, t.description
                    );
                }
            }
            Err(e) => println!("  {:<12} {:<12} 失败：{e}", z.region, z.zone),
        }
    }
    println!();

    // ---------- 4. 项目列表（ProjectId 可能是过滤原因） ----------
    println!("========== [4] 项目列表（排查 ProjectId 过滤） ==========");
    // 文档：通过向同一网关发送 GetProjectList 获取 ProjectId
    match client.call("GetProjectList", Params::new(), Scope::None) {
        Ok(v) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            );
        }
        Err(e) => println!("失败（该接口可能不属于本产品线）：{e}"),
    }

    Ok(())
}
