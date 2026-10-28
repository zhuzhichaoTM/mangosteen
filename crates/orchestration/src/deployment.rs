//! 部署领域产物与纯编排辅助函数。
//!
//! 这些类型属于第三层编排契约，网关只负责把 HTTP/CLI 请求收口到它们。

use crate::assess::{DeploymentRequest, Evaluated};
use mangosteen_core::plan::ComputeOrigin;

/// 「准备」阶段的产物。
///
/// 把「评估 + 选择 + 配置」与「启动」分开，是为了让**下载夹在中间**：
/// UI 对未下载的模型要走「评估 → 选设备 → 配置 → 下载 → 启动」，
/// 先让用户看到跑不跑得动，再花时间下几 GB。
#[derive(Debug)]
pub enum PreparedDeployment {
    /// 决策为本地：参数已定、台账已落，等待下载（若需要）后启动。
    ///
    /// `Box` 与 [`PreparedDeployment::Done`] 同理：两个变体体积相差一个数量级。
    Local(Box<PreparedLocal>),
    /// 决策为云端：云端链路不需要本地文件，已在 prepare 内完成。
    ///
    /// `Box` 是因为 `DeploymentPlan` 远大于 `PreparedLocal`（含云端规格、
    /// 引擎参数等整块配置）；不装箱会让每次 `prepare` 的返回值都搬几百字节。
    Done(Box<mangosteen_core::plan::DeploymentPlan>),
}

/// 本地部署的「已准备」状态，等待 [`Gateway::commit_local`] 启动。
///
/// 为什么带 `evaluated`：启动后要按它回填/展示模型规格，而重新评估一次
/// 既浪费（远端预取要发 HTTP）又可能与用户刚看到的结果不一致。
///
/// 为什么带 `req`：启动失败时要判断「是否允许降级云端」，而
/// `req.target.forces_local()` 是唯一的判据——静默降级会直接产生计费。
#[derive(Debug)]
pub struct PreparedLocal {
    /// 部署 ID（台账里已有 `provisioning` 记录）
    pub id: String,
    /// 评估结论（含资源需求分解，供 UI 展示）
    pub evaluated: Evaluated,
    /// 本地启动参数
    pub plan: mangosteen_core::plan::LocalPlan,
    /// 原始部署诉求（降级判据需要）
    pub req: DeploymentRequest,
}

impl PreparedLocal {
    /// 补上模型文件的**实际落盘路径**。
    ///
    /// ## 为什么必须有这一步
    ///
    /// `prepare` 发生在下载**之前**，此时文件还不存在：远端评估
    /// （`evaluate_remote`）和粗估（`evaluate_estimated`）构造的
    /// `Evaluated` 里 `model_path` 是空的，`LocalPlan::from_assessment`
    /// 照抄这个空值。若不在启动前补上，引擎会收到空路径并报
    /// 「模型文件不存在：」——冒号后面什么都没有，极难排查。
    ///
    /// 调用方（网关的下载任务管理器）知道下载落地位置，
    /// 在启动前调本方法写回。
    ///
    /// 模型已在本地时（`evaluate_file`）路径本来就是对的，
    /// 重复设置成同一路径无害。
    pub fn with_model_path(mut self, path: impl Into<String>) -> Self {
        let p = path.into();
        if !p.is_empty() {
            self.plan.model_path = p.clone();
            self.evaluated.model_path = std::path::PathBuf::from(p);
        }
        self
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
        "{} · {} · ctx={} · {}　| 本地{}",
        evaluated.spec.name,
        a.placement.describe(evaluated.spec.n_layers),
        a.context_len,
        a.requirement.describe(),
        if a.placement.is_local_satisfiable() {
            "可行"
        } else {
            "不可行"
        }
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
    /// 本地是否可行
    pub local_viable: bool,
    /// 本地不可行的具体原因（本地可行时为 `None`）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_reason: Option<String>,
    /// 将会使用的算力来源
    pub origin: ComputeOrigin,
    /// 本地方案（本地可行时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalPreview>,
    /// 云端方案与费用（需要上云且已选到节点时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud: Option<CloudPreview>,
    /// 备选节点（按打分排序，最多 5 个）
    pub cloud_candidates: Vec<CloudCandidateBrief>,
    /// 选点失败原因（无可用服务商/无满足约束的规格）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_error: Option<String>,
    /// 用户选择了「本地算力」但本地不可行 → 部署会**停止**，不会上云。
    ///
    /// 为什么要单独一个字段：此时 `local_viable=false` 且 `cloud` 可能有值
    /// （因为我们仍会算一次云端报价供用户参考「改选云端要花多少钱」），
    /// 前端若按「cloud 有值 ⇒ 会创建实例」渲染，就会让用户误以为点确认
    /// 会上云。实际行为是拒绝部署。
    pub blocked: bool,
}

/// 本地方案预览。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LocalPreview {
    /// 落位形态
    pub placement: String,
    /// 上下文
    pub context_len: u32,
    /// KV 精度
    pub kv_cache: String,
    /// 量化
    pub quant: String,
    /// 权重字节
    pub weights_bytes: u64,
    /// KV 字节
    pub kv_cache_bytes: u64,
    /// 计算缓冲字节
    pub compute_bytes: u64,
    /// 运行时开销字节
    pub overhead_bytes: u64,
    /// 合计
    pub total_bytes: u64,
    /// 线程数
    pub threads: u32,
    /// 加速卡预算
    pub accelerator_budget_bytes: u64,
    /// 主机内存预算
    pub host_budget_bytes: u64,
    /// 提示
    pub warnings: Vec<String>,
}

impl LocalPreview {
    pub fn from_assessment(e: &Evaluated) -> Self {
        let a = &e.assessment;
        Self {
            placement: a.placement.describe(e.spec.n_layers),
            context_len: a.context_len,
            kv_cache: a.kv_cache.as_cli_value().to_string(),
            quant: a.quant.to_string(),
            weights_bytes: a.requirement.weights_bytes,
            kv_cache_bytes: a.requirement.kv_cache_bytes,
            compute_bytes: a.requirement.compute_buffer_bytes,
            overhead_bytes: a.requirement.runtime_overhead_bytes,
            total_bytes: a.requirement.total_bytes(),
            threads: a.threads,
            accelerator_budget_bytes: a.accelerator_budget_bytes,
            host_budget_bytes: a.host_budget_bytes,
            warnings: a.warnings.iter().map(|w| w.message()).collect(),
        }
    }
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
