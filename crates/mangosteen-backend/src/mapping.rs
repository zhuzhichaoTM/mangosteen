//! 厂商 → 领域模型的映射规则。
//!
//! 归属：**算力执行层 · 云端后端管理**（适配逻辑）。
//!
//! 这里是「Compshare 私有表示」到「Mangosteen 统一表示」的**唯一换算点**。
//! 上层只看到 [`mangosteen_core::cloud`] 的类型，完全不知道 `uhost-` 前缀、
//! `Install` 状态、MB 单位这些厂商细节。
//!
//! ## 映射原则
//!
//! 1. **未知状态不能乐观**：厂商新增状态时一律归为 `Failed` 而不是 `Running`。
//!    把不可用实例标成 `Running` 会让网关向它转发推理请求，症状比直接失败更难查。
//! 2. **单位换算集中在此**：MB↔字节、GB↔字节各写一次，并用单测钉死。
//! 3. **SSH 端口从登录命令里解析**：厂商不单独返回端口字段，
//!    只给 `ssh root@x.x.x.x -p 60022` 这种命令串。

use mangosteen_core::cloud::{
    AvailableSpec, GpuSpec, Instance, InstanceMetrics, InstanceSpec, InstanceState, NetworkEndpoint,
    PriceQuote,
};

use crate::model::{state, AvailableInstanceType, GraphicsMemory, PriceDetail, UHost};

/// 单位换算（单一实现点）。
pub mod units {
    /// MiB 与字节的换算基数（厂商的 `Memory` 按 1024 进制计）。
    pub const MIB: u64 = 1 << 20;
    /// GiB 与字节的换算基数。
    pub const GIB: u64 = 1 << 30;

    /// MB → 字节。厂商 `Memory` 字段用 1024 进制，故按 MiB 处理。
    pub fn mb_to_bytes(mb: u64) -> u64 {
        mb.saturating_mul(MIB)
    }

    /// GB → 字节。
    pub fn gb_to_bytes(gb: u64) -> u64 {
        gb.saturating_mul(GIB)
    }

    /// 字节 → GB（向下取整，用于构造厂商请求）。
    pub fn bytes_to_gb(bytes: u64) -> u64 {
        bytes / GIB
    }
}

/// 厂商状态 → 归一化状态。
///
/// 映射表（大小写不敏感）：
///
/// | 厂商 `State` | 归一化 |
/// |--------------|--------|
/// | `Running` | `Running` |
/// | `Stopped` | `Stopped` |
/// | `Starting` | `Starting` |
/// | `Stopping` | `Stopping` |
/// | `Rebooting` | `Starting`（重启后必然回到运行，对上层等价于「启动中」） |
/// | `Install` | `Creating`（镜像安装阶段，尚未就绪） |
/// | 其它 | `Failed`（保守） |
pub fn normalize_state(raw: &str) -> InstanceState {
    match raw.trim().to_ascii_lowercase().as_str() {
        "running" => InstanceState::Running,
        "stopped" | "shutdown" | "shutoff" => InstanceState::Stopped,
        "starting" => InstanceState::Starting,
        "stopping" => InstanceState::Stopping,
        // 重启完成后恢复 Running，语义上等价于「启动中」
        "rebooting" => InstanceState::Starting,
        // 创建/初始化阶段的中间态。**必须显式列出**，不能靠 `_` 兜底：
        // 优云新建实例的头几十秒是 `Initializing`，落到 `_` 会被判成
        // `Failed`，编排器随即销毁一台正在正常开机的机器、换下一个候选，
        // 表现为「每个节点都创建成功但全都失败」，并且反复计费。
        "install" | "installing" | "initializing" | "initialising" | "creating"
        | "pending" | "deploying" => InstanceState::Creating,
        _ => InstanceState::Failed,
    }
}

/// 从 SSH 登录命令里解析端口。
///
/// 支持 `ssh root@1.2.3.4 -p 60022`、`ssh -p 60022 root@host`、
/// `ssh root@host -p60022` 三种常见写法；缺省返回 22。
pub fn parse_ssh_port(command: &str) -> Option<u16> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if t == "-p" {
            if let Some(v) = tokens.get(i + 1) {
                if let Ok(p) = v.parse::<u16>() {
                    return Some(p);
                }
            }
        } else if let Some(rest) = t.strip_prefix("-p") {
            // `-p60022` 紧连写法
            if !rest.is_empty() {
                if let Ok(p) = rest.parse::<u16>() {
                    return Some(p);
                }
            }
        }
        i += 1;
    }
    None
}

/// 从 SSH 登录命令里解析登录用户（缺省 `root`）。
pub fn parse_ssh_user(command: &str) -> Option<String> {
    for tok in command.split_whitespace() {
        if let Some((user, _host)) = tok.split_once('@') {
            if !user.is_empty() {
                return Some(user.to_string());
            }
        }
    }
    None
}

/// 生成 GPU 归一化型号名。
///
/// 厂商只给代号（`4090`、`A100`、`4090_48G`），而节点选择器的偏好表按
/// `RTX 4090` 这类规范名匹配。这里把纯数字代号补上 `RTX ` 前缀，
/// 数据中心卡（`A`/`H`/`V` 开头）与带后缀的变体保持原样。
pub fn normalize_gpu_model(code: &str) -> String {
    let trimmed = code.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    let upper = trimmed.to_ascii_uppercase();
    // 纯数字或以 "4090_48G" 形式出现的消费级卡 → 补 RTX 前缀
    let first = upper.chars().next().unwrap_or(' ');
    if first.is_ascii_digit() {
        // 保留后缀信息（4090_48G 与 4090 显存不同，不能合并）
        let display = upper.replace('_', "-");
        return format!("RTX {display}");
    }
    if upper.starts_with("RTX") || upper.starts_with("GTX") {
        let rest = upper.trim_start_matches("RTX").trim_start_matches("GTX").trim();
        let prefix = if upper.starts_with("RTX") { "RTX" } else { "GTX" };
        return format!("{prefix} {rest}");
    }
    upper
}

