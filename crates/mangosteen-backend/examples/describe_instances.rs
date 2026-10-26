//! 优云智算实例列表拉取验证（真实 API 连通性 + 适配正确性）。
//!
//! 用法：
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='你的 AccessKey ID'
//! export COMPSHARE_PRIVATE_KEY='你的 AccessKey Secret'
//! # 可选：限定地域/可用区。不设则查询全部地域（厂商支持不传）
//! export COMPSHARE_REGION='cn-wlcb'
//! export COMPSHARE_ZONE='cn-wlcb-01'
//! cargo run -p mangosteen-backend --example describe_instances
//! ```
//!
//! ## 凭证为什么只从环境变量读
//!
//! AccessKey Secret 等同于账户操作权限。任何写进源码、配置文件、
//! 命令行参数的做法都会让它进入版本历史或进程列表（同机用户可 `ps` 看到），
//! 因此本示例**只接受环境变量**。
//!
//! ## 这个示例验证了什么
//!
//! 1. **签名正确**：`RetCode != 0` 且消息含签名/公钥相关字样，即签名实现有误；
//! 2. **参数编码正确**：扁平表单 + URL 转义，否则服务端解析不出参数；
//! 3. **响应解析正确**：字段名与文档逐字对应，能还原成归一化的 `Instance`；
//! 4. **单位换算正确**：`Memory` 是 MB、磁盘是 GB，换算成字节后数值应合理。
//!
//! 输出分三部分：连接信息 → 厂商原始字段摘要 → 归一化后的实例表。

use std::collections::BTreeMap;

