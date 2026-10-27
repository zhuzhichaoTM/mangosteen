//! 厂商返回码的语义归类。
//!
//! 归属：**算力执行层 · 云端后端管理**（错误归一化）。
//!
//! ## 为什么要单独归类
//!
//! 上层（调度决策器）需要区分三类失败，处理方式完全不同：
//!
//! | 类别 | 例子 | 正确处置 |
//! |------|------|----------|
//! | 库存/容量 | `226604` 资源不足 | **换节点重试**（本次部署换一家/换机型） |
//! | 账户状态 | `226601` 欠费、`226606` 未实名 | **停止重试**，提示用户去控制台处理 |
//! | 并发冲突 | `InstanceOperationInProgress` | **稍后重试**（厂商侧 10 分钟操作锁） |
//!
//! 若把「欠费」当瞬时故障重试，只会连续打厂商接口并持续失败；
//! 若把「资源不足」当终态，则会错失其它可用节点。所以归类是必要的一层。
//!
//! 归类结果不改变 [`MnError`] 的类型（仍是 `Provider`，携带厂商原始码），
//! 只用于生成可操作的提示文案，并供本 crate 内部决定是否继续尝试。

use mangosteen_core::{MnError, MnResult};

/// 失败语义类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFailure {
    /// 库存不足（可换节点重试）
    ResourceNotEnough,
    /// 账户欠费
    Arrears,
    /// 实例/资源已过期
    Expired,
    /// 镜像与卡型不匹配
    ImageIncompatible,
    /// 账号未实名认证
    NotVerified,
    /// 厂商侧操作锁未释放（同一实例 10 分钟 TTL）
    OperationInProgress,
    /// 厂商服务异常
    ServiceError,
    /// 鉴权失败（公钥/私钥错误）
    AuthFailed,
    /// 参数非法
    InvalidParameter,
    /// 未归类
    Other,
}

impl ApiFailure {
    /// 是否值得「换一个节点/规格再试」。
    ///
    /// 只有库存类与厂商侧瞬时故障值得重试；账户类问题重试必然再失败。
    pub fn is_worth_retry(self) -> bool {
        matches!(
            self,
            ApiFailure::ResourceNotEnough
                | ApiFailure::ServiceError
                | ApiFailure::OperationInProgress
        )
    }

    /// 面向用户的可操作提示。
    pub fn hint(self) -> &'static str {
        match self {
            ApiFailure::ResourceNotEnough => {
                "该规格当前无库存。可稍后重试、换用其它 GPU 型号或其它服务商。"
            }
            ApiFailure::Arrears => {
                "账户存在欠费，厂商侧拒绝创建。请到优云智算控制台「订单管理」结清欠费后重试。"
            }
            ApiFailure::Expired => "实例或资源已过期。请到控制台「续费管理」处理后再试。",
            ApiFailure::ImageIncompatible => {
                "所选镜像不支持该 GPU 卡型。请换一个卡型，或改用该卡型支持的镜像。"
            }
            ApiFailure::NotVerified => "账号尚未实名认证。优云智算要求实名后才能创建实例。",
            ApiFailure::OperationInProgress => {
                "该实例上已有操作正在进行（厂商侧操作锁 10 分钟）。请等待上一条操作完成后再试。"
            }
            ApiFailure::ServiceError => "厂商服务异常，可稍后重试。",
            ApiFailure::AuthFailed => {
                "鉴权失败。请检查 PublicKey / PrivateKey 是否正确，以及密钥是否已启用。"
            }
            ApiFailure::InvalidParameter => "请求参数被厂商拒绝，请检查规格/镜像/磁盘配置。",
            ApiFailure::Other => "",
        }
    }
}

/// 已知厂商返回码（来自官方「常见错误码」文档）。
pub mod codes {
    /// 实例已欠费
    pub const ARREARS: i64 = 226_601;
    /// 实例已过期
    pub const EXPIRED: i64 = 226_602;
    /// 所选镜像不支持该卡型
    pub const IMAGE_NOT_SUPPORT_GPU: i64 = 226_603;
    /// 当前资源不足
    pub const RESOURCE_NOT_ENOUGH: i64 = 226_604;
    /// 更新镜像使用时长任务已存在
    pub const IMAGE_TASK_EXISTS: i64 = 226_605;
    /// 账号未实名认证
    pub const NOT_VERIFIED: i64 = 226_606;
    /// 服务异常
    pub const SERVICE_ERROR: i64 = 8_433;
}

/// 归类一个厂商返回码。
///
/// 除了数值码，还要看 `message`：厂商把并发锁这类状态放在错误消息里
/// （`InstanceOperationInProgress`），没有稳定的数值码。
pub fn classify(ret_code: i64, message: &str) -> ApiFailure {
    match ret_code {
        codes::ARREARS => return ApiFailure::Arrears,
        codes::EXPIRED => return ApiFailure::Expired,
        codes::IMAGE_NOT_SUPPORT_GPU => return ApiFailure::ImageIncompatible,
        codes::RESOURCE_NOT_ENOUGH => return ApiFailure::ResourceNotEnough,
        codes::NOT_VERIFIED => return ApiFailure::NotVerified,
        codes::SERVICE_ERROR => return ApiFailure::ServiceError,
        _ => {}
    }

    let lower = message.to_ascii_lowercase();
    // 并发锁：没有稳定数值码，只能按消息识别
    if lower.contains("instanceoperationinprogress") || message.contains("操作正在进行") {
        return ApiFailure::OperationInProgress;
    }
    if lower.contains("resource not enough") || message.contains("资源不足") {
        return ApiFailure::ResourceNotEnough;
    }
    if lower.contains("signature") || message.contains("签名") {
        return ApiFailure::AuthFailed;
    }
    // 注意 `public key` 与 `publickey` 两种写法都要覆盖：
    // 厂商不同接口的文案并不统一，只匹配带空格的会把鉴权失败误判为未知错误。
    if lower.contains("public key")
        || lower.contains("publickey")
        || lower.contains("private key")
        || lower.contains("privatekey")
        || message.contains("公钥")
        || message.contains("密钥")
    {
        return ApiFailure::AuthFailed;
    }
    if lower.contains("parameter") || message.contains("参数") {
        return ApiFailure::InvalidParameter;
    }
    ApiFailure::Other
}

