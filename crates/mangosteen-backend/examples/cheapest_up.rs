//! 找出最便宜的可用 GPU 并启动它。
//!
//! 支持两种查询范围：
//!
//! ```bash
//! # 全区（默认）：扫所有可用区，找全平台最便宜
//! cargo run -p mangosteen-backend --example cheapest_up -- --all
//!
//! # 单可用区：只看某个区
//! cargo run -p mangosteen-backend --example cheapest_up -- --zone cn-wlcb/cn-wlcb-01
//! ```
//!
//! 环境变量：
//!
//! | 变量 | 说明 |
//! |------|------|
//! | `COMPSHARE_PUBLIC_KEY` | 必填，AccessKey ID |
//! | `COMPSHARE_PRIVATE_KEY` | 必填，AccessKey Secret |
//! | `COMPSHARE_CHARGE_TYPE` | 计费模式，默认 `Postpay` |
//! | `COMPSHARE_GPU_TYPES` | 卡型白名单，如 `4090,5090` |
//! | `COMPSHARE_IMAGE_ID` | 镜像 ID；不设则自动挑第一个 Available 的系统镜像 |
//! | `COMPSHARE_MAX_ATTEMPTS` | 最多尝试启动几台（默认 3） |
//!
//! ## 两种运行模式
//!
//! * **默认（dry-run）**：只扫描 + 查价，打印最便宜的候选，**不创建任何资源**；
//! * **`--launch`**：在 dry-run 结果基础上真正创建并等待 Running。
//!
//! 刻意把「查」与「开」分开：GPU 实例按小时计费，误开一台就是真金白银。
//! 必须先让用户看到「要开哪台、多少钱」，再加 `--launch` 确认。

use std::time::Instant;

