//! 部署领域产物与纯编排辅助函数。
//!
//! 这些类型属于第三层编排契约，网关只负责把 HTTP/CLI 请求收口到它们。

use crate::assess::Evaluated;
use mangosteen_core::plan::ComputeOrigin;

/// 「准备」阶段的产物。
///
/// 把「评估 + 选择 + 配置」与「启动」分开，是为了让**下载夹在中间**：
/// UI 对未下载的模型要走「评估 → 选设备 → 配置 → 下载 → 启动」，
/// 先让用户看到跑不跑得动，再花时间下几 GB。
///
/// 云端同样两段：**设备配置**（建实例/连 SSH/准备二进制/远端下载/建隧道）
/// 与**启动模型**（拉起引擎 + 探活）。用户要求「节点选好后先完成设备初始化，
/// 再启动推理」，因此云端不能在一次调用里跑完。
#[derive(Debug)]
pub enum PreparedDeployment {
    /// 云端/远端节点：资源已就绪，等待 [`crate::Orchestrator::commit_cloud`] 启动引擎。
    Cloud(Box<PreparedCloud>),
}

/// 云端/远端节点的「已准备」状态，等待启动引擎。
///
/// ## 为什么不把 SSH 密码写进台账
///
/// `PreparedCloud` 只活在内存里（prepare → commit 之间），密码随它一起
/// 在进程内传递；台账里只记节点 IP、端口、用户名，绝不落盘密码。
#[derive(Debug)]
pub struct PreparedCloud {
    /// 部署 ID（台账里已有 `provisioning` 记录）
    pub id: String,
    /// 完整方案（含云端参数，台账已写入）
    pub plan: mangosteen_core::plan::DeploymentPlan,
    /// 远端连接信息（内存持有，不落盘）
    pub ssh: mangosteen_device::remote::SshTarget,
    /// 远端工作目录
    pub workdir: String,
}

/// 卸载结果：说明停了什么、保留了什么。
///
/// UI 需要据此给出准确反馈——尤其要明确「机器是否还在计费」，
/// 这是用户唯一无法自己观察到的信息。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnloadOutcome {
    /// 被**停止**（关机）的云实例 ID。
    ///
    /// 注意语义：卸载只停不删，实例与磁盘保留以便下次复用；
    /// 真正销毁由系统按策略回收或用户删除节点时处理。
    pub stopped_instance: Option<String>,
    /// 是否保留了节点（自定义节点：机器与文件都不动）。
    pub node_kept: bool,
}

/// 已保存设备的连接信息（内存态，密码来自配置）。
#[derive(Debug, Clone)]
pub struct DeviceTarget {
    /// 配置里的设备条目（不含解析后的密码）
    pub device: mangosteen_core::config::RemoteDevice,
    /// 远端连接信息（含解析后的密码）
    pub ssh: mangosteen_device::remote::SshTarget,
    /// 远端工作目录
    pub workdir: String,
}

impl DeviceTarget {
    /// 由配置条目构造，解析密码引用。
    ///
    /// 密码解析失败（如引用的环境变量未设置）不在这里报错：此时还没有
    /// 做任何破坏性动作，留到 SSH 连接时给出更具体的错误更合适。
    pub fn from_config(d: &mangosteen_core::config::RemoteDevice, default_workdir: &str) -> Self {
        let password = d
            .password
            .resolve(mangosteen_platform::paths::real_env)
            .ok()
            .filter(|p| !p.is_empty());
        let workdir = if d.workdir.trim().is_empty() {
            default_workdir.trim_end_matches('/').to_string()
        } else {
            d.workdir.trim_end_matches('/').to_string()
        };
        Self {
            device: d.clone(),
            ssh: mangosteen_device::remote::SshTarget {
                host: d.host.clone(),
                port: d.port,
                user: d.user.clone(),
                password,
                identity_file: None,
            },
            workdir,
        }
    }
}

/// 部署弹窗里的「已保存节点」条目（不含密码）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceBrief {
    /// 设备 ID
    pub id: String,
    /// 设备名称
    pub name: String,
    /// 主机名或 IP
    pub host: String,
    /// SSH 端口
    pub port: u16,
    /// 登录用户
    pub user: String,
    /// 是否已配置登录密码（**绝不回显密码本身**）
    pub password_configured: bool,
    /// 远端工作目录
    pub workdir: String,
}

impl DeviceBrief {
    /// 由配置条目构造。
    pub fn from_config(d: &mangosteen_core::config::RemoteDevice) -> Self {
        Self {
            id: if d.id.trim().is_empty() {
                mangosteen_core::config::RemoteDevice::derive_id(&d.host, d.port)
            } else {
                d.id.clone()
            },
            name: if d.name.trim().is_empty() {
                format!("自定义节点 {}:{}", d.host, d.port)
            } else {
                d.name.clone()
            },
            host: d.host.clone(),
            port: d.port,
            user: d.user.clone(),
            password_configured: d.has_password(),
            workdir: d.workdir.clone(),
        }
    }
}

