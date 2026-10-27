//! 部署阶段与进度上报。
//!
//! ## 为什么单独成一个模块
//!
//! 「部署到了哪一步」是**编排层的领域概念**，不属于下载任务表
//! （[`crate::pulljobs`]），也不属于 HTTP 层。放在这里让两边都能引用：
//!
//! - [`crate::gateway::Gateway::deploy_with_progress`] 在关键节点上报
//! - [`crate::pulljobs::PullJob`] 持有「当前阶段 + 轨迹」供前端轮询
//! - [`crate::server`] 把它序列化成 JSON
//!
//! ## 阶段顺序
//!
//! ```text
//! 模型评估 → 设备选择 → 设备配置 → 模型下载 → 模型量化 → 启动模型
//! ```
//!
//! **下载排在设备选择与配置之后**，这是刻意的产品决策：
//!
//! 用户点「部署」后先看到的是「这台机器跑不跑得动、要占多少内存、
//! 用哪个设备、什么参数」，确认无误后才花时间下载几 GB。
//! 反过来（先下再评估）会让用户白等几个小时才发现跑不动。
//!
//! ## 评估如何在下载前拿到真实参数
//!
//! GGUF 的架构参数（层数 / KV 头数 / 隐藏维度）只在文件里。为了在下载前
//! 就能**准确**评估，部署链路用 HTTP `Range: bytes=0-N` 只取文件头部
//! （元数据段通常远小于张量数据，实测 2~3 秒），走
//! [`mangosteen_core::gguf::parse_stream`] 解析。
//!
//! 若源不支持 Range 或网络抖动，**降级为按文件大小估算**并在该阶段
//! 明确标注「估算值」——不能让预取失败阻断整个部署，但也不能假装
//! 那是精确结果。
//!
//! ## 「模型量化」为什么常常是「跳过」
//!
//! 当前部署链路**不做自动量化**：用户下载的已经是量化好的 GGUF
//! （`q4_k_m` 等），再量化一次只会降精度。`base::quantize` 是独立
//! 能力（CLI 侧），不在部署主链路上。因此这一阶段如实标为
//! `Skipped` 并说明原因，而不是假装跑过——假装的结果是用户以为
//! 多花了几分钟，实际什么也没发生。

/// 部署阶段。
///
/// 顺序即 [`DeployStage::STEPS`] 的顺序；`Ready` / `Failed` 是终态，
/// 不占步骤位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployStage {
    /// 模型评估：解析规格、估算资源需求、判断本地是否可承载
    Assessing,
    /// 设备选择：本地还是云端，具体哪个设备
    Selecting,
    /// 设备配置：上下文长度、线程数、端口、卸载策略
    Configuring,
    /// 模型下载：含 SHA 校验与台账登记
    Downloading,
    /// 模型量化：当前链路不做，通常为跳过
    Quantizing,
    /// 启动模型：拉起引擎进程并等待健康检查
    Launching,
    /// 已就绪
    Ready,
    /// 失败
    Failed,
}

impl DeployStage {
    /// 正常流程的全部步骤（不含终态）。
    ///
    /// 前端按这个数组渲染步骤条，所以**顺序即 UI 顺序**。
    pub const STEPS: &'static [DeployStage] = &[
        DeployStage::Assessing,
        DeployStage::Selecting,
        DeployStage::Configuring,
        DeployStage::Downloading,
        DeployStage::Quantizing,
        DeployStage::Launching,
    ];

    /// 稳定标识（JSON 字段值，前端按它匹配样式）。
    ///
    /// 刻意不用中文：前端要按 id 查颜色/图标，中文做键易出错。
    pub fn id(&self) -> &'static str {
        match self {
            DeployStage::Assessing => "assessing",
            DeployStage::Downloading => "downloading",
            DeployStage::Selecting => "selecting",
            DeployStage::Configuring => "configuring",
            DeployStage::Quantizing => "quantizing",
            DeployStage::Launching => "launching",
            DeployStage::Ready => "ready",
            DeployStage::Failed => "failed",
        }
    }

    /// 中文名（UI 展示）。
    pub fn label(&self) -> &'static str {
        match self {
            DeployStage::Assessing => "模型评估",
            DeployStage::Downloading => "模型下载",
            DeployStage::Selecting => "设备选择",
            DeployStage::Configuring => "设备配置",
            DeployStage::Quantizing => "模型量化",
            DeployStage::Launching => "启动模型",
            DeployStage::Ready => "已就绪",
            DeployStage::Failed => "失败",
        }
    }

    /// 步骤序号（1 起）。终态返回 `STEPS.len() + 1`，方便前端算「第 N 步」。
    pub fn index(&self) -> usize {
        match self {
            DeployStage::Ready | DeployStage::Failed => Self::STEPS.len() + 1,
            s => Self::STEPS
                .iter()
                .position(|x| x == s)
                .map(|i| i + 1)
                .unwrap_or(1),
        }
    }

    /// 总步数。
    pub fn total() -> usize {
        Self::STEPS.len()
    }

    /// 从 id 还原（供反序列化与测试）。
    pub fn from_id(id: &str) -> Option<Self> {
        Self::STEPS
            .iter()
            .copied()
            .chain([DeployStage::Ready, DeployStage::Failed])
            .find(|s| s.id() == id)
    }
}

