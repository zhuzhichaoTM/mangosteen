//! 跨层统一错误类型。
//!
//! 设计要点：
//! * 错误是**领域概念**，不绑定具体传输（HTTP / CLI / 日志）表现形式；
//! * 每个变体都携带足够上下文，便于在网关层直接映射为 HTTP 状态码与可读消息；
//! * [`MnError::is_retryable`] 供调度决策器判断是否值得降级重试。

use std::fmt;
use std::path::PathBuf;

/// 便捷结果别名。
pub type MnResult<T> = Result<T, MnError>;

/// Mangosteen 统一错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MnError {
    /// 配置缺失或非法。`field` 为点分路径（如 `providers.autodl.api_key`）。
    Config { field: String, reason: String },

    /// 配置文件读取 / 解析失败。
    ConfigIo { path: PathBuf, reason: String },

    /// 模型文件不存在或不可读。
    ModelNotFound { path: PathBuf },

    /// GGUF 文件格式非法（magic 不符、版本不支持、元数据越界等）。
    GgufFormat { path: PathBuf, reason: String },

    /// 模型哈希与台账记录不一致。
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    /// 本地硬件不足以完成该模型的**任何**部署形态（含 CPU 回退）。
    InsufficientLocalResource { detail: String },

    /// 本地推理引擎（llama.cpp）启动失败。
    EngineLaunch { binary: PathBuf, reason: String },

    /// 推理引擎进程在运行期退出。
    EngineExited {
        pid: u32,
        code: Option<i32>,
        stderr_tail: String,
    },

    /// 云端驱动未注册或未启用。
    DriverUnavailable { provider: String },

    /// 云端服务商返回的业务错误（已归一化）。
    Provider { provider: String, code: String, message: String },

    /// 云端实例状态迁移非法（如对 `Destroyed` 实例执行 `stop`）。
    InvalidTransition {
        instance: String,
        from: String,
        action: String,
    },

    /// 节点选择器在候选集中找不到可用实例。
    NoViableCandidate { reason: String },

    /// 网络工具集操作失败（隧道、端口映射、连通性探测）。
    Network { stage: String, reason: String },

    /// 文件访问控制拒绝。
    AccessDenied { path: PathBuf, reason: String },

    /// 请求参数非法（供 OpenAI 兼容层返回 400）。
    BadRequest { reason: String },

    /// 底层 IO 错误。
    Io { context: String, reason: String },

    /// 操作被用户主动取消。
    ///
    /// 与「失败」区分开是必须的：取消要走**回滚**（删掉半成品、
    /// 释放端口、撤销台账记录），而失败要走**重试/降级**。
    /// 混成一个 `Network` 错误会让重试逻辑把用户刚取消的下载
    /// 又拉起来——用户会觉得「点了停止反而更忙了」。
    Cancelled { stage: String },

    /// 云端直连下载模型失败（**部署就此停止**）。
    ///
    /// ## 为什么单独一个变体
    ///
    /// 它需要携带「哪个地址失败了」——用户据此才能判断是换源、还是等网络好
    /// 了重试。塞进 `Network` 只能给一句笼统的「网络操作失败」。
    ///
    /// ## 没有本地上传兜底
    ///
    /// 云端部署的语义就是「模型落在云上」。失败后偷偷改成「本地下载几 GB
    /// 再传上去」会背离这个语义，且占用用户磁盘与带宽。所以这里就是终点：
    /// 实例已被回收，用户自己决定换源重试或改用本地部署。
    ///
    /// 在 [`MnError::is_retryable`] 里返回 `false`——自动重试会把几 GB
    /// 流量重复消耗，而失败原因（源不可达、被墙）通常不会自愈。
    CloudDownloadFailed {
        /// 远端尝试下载的地址
        url: String,
        /// 失败原因（厂商/网络原文）
        reason: String,
    },
}