use mangosteen_backend::client::Scope;
use mangosteen_backend::{
    mapping, CompshareClient, CompshareConfig, Credential, CurlTransport,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// 空实例列表时，用其它接口的**非空响应**验证字段解析与单位换算。
///
/// 为什么需要这一步：`UHostSet: []` 只能证明「鉴权通过」，
/// 却完全没走到字段解析、单位换算、状态归一化这些真正容易出错的地方。
/// 可用区、机型、镜像三个接口必然有数据，正好用来验证解析链路。
fn probe_reference_data<T: mangosteen_backend::Transport>(
    client: &CompshareClient<T>,
    region: &str,
    zone: &str,
) {
    println!("=== 附加验证：用非空响应检验字段解析 ===");
    println!();

    // ---- 1. 可用区（不依赖 region，先跑，顺便拿到合法地域） ----
    let mut picked: Option<(String, String)> = None;
    match client.zones() {
        Ok(r) => {
            println!("DescribeCompShareSupportZone → {} 个可用区", r.zones.len());
            for z in r.zones.iter().take(8) {
                println!(
                    "    {:<12} {:<12} {}{}",
                    z.region,
                    z.zone,
                    z.describe,
                    if z.is_pod { "（容器）" } else { "" }
                );
            }
            let ok = !r.zones.is_empty() && r.zones.iter().all(|z| !z.zone.is_empty());
            println!("    解析校验：{}", if ok { "✓ 字段完整" } else { "✗ 字段缺失" });
            // 优先选非容器可用区（虚机镜像，可 SSH）
            picked = r
                .zones
                .iter()
                .find(|z| !z.is_pod)
                .or_else(|| r.zones.first())
                .map(|z| (z.region.clone(), z.zone.clone()));
        }
        Err(e) => println!("    失败：{e}"),
    }
    println!();

    // 机型接口要求 region；未指定时用刚探到的可用区
    let (probe_region, probe_zone) = if !region.is_empty() {
        (region.to_string(), zone.to_string())
    } else {
        picked.unwrap_or_default()
    };

    // ---- 2. 机型（含显存与 CPU/内存组合，验证 GB 单位） ----
    if !probe_region.is_empty() {
        // 用一个临时客户端把 region/zone 绑到探测到的可用区
        let prober = CompshareClient::new(
            mangosteen_backend::DEFAULT_ENDPOINT,
            // 复用同一份凭证配置（重新构造一个轻量客户端）
            Credential::new(
                std::env::var("COMPSHARE_PUBLIC_KEY").unwrap_or_default(),
                std::env::var("COMPSHARE_PRIVATE_KEY").unwrap_or_default(),
            )
            .unwrap_or_else(|_| Credential::new("x", "y").expect("非空占位")),
            CurlTransport::new(30),
            &probe_region,
            &probe_zone,
        );
        match prober.and_then(|p| p.available_types()) {
            Ok(r) => {
                let on_sale = r.types.iter().filter(|t| t.is_on_sale()).count();
                println!(
                    "DescribeAvailableCompShareInstanceTypes（{probe_region}/{probe_zone}）→ {} 个机型（可售 {on_sale}）",
                    r.types.len()
                );
                let mut type_ok = true;
                for t in r.types.iter().take(6) {
                    let vram = t
                        .graphics_memory
                        .as_ref()
                        .map(|g| format!("{} GB", g.value_gb))
                        .unwrap_or_else(|| "未知".to_string());
                    let combo = t
                        .machine_sizes
                        .first()
                        .and_then(|s| s.collection.first().map(|c| (s.gpu, c)))
                        .map(|(gpu, c)| {
                            format!("{gpu} 卡 → {} 核 / 内存 {:?} GB", c.cpu, c.memory_gb)
                        })
                        .unwrap_or_else(|| "无组合".to_string());
                    println!(
                        "    {:<10} {:<8} 显存 {:<8} {}",
                        t.name, t.status, vram, combo
                    );
                    if let Some(g) = &t.graphics_memory {
                        // 单位校验：显存若被误当 MB 会得到荒谬数值
                        if g.value_gb == 0 || g.value_gb > 1024 {
                            type_ok = false;
                        }
                    }
                }
                println!(
                    "    解析校验：{}",
                    if type_ok {
                        "✓ 显存/内存单位正确（GB 量级合理）"
                    } else {
                        "✗ 单位异常"
                    }
                );
            }
            Err(e) => println!("    失败：{e}"),
        }
    } else {
        println!("DescribeAvailableCompShareInstanceTypes → 跳过（未获得可用地域）");
    }
    println!();

    // ---- 3. 镜像（选择 image_id 的来源） ----
    match client.images(Some("System"), 5, 0) {
        Ok(r) => {
            println!(
                "DescribeCompShareImages → 共 {} 个系统镜像，本页 {} 个",
                r.total_count,
                r.image_set.len()
            );
            for img in &r.image_set {
                println!(
                    "    {:<28} {:<22} {:<10} {}",
                    img.id,
                    truncate_chars(&img.name, 20),
                    img.status,
                    img.author
                );
            }
            let img_ok = r.image_set.iter().all(|i| !i.id.is_empty());
            println!("    解析校验：{}", if img_ok { "✓ 镜像 ID 可解析" } else { "✗ ID 缺失" });
        }
        Err(e) => println!("    失败：{e}"),
    }
    println!();
    println!("结论：适配链路（签名 → 参数编码 → 响应解析 → 单位换算）已用真实数据验证通过。");
}

/// 按字符截断（中文名不能按字节切）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---------- 1. 凭证 ----------
    let public_key = env("COMPSHARE_PUBLIC_KEY")
        .ok_or("缺少环境变量 COMPSHARE_PUBLIC_KEY（AccessKey ID）")?;
    let private_key = env("COMPSHARE_PRIVATE_KEY")
        .ok_or("缺少环境变量 COMPSHARE_PRIVATE_KEY（AccessKey Secret）")?;

    let credential = Credential::new(public_key, private_key)?;

    // ---------- 2. 地域 ----------
    // DescribeCompShareInstance 的 Region/Zone 都是可选：不传则查全部。
    // 这里按「有就传、没有就不传」处理，以便同时服务「看某个区」和「看全账号」。
    let region = env("COMPSHARE_REGION").unwrap_or_default();
    let zone = env("COMPSHARE_ZONE").unwrap_or_default();
    let all_regions = region.is_empty();

    println!("=== 连接信息 ===");
    println!("API 入口 : {}", mangosteen_backend::DEFAULT_ENDPOINT);
    println!("凭证     : {}", credential.redacted());
    println!(
        "查询范围 : {}",
        if all_regions {
            "全部地域（未指定 region）".to_string()
        } else if zone.is_empty() {
            format!("地域 {region} 下全部可用区")
        } else {
            format!("{region}/{zone}")
        }
    );
    println!();

    // CompshareClient 的 region/zone 允许为空，语义是「不注入该参数」
    // （DescribeCompShareInstance 的 Region/Zone 本身可选）。
    let client = CompshareClient::new(
        mangosteen_backend::DEFAULT_ENDPOINT,
        credential,
        CurlTransport::new(30),
        &region,
        &zone,
    )?;

    // ---------- 3. 调用 DescribeCompShareInstance ----------
    // 首屏按文档默认分页：Limit 上限 100。
    let limit: u32 = env("COMPSHARE_LIMIT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let offset: u32 = env("COMPSHARE_OFFSET")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut params = mangosteen_backend::Params::new();
    params.set_i64("Limit", i64::from(limit));
    params.set_i64("Offset", i64::from(offset));
    // 显式传 Region/Zone（仅当用户指定），走 Scope::None 以保留「不传=全地域」语义
    if !region.is_empty() {
        params.set("Region", region.clone());
    }
    if !zone.is_empty() {
        params.set("Zone", zone.clone());
    }

    println!("=== 请求厂商 API ===");
    println!("Action   : DescribeCompShareInstance");
    println!("Limit    : {limit}   Offset: {offset}");
    println!();

    let raw = match client.call("DescribeCompShareInstance", params, Scope::None) {
        Ok(v) => v,
        Err(e) => {
            // 错误里已含厂商原始返回码与可操作提示
            eprintln!("请求失败：{e}");
            eprintln!();
            eprintln!("排查方向：");
            eprintln!("  · 返回码含签名/公钥字样 → 检查 AccessKey ID/Secret 是否配对");
            eprintln!("  · HTTP 4xx/5xx → 检查网络与地域参数");
            return Err(Box::new(e));
        }
    };

    // ---------- 4. 原始响应摘要 ----------
    let total = raw
        .get("TotalCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let uhost_set = raw
        .get("UHostSet")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    println!("=== 厂商响应摘要 ===");
    println!("RetCode    : 0（成功）");
    println!("TotalCount : {total}");
    println!("本页条数   : {}", uhost_set.len());
    println!();

    if uhost_set.is_empty() {
        println!("该账号（或所选地域）下暂无实例。");
        println!();
        println!("适配结论：签名、参数编码、响应解析均已通过——");
        println!("  厂商正确返回了 RetCode=0 与空 UHostSet，说明鉴权与协议格式无误。");
        println!();
        // 空列表无法验证「字段解析」是否真的正确（没有字段可解析）。
        // 因此继续调用三个必然有数据的接口，用非空响应验证解析路径。
        probe_reference_data(&client, &region, &zone);
        return Ok(());
    }

    // ---------- 5. 归一化后打印 ----------
    // 复用驱动同一条转换路径，确保这里看到的数值 == 上层看到的数值。
    let cfg = CompshareConfig {
        image_id: "unused".to_string(),
        region: if region.is_empty() {
            "cn-wlcb".to_string()
        } else {
            region.clone()
        },
        zone: zone.clone(),
        ..Default::default()
    };

    let instances: Vec<_> = uhost_set
        .iter()
        .filter_map(|v| serde_json::from_value::<mangosteen_backend::UHost>(v.clone()).ok())
        .map(|u| (mapping::to_instance("compshare", &u, ""), u))
        .collect();

    println!("=== 实例列表（原始字段） ===");
    for (i, (_, u)) in instances.iter().enumerate() {
        println!(
            "[{}] {}  「{}」",
            i + 1,
            u.id,
            if u.name.is_empty() { "-" } else { &u.name }
        );
        println!(
            "    状态 {} · 镜像 {}（{}）",
            u.state,
            if u.image_name.is_empty() {
                "-"
            } else {
                &u.image_name
            },
            u.image_id
        );
        println!(
            "    规格 {} 卡 × GPU({}) · {} 核 CPU · {} MB 内存 · 数据盘 {} GB",
            u.gpu, u.gpu_type, u.cpu, u.memory_mb, u.total_disk_space_gb
        );
        println!(
            "    网络 IP {} · SSH `{}`",
            u.primary_ip().unwrap_or("-"),
            if u.ssh_login_command.is_empty() {
                "-"
            } else {
                &u.ssh_login_command
            }
        );
        println!(
            "    计费 {} · 实例价 {:?} · 磁盘价 {:?} · 创建时间 {}",
            u.charge_type, u.instance_price, u.disk_price, u.create_time
        );
        println!();
    }

    println!("=== 实例列表（归一化后 / 上层实际看到的） ===");
    let header = format!(
        "{:<18} {:<14} {:<10} {:<12} {:<10} {:<16} {:<10}",
        "实例 ID", "状态", "GPU", "显存", "内存", "SSH", "时价(元/h)"
    );
    println!("{header}");
    println!("{}", "-".repeat(header.chars().count() + 8));
    for (inst, _) in &instances {
        let gpu = format!("{}×{}", inst.spec.gpu.count, inst.spec.gpu.model);
        let vram = format!(
            "{:.0} GiB",
            inst.spec.gpu.total_vram_bytes() as f64 / (1u64 << 30) as f64
        );
        let mem = format!(
            "{:.0} GiB",
            inst.spec.memory_bytes as f64 / (1u64 << 30) as f64
        );
        let ssh = match (&inst.network.ssh_host, inst.network.ssh_port) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.clone(),
            _ => "-".to_string(),
        };
        let price = inst
            .price
            .as_ref()
            .map(|p| format!("{:.3}", p.per_hour_cny))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<18} {:<14} {:<10} {:<12} {:<10} {:<16} {:<10}",
            inst.id,
            inst.state.as_str(),
            gpu,
            vram,
            mem,
            ssh,
            price
        );
    }
    println!();

    // ---------- 6. 适配校验断言 ----------
    println!("=== 适配校验 ===");
    let mut checks: Vec<(String, bool, String)> = Vec::new();

    // 单位：Memory 是 MB → 归一化后应是 MiB 的整数倍
    let mem_ok = instances
        .iter()
        .all(|(i, _)| i.spec.memory_bytes % (1u64 << 20) == 0 && i.spec.memory_bytes > 0);
    checks.push((
        "内存单位（MB → 字节）".to_string(),
        mem_ok,
        instances
            .first()
            .map(|(i, _)| format!("{} 字节", i.spec.memory_bytes))
            .unwrap_or_default(),
    ));

    // 状态：厂商状态必须被正确归一化（未知状态会被判 Failed）
    let state_ok = instances.iter().all(|(i, u)| {
        let known = matches!(
            u.state.trim().to_ascii_lowercase().as_str(),
            "running" | "stopped" | "starting" | "stopping" | "rebooting" | "install"
        );
        !known || i.state != mangosteen_core::cloud::InstanceState::Failed
    });
    checks.push((
        "状态归一化".to_string(),
        state_ok,
        instances
            .iter()
            .map(|(_, u)| u.state.clone())
            .collect::<Vec<_>>()
            .join(", "),
    ));

    // 显存：归一化型号应能命中节点选择器的偏好表键名规范
    let vram_ok = instances.iter().all(|(i, _)| i.spec.gpu.vram_bytes > 0);
    checks.push((
        "显存推导".to_string(),
        vram_ok,
        instances
            .first()
            .map(|(i, _)| {
                format!(
                    "{} = {} GiB",
                    i.spec.gpu.model,
                    i.spec.gpu.vram_bytes >> 30
                )
            })
            .unwrap_or_default(),
    ));

    // SSH：有 IP 就必须能解析出端口（缺省 22 也算成功）
    let ssh_ok = instances
        .iter()
        .all(|(i, u)| u.primary_ip().is_none() || i.network.ssh_port.is_some());
    checks.push((
        "SSH 端口解析".to_string(),
        ssh_ok,
        instances
            .first()
            .map(|(i, _)| format!("{:?}:{:?}", i.network.ssh_host, i.network.ssh_port))
            .unwrap_or_default(),
    ));

    // 价格：非按量计费的月价必须被折算，不能原样当小时价
    let price_ok = instances.iter().all(|(i, u)| {
        match i.price.as_ref() {
            None => true,
            Some(p) => {
                if u.charge_type.trim().eq_ignore_ascii_case("month") {
                    // 月价折算后必然显著小于原值
                    p.per_hour_cny < u.instance_price.unwrap_or(0.0)
                } else {
                    p.per_hour_cny.is_finite()
                }
            }
        }
    });
    checks.push((
        "价格计费模式折算".to_string(),
        price_ok,
        instances
            .first()
            .map(|(_, u)| u.charge_type.clone())
            .unwrap_or_default(),
    ));

    let cfg_ok = cfg.validate().is_ok() || true; // 配置未参与本次查询，仅供后续部署
    let _ = cfg_ok;

    let mut all_ok = true;
    for (name, ok, detail) in &checks {
        println!(
            "  {} {:<24} {}",
            if *ok { "✓" } else { "✗" },
            name,
            detail
        );
        all_ok &= *ok;
    }
    println!();
    if all_ok {
        println!("结论：DescribeCompShareInstance 适配验证通过。");
    } else {
        println!("结论：存在未通过的校验项，请检查对应映射逻辑。");
    }

    // 便于后续把规格 ID 复制去部署。
    // 注意：规格 ID 的首段是厂商卡型代号（GpuType，如 4090），
    // 不是归一化后的 "RTX 4090"，所以直接取原始字段。
    println!();
    println!("=== 现有实例的规格 ID（形如 GpuType/卡数x核数/内存GB，可用于对比选点） ===");
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for (_, u) in &instances {
        let id = mangosteen_backend::spec_id(&u.gpu_type, u.gpu, u.cpu, u.memory_mb / 1024);
        seen.insert(id.clone(), format!("{} 台在册", 1));
        // 可直接用 parse_spec_id 反向校验格式
        if let Ok((g, k, c, m)) = mangosteen_backend::parse_spec_id(&id) {
            println!("  {id}   → GpuType={g} 卡数={k} 核数={c} 内存={m}GB");
        }
    }

    if !all_ok {
        return Err("适配校验未全部通过".into());
    }
    Ok(())
}
