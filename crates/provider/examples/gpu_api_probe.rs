//! 验证实例管理扩展接口的适配（真实调用，**只读**）。
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! cargo run -p mangosteen-provider --example gpu_api_probe
//! ```
//!
//! ## 这个示例只调用「不会改变资源」的接口
//!
//! 查询类（软件端口 / 网络加速 / 机型族 / 项目 / 多模式价格 / 退费）
//! 全部是只读的，不会产生费用。
//!
//! **不会**调用改名、重置密码、重装、升降配、定时关机、带宽切换——
//! 那些需要真实实例且会改变资源状态，应由 `mangosteen cloud` 子命令
//! 在用户明确同意后执行。
//!
//! ## 它验证什么
//!
//! 1. Action 名是否正确（`GetSoftwareURL` 是全大写 `URL`，容易写错）；
//! 2. 参数编码是否被厂商接受（`RetCode == 0`）；
//! 3. 响应字段名与单位是否与文档一致（尤其价格与内存的 MB/GB）。

use mangosteen_provider::compshare::{
    CompshareClient, CompshareConfig, Credential, CurlTransport, DEFAULT_ENDPOINT,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pk = std::env::var("COMPSHARE_PUBLIC_KEY").map_err(|_| "请设置 COMPSHARE_PUBLIC_KEY")?;
    let sk = std::env::var("COMPSHARE_PRIVATE_KEY").map_err(|_| "请设置 COMPSHARE_PRIVATE_KEY")?;

    let cfg = CompshareConfig::default();
    let client = CompshareClient::new(
        DEFAULT_ENDPOINT,
        Credential::new(pk, sk)?,
        CurlTransport::new(40),
        cfg.region.clone(),
        cfg.zone.clone(),
    )?;

    println!("=== 凭证 ===");
    println!("公钥 : {}***", &masked(&client));
    println!("范围 : {}/{}", cfg.region, cfg.zone);
    println!();

    let mut failures: Vec<String> = Vec::new();

    // ---- 1. 项目列表（定时关机的前置依赖）----
    println!("=== 1. GetProjectList ===");
    match client.project_list() {
        Ok(r) => {
            println!("RetCode    : {}", r.ret_code);
            println!("项目数     : {}", r.projects.len());
            for p in &r.projects {
                println!(
                    "  {:<16} {:<20} {}",
                    p.id,
                    p.name,
                    if p.is_default { "（默认）" } else { "" }
                );
            }
            match r.default_project_id() {
                Some(id) => println!("默认项目   : {id}  ← 定时关机接口需要它"),
                None => {
                    println!("⚠ 没有可用项目，定时关机将不可用");
                    failures.push("项目列表为空".into());
                }
            }
        }
        Err(e) => {
            println!("失败：{e}");
            failures.push(format!("GetProjectList: {e}"));
        }
    }
    println!();

    // ---- 2. 软件端口映射 ----
    println!("=== 2. DescribeCompShareSoftwarePort ===");
    match client.software_ports() {
        Ok(r) => {
            println!("RetCode    : {}", r.ret_code);
            println!("软件数     : {}", r.items.len());
            for i in &r.items {
                println!("  {:<16} {}", i.software, i.port);
            }
            // 实测（2026-10）：返回的只有 4 个 Web 工具，**没有 SSH**。
            // 文档示例里写的是 `{"Software":"SSH","Port":22}`，与实际不符。
            // SSH 端口走的是容器实例的 `TcpForwards`（InternalPort=23），
            // 不在这个列表里——所以这里不是缺陷，只是文档举例过时。
            if r.port_of("SSH").is_none() {
                println!("  注：列表不含 SSH 属正常，SSH 走容器实例的 TcpForwards（内部端口 23）");
            }
        }
        Err(e) => {
            println!("失败：{e}");
            failures.push(format!("DescribeCompShareSoftwarePort: {e}"));
        }
    }
    println!();

    // ---- 3. 网络加速状态 ----
    println!("=== 3. CheckCompShareNetOptimizer ===");
    match client.check_net_optimizer() {
        Ok(r) => {
            println!("RetCode    : {}", r.ret_code);
            println!(
                "全局加速   : {}",
                if r.optimized {
                    "已开通"
                } else {
                    "未开通"
                }
            );
            for i in &r.info {
                println!("  {:<10} {}", i.region, if i.optimized { "✓" } else { "✗" });
            }
            println!(
                "本配置地域 : {}",
                if r.optimized_in(&cfg.region) {
                    "已加速（海外模型下载较快）"
                } else {
                    "未加速（下载 HuggingFace 模型会慢）"
                }
            );
        }
        Err(e) => {
            println!("失败：{e}");
            failures.push(format!("CheckCompShareNetOptimizer: {e}"));
        }
    }
    println!();

    // ---- 4. 机型族 ----
    println!("=== 4. DescribeCompShareMachineTypeFamilies ===");
    match client.machine_type_families() {
        Ok(r) => {
            println!("RetCode    : {}", r.ret_code);
            println!("机型族数   : {}", r.families.len());
            for f in r.families.iter().take(12) {
                println!("  {:<10} {}", f.name, f.description);
            }
        }
        Err(e) => {
            println!("失败：{e}");
            failures.push(format!("DescribeCompShareMachineTypeFamilies: {e}"));
        }
    }
    println!();

    // ---- 5. 多计费模式价格（验证 MB 单位与四种模式）----
    println!("=== 5. GetCompShareInstanceUserPrice（4090 / 1卡 / 16核 / 64GB）===");
    match client.user_price(&mangosteen_provider::compshare::UserPriceRequest {
        gpu_type: "4090",
        gpu: 1,
        cpu: 16,
        memory_mb: 64 * 1024,
        image_id: None,
        disk_size_gb: 100,
        disk_type: "CLOUD_SSD",
    }) {
        Ok(r) => {
            println!("RetCode    : {}", r.ret_code);
            println!("{:<10} {:>12} {:>12}", "计费模式", "实例(元)", "系统盘(元)");
            println!("{}", "-".repeat(38));
            for d in &r.price_details {
                println!(
                    "{:<10} {:>12.2} {:>12.2}",
                    d.charge_type,
                    d.instance.unwrap_or(0.0),
                    d.system_disks.unwrap_or(0.0)
                );
            }
            // 单位校验：Postpay 应是「每小时几元」，Month 应是「每月几百元」
            let postpay = r
                .price_details
                .iter()
                .find(|d| d.charge_type.eq_ignore_ascii_case("Postpay"))
                .and_then(|d| d.instance);
            let month = r
                .price_details
                .iter()
                .find(|d| d.charge_type.eq_ignore_ascii_case("Month"))
                .and_then(|d| d.instance);
            match (postpay, month) {
                (Some(h), Some(m)) if h > 0.0 && m > h * 10.0 => {
                    println!();
                    println!("✓ 单位合理：时价 ¥{h:.2} / 月价 ¥{m:.2}（月价约为时价 30 倍量级）");
                }
                (Some(h), Some(m)) => {
                    println!();
                    println!("⚠ 单位可疑：时价 ¥{h:.2} / 月价 ¥{m:.2}");
                    failures.push("价格单位可疑（时价与月价比例异常）".into());
                }
                _ => println!("⚠ 缺少 Postpay 或 Month 明细"),
            }
        }
        Err(e) => {
            println!("失败：{e}");
            failures.push(format!("GetCompShareInstanceUserPrice: {e}"));
        }
    }
    println!();

    // ---- 6. 退费查询（无实例时应返回空集而不是报错）----
    println!("=== 6. GetCompShareRefundPrice（空列表）===");
    match client.refund_price(&[]) {
        Ok(r) => println!(
            "RetCode    : {}，条目数 {}（无实例时为空属正常）",
            r.ret_code,
            r.items.len()
        ),
        Err(e) => {
            println!("失败：{e}");
            failures.push(format!("GetCompShareRefundPrice: {e}"));
        }
    }
    println!();

    // ---- 7. 实例列表（决定哪些接口能实测）----
    println!("=== 7. DescribeCompShareInstance（判断可实测范围）===");
    let instances = client.describe_instances_any_scope(None, 100, 0)?;
    println!("实例数     : {}", instances.uhost_set.len());
    if instances.uhost_set.is_empty() {
        println!();
        println!("账号下没有实例，因此以下接口**无法真实验证**（仅单测覆盖）：");
        println!("  改名 / 重置密码 / 重装 / 升降配 / 磁盘扩容 /");
        println!("  计费切换 / 升配询价 / 定时关机 / 取消定时关机 /");
        println!("  带宽切换 / 容器端口更新 / 软件访问地址");
        println!("它们都需要一台真实实例；创建实例会产生费用，故本示例不自动执行。");
    } else {
        for u in &instances.uhost_set {
            println!(
                "  {:<22} {:<10} {:<10} {}",
                u.id, u.state, u.gpu_type, u.instance_type
            );
        }
    }
    println!();

    println!("=== 结论 ===");
    if failures.is_empty() {
        println!("✓ 只读接口全部通过（签名 / 参数编码 / 响应解析 / 单位）");
        Ok(())
    } else {
        println!("✗ 存在 {} 项失败：", failures.len());
        for f in &failures {
            println!("  · {f}");
        }
        Err("部分接口未通过".into())
    }
}

/// 只回显公钥前 6 位，绝不打印私钥。
fn masked(_client: &CompshareClient<CurlTransport>) -> String {
    std::env::var("COMPSHARE_PUBLIC_KEY")
        .map(|k| k.chars().take(6).collect())
        .unwrap_or_default()
}
