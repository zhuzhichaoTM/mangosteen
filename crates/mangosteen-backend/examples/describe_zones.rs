//! 可用区列表（`DescribeCompShareSupportZone`）适配演示与验证。
//!
//! 用法：
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! # 可选：只看某个地域
//! export COMPSHARE_REGION='cn-sh2'
//! cargo run -p mangosteen-backend --example describe_zones
//! ```
//!
//! ## 这个接口为什么重要
//!
//! 它返回的 `Region` / `Zone` 是**其它所有接口的定位参数来源**：
//! 创建实例、查机型、查库存、查价格都必须带这两个值。
//! 而且它区分了两种可用区：
//!
//! * `IsPod = false`（云主机）：有独立虚机，可 SSH 登录，能自建推理服务；
//! * `IsPod = true`（容器）：只跑容器镜像，**不能 SSH**。
//!
//! 本项目的部署方式依赖 SSH 隧道，因此自动选点必须优先 `IsPod = false`。

use mangosteen_backend::client::Scope;
use mangosteen_backend::{CompshareClient, Credential, CurlTransport, Params};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let public_key = env("COMPSHARE_PUBLIC_KEY").ok_or("缺少 COMPSHARE_PUBLIC_KEY")?;
    let private_key = env("COMPSHARE_PRIVATE_KEY").ok_or("缺少 COMPSHARE_PRIVATE_KEY")?;
    let credential = Credential::new(public_key, private_key)?;
    let filter = env("COMPSHARE_REGION");

    let client = CompshareClient::new(
        mangosteen_backend::DEFAULT_ENDPOINT,
        credential,
        CurlTransport::new(30),
        "",
        "",
    )?;

    println!("=== DescribeCompShareSupportZone ===");
    println!(
        "筛选地域 : {}",
        filter.clone().unwrap_or_else(|| "不筛选（全部地域）".to_string())
    );
    println!();

    // ---------- 1. 原始响应（确认字段与文档一致） ----------
    let mut p = Params::new();
    if let Some(r) = &filter {
        p.set("Region", r.clone());
    }
    let raw = client.call("DescribeCompShareSupportZone", p, Scope::None)?;
    println!("--- 厂商原始响应 ---");
    println!(
        "{}",
        serde_json::to_string_pretty(&raw).unwrap_or_else(|_| raw.to_string())
    );
    println!();

    // ---------- 2. 结构化解析 ----------
    let resp = if filter.is_some() {
        client.zones_filtered(filter.as_deref())?
    } else {
        client.zones_filtered(Some(""))?
    };

    println!("--- 解析结果 ---");
    println!("RetCode      : {}", resp.ret_code);
    println!("request_uuid : {}", resp.request_uuid);
    println!("可用区数量   : {}", resp.zones.len());
    println!("去重地域     : {}", resp.regions().join(", "));
    println!();

    // ---------- 3. 逐个可用区明细 ----------
    println!(
        "{:<10} {:<12} {:<10} {:<12} {:<10} {:<8} {:<8} {}",
        "地域", "地域ID", "可用区", "可用区ID", "名称", "类型", "可部署", "不支持镜像类型"
    );
    println!("{}", "-".repeat(96));
    for z in &resp.zones {
        println!(
            "{:<10} {:<12} {:<10} {:<12} {:<10} {:<8} {:<8} {}",
            z.region,
            z.region_id,
            z.zone,
            z.zone_id,
            z.describe,
            z.resource_kind(),
            if z.is_deployable() { "✓" } else { "✗ 仅容器" },
            if z.unsupported_image_types.is_empty() {
                "-".to_string()
            } else {
                z.unsupported_image_types.join(", ")
            }
        );
    }
    println!();

    // ---------- 4. 校验 ----------
    println!("--- 适配校验 ---");
    let mut ok = true;

    // 字段完整性：地域与可用区名不能为空（否则无法用于其它接口）
    let fields_ok = resp.zones.iter().all(|z| !z.region.is_empty() && !z.zone.is_empty());
    println!(
        "  {} Region/Zone 字段完整",
        if fields_ok { "✓" } else { "✗" }
    );
    ok &= fields_ok;

    // ID 字段：文档承诺有值，实测应全部非 0
    let ids_ok = resp.zones.iter().all(|z| z.region_id != 0 && z.zone_id != 0);
    println!(
        "  {} RegionId/ZoneId 均有值（{} 个）",
        if ids_ok { "✓" } else { "✗" },
        resp.zones.len()
    );
    ok &= ids_ok;

    // 类型区分：应能正确识别虚机与容器
    let vm = resp.deployable();
    let pod = resp.zones.len() - vm.len();
    println!(
        "  ✓ 可用区分类：虚机 {} 个 / 容器 {} 个",
        vm.len(),
        pod
    );

    // 镜像兼容性：能识别「不支持 System 镜像」的可用区
    let sys_zones = resp
        .zones
        .iter()
        .filter(|z| z.supports_system_image())
        .count();
    println!(
        "  {} 镜像兼容性识别：{} 个可用区支持 System 镜像",
        if sys_zones > 0 { "✓" } else { "✗" },
        sys_zones
    );
    ok &= sys_zones > 0;

    // 自动选点：每个地域都应能挑出一个可用区，且优先满足镜像兼容
    let mut pick_ok = true;
    for region in resp.regions() {
        match client.pick_zone_for_image(region, "System")? {
            Some(z) => {
                let good = z.is_ready_for_deploy("System");
                println!(
                    "  {} pick_zone({}) → {} {}（{}{}）",
                    if good { "✓" } else { "!" },
                    region,
                    z.zone,
                    z.describe,
                    z.resource_kind(),
                    if good { "" } else { "，无系统镜像兼容区" }
                );
                // 只对「存在兼容区的地域」要求选中兼容区
                let has_compat = resp
                    .zones_in_region(region)
                    .iter()
                    .any(|x| x.is_ready_for_deploy("System"));
                if has_compat && !good {
                    pick_ok = false;
                }
            }
            None => {
                println!("  ✗ pick_zone({region}) 未找到可用区");
                pick_ok = false;
            }
        }
    }
    ok &= pick_ok;
    println!();

    // ---------- 5. 可直接用于配置的推荐值 ----------
    println!("--- 推荐配置（写入 providers.<name>.options） ---");
    // 只在「虚机 + 支持系统镜像」的可用区里选，华北二A 机型最全
    if let Some(best) = resp
        .zones
        .iter()
        .filter(|z| z.is_ready_for_deploy("System"))
        .max_by_key(|z| if z.region == "cn-wlcb" { 1 } else { 0 })
    {
        println!("region = \"{}\"", best.region);
        println!("zone   = \"{}\"", best.zone);
        println!(
            "（{} · {} · 支持系统镜像）",
            best.describe,
            best.resource_kind()
        );
    }
    println!();

    if ok {
        println!("结论：DescribeCompShareSupportZone 适配验证通过。");
        Ok(())
    } else {
        Err("适配校验存在未通过项".into())
    }
}