/// 取错误信息的第一行（弹窗里只需要一句能读懂的话）。
pub fn first_line(msg: &str) -> String {
    msg.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("引擎无法启动")
        .trim()
        .to_string()
}

/// 「模型评估」阶段的结果文字。
///
/// 必须包含**资源占用明细**：这是用户决定要不要继续部署的唯一依据。
/// 只写「评估完成」等于没说——用户想问的正是「要占多少内存 / 跑不跑得动」。
///
/// 用 [`ResourceRequirement::describe`] 而不是自己拼：那个函数已被 CLI
/// 复用，两处格式一旦分叉，用户会看到同一份数据两种说法。
pub fn assessed_detail(evaluated: &Evaluated) -> String {
    let a = &evaluated.assessment;
    format!(
        "{} · {} · ctx={} · {}",
        evaluated.spec.name,
        a.placement.describe(evaluated.spec.n_layers),
        a.context_len,
        a.requirement.describe(),
    )
}

/// 「设备配置」阶段的结果文字。
///
/// 把真正会传给引擎的参数摊开写：用户排障时要对照这些值，
/// 含糊的「配置完成」没法用来核对。
pub fn configured_detail(plan: &mangosteen_core::plan::LocalPlan) -> String {
    format!(
        "本地 · {} 线程 · 批 {} · 上下文 {} · 端口 {} · 引擎 {} · {} · KV {}/{}",
        plan.threads,
        plan.batch_size,
        plan.context_size,
        plan.port,
        plan.engine.display_name(),
        if plan.gpu_layers == 0 {
            "纯 CPU".to_string()
        } else {
            format!("{} 层到 GPU", plan.gpu_layers)
        },
        plan.kv_cache_type_k.as_cli_value(),
        plan.kv_cache_type_v.as_cli_value(),
    )
}

/// 云端费用的默认预估时长（小时）。
///
/// 为什么是 1 小时：个人用户跑一次推理调试通常在这个量级。给「按小时」
/// 口径是为了让用户能自己乘——不假装知道他要跑多久。
pub fn default_estimate_hours() -> u32 {
    1
}

/// 部署预估结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeploymentPreview {
    /// 模型名
    pub model: String,
    /// 完整模型 ID
    pub model_id: String,
    /// 将会使用的算力来源
    pub origin: ComputeOrigin,
    /// 云端方案与费用（需要上云且已选到节点时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudPreview>,
    /// 备选节点（按打分排序，最多 5 个）
    pub cloud_candidates: Vec<CloudCandidateBrief>,
    /// 选点失败原因（无可用服务商/无满足约束的规格）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_error: Option<String>,
    /// 当前没有任何可用节点（既无已保存设备，也无云候选）→ 前端不得继续。
    ///
    /// 为什么要单独一个字段：此时 `cloud` 为 `None` 且 `cloud_error` 有值，
    /// 前端若只按「列表为空就禁用」渲染，用户看不到原因；按 `blocked`
    /// 渲染可以给出明确的下一步（去设置页添加设备 / 检查凭证）。
    pub blocked: bool,
    /// 已保存的 SSH 设备（不含密码）。前端优先展示这一组。
    pub devices: Vec<DeviceBrief>,
    /// 默认选中的设备 ID（`None` 表示没有设备）。
    pub default_device_id: Option<String>,
}

/// 云端方案预览（含费用）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CloudPreview {
    /// 服务商
    pub provider: String,
    /// 规格 ID
    pub spec_id: String,
    /// GPU 型号
    pub gpu_model: String,
    /// 卡数
    pub gpu_count: u32,
    /// 显存字节
    pub vram_bytes: u64,
    /// 地域
    pub region: String,
    /// 延迟
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 时价（元/小时）
    pub price_per_hour: f64,
    /// 预估时长（小时）
    pub estimate_hours: u32,
    /// 预估总额（元）
    pub estimate_total: f64,
    /// 该模型在云上需要的显存
    pub required_vram_bytes: u64,
}

/// 备选节点摘要。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CloudCandidateBrief {
    /// 服务商
    pub provider: String,
    /// 规格 ID（厂商私有）。用户选定后原样回传给部署接口，
    /// 编排层据此优先创建该规格，而不是重新挑。
    pub spec_id: String,
    /// GPU 型号
    pub gpu_model: String,
    /// 地域
    pub region: String,
    /// 时价
    pub price_per_hour: f64,
    /// 显存
    pub vram_bytes: u64,
    /// 延迟
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 总分
    pub score_total: f64,
    /// 档位：`minimal`（最低价可跑）/ `balanced`（综合最优）/
    /// `performance`（性能最强）。同一规格只会出现一次。
    pub tier: String,
    /// 厂商报的库存（`None` 表示厂商不提供）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stock: Option<u32>,
    /// CPU 核数
    pub cpu_cores: u32,
    /// 内存字节
    pub memory_bytes: u64,
}
/// 截断原因文本（避免轨迹过长）。
pub fn short(s: &str) -> String {
    s.chars().take(80).collect()
}

/// 把模型名转成 ID 安全片段。
pub fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "model".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}