/// 单条阶段事件的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// 进行中
    Running,
    /// 已完成
    Done,
    /// 已跳过（附原因，如「无需量化」）
    Skipped,
    /// 失败
    Failed,
}

impl StageOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            StageOutcome::Running => "running",
            StageOutcome::Done => "done",
            StageOutcome::Skipped => "skipped",
            StageOutcome::Failed => "failed",
        }
    }
}

/// 一条阶段记录。
///
/// 追加式：同一阶段可能有多条（如「模型评估」先预检后资源评估），
/// 前端取**最后一条**作为该步的当前状态，历史保留供展开查看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageEvent {
    /// 阶段标识
    pub stage: &'static str,
    /// 阶段中文名（冗余存一份，前端不必再查表）
    pub label: &'static str,
    /// 结果
    pub outcome: StageOutcome,
    /// 细节说明（如「本地 · 4 线程 · 上下文 8192」）
    pub detail: String,
    /// 时间（Unix 毫秒）
    pub at_ms: u64,
}

impl StageEvent {
    /// 构造一条事件，时间取当前。
    pub fn now(stage: DeployStage, outcome: StageOutcome, detail: impl Into<String>) -> Self {
        Self {
            stage: stage.id(),
            label: stage.label(),
            outcome,
            detail: detail.into(),
            at_ms: mangosteen_core::time::now_unix_millis() as u64,
        }
    }
}

/// 进度上报回调。
///
/// 用 `dyn Fn` 而不是泛型：`Gateway` 的方法签名不该被回调类型参数化，
/// 否则每个调用点都会实例化一份单态代码（且 `&mut self` 上的泛型方法
/// 会污染整个 impl 的可用性）。
///
/// `detail` 取 `String` 而非 `&str`：事件最终要存进轨迹（`Vec<StageEvent>`），
/// 传 `&str` 会让每次上报都多一次 clone，且调用点的 `&format!(..)`
/// 临时值生命周期很别扭。
pub type ProgressSink<'a> = &'a (dyn Fn(DeployStage, StageOutcome, String) + Send + Sync);

/// 空实现（CLI 等不需要进度的调用方用）。
pub fn noop_sink(_stage: DeployStage, _outcome: StageOutcome, _detail: String) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_cover_all_non_terminal_stages() {
        // 除 Ready/Failed 外，每个阶段都必须在 STEPS 里，
        // 否则前端步骤条会漏掉一步。
        for s in [
            DeployStage::Assessing,
            DeployStage::Downloading,
            DeployStage::Selecting,
            DeployStage::Configuring,
            DeployStage::Quantizing,
            DeployStage::Launching,
        ] {
            assert!(DeployStage::STEPS.contains(&s), "{} 不在 STEPS 里", s.id());
        }
    }

    #[test]
    fn index_is_one_based_and_monotonic() {
        let mut prev = 0;
        for s in DeployStage::STEPS {
            let i = s.index();
            assert!(i > prev, "{} 的序号必须递增", s.id());
            assert!(i >= 1 && i <= DeployStage::total());
            prev = i;
        }
        assert_eq!(DeployStage::Ready.index(), DeployStage::total() + 1);
        assert_eq!(DeployStage::Failed.index(), DeployStage::total() + 1);
    }

    #[test]
    fn id_roundtrip() {
        for s in DeployStage::STEPS
            .iter()
            .copied()
            .chain([DeployStage::Ready, DeployStage::Failed])
        {
            assert_eq!(DeployStage::from_id(s.id()), Some(s));
        }
        assert_eq!(DeployStage::from_id("nope"), None);
    }

    #[test]
    fn ids_are_unique() {
        let mut seen = Vec::new();
        for s in DeployStage::STEPS
            .iter()
            .copied()
            .chain([DeployStage::Ready, DeployStage::Failed])
        {
            assert!(!seen.contains(&s.id()), "id 重复：{}", s.id());
            seen.push(s.id());
        }
    }

    #[test]
    fn event_carries_label_and_timestamp() {
        let e = StageEvent::now(DeployStage::Selecting, StageOutcome::Done, "本地 CPU");
        assert_eq!(e.stage, "selecting");
        assert_eq!(e.label, "设备选择");
        assert_eq!(e.outcome, StageOutcome::Done);
        assert!(e.at_ms > 0);
    }
}
