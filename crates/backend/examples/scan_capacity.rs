//! 全区域 GPU 库存扫描（`CheckCompShareResourceCapacity` 跨可用区批量调用）。
//!
//! 用法：
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! # 可选
//! export COMPSHARE_IMAGE_ID='compshareImage-xxxx'   # 不设则自动挑第一个 Available 的系统镜像
//! export COMPSHARE_CHARGE_TYPE='Postpay'            # Month / Day / Dynamic / Postpay / Spot
//! export COMPSHARE_GPU_TYPES='4090,5090'            # 只看这些卡型
//! cargo run -p mangosteen-backend --example scan_capacity
//! ```
//!
//! ## 这个接口为什么必须「矩阵式」调用
//!
//! 厂商的库存接口只回答「**某可用区 + 某卡型 + 某镜像 + 某计费模式**下，
//! 哪些 (GPU卡数, CPU核数, 内存GB) 组合有货」。它不接受通配符，
//! 所以要回答「全平台现在哪里能开到 4090」，只能逐个组合去问。
//!
//! 本示例的调用策略（约 5 + 3 + N 次请求）：
//!
//! 1. 查可用区（1 次）→ 过滤掉容器区与不支持系统镜像的区
//! 2. 每个可用区查机型表（每区 1 次）→ 只对该区真实存在的卡型发库存请求
//! 3. 逐 (可用区, 卡型) 查库存 → 汇总
//!
//! ## 安全说明
//!
//! 全程只读：`CheckCompShareResourceCapacity` 是**模拟调度**，不创建任何资源、
//! 不产生费用。产线环境可以放心反复跑。

