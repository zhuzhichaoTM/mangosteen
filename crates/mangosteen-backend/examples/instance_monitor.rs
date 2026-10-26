//! 验证 `GetCompShareInstanceMonitor` 适配（真实调用）。
//!
//! 用法：
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! # 可选：不指定则自动取账号下第一个 Running 实例
//! export COMPSHARE_INSTANCE_ID='uhost-xxxx'
//! cargo run -p mangosteen-backend --example instance_monitor
//! ```
//!
//! ## 这个示例验证什么
//!
//! 1. 签名 / 参数编码 / 鉴权是否被厂商接受（`RetCode == 0`）；
//! 2. `UHostIds` 是否按数组正确传递（写成单数会被判参数错误）；
//! 3. 百分数 → 0~1 比例的换算是否正确（错用会出现「CPU 3500%」）；
//! 4. 多卡场景是否取到最忙的一张。
//!
//! 凭证只从环境变量读取，不落盘、不进源码。

use mangosteen_backend::{
    mapping, CompshareClient, CompshareConfig, Credential, CurlTransport, DEFAULT_ENDPOINT,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let public_key = std::env::var("COMPSHARE_PUBLIC_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or("请设置 COMPSHARE_PUBLIC_KEY")?;
    let private_key = std::env::var("COMPSHARE_PRIVATE_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or("请设置 COMPSHARE_PRIVATE_KEY")?;

    println!("=== 凭证 ===");
    println!("公钥 : {}", mask(&public_key));
    println!("私钥 : 已从环境变量读取（不显示）");
    println!();

    let credential = Credential::new(public_key, private_key)?;
    // 地域留空：先不限地域查实例，拿到实例真实地域再查监控。
    // 这正是驱动内部的做法——按配置地域硬查会把「地域不同」误判成「不存在」。
    let client =
        CompshareClient::new(DEFAULT_ENDPOINT, credential, CurlTransport::new(30), "", "")?;

    // ---- 1. 找到要监控的实例 ----
    println!("=== 1. 定位实例（不限地域） ===");
    let list = client.describe_instances_any_scope(None, 100, 0)?;
    println!("RetCode    : {}", list.ret_code);
    println!("实例总数   : {}", list.total_count);

    if list.uhost_set.is_empty() {
        println!();
        println!("账号下没有实例，无法验证监控接口。");
        println!("可先启动一台：`cargo run -p mangosteen-backend --example cheapest_up -- --all --launch`");
        return Ok(());
    }

    for u in &list.uhost_set {
        println!(
            "  {:<22} {:<10} {:<10} {} 卡  {}/{}",
            u.id, u.state, u.gpu_type, u.gpu, u.region, u.zone
        );
    }
    println!();

    let target = match std::env::var("COMPSHARE_INSTANCE_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        Some(id) => list
            .uhost_set
            .iter()
            .find(|u| u.id == id)
            .ok_or_else(|| format!("实例 {id} 不在列表中"))?,
        None => list
            .uhost_set
            .iter()
            .find(|u| u.state.eq_ignore_ascii_case("Running"))
            .or_else(|| list.uhost_set.first())
            .ok_or("没有可监控的实例")?,
    };

    println!("=== 2. 目标实例 ===");
    println!("ID         : {}", target.id);
    println!("状态       : {}", target.state);
    println!("地域/可用区: {}/{}", target.region, target.zone);
    println!("卡型       : {} × {}", target.gpu_type, target.gpu);
    println!("内存       : {} MB", target.memory_mb);
    println!();

    // ---- 2. 用实例自己的地域查监控（Region 必填且必须一致） ----
    let region = if target.region.trim().is_empty() {
        CompshareConfig::default().region
    } else {
        target.region.clone()
    };
    let zone = if target.zone.trim().is_empty() {
        CompshareConfig::default().zone
    } else {
        target.zone.clone()
    };
    let scoped = client.rebind(region.clone(), zone.clone())?;

    println!("=== 3. GetCompShareInstanceMonitor（Region={region}） ===");
    let ids = [target.id.clone()];
    let resp = scoped.instance_monitor(&ids)?;
    println!("RetCode    : {}", resp.ret_code);
    if resp.ret_code != 0 {
        println!("Message    : {}", resp.message);
        return Err("厂商返回非 0 返回码".into());
    }
    println!("返回实例数 : {}", resp.data.list.len());
    println!();

    let Some(inst) = resp.find(&target.id) else {
        println!("该实例暂无监控数据。");
        println!("刚开机的一两分钟内厂商通常还没有数据点，稍等再试。");
        return Ok(());
    };

    // ---- 3. 原始指标（厂商原文，百分数） ----
    println!("=== 4. 厂商原始指标 ===");
    println!(
        "{:<36} {:>10}  {:>10}  采样时间",
        "MetricKey", "最新值", "最大值"
    );
    println!("{}", "-".repeat(84));
    for m in &inst.metrics {
        let latest = m.latest().map(|v| v.value);
        let max = m.max_value();
        println!(
            "{:<36} {:>10}  {:>10}  {}",
            m.key,
            latest
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "—".into()),
            max.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into()),
            m.latest_timestamp()
                .map(|t| t.to_string())
                .unwrap_or_else(|| "—".into())
        );
    }
    println!();

    // ---- 4. 归一化结果（本项目统一表示） ----
    let metrics = mapping::to_instance_metrics(inst);
    println!("=== 5. 归一化结果（0~1 比例） ===");
    let row = |name: &str, v: Option<f64>| {
        println!(
            "{:<14} {}",
            name,
            v.map(|x| format!("{:>7.2}%", x * 100.0))
                .unwrap_or_else(|| "      —".into())
        );
    };
    row("CPU", metrics.cpu_usage);
    row("内存", metrics.memory_usage);
    row("GPU", metrics.gpu_usage);
    row("显存", metrics.gpu_memory_usage);
    row("系统盘", metrics.disk_usage);
    row("数据盘", metrics.data_disk_usage);
    if !metrics.extra.is_empty() {
        println!("未建模指标（原始百分数）：{:?}", metrics.extra);
    }
    println!("采样时间       : {}", metrics.sampled_at);
    println!();

    // ---- 5. 校验 ----
    println!("=== 6. 适配校验 ===");
    let mut ok = true;
    let check = |name: &str, pass: bool, detail: &str| {
        println!("  {} {:<22} {}", if pass { "✓" } else { "✗" }, name, detail);
        pass
    };

    ok &= check("RetCode 为 0", resp.ret_code == 0, "签名与鉴权通过");
    ok &= check(
        "按 ID 精确命中",
        inst.id == target.id,
        &format!("返回 {}", inst.id),
    );
    ok &= check(
        "至少一个已建模指标",
        metrics.cpu_usage.is_some() || metrics.gpu_usage.is_some(),
        "CPU/GPU 至少一项有值",
    );
    let in_range = [
        metrics.cpu_usage,
        metrics.memory_usage,
        metrics.gpu_usage,
        metrics.gpu_memory_usage,
        metrics.disk_usage,
        metrics.data_disk_usage,
    ]
    .iter()
    .flatten()
    .all(|v| (0.0..=1.0).contains(v));
    ok &= check(
        "比例在 0~1 内",
        in_range,
        "百分数已除以 100（否则会显示成 3500%）",
    );
    ok &= check(
        "采样时间合理",
        metrics.sampled_at > 1_600_000_000,
        &format!("{}", metrics.sampled_at),
    );
    println!();

    if ok {
        println!("结论：GetCompShareInstanceMonitor 适配验证通过。");
    } else {
        println!("结论：存在未通过的校验项，请检查对应映射逻辑。");
        return Err("适配校验未全部通过".into());
    }
    Ok(())
}

/// 凭证脱敏：只留前 6 位。
fn mask(s: &str) -> String {
    let n = s.chars().count();
    if n <= 6 {
        return "***".to_string();
    }
    format!("{}***", s.chars().take(6).collect::<String>())
}