/// 显存字节数。
///
/// 优先用机型表给的显存（`GraphicsMemory.Value`，单位 GB）；
/// 缺失时用内置代号表兜底（见 [`known_vram_gb`]）。
pub fn vram_bytes(gpu_code: &str, graphics: Option<&GraphicsMemory>) -> u64 {
    match graphics {
        Some(g) if g.value_gb > 0 => units::gb_to_bytes(g.value_gb),
        _ => units::gb_to_bytes(known_vram_gb(gpu_code)),
    }
}

/// 已知卡型的显存（GB）。
///
/// 数据来自厂商文档「GPU 类型列表」。这张表只作为**兜底**：
/// 正常路径应使用接口返回的 `GraphicsMemory`。厂商新增卡型时
/// 该函数返回 0，上层据此跳过打分而不是误判为可部署。
pub fn known_vram_gb(gpu_code: &str) -> u64 {
    match gpu_code.trim().to_ascii_uppercase().as_str() {
        "4090" => 24,
        "4090_48G" => 48,
        "5090" => 32, // 厂商文档未列显存，按公版 32GB
        "3090" => 24,
        "3080TI" => 12,
        "2080TI" => 11,
        "2080" => 11,
        "A100" => 80,
        "A800" => 80,
        "H20" => 96,
        "P40" => 24,
        "V100S" => 32,
        _ => 0,
    }
}

/// 实例价格（元/小时）。
///
/// 厂商返回的是**针对当前计费模式的金额**：`Postpay` 即元/小时，
/// 而 `Month` 是月价。为了避免把月价当小时价（会高估 720 倍并导致
/// 节点选择器完全跑偏），这里按计费模式折算。
pub fn hourly_price(charge_type: &str, amount_cny: f64) -> Option<f64> {
    if !amount_cny.is_finite() || amount_cny <= 0.0 {
        return None;
    }
    // 厂商用极端值当「无报价」哨兵：实测优云对 A100 返回
    // 1.7976931348623157e308（正好是 f64::MAX）。它 is_finite() 为真、
    // 大于 0，能通过上面的检查，然后被格式化成 300 多位数字显示在 UI 上，
    // 并参与「最便宜」排序。单台消费级/单卡节点的时价不可能超过
    // 这个上限，超过一律视为「没有有效报价」。
    const MAX_SANE_HOURLY_CNY: f64 = 1_000.0;
    if amount_cny >= f64::MAX / 2.0 {
        return None;
    }
    let per_hour = match charge_type.trim().to_ascii_lowercase().as_str() {
        "postpay" | "dynamic" | "spot" | "" => amount_cny,
        // 月按 30 天、天按 24 小时折算
        "month" => amount_cny / (30.0 * 24.0),
        "day" => amount_cny / 24.0,
        _ => return None,
    };
    if !per_hour.is_finite() || per_hour <= 0.0 || per_hour > MAX_SANE_HOURLY_CNY {
        return None;
    }
    Some(per_hour)
}

/// 是否为「按量可随时释放」的计费模式。
pub fn is_spot_releasable(charge_type: &str) -> bool {
    matches!(
        charge_type.trim().to_ascii_lowercase().as_str(),
        "postpay" | "dynamic" | "spot"
    )
}

/// 判断镜像是否适合本项目部署（需要 Linux + SSH）。
///
/// ## 为什么必须判断
///
/// 实测踩坑：自动挑「第一个 Available 的系统镜像」挑到了
/// **Windows-nvidia 2022**，开出来的实例 `SshLoginCommand` 为空、
/// 无法 SSH，更跑不了 llama.cpp——白花计费时间。
///
/// 厂商的系统镜像列表里 Windows 排在前面，所以**不能只看
/// `Status == Available`**，必须按操作系统筛选。
pub fn is_linux_image(name: &str, os_hint: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let o = os_hint.to_ascii_lowercase();

    // 明确的 Windows 标识：优先排除（宁可错杀，不要开错）
    if n.contains("windows") || o.contains("windows") {
        return false;
    }
    // 明确的 Linux 标识
    let linux_markers = [
        "ubuntu", "centos", "debian", "rocky", "almalinux", "fedora", "alpine", "linux",
    ];
    if linux_markers.iter().any(|m| n.contains(m)) || o.contains("linux") {
        return true;
    }
    // 判断不出来时保守视为不适合（不猜）
    false
}

/// 在镜像列表中挑一个适合部署的 Linux 镜像。
///
/// 优先顺序：
/// 1. `Available` 且名含 `Ubuntu`（NVIDIA 驱动齐全、社区支持最好）；
/// 2. `Available` 且判定为 Linux；
/// 3. `None` —— 由调用方显式要求用户指定 `image_id`。
///
/// 刻意**不**回退到「随便一个可用镜像」：开出一台 Windows 实例
/// 既不能 SSH 也不能跑推理，纯属浪费计费时间。
pub fn pick_linux_image(images: &[crate::model::ImageInfo]) -> Option<&crate::model::ImageInfo> {
    let available: Vec<&crate::model::ImageInfo> = images
        .iter()
        .filter(|i| i.status.eq_ignore_ascii_case("Available"))
        .collect();
    if available.is_empty() {
        return None;
    }
    if let Some(i) = available
        .iter()
        .find(|i| i.name.to_ascii_lowercase().contains("ubuntu"))
    {
        return Some(i);
    }
    available
        .into_iter()
        .find(|i| is_linux_image(&i.name, ""))
}