use mangosteen_backend::{
    price_quotes, provision_cheapest, scan, CompshareClient, Credential, CurlTransport,
    ScanOptions, ScanScope,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---------- 参数 ----------
    let args: Vec<String> = std::env::args().skip(1).collect();
    let launch = args.iter().any(|a| a == "--launch");
    let mut scope = ScanScope::AllZones;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--zone" {
            if let Some(v) = args.get(i + 1) {
                let (r, z) = v
                    .split_once('/')
                    .ok_or("--zone 格式应为 region/zone，如 cn-wlcb/cn-wlcb-01")?;
                scope = ScanScope::zone(r, z);
            }
            i += 2;
            continue;
        }
        if args[i] == "--all" {
            scope = ScanScope::AllZones;
        }
        i += 1;
    }

    // ---------- 凭证 ----------
    let credential = Credential::new(
        env("COMPSHARE_PUBLIC_KEY").ok_or("缺少 COMPSHARE_PUBLIC_KEY")?,
        env("COMPSHARE_PRIVATE_KEY").ok_or("缺少 COMPSHARE_PRIVATE_KEY")?,
    )?;
    let client = CompshareClient::new(
        mangosteen_backend::DEFAULT_ENDPOINT,
        credential,
        CurlTransport::new(40),
        "",
        "",
    )?;

    // ---------- 镜像 ----------
    // 必须挑 Linux 镜像：实测自动挑「第一个 Available」会选到 Windows，
    // 开出来无法 SSH、跑不了 llama.cpp。
    let image_id = match env("COMPSHARE_IMAGE_ID") {
        Some(id) => id,
        None => {
            let imgs = client.images(Some("System"), 50, 0)?;
            mangosteen_backend::pick_linux_image(&imgs.image_set)
                .map(|i| i.id.clone())
                .ok_or(
                    "未找到可用的 Linux 系统镜像（Windows 镜像无法 SSH/跑推理）。\
                     请用 COMPSHARE_IMAGE_ID 显式指定，例如 Ubuntu-nvidia 系列",
                )?
        }
    };

    let charge_type = env("COMPSHARE_CHARGE_TYPE").unwrap_or_else(|| "Postpay".to_string());
    let opts = ScanOptions {
        charge_type: charge_type.clone(),
        gpu_allowlist: env("COMPSHARE_GPU_TYPES")
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    };

    println!("=== 查找最便宜的可用 GPU ===");
    println!(
        "查询范围 : {}",
        match &scope {
            ScanScope::AllZones => "全区（所有可用区）".to_string(),
            ScanScope::Zone { region, zone } => format!("单可用区 {region}/{zone}"),
        }
    );
    println!("镜像     : {image_id}");
    println!("计费模式 : {charge_type}");
    println!(
        "卡型过滤 : {}",
        if opts.gpu_allowlist.is_empty() {
            "不限制".to_string()
        } else {
            opts.gpu_allowlist.join(", ")
        }
    );
    println!("模式     : {}", if launch { "真实启动（--launch）" } else { "仅查询（dry-run）" });
    println!();

    // 计费提醒
    if launch {
        println!("⚠ 即将创建真实实例并开始计费。");
        println!();
    }

    let started = Instant::now();

    // ---------- 1. 扫描库存 ----------
    let matrix = scan(&client, &scope, &image_id, &opts)?;
    println!(
        "库存扫描完成：查询 {} 个组合，有货 {} 个，跳过 {} 项（耗时 {:.1}s）",
        matrix.entries.len(),
        matrix.in_stock_count(),
        matrix.skipped.len(),
        started.elapsed().as_secs_f64()
    );
    for s in &matrix.skipped {
        println!("  · 跳过：{s}");
    }
    println!();

    let quotable = matrix.quotable_specs();
    if quotable.is_empty() {
        println!("没有可用的 GPU 规格。建议：");
        println!("  · 换计费模式（Spot 库存池独立，可能全空）");
        println!("  · 换可用区（部分区全部缺货）");
        return Ok(());
    }

    // ---------- 2. 查真实价格 ----------
    let board = price_quotes(
        &client,
        &quotable,
        &charge_type,
        &opts.disk_type,
        opts.disk_size_gb,
    );
    println!(
        "价格查询完成：成功 {} 个，失败 {} 个",
        board.options.len(),
        board.failed.len()
    );
    for (s, e) in board.failed.iter().take(5) {
        println!("  · {} 查价失败：{e}", s.spec_id());
    }
    println!();

    if board.options.is_empty() {
        println!("所有有货配置都无法取得价格，无法比较。");
        return Ok(());
    }

    // ---------- 3. 价格榜单 ----------
    println!("=== 价格榜单（元/小时，从低到高） ===");
    println!(
        "{:<4} {:<22} {:<12} {:<18} {:<9} {}",
        "排名", "规格 ID", "可用区", "卡型/显存", "时价", "月成本估算"
    );
    println!("{}", "-".repeat(96));
    for (rank, o) in board.options.iter().take(15).enumerate() {
        let vram = mangosteen_backend::known_vram_gb(&o.spec.gpu_type) * o.spec.gpu as u64;
        println!(
            "{:<4} {:<22} {:<12} {:<18} ¥{:<8.3} ¥{:.0}",
            rank + 1,
            o.spec.spec_id(),
            o.spec.zone,
            format!("{} · {}GiB", o.spec.gpu_type, vram),
            o.per_hour_cny,
            o.per_hour_cny * 24.0 * 30.0
        );
    }
    if board.options.len() > 15 {
        println!("…（共 {} 个候选）", board.options.len());
    }
    println!();

    // ---------- 4. 性价比视角 ----------
    println!("=== 性价比（元 / GiB显存·小时，从优到劣） ===");
    let by_eff = board.sorted_by_vram_efficiency(|t| mangosteen_backend::known_vram_gb(t));
    for o in by_eff.iter().take(8) {
        let per_gib = o.per_hour_cny
            / (mangosteen_backend::known_vram_gb(&o.spec.gpu_type).max(1) as f64);
        println!(
            "  {:<10} {:<22} ¥{:.4}/GiB·h   时价 ¥{:.3}",
            o.spec.gpu_type,
            o.spec.spec_id(),
            per_gib,
            o.per_hour_cny
        );
    }
    println!();

    // ---------- 5. 最便宜 ----------
    let cheapest = board.cheapest().ok_or("无有效报价")?;
    println!("=== 最便宜的可用 GPU ===");
    println!("规格 ID   : {}", cheapest.spec.spec_id());
    println!(
        "位置      : {}/{}（{}）",
        cheapest.spec.region, cheapest.spec.zone, cheapest.spec.zone_name
    );
    println!(
        "配置      : {} 卡 {} · {} 核 · {} GB 内存",
        cheapest.spec.gpu, cheapest.spec.gpu_type, cheapest.spec.cpu, cheapest.spec.mem_gb
    );
    println!("时价      : ¥{:.4} / 小时", cheapest.per_hour_cny);
    println!(
        "月成本估算: ¥{:.0}（按 30×24 小时连续运行）",
        cheapest.per_hour_cny * 24.0 * 30.0
    );
    println!("计费模式  : {}", cheapest.charge_type);
    println!("价格构成  : {}", cheapest.breakdown);
    println!();

    // ---------- 6. 启动 ----------
    if !launch {
        println!("=== 未启动（dry-run） ===");
        println!("确认无误后加 --launch 真正创建实例：");
        println!("  cargo run -p mangosteen-backend --example cheapest_up -- --launch");
        println!();
        println!("提醒：创建的实例会持续计费。用完务必销毁：");
        println!("  （示例未内置销毁；请用控制台或后续命令释放）");
        return Ok(());
    }

    let max_attempts: usize = env("COMPSHARE_MAX_ATTEMPTS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);

    println!("=== 开始启动（最多尝试 {max_attempts} 台，从最便宜开始） ===");
    let launched = provision_cheapest(&client, &scope, &image_id, &opts, max_attempts)?;

    let inst = &launched.instance;
    println!();
    println!("=== 启动成功 ===");
    println!("实例 ID   : {}", inst.id);
    println!("状态      : {}", inst.state);
    println!("服务商    : {}", inst.provider);
    println!(
        "规格      : {}×{} · {} 核 · {} GiB 内存 · 显存 {} GiB",
        inst.spec.gpu.count,
        inst.spec.gpu.model,
        inst.spec.cpu_cores,
        inst.spec.memory_bytes >> 30,
        inst.spec.gpu.total_vram_bytes() >> 30
    );
    println!(
        "SSH       : {}",
        match (&inst.network.ssh_host, inst.network.ssh_port) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.clone(),
            _ => "-".to_string(),
        }
    );
    println!("实际价格  : ¥{:.4} / 小时", launched.per_hour_cny);
    if !launched.sold_out.is_empty() {
        println!("已跳过（下单前复核发现售罄）:");
        for s in &launched.sold_out {
            println!("  · {s}");
        }
    }
    if launched.tried.len() > 1 {
        println!("尝试过程  :");
        for (i, t) in launched.tried.iter().enumerate() {
            println!("  {}. {t}", i + 1);
        }
    }
    println!();
    println!("⚠ 该实例正在计费。用完请及时释放，避免持续扣费。");

    if let Some(display) = &inst.display_name {
        println!("实例名    : {display}");
    }

    Ok(())
}