/// 把厂商返回码转成统一错误。
///
/// `code` 保留厂商原始数值（便于用户对照官方文档搜），
/// `message` 前置可操作提示，后面附厂商原文——两者都不能省：
/// 前者告诉用户「该怎么做」，后者是排障的唯一线索。
pub fn provider_error(provider: &str, ret_code: i64, vendor_message: &str) -> MnError {
    let kind = classify(ret_code, vendor_message);
    let hint = kind.hint();
    let detail = if hint.is_empty() {
        vendor_message.trim().to_string()
    } else {
        format!("{hint}（厂商原文：{}）", vendor_message.trim())
    };
    MnError::Provider {
        provider: provider.to_string(),
        code: ret_code.to_string(),
        message: detail,
    }
}

/// 校验返回码，非 0 即错误。
pub fn ensure_success(provider: &str, ret_code: i64, message: &str) -> MnResult<()> {
    if ret_code == 0 {
        Ok(())
    } else {
        Err(provider_error(provider, ret_code, message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_map_to_semantics() {
        assert_eq!(classify(226_601, ""), ApiFailure::Arrears);
        assert_eq!(classify(226_602, ""), ApiFailure::Expired);
        assert_eq!(classify(226_603, ""), ApiFailure::ImageIncompatible);
        assert_eq!(classify(226_604, ""), ApiFailure::ResourceNotEnough);
        assert_eq!(classify(226_606, ""), ApiFailure::NotVerified);
        assert_eq!(classify(8_433, ""), ApiFailure::ServiceError);
    }

    #[test]
    fn operation_lock_is_detected_from_message() {
        // 文档明确：重复操作返回 InstanceOperationInProgress，但没有稳定数值码
        assert_eq!(
            classify(999_999, "InstanceOperationInProgress"),
            ApiFailure::OperationInProgress
        );
        assert_eq!(
            classify(999_999, "instance is locked: InstanceOperationInProgress"),
            ApiFailure::OperationInProgress
        );
    }

    #[test]
    fn auth_failures_are_recognized() {
        assert_eq!(classify(1, "signature not match"), ApiFailure::AuthFailed);
        assert_eq!(classify(1, "PublicKey invalid"), ApiFailure::AuthFailed);
    }

    #[test]
    fn retry_policy_per_category() {
        assert!(ApiFailure::ResourceNotEnough.is_worth_retry());
        assert!(ApiFailure::ServiceError.is_worth_retry());
        assert!(ApiFailure::OperationInProgress.is_worth_retry());
        // 账户类问题重试没有意义
        assert!(!ApiFailure::Arrears.is_worth_retry());
        assert!(!ApiFailure::Expired.is_worth_retry());
        assert!(!ApiFailure::NotVerified.is_worth_retry());
        assert!(!ApiFailure::AuthFailed.is_worth_retry());
        assert!(!ApiFailure::ImageIncompatible.is_worth_retry());
    }

    #[test]
    fn every_category_except_other_has_a_hint() {
        for k in [
            ApiFailure::ResourceNotEnough,
            ApiFailure::Arrears,
            ApiFailure::Expired,
            ApiFailure::ImageIncompatible,
            ApiFailure::NotVerified,
            ApiFailure::OperationInProgress,
            ApiFailure::ServiceError,
            ApiFailure::AuthFailed,
            ApiFailure::InvalidParameter,
        ] {
            assert!(!k.hint().is_empty(), "{k:?} 缺少提示");
        }
        assert!(ApiFailure::Other.hint().is_empty());
    }

    #[test]
    fn provider_error_keeps_vendor_code_and_adds_hint() {
        let e = provider_error("compshare", 226_604, "Resource not enough");
        match &e {
            MnError::Provider {
                provider,
                code,
                message,
            } => {
                assert_eq!(provider, "compshare");
                assert_eq!(code, "226604", "必须保留厂商原始码");
                assert!(message.contains("无库存"), "{message}");
                assert!(message.contains("Resource not enough"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
        // 云端瞬时故障参与降级重试
        assert!(e.is_retryable());
    }

    #[test]
    fn unknown_code_keeps_raw_message_only() {
        let e = provider_error("compshare", 123_456, "something odd");
        let s = e.to_string();
        assert!(s.contains("123456"), "{s}");
        assert!(s.contains("something odd"), "{s}");
    }

    #[test]
    fn ensure_success_passes_zero() {
        assert!(ensure_success("compshare", 0, "").is_ok());
        assert!(ensure_success("compshare", 226_604, "x").is_err());
    }
}