/// 网络端点：从 IP 与登录命令推导。
pub fn network_endpoint(uhost: &UHost) -> NetworkEndpoint {
    NetworkEndpoint {
        ssh_host: uhost.primary_ip().map(|s| s.to_string()),
        ssh_port: parse_ssh_port(&uhost.ssh_login_command).or(Some(22)),
        inference_host: None,
        inference_port: None,
        public_url: None,
        // 优云智算不返回密码，由上层主动设置后经 TunnelSpec 传入
        ssh_password: None,
    }
}

/// 厂商百分数 → 0~1 比例。
///
/// ## 为什么必须在这里换算
///
/// 厂商监控接口的 `Value` 是 **0~100 的百分数**，而本项目本地监控
/// （`/api/sysinfo`）用的是 **0~1 比例**。两种单位一旦混用，
/// 前端会出现「CPU 3500%」或「占用 0%」这类荒谬显示。
///
/// 赋值时**先除以 100 再 clamp**：厂商偶发返回 `-1`（无效值）或
/// 明显越界的脏数据时，clamp 能把它们压回合理区间，
/// 而不是让进度环画出反方向。
pub fn percent_to_ratio(raw: f64) -> f64 {
    if !raw.is_finite() {
        return 0.0;
    }
    (raw / 100.0).clamp(0.0, 1.0)
}

/// 把厂商监控数据归一化为 [`InstanceMetrics`]。
///
/// ## 两个刻意的选择
///
/// 1. **多卡取最大值**（`max_of` 而不是 `latest`）：一块 4090 跑满、
///    另一块空转时，用户关心的是「有没有卡被打满」，取平均会掩盖瓶颈。
/// 2. **最新时间戳取所有指标的最大值**：各指标上传有先后，
///    取最大能反映「这份快照有多新」，避免用旧指标的时间冒充整体时间。
pub fn to_instance_metrics(m: &crate::model::MonitorInstance) -> InstanceMetrics {
    use crate::model::metric;

    let mut out = InstanceMetrics {
        instance_id: m.id.clone(),
        sampled_at: m.sampled_at(),
        cpu_usage: m.max_of(metric::CPU_USED).map(percent_to_ratio),
        memory_usage: m.max_of(metric::MEMORY_USAGE).map(percent_to_ratio),
        gpu_usage: m.max_of(metric::GPU_UTIL).map(percent_to_ratio),
        gpu_memory_usage: m.max_of(metric::GPU_MEMORY_USAGE).map(percent_to_ratio),
        disk_usage: m.max_of(metric::SYS_DISK_USED).map(percent_to_ratio),
        data_disk_usage: m.max_of(metric::DATA_DISK_USED).map(percent_to_ratio),
        extra: std::collections::BTreeMap::new(),
    };

    // 未建模的指标不丢弃：厂商随时会加新指标，丢掉会让排障时缺少线索。
    // 这里保留**原始百分数**，与其他字段的比例语义不同，故放进 extra 并注明。
    const KNOWN: [&str; 6] = [
        metric::CPU_USED,
        metric::MEMORY_USAGE,
        metric::GPU_UTIL,
        metric::GPU_MEMORY_USAGE,
        metric::SYS_DISK_USED,
        metric::DATA_DISK_USED,
    ];
    for metric in &m.metrics {
        if metric.key.is_empty() || KNOWN.iter().any(|k| k.eq_ignore_ascii_case(&metric.key)) {
            continue;
        }
        if let Some(v) = metric.max_value() {
            out.extra.insert(metric.key.clone(), v);
        }
    }

    out
}

/// 从 `MonitorMessages` 快照构造监控数据（监控接口无数据时的兜底）。
///
/// ## 为什么需要兜底
///
/// 实测发现：实例**关机**（或刚开机数据未就绪）时，
/// `GetCompShareInstanceMonitor` 会正常返回 6 个指标键，但每个的
/// `Results` 都是空数组。若只认时间序列，设备页会显示「无数据」，
/// 用户无法区分「厂商没数据」与「我们没接好」。
///
/// `DescribeCompShareInstance` 的 `MonitorMessages` 给出当前值，
/// 正好补上这个缺口（厂商文档也说明它可作为替代）。
///
/// GPU 部分：厂商未文档化 `GpuInfo` 结构（关机时为 `null`），
/// 这里按「尽力提取」处理——在对象/数组里找含 `gpu` 且含 `util`/`usage`
/// 的数值键，取最大。取不到就留空，绝不猜测默认值。
pub fn to_metrics_from_snapshot(uhost: &UHost) -> InstanceMetrics {
    let snap = &uhost.monitor_messages;

    let mut out = InstanceMetrics {
        instance_id: uhost.id.clone(),
        // 快照没有采样时间，用实例创建时间没有意义；
        // 填 0 表示「厂商未给出」，前端据此显示「实时快照」而不是具体时刻。
        sampled_at: 0,
        cpu_usage: snap.cpu_usage_rate.map(percent_to_ratio),
        memory_usage: snap.mem_usage_rate.map(percent_to_ratio),
        gpu_usage: None,
        gpu_memory_usage: None,
        disk_usage: None,
        data_disk_usage: None,
        extra: std::collections::BTreeMap::new(),
    };

    if let Some(info) = &snap.gpu_info {
        if let Some(v) = pick_gpu_percent(info, &["util"]) {
            out.gpu_usage = Some(percent_to_ratio(v));
        }
        if let Some(v) = pick_gpu_percent(info, &["mem", "memory"]) {
            out.gpu_memory_usage = Some(percent_to_ratio(v));
        }
    }
    out
}