use mangosteen_backend::{scan_all, CompshareClient, Credential, CurlTransport, ScanOptions};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let public_key = env("COMPSHARE_PUBLIC_KEY").ok_or("缺少 COMPSHARE_PUBLIC_KEY")?;
    let private_key = env("COMPSHARE_PRIVATE_KEY").ok_or("缺少 COMPSHARE_PRIVATE_KEY")?;
    let credential = Credential::new(public_key, private_key)?;

    let client = CompshareClient::new(
        mangosteen_backend::DEFAULT_ENDPOINT,
        credential,
        CurlTransport::new(40),
        "",
        "",
    )?;

    // ---------- 1. 确定镜像 ----------
    // 库存检查会拿镜像做模拟调度，镜像非法会让所有组合都「不足」，
    // 所以先挑一个确定 Available 的系统镜像。
    let image_id = match env("COMPSHARE_IMAGE_ID") {
        Some(id) => id,
        None => {
            // 只挑 Linux：Windows 镜像无法 SSH、跑不了推理（实测踩过）
            let imgs = client.images(Some("System"), 50, 0)?;
            mangosteen_backend::pick_linux_image(&imgs.image_set)
                .map(|i| i.id.clone())
                .ok_or("未找到可用的 Linux 系统镜像，请用 COMPSHARE_IMAGE_ID 显式指定")?
        }
    };

    let charge_type = env("COMPSHARE_CHARGE_TYPE").unwrap_or_else(|| "Postpay".to_string());
    let gpu_allowlist: Vec<String> = env("COMPSHARE_GPU_TYPES")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();

    println!("=== 全区域 GPU 库存扫描 ===");
    println!("镜像     : {image_id}");
    println!("计费模式 : {charge_type}");
    println!(
        "卡型过滤 : {}",
        if gpu_allowlist.is_empty() {
            "不限制（全部可售卡型）".to_string()
        } else {
            gpu_allowlist.join(", ")
        }
    );
    println!();
    println!("说明：CheckCompShareResourceCapacity 是模拟调度，全程只读，不产生费用。");
    println!();

    let opts = ScanOptions {
        charge_type,
        gpu_allowlist,
        ..Default::default()
    };

    // ---------- 2. 扫描 ----------
    let matrix = scan_all(&client, &image_id, &opts)?;

    // ---------- 3. 按卡型汇总 ----------
    println!("=== 按卡型汇总（全平台有货情况） ===");
    let by_type = matrix.by_gpu_type();
    if by_type.is_empty() {
        println!("  当前没有任何卡型有货。");
    } else {
        println!(
            "{:<10} {:<10} {:<28} 最小可用配置",
            "卡型", "有货区数", "有货可用区"
        );
        println!("{}", "-".repeat(92));
        for (gpu, entries) in &by_type {
            let zones: Vec<String> = entries.iter().map(|e| e.zone.clone()).collect();
            // 取全平台最小的可用配置
            let smallest = entries
                .iter()
                .filter_map(|e| e.smallest_available())
                .min_by_key(|s| (s.gpu, s.cpu, s.mem_gb));
            let spec_desc = smallest
                .map(|s| format!("{}卡 / {}核 / {}GB", s.gpu, s.cpu, s.mem_gb))
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<10} {:<8} {:<28} {}",
                gpu,
                zones.len(),
                zones.join(", "),
                spec_desc
            );
        }
    }
    println!();

    // ---------- 4. 按可用区明细 ----------
    println!("=== 按可用区明细 ===");
    let by_region = matrix.by_region();
    if by_region.is_empty() {
        println!("  没有可用区报告有货。");
    } else {
        for (region, entries) in &by_region {
            println!("[{region}]");
            for e in entries {
                let avail = e.available_specs();
                println!(
                    "  {:<12} {:<10} 有货组合 {} 个",
                    e.zone,
                    e.gpu_type,
                    avail.len()
                );
                for s in avail.iter().take(4) {
                    println!("      {}卡 / {}核 / {}GB", s.gpu, s.cpu, s.mem_gb);
                }
                if avail.len() > 4 {
                    println!("      …还有 {} 个组合", avail.len() - 4);
                }
            }
            println!();
        }
    }

    // ---------- 5. 明细矩阵（全部组合，含缺货） ----------
    println!("=== 详细矩阵（含缺货，按可用区 + 卡型） ===");
    println!(
        "{:<10} {:<12} {:<10} {:<10} 可用组合数",
        "地域", "可用区", "卡型", "状态"
    );
    println!("{}", "-".repeat(74));
    for e in &matrix.entries {
        let state = match &e.error {
            Some(err) => format!("失败：{err}"),
            None if e.has_any() => "有货".to_string(),
            None => "缺货".to_string(),
        };
        println!(
            "{:<10} {:<12} {:<10} {:<10} {}",
            e.region,
            e.zone,
            e.gpu_type,
            if state.starts_with("失败") {
                "查询失败"
            } else {
                &state
            },
            e.available_specs().len()
        );
        if let Some(err) = &e.error {
            println!("      → {err}");
        }
    }
    println!();

    // ---------- 6. 跳过的组合 ----------
    if !matrix.skipped.is_empty() {
        println!("=== 已跳过（不发起库存查询） ===");
        for s in &matrix.skipped {
            println!("  · {s}");
        }
        println!();
    }

    // ---------- 7. 统计与推荐 ----------
    println!("=== 统计 ===");
    println!("查询组合      : {} 个", matrix.entries.len());
    println!("有货          : {} 个", matrix.in_stock_count());
    println!(
        "缺货          : {} 个",
        matrix.entries.len() - matrix.in_stock_count() - matrix.failed_count()
    );
    println!("查询失败      : {} 个", matrix.failed_count());
    println!("跳过          : {} 项", matrix.skipped.len());
    let out_of_stock = matrix.out_of_stock_types();
    if !out_of_stock.is_empty() {
        println!("全平台缺货卡型 : {}", out_of_stock.join(", "));
    }
    println!();

    match matrix.recommend() {
        Some(best) => {
            println!("=== 推荐（性价比优先：消费级卡 + 最小可用配置） ===");
            println!("region    = \"{}\"", best.region);
            println!("zone      = \"{}\"  （{}）", best.zone, best.zone_name);
            println!("gpu_type  = \"{}\"", best.gpu_type);
            if let Some(s) = best.smallest_available() {
                println!(
                    "规格      = {}卡 / {}核 / {}GB  → 规格 ID `{}/{}x{}c/{}g`",
                    s.gpu, s.cpu, s.mem_gb, best.gpu_type, s.gpu, s.cpu, s.mem_gb
                );
            }
            println!();
            println!("可直接用于配置：");
            println!("  [providers.compshare.options]");
            println!("  region = \"{}\"", best.region);
            println!("  zone   = \"{}\"", best.zone);
            println!("  image_id = \"{image_id}\"");
        }
        None => {
            println!("=== 推荐 ===");
            println!("当前无任何可用组合。可考虑：");
            println!("  · 换计费模式重试（Spot / Dynamic 与 Postpay 库存池不同）");
            println!("  · 放宽卡型过滤（当前：{:?}）", opts.gpu_allowlist);
            println!("  · 稍后重试（库存是动态的）");
        }
    }
    println!();

    // ---------- 8. 计费模式提示 ----------
    // 实测：Spot 与 Postpay/Dynamic/Month/Day 的库存池不同，
    // 换模式可能是「查到货」的关键。
    println!("=== 提示 ===");
    println!(
        "本次使用计费模式 `{}`。库存结果随计费模式变化：",
        opts.charge_type
    );
    println!("  · Postpay / Dynamic / Month / Day 通常共享同一库存池");
    println!("  · Spot（抢占式）库存池独立，可能整片为空");
    println!("若目标卡型显示缺货，可用 COMPSHARE_CHARGE_TYPE=Postpay 复测一次。");
    println!();
    println!("另外：CheckCompShareResourceCapacity 是模拟调度接口，");
    println!("它只证明「此刻能调度到」，不锁定资源——真正下单前仍可能被别人抢占。");

    Ok(())
}