impl MnError {
    /// 是否为「用户主动取消」（调用方据此跳过重试与降级）。
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }

    /// 构造配置错误。
    pub fn config(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Config {
            field: field.into(),
            reason: reason.into(),
        }
    }

    /// 构造请求参数错误。
    pub fn bad_request(reason: impl Into<String>) -> Self {
        Self::BadRequest {
            reason: reason.into(),
        }
    }

    /// 构造「云端下载失败，需用户决定是否改用本地上传」错误。
    pub fn cloud_download_failed(url: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::CloudDownloadFailed {
            url: url.into(),
            reason: reason.into(),
        }
    }

    /// 是否为「云端下载失败，需用户确认是否改用本地上传」。
    pub fn is_cloud_download_failed(&self) -> bool {
        matches!(self, Self::CloudDownloadFailed { .. })
    }

    /// 构造 IO 错误。
    pub fn io(context: impl Into<String>, err: &std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            reason: err.to_string(),
        }
    }

    /// 该错误是否具备「换一条链路再试」的价值。
    ///
    /// 语义：返回 `true` 时，调度决策器可以触发降级重试（本地 → 云端，
    /// 或云端换节点）。**配置类与请求类错误不参与重试**，因为它们与
    /// 算力来源无关，换链路也不会成功。
    pub fn is_retryable(&self) -> bool {
        match self {
            // 资源不足与引擎启动失败：本地链路的典型降级触发点。
            MnError::InsufficientLocalResource { .. } => true,
            MnError::EngineLaunch { .. } => true,
            MnError::EngineExited { .. } => true,
            // 云端瞬时故障：值得换节点。
            MnError::Provider { .. } => true,
            MnError::NoViableCandidate { .. } => true,
            MnError::Network { .. } => true,
            MnError::InvalidTransition { .. } => false,
            // 与链路无关的错误，重试无意义。
            MnError::Config { .. }
            | MnError::ConfigIo { .. }
            | MnError::ModelNotFound { .. }
            | MnError::GgufFormat { .. }
            | MnError::HashMismatch { .. }
            | MnError::DriverUnavailable { .. }
            | MnError::AccessDenied { .. }
            | MnError::BadRequest { .. }
            // 用户主动取消：绝不能重试，否则「点了停止反而更忙」
            | MnError::Cancelled { .. }
            // 云端下载失败是「岔路口」不是终点：要不要改走本地上传必须
            // 由用户决定。自动重试/降级会绕过用户的知情权。
            | MnError::CloudDownloadFailed { .. }
            | MnError::Io { .. } => false,
        }
    }

    /// 稳定错误码。用于 CLI 退出码映射与 HTTP 错误体中的 `code` 字段。
    pub fn code(&self) -> &'static str {
        match self {
            MnError::Config { .. } | MnError::ConfigIo { .. } => "config_invalid",
            MnError::ModelNotFound { .. } => "model_not_found",
            MnError::GgufFormat { .. } => "gguf_format_invalid",
            MnError::HashMismatch { .. } => "hash_mismatch",
            MnError::InsufficientLocalResource { .. } => "insufficient_local_resource",
            MnError::EngineLaunch { .. } => "engine_launch_failed",
            MnError::EngineExited { .. } => "engine_exited",
            MnError::DriverUnavailable { .. } => "driver_unavailable",
            MnError::Provider { .. } => "provider_error",
            MnError::InvalidTransition { .. } => "invalid_transition",
            MnError::NoViableCandidate { .. } => "no_viable_candidate",
            MnError::Network { .. } => "network_error",
            MnError::AccessDenied { .. } => "access_denied",
            MnError::BadRequest { .. } => "bad_request",
            MnError::Cancelled { .. } => "cancelled",
            MnError::CloudDownloadFailed { .. } => "cloud_download_failed",
            MnError::Io { .. } => "io_error",
        }
    }

    /// 面向 CLI 的进程退出码。沿用 BSD sysexits 习惯，便于脚本化调用。
    pub fn exit_code(&self) -> i32 {
        match self {
            MnError::BadRequest { .. } => 64,        // EX_USAGE
            MnError::ModelNotFound { .. }
            | MnError::GgufFormat { .. }
            | MnError::HashMismatch { .. } => 66,    // EX_NOINPUT
            MnError::Config { .. } | MnError::ConfigIo { .. } => 78, // EX_CONFIG
            MnError::AccessDenied { .. } => 77,      // EX_NOPERM
            MnError::InsufficientLocalResource { .. } => 69, // EX_UNAVAILABLE
            _ => 70,                                 // EX_SOFTWARE
        }
    }
}