/// 在未知结构的 JSON 里找「名字含 `gpu` 且含给定关键词」的数值，取最大值。
///
/// 刻意做成**保守提取**：匹配不到就返回 `None`，不用 0 冒充数据。
fn pick_gpu_percent(v: &serde_json::Value, hints: &[&str]) -> Option<f64> {
    use serde_json::Value;

    fn walk(v: &Value, hints: &[&str], acc: &mut Vec<f64>) {
        match v {
            Value::Object(map) => {
                for (k, val) in map {
                    let key = k.to_ascii_lowercase();
                    let named = key.contains("gpu") && hints.iter().any(|h| key.contains(h));
                    if named {
                        match val {
                            Value::Number(n) => {
                                if let Some(f) = n.as_f64() {
                                    acc.push(f);
                                }
                            }
                            Value::String(s) => {
                                if let Ok(f) = s.trim().parse::<f64>() {
                                    acc.push(f);
                                }
                            }
                            _ => {}
                        }
                    }
                    walk(val, hints, acc);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, hints, acc);
                }
            }
            _ => {}
        }
    }

    let mut acc = Vec::new();
    walk(v, hints, &mut acc);
    acc.into_iter().fold(None, |m: Option<f64>, x| {
        Some(m.map_or(x, |cur| cur.max(x)))
    })
}

/// 把厂商实例转成归一化 [`Instance`]。
pub fn to_instance(provider: &str, uhost: &UHost, fallback_image: &str) -> Instance {
    let state = normalize_state(&uhost.state);
    let gpu_count = uhost.gpu.max(1);
    let vram = units::gb_to_bytes(known_vram_gb(&uhost.gpu_type));

    let price = uhost
        .instance_price
        .and_then(|p| hourly_price(&uhost.charge_type, p))
        .zip(uhost.disk_price.and_then(|d| {
            // 磁盘价格同样受计费模式影响，单位与实例价一致
            hourly_price(&uhost.charge_type, d)
        }))
        .map(|(inst, disk)| PriceQuote {
            per_hour_cny: inst + disk,
            billing_granularity_secs: 0,
            spot_release_allowed: is_spot_releasable(&uhost.charge_type),
            note: Some(format!("计费模式 {}", uhost.charge_type)),
        })
        .or_else(|| {
            uhost
                .instance_price
                .and_then(|p| hourly_price(&uhost.charge_type, p))
                .map(|inst| PriceQuote {
                    per_hour_cny: inst,
                    billing_granularity_secs: 0,
                    spot_release_allowed: is_spot_releasable(&uhost.charge_type),
                    note: Some(format!("计费模式 {}", uhost.charge_type)),
                })
        });

    Instance {
        id: uhost.id.clone(),
        provider: provider.to_string(),
        state,
        spec: InstanceSpec {
            gpu: GpuSpec {
                model: normalize_gpu_model(&uhost.gpu_type),
                count: gpu_count,
                vram_bytes: vram,
            },
            cpu_cores: uhost.cpu.max(1),
            memory_bytes: units::mb_to_bytes(uhost.memory_mb),
            disk_bytes: units::gb_to_bytes(uhost.total_disk_space_gb),
            image: if uhost.image_id.is_empty() {
                fallback_image.to_string()
            } else {
                uhost.image_id.clone()
            },
        },
        network: network_endpoint(uhost),
        created_at: uhost.create_time,
        price,
        display_name: if uhost.name.is_empty() {
            None
        } else {
            Some(uhost.name.clone())
        },
    }
}

/// 从机型表与价格明细构造一个候选规格。
///
/// 返回 `None` 的情形：
/// * 机型售罄；
/// * 缺少显存信息（无法参与显存约束判定，宁可跳过也不猜）；
/// * 没有可用的 CPU/内存组合。
pub fn to_available_spec(
    item: &AvailableInstanceType,
    gpu: u32,
    cpu: u32,
    memory_gb: u64,
    price: Option<&PriceDetail>,
    default_image: &str,
    stock: Option<u32>,
) -> Option<AvailableSpec> {
    if !item.is_on_sale() {
        return None;
    }
    let vram = vram_bytes(&item.name, item.graphics_memory.as_ref());
    if vram == 0 {
        return None;
    }
    let hourly = price
        .and_then(|p| hourly_price(&p.charge_type, p.total()))
        .or_else(|| price.and_then(|p| hourly_price(&p.charge_type, p.instance.unwrap_or(0.0))));

    let mut extra = std::collections::BTreeMap::new();
    if !item.description.is_empty() {
        extra.insert("description".to_string(), item.description.clone());
    }
    extra.insert("gpu_type".to_string(), item.name.clone());

    Some(AvailableSpec {
        // 规格 ID 必须能还原出创建参数，格式：卡型/卡数/CPU/内存GB
        spec_id: spec_id(&item.name, gpu, cpu, memory_gb),
        spec: InstanceSpec {
            gpu: GpuSpec {
                model: normalize_gpu_model(&item.name),
                count: gpu,
                vram_bytes: vram,
            },
            cpu_cores: cpu,
            memory_bytes: units::gb_to_bytes(memory_gb),
            // 厂商没有「规格自带磁盘」的概念，用系统盘最小值占位
            disk_bytes: units::gb_to_bytes(100),
            image: default_image.to_string(),
        },
        price: PriceQuote {
            per_hour_cny: hourly.unwrap_or(f64::MAX),
            billing_granularity_secs: 0,
            spot_release_allowed: price
                .map(|p| is_spot_releasable(&p.charge_type))
                .unwrap_or(true),
            note: price.map(|p| format!("计费模式 {}", p.charge_type)),
        },
        region: if item.zone.is_empty() {
            "default".to_string()
        } else {
            item.zone.clone()
        },
        stock,
        extra,
    })
}

/// 构造规格 ID。
pub fn spec_id(gpu_type: &str, gpu: u32, cpu: u32, memory_gb: u64) -> String {
    format!("{gpu_type}/{gpu}x{cpu}c/{memory_gb}g")
}

/// 解析规格 ID，还原创建参数。
///
/// 格式非法时返回说明性错误——比静默用默认值创建错规格要好得多：
/// 后者会产生一张意外账单。
pub fn parse_spec_id(spec_id: &str) -> mangosteen_core::MnResult<(String, u32, u32, u64)> {
    use mangosteen_core::MnError;

    let bad = || {
        MnError::bad_request(format!(
            "规格 ID `{spec_id}` 格式非法，应形如 `4090/1x16c/64g`\
             （卡型号/卡数x核数/内存GB）"
        ))
    };

    let mut parts = spec_id.split('/');
    let gpu_type = parts.next().filter(|s| !s.trim().is_empty()).ok_or_else(bad)?;
    let mid = parts.next().ok_or_else(bad)?;
    let mem = parts.next().ok_or_else(bad)?;
    if parts.next().is_some() {
        return Err(bad());
    }

    let (gpu, cpu) = mid.split_once('x').ok_or_else(bad)?;
    let gpu: u32 = gpu.trim().parse().map_err(|_| bad())?;
    let cpu: u32 = cpu.trim_end_matches('c').trim().parse().map_err(|_| bad())?;
    let memory_gb: u64 = mem
        .trim_end_matches('g')
        .trim()
        .parse()
        .map_err(|_| bad())?;

    if gpu == 0 || cpu == 0 || memory_gb == 0 {
        return Err(bad());
    }
    Ok((gpu_type.trim().to_string(), gpu, cpu, memory_gb))
}