impl fmt::Display for MnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MnError::Config { field, reason } => {
                write!(f, "配置项 `{field}` 非法：{reason}")
            }
            MnError::ConfigIo { path, reason } => {
                write!(f, "配置文件 {} 不可用：{reason}", path.display())
            }
            MnError::ModelNotFound { path } => {
                write!(f, "模型文件不存在：{}", path.display())
            }
            MnError::Cancelled { stage } => {
                write!(f, "已取消：{stage}")
            }
            MnError::CloudDownloadFailed { url, reason } => {
                // 说清三件事：在哪失败、为什么、实例已处理。
                // 「实例已回收」必须写——否则用户会担心是不是还在计费。
                write!(
                    f,
                    "云端下载模型失败：{reason}\n  地址：{url}\n  \
                     已回收本次创建的云端实例，未产生持续费用。\
                     可换个下载源（如 hf-mirror）重试"
                )
            }
            MnError::GgufFormat { path, reason } => {
                write!(f, "GGUF 格式错误 {}：{reason}", path.display())
            }
            MnError::HashMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "模型哈希校验失败 {}：期望 {expected}，实际 {actual}",
                path.display()
            ),
            MnError::InsufficientLocalResource { detail } => {
                write!(f, "本地资源不足：{detail}")
            }
            MnError::EngineLaunch { binary, reason } => {
                write!(f, "推理引擎启动失败（{}）：{reason}", binary.display())
            }
            MnError::EngineExited {
                pid,
                code,
                stderr_tail,
            } => match code {
                Some(c) => write!(f, "推理引擎进程 {pid} 退出，退出码 {c}：{stderr_tail}"),
                None => write!(f, "推理引擎进程 {pid} 被信号终止：{stderr_tail}"),
            },
            MnError::DriverUnavailable { provider } => {
                write!(f, "云端驱动 `{provider}` 未注册或未启用")
            }
            MnError::Provider {
                provider,
                code,
                message,
            } => write!(f, "服务商 {provider} 返回错误 [{code}]：{message}"),
            MnError::InvalidTransition {
                instance,
                from,
                action,
            } => write!(f, "实例 {instance} 处于 {from} 状态，无法执行 `{action}`"),
            MnError::NoViableCandidate { reason } => {
                write!(f, "无可用云端算力节点：{reason}")
            }
            MnError::Network { stage, reason } => {
                write!(f, "网络操作失败（{stage}）：{reason}")
            }
            MnError::AccessDenied { path, reason } => {
                write!(f, "拒绝访问 {}：{reason}", path.display())
            }
            MnError::BadRequest { reason } => write!(f, "请求非法：{reason}"),
            MnError::Io { context, reason } => write!(f, "IO 失败（{context}）：{reason}"),
        }
    }
}

impl std::error::Error for MnError {}

impl From<std::io::Error> for MnError {
    fn from(e: std::io::Error) -> Self {
        MnError::Io {
            context: "unclassified".into(),
            reason: e.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_matrix() {
        assert!(MnError::InsufficientLocalResource { detail: "x".into() }.is_retryable());
        assert!(MnError::EngineLaunch {
            binary: PathBuf::from("llama-server"),
            reason: "not found".into()
        }
        .is_retryable());
        assert!(!MnError::bad_request("bad").is_retryable());
        assert!(!MnError::config("a.b", "missing").is_retryable());
        assert!(!MnError::ModelNotFound {
            path: PathBuf::from("/m.gguf")
        }
        .is_retryable());
        assert!(!MnError::DriverUnavailable {
            provider: "autodl".into()
        }
        .is_retryable());
    }

    #[test]
    fn exit_codes_are_distinct_per_class() {
        assert_eq!(MnError::bad_request("x").exit_code(), 64);
        assert_eq!(
            MnError::ModelNotFound {
                path: PathBuf::from("/x")
            }
            .exit_code(),
            66
        );
        assert_eq!(MnError::config("a", "b").exit_code(), 78);
        assert_eq!(
            MnError::InsufficientLocalResource { detail: "x".into() }.exit_code(),
            69
        );
    }

    #[test]
    fn display_contains_actionable_context() {
        let e = MnError::HashMismatch {
            path: PathBuf::from("/models/q.gguf"),
            expected: "aaa".into(),
            actual: "bbb".into(),
        };
        let s = e.to_string();
        assert!(s.contains("/models/q.gguf"));
        assert!(s.contains("aaa"));
    }
}