/// 厂商状态常量（供外部对照）。
pub fn vendor_state_constants() -> [&'static str; 6] {
    [
        state::RUNNING,
        state::STOPPED,
        state::STARTING,
        state::STOPPING,
        state::INSTALL,
        state::REBOOTING,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        IpInfo, MonitorInstance, MonitorMessages, MonitorMetric, MonitorResult, MonitorValue,
    };

    /// 构造一个带单点数据点的指标。
    fn metric(key: &str, value: f64, ts: i64) -> MonitorMetric {
        MonitorMetric {
            key: key.to_string(),
            tags: serde_json::Value::Null,
            results: vec![MonitorResult {
                values: vec![MonitorValue {
                    timestamp: ts,
                    value,
                }],
            }],
        }
    }

    #[test]
    fn percent_is_converted_to_ratio() {
        // 厂商 0~100 → 本项目 0~1
        assert!((percent_to_ratio(35.2) - 0.352).abs() < 1e-9);
        assert_eq!(percent_to_ratio(0.0), 0.0);
        assert_eq!(percent_to_ratio(100.0), 1.0);
        // 越界与脏数据：clamp 回合理区间，不让进度环反向
        assert_eq!(percent_to_ratio(-1.0), 0.0);
        assert_eq!(percent_to_ratio(3500.0), 1.0);
        assert_eq!(percent_to_ratio(f64::NAN), 0.0);
        assert_eq!(percent_to_ratio(f64::INFINITY), 0.0);
    }

    #[test]
    fn metrics_take_busiest_gpu_and_keep_unknown_keys() {
        // 两张卡：一张闲（10%）、一张忙（92%）
        let gpu = MonitorMetric {
            key: "cloudwatch_gpu_util".into(),
            tags: serde_json::Value::Null,
            results: vec![
                MonitorResult {
                    values: vec![MonitorValue {
                        timestamp: 100,
                        value: 10.0,
                    }],
                },
                MonitorResult {
                    values: vec![MonitorValue {
                        timestamp: 100,
                        value: 92.0,
                    }],
                },
            ],
        };
        let m = MonitorInstance {
            id: "uhost-1".into(),
            metrics: vec![
                metric("uhost_cpu_used", 25.0, 100),
                metric("cloudwatch_memory_usage", 60.0, 100),
                gpu,
                // 厂商新增的未知指标必须保留
                metric("vendor_new_metric", 7.0, 100),
            ],
        };

        let out = to_instance_metrics(&m);
        assert_eq!(out.instance_id, "uhost-1");
        assert_eq!(out.sampled_at, 100);
        assert!((out.cpu_usage.unwrap() - 0.25).abs() < 1e-9);
        assert!((out.memory_usage.unwrap() - 0.60).abs() < 1e-9);
        // 多卡取最忙的一张：0.92 而不是平均 0.51
        assert!((out.gpu_usage.unwrap() - 0.92).abs() < 1e-9);
        assert!(out.gpu_memory_usage.is_none());
        assert_eq!(out.extra.get("vendor_new_metric"), Some(&7.0));
        // 已建模的指标不应重复进 extra
        assert!(!out.extra.contains_key("uhost_cpu_used"));
        assert!(out.has_any());
    }

    #[test]
    fn metrics_use_latest_timestamp_not_last_element() {
        // 厂商不保证 Values 有序：最后一个元素可能是最旧的
        let m = MonitorInstance {
            id: "u".into(),
            metrics: vec![MonitorMetric {
                key: "uhost_cpu_used".into(),
                tags: serde_json::Value::Null,
                results: vec![MonitorResult {
                    values: vec![
                        MonitorValue {
                            timestamp: 300,
                            value: 99.0,
                        },
                        MonitorValue {
                            timestamp: 200,
                            value: 50.0,
                        },
                    ],
                }],
            }],
        };
        assert_eq!(m.sampled_at(), 300);
        // 最新点应取 300 那个（2026-10-25 之前一直有实现取 last() 的隐患）
        assert!((to_instance_metrics(&m).cpu_usage.unwrap() - 0.99).abs() < 1e-9);
    }

    #[test]
    fn snapshot_fallback_fills_cpu_and_memory() {
        // 实测样本：关机实例的 MonitorMessages（字符串承载数值，GpuInfo 为 null）
        let mut u = sample_uhost();
        u.monitor_messages = MonitorMessages {
            cpu_usage_rate: Some(35.2),
            mem_usage_rate: Some(61.0),
            gpu_info: None,
        };
        let m = to_metrics_from_snapshot(&u);
        assert!((m.cpu_usage.unwrap() - 0.352).abs() < 1e-9);
        assert!((m.memory_usage.unwrap() - 0.61).abs() < 1e-9);
        // GpuInfo 为 null 时不得瞎猜 GPU 数据
        assert!(m.gpu_usage.is_none());
        assert!(m.gpu_memory_usage.is_none());
        // 快照没有采样时间，必须是 0（前端据此显示「实时快照」而非具体时刻）
        assert_eq!(m.sampled_at, 0);
        assert!(m.has_any());
    }

    #[test]
    fn snapshot_empty_means_no_data_not_zero() {
        // 厂商无数据时给空串，绝不能当成 0% —— 那会让用户以为机器完全空闲
        let mut u = sample_uhost();
        u.monitor_messages = MonitorMessages {
            cpu_usage_rate: None,
            mem_usage_rate: None,
            gpu_info: None,
        };
        let m = to_metrics_from_snapshot(&u);
        assert!(m.cpu_usage.is_none(), "空串必须解析成 None，而不是 0");
        assert!(!m.has_any());
    }

    #[test]
    fn snapshot_extracts_gpu_from_undocumented_structure() {
        // 厂商未文档化 GpuInfo，未来可能给出这些形态；尽力提取
        let mut u = sample_uhost();
        u.monitor_messages = MonitorMessages {
            cpu_usage_rate: Some(10.0),
            mem_usage_rate: Some(20.0),
            gpu_info: Some(serde_json::json!([
                {"gpu_util": "88.5", "gpu_memory_usage": 40.0},
                {"gpu_util": 12.0, "gpu_memory_usage": 5.0}
            ])),
        };
        let m = to_metrics_from_snapshot(&u);
        // 多卡取最忙
        assert!((m.gpu_usage.unwrap() - 0.885).abs() < 1e-9);
        assert!((m.gpu_memory_usage.unwrap() - 0.40).abs() < 1e-9);
    }

    #[test]
    fn units_are_consistent() {
        assert_eq!(units::mb_to_bytes(65536), 64 << 30);
        assert_eq!(units::gb_to_bytes(24), 24 << 30);
        assert_eq!(units::bytes_to_gb(64 << 30), 64);
        // 饱和而不是溢出
        assert_eq!(units::mb_to_bytes(u64::MAX), u64::MAX);
    }

    #[test]
    fn all_documented_states_map() {
        for s in vendor_state_constants() {
            let n = normalize_state(s);
            assert_ne!(n, InstanceState::Failed, "{s} 不应被判为失败");
        }
        assert_eq!(normalize_state("Running"), InstanceState::Running);
        assert_eq!(normalize_state("  RUNNING "), InstanceState::Running);
        assert_eq!(normalize_state("Stopped"), InstanceState::Stopped);
        assert_eq!(normalize_state("Starting"), InstanceState::Starting);
        assert_eq!(normalize_state("Stopping"), InstanceState::Stopping);
        assert_eq!(normalize_state("Rebooting"), InstanceState::Starting);
        assert_eq!(normalize_state("Install"), InstanceState::Creating);
    }

    #[test]
    /// 新建实例的中间态必须映射成 Creating，不能是 Failed。
    #[test]
    fn transitional_states_are_creating() {
        for raw in [
            "Initializing",
            "initialising",
            "Creating",
            "Pending",
            "Deploying",
            "Install",
        ] {
            assert_eq!(
                normalize_state(raw),
                InstanceState::Creating,
                "`{raw}` 应映射为 Creating"
            );
        }
        assert_eq!(normalize_state("Shutdown"), InstanceState::Stopped);
        assert_eq!(normalize_state("Running"), InstanceState::Running);
    }

    #[test]
    fn unknown_state_is_not_running() {
        // 乐观映射会让网关向不可用实例转发请求
        let s = normalize_state("SomeNewVendorState");
        assert_eq!(s, InstanceState::Failed);
        assert!(!s.is_serving());
        assert_eq!(normalize_state(""), InstanceState::Failed);
    }

    #[test]
    fn ssh_port_parsing_variants() {
        assert_eq!(parse_ssh_port("ssh root@1.2.3.4 -p 60022"), Some(60022));
        assert_eq!(parse_ssh_port("ssh -p 2222 root@host"), Some(2222));
        assert_eq!(parse_ssh_port("ssh root@host -p2222"), Some(2222));
        assert_eq!(parse_ssh_port("ssh root@host"), None);
        assert_eq!(parse_ssh_port("ssh root@host -p abc"), None);
    }

    #[test]
    fn ssh_user_parsing() {
        assert_eq!(parse_ssh_user("ssh root@1.2.3.4 -p 22").as_deref(), Some("root"));
        assert_eq!(parse_ssh_user("ssh ubuntu@host").as_deref(), Some("ubuntu"));
        assert_eq!(parse_ssh_user("ssh host"), None);
    }

    #[test]
    fn gpu_model_normalization_matches_preference_table() {
        // 偏好表里的键是 "RTX 4090" / "A100"，映射必须命中
        assert_eq!(normalize_gpu_model("4090"), "RTX 4090");
        assert_eq!(normalize_gpu_model("5090"), "RTX 5090");
        assert_eq!(normalize_gpu_model("A100"), "A100");
        // 变体保留后缀，不能与标准版合并（显存不同）
        assert_eq!(normalize_gpu_model("4090_48G"), "RTX 4090-48G");
        assert_eq!(normalize_gpu_model("3080Ti"), "RTX 3080TI");
        assert_eq!(normalize_gpu_model(""), "unknown");

        // 与节点选择器的归一化结果对齐（用于命中偏好表）
        let pref = mangosteen_core::policy::default_gpu_preference();
        assert!(pref.contains_key(&normalize_gpu_model("4090")));
        assert!(pref.contains_key(&normalize_gpu_model("5090")));
        assert!(pref.contains_key(&normalize_gpu_model("A100")));
    }

    #[test]
    fn vram_prefers_api_value_over_table() {
        let g = GraphicsMemory { value_gb: 48, rate: 4 };
        assert_eq!(vram_bytes("4090", Some(&g)), 48 << 30);
        // 无 API 值时用内置表
        assert_eq!(vram_bytes("4090", None), 24 << 30);
        // 未知卡型返回 0（调用方据此跳过）
        assert_eq!(vram_bytes("Unknown-9999", None), 0);
    }

    #[test]
    fn known_vram_covers_documented_types() {
        for (code, gb) in [
            ("4090", 24u64),
            ("4090_48G", 48),
            ("3090", 24),
            ("3080Ti", 12),
            ("A100", 80),
            ("A800", 80),
            ("H20", 96),
            ("P40", 24),
            ("V100S", 32),
        ] {
            assert_eq!(known_vram_gb(code), gb, "{code}");
        }
    }

    #[test]
    fn hourly_price_converts_by_charge_type() {
        // 按量付费：直接用
        assert_eq!(hourly_price("Postpay", 2.5), Some(2.5));
        // 月价必须折算，否则会高估 720 倍
        let m = hourly_price("Month", 720.0).unwrap();
        assert!((m - 1.0).abs() < 1e-9, "got {m}");
        let d = hourly_price("Day", 24.0).unwrap();
        assert!((d - 1.0).abs() < 1e-9, "got {d}");
        // 非法输入
        assert_eq!(hourly_price("Postpay", 0.0), None);
        assert_eq!(hourly_price("Postpay", -1.0), None);
        assert_eq!(hourly_price("Weird", 10.0), None);
        // 厂商「无报价」哨兵值：优云对 A100 返回 f64::MAX
        assert_eq!(hourly_price("Postpay", f64::MAX), None);
        assert_eq!(hourly_price("Postpay", 1.79e308), None);
        // 折算后仍超上限的月价也要拒
        assert_eq!(hourly_price("Month", 1.0e9), None);
    }

    #[test]
    fn releasable_modes() {
        assert!(is_spot_releasable("Postpay"));
        assert!(is_spot_releasable("Spot"));
        assert!(!is_spot_releasable("Month"));
        assert!(!is_spot_releasable("Day"));
    }

    fn sample_uhost() -> UHost {
        UHost {
            id: "uhost-abc".into(),
            name: "mangosteen-11435".into(),
            state: "Running".into(),
            zone: "cn-wlcb-01".into(),
            region: "cn-wlcb".into(),
            cpu: 16,
            memory_mb: 65536,
            gpu: 1,
            gpu_type: "4090".into(),
            charge_type: "Postpay".into(),
            create_time: 1_712_563_200,
            image_id: "img-1".into(),
            image_name: "PyTorch 2.1".into(),
            instance_type: "UHost".into(),
            os_type: "Linux".into(),
            ssh_login_command: "ssh root@203.0.113.10 -p 60022".into(),
            ip_set: vec![IpInfo {
                ip: "203.0.113.10".into(),
                kind: "Bgp".into(),
                mode: "IPv4".into(),
            }],
            total_disk_space_gb: 100,
            instance_price: Some(2.5),
            disk_price: Some(0.0),
            is_spot: false,
            remark: "managed-by-mangosteen".into(),
            support_without_gpu_start: true,
            monitor_messages: Default::default(),
        }
    }

    #[test]
    fn linux_image_detection() {
        // 实测列表里的真实镜像名
        assert!(is_linux_image("Ubuntu-nvidia 22.04", ""));
        assert!(!is_linux_image("Windows-nvidia 2022 64位", ""));
        assert!(!is_linux_image("Windows 2022 64位 EN", ""));
        // os 提示也能判
        assert!(!is_linux_image("some-image", "Windows"));
        assert!(is_linux_image("some-image", "Linux"));
        // 判断不出来 → 保守为 false
        assert!(!is_linux_image("mystery-image", ""));
        assert!(is_linux_image("CentOS 7 GPU", ""));
    }

    #[test]
    fn pick_linux_image_prefers_ubuntu_and_skips_windows() {
        use crate::model::ImageInfo;
        let mk = |id: &str, name: &str, status: &str| ImageInfo {
            id: id.into(),
            name: name.into(),
            status: status.into(),
            ..Default::default()
        };

        // 实测顺序：Windows 在前，Ubuntu 在后 —— 必须挑到 Ubuntu
        let images = vec![
            mk("img-win1", "Windows-nvidia 2022 64位", "Available"),
            mk("img-win2", "Windows 2022 64位 EN", "Available"),
            mk("img-ubuntu", "Ubuntu-nvidia 22.04", "Available"),
        ];
        let picked = pick_linux_image(&images).unwrap();
        assert_eq!(picked.id, "img-ubuntu");

        // 只有 Windows → 返回 None（不将就）
        let only_win = vec![mk("w", "Windows-nvidia 2022", "Available")];
        assert!(pick_linux_image(&only_win).is_none());

        // Ubuntu 但不可用 → 退回其它 Linux
        let with_unavailable_ubuntu = vec![
            mk("u", "Ubuntu-nvidia 22.04", "Unavailable"),
            mk("c", "CentOS 7 GPU", "Available"),
        ];
        assert_eq!(
            pick_linux_image(&with_unavailable_ubuntu).map(|i| i.id.as_str()),
            Some("c")
        );

        // 空列表
        assert!(pick_linux_image(&[]).is_none());
    }

    #[test]
    fn instance_conversion_is_correct_in_units() {
        let inst = to_instance("compshare", &sample_uhost(), "fallback-img");
        assert_eq!(inst.id, "uhost-abc");
        assert_eq!(inst.provider, "compshare");
        assert_eq!(inst.state, InstanceState::Running);
        assert_eq!(inst.spec.gpu.model, "RTX 4090");
        assert_eq!(inst.spec.gpu.count, 1);
        assert_eq!(inst.spec.gpu.vram_bytes, 24 << 30);
        assert_eq!(inst.spec.cpu_cores, 16);
        // MB → 字节
        assert_eq!(inst.spec.memory_bytes, 64 << 30);
        assert_eq!(inst.spec.disk_bytes, 100 << 30);
        assert_eq!(inst.spec.image, "img-1");
        // SSH 从命令里解析
        assert_eq!(inst.network.ssh_host.as_deref(), Some("203.0.113.10"));
        assert_eq!(inst.network.ssh_port, Some(60022));
        // 价格：实例价 + 磁盘价（Postpay 直接相加）
        let p = inst.price.unwrap();
        assert!((p.per_hour_cny - 2.5).abs() < 1e-9);
        assert!(p.spot_release_allowed);
        assert_eq!(inst.display_name.as_deref(), Some("mangosteen-11435"));
    }

    #[test]
    fn instance_conversion_falls_back_for_missing_fields() {
        let mut u = sample_uhost();
        u.image_id = String::new();
        u.ssh_login_command = "ssh root@1.1.1.1".into();
        u.name = String::new();
        let inst = to_instance("compshare", &u, "fallback-img");
        assert_eq!(inst.spec.image, "fallback-img");
        // 无 -p 参数 → 默认 22
        assert_eq!(inst.network.ssh_port, Some(22));
        assert!(inst.display_name.is_none());
    }

    #[test]
    fn monthly_price_is_not_used_as_hourly() {
        let mut u = sample_uhost();
        u.charge_type = "Month".into();
        u.instance_price = Some(720.0);
        u.disk_price = Some(0.0);
        let inst = to_instance("compshare", &u, "i");
        let quote = inst.price.expect("按月计费也应有报价");
        let hourly = quote.per_hour_cny;
        assert!(hourly < 2.0, "月价被当成小时价：{hourly}");
        assert!(!quote.spot_release_allowed);
    }

    #[test]
    fn spec_id_roundtrip() {
        let id = spec_id("4090", 1, 16, 64);
        assert_eq!(id, "4090/1x16c/64g");
        let (gpu_type, gpu, cpu, mem) = parse_spec_id(&id).unwrap();
        assert_eq!((gpu_type.as_str(), gpu, cpu, mem), ("4090", 1, 16, 64));

        // 4 卡 A100
        let id2 = spec_id("A100", 4, 64, 512);
        assert_eq!(parse_spec_id(&id2).unwrap(), ("A100".to_string(), 4, 64, 512));
    }

    #[test]
    fn malformed_spec_id_is_rejected() {
        for bad in [
            "",
            "4090",
            "4090/1x16c",
            "4090/1x16c/64g/extra",
            "4090/0x16c/64g",
            "4090/1x0c/64g",
            "4090/1x16c/0g",
            "4090/ax16c/64g",
        ] {
            assert!(parse_spec_id(bad).is_err(), "应拒绝 `{bad}`");
        }
        let e = parse_spec_id("4090").unwrap_err();
        assert!(e.to_string().contains("格式非法"), "{e}");
    }

    #[test]
    fn available_spec_requires_vram_and_sale() {
        let item = AvailableInstanceType {
            name: "4090".into(),
            zone: "cn-wlcb-01".into(),
            status: "Normal".into(),
            graphics_memory: Some(GraphicsMemory { value_gb: 24, rate: 3 }),
            ..Default::default()
        };
        let price = PriceDetail {
            charge_type: "Postpay".into(),
            instance: Some(2.5),
            ..Default::default()
        };
        let s = to_available_spec(&item, 1, 16, 64, Some(&price), "img", Some(3))
            .expect("应生成候选");
        assert_eq!(s.spec_id, "4090/1x16c/64g");
        assert_eq!(s.spec.gpu.vram_bytes, 24 << 30);
        assert_eq!(s.spec.memory_bytes, 64 << 30);
        assert_eq!(s.price.per_hour_cny, 2.5);
        assert_eq!(s.stock, Some(3));
        assert_eq!(s.region, "cn-wlcb-01");
        assert_eq!(s.extra.get("gpu_type").map(|s| s.as_str()), Some("4090"));

        // 售罄 → 无候选
        let mut sold = item.clone();
        sold.status = "SoldOut".into();
        assert!(to_available_spec(&sold, 1, 16, 64, Some(&price), "img", Some(0)).is_none());

        // 无显存信息 → 无候选
        let mut no_vram = item.clone();
        no_vram.graphics_memory = None;
        no_vram.name = "UnknownCard".into();
        assert!(to_available_spec(&no_vram, 1, 16, 64, Some(&price), "img", None).is_none());
    }

    #[test]
    fn available_spec_without_price_is_marked_unpriced() {
        let item = AvailableInstanceType {
            name: "4090".into(),
            status: "Normal".into(),
            graphics_memory: Some(GraphicsMemory { value_gb: 24, rate: 3 }),
            ..Default::default()
        };
        let s = to_available_spec(&item, 1, 16, 64, None, "img", None).unwrap();
        // 价格未知时用 MAX 让节点选择器把它排在最后，而不是当成免费
        assert_eq!(s.price.per_hour_cny, f64::MAX);
        assert!(s.price.note.is_none());
        // zone 缺失时给默认，避免被地域白名单误杀
        assert_eq!(s.region, "default");
    }
}
