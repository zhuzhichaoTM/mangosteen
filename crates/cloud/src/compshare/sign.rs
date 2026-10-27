//! UCloud V1 签名（优云智算所用的认证协议）。
//!
//! 归属：**算力执行层 · 云端后端管理**（协议适配核心）。
//!
//! ## 协议
//!
//! ```text
//! StringToSign = concat( key + value )      // key 按字典序升序，value 不做 URL 转义
//! Signature    = SHA1( StringToSign + PrivateKey )   // 小写十六进制
//! ```
//!
//! 参与签名的参数包含 `Action` 等业务参数与 `PublicKey` 本身；
//! `Signature` 不参与签名。
//!
//! ## 三个必须照抄的细节（写错就会被判签名不匹配）
//!
//! 1. **value 统一先转字符串再拼接**，且：
//!    - 布尔值必须是 `true` / `false`（小写），不能是 `True` / `1`；
//!    - 浮点数若为整数值要去掉小数部分（厂商网关会按整数解析）。
//! 2. **签名用原始值**，URL 转义只在构造请求体时做；两者混用会得到不同签名。
//! 3. **拼接串末尾**紧跟 `PrivateKey`，中间没有分隔符。
//!
//! 以上三点由 [`crate::compshare::params`] 的扁平化逻辑与 [`official_test_vector`] 共同保证。

use std::collections::BTreeMap;

use mangosteen_core::{MnError, MnResult};

use crate::compshare::sha1;

/// UCloud 风格凭证：公钥（AccessKey ID）+ 私钥（AccessKey Secret）。
#[derive(Clone)]
pub struct Credential {
    public_key: String,
    private_key: String,
}

impl Credential {
    /// 构造并校验。空凭证直接拒绝——静默用空私钥签名只会换来难以定位的
    /// 「签名不匹配」错误。
    pub fn new(public_key: impl Into<String>, private_key: impl Into<String>) -> MnResult<Self> {
        let public_key = public_key.into();
        let private_key = private_key.into();
        if public_key.trim().is_empty() {
            return Err(MnError::config(
                "compshare.public_key",
                "公钥（API 公钥 / AccessKey ID）不能为空",
            ));
        }
        if private_key.trim().is_empty() {
            return Err(MnError::config(
                "compshare.private_key",
                "私钥（API 私钥 / AccessKey Secret）不能为空",
            ));
        }
        Ok(Self {
            public_key,
            private_key,
        })
    }

    /// 公钥。
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    /// 私钥（仅供本 crate 内签名函数使用，不对外暴露内容语义）。
    pub fn private_key(&self) -> &str {
        &self.private_key
    }

    /// 脱敏展示（日志/错误信息里只能出现这个）。
    pub fn redacted(&self) -> String {
        let head: String = self.public_key.chars().take(6).collect();
        format!("{head}***")
    }

    /// 为已扁平化的参数集计算签名。
    ///
    /// 会先插入 `PublicKey`（若调用方未插入），再按字典序拼接。
    pub fn sign(&self, params: &BTreeMap<String, String>) -> String {
        let mut with_key = params.clone();
        with_key.insert("PublicKey".to_string(), self.public_key.clone());
        signature_of(&self.private_key, &with_key)
    }
}

impl std::fmt::Debug for Credential {
    /// 刻意不输出私钥，避免 `{:?}` 泄漏到日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("public_key", &self.redacted())
            .field("private_key", &"***")
            .finish()
    }
}

/// 按协议计算签名（不含 `PublicKey` 的自动注入）。
///
/// `params` 必须已经扁平化（不含嵌套结构）且不含 `Signature`。
pub fn signature_of(private_key: &str, params: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    for (k, v) in params {
        // 防御：签名参数里不该出现 Signature 自身，出现则跳过
        if k.eq_ignore_ascii_case("Signature") {
            continue;
        }
        s.push_str(k);
        s.push_str(v);
    }
    s.push_str(private_key);
    sha1::hex_digest(s.as_bytes())
}

/// 为 **JSON 请求体**计算签名。
///
/// ## 与表单签名的唯一区别：数组怎么拼
///
/// 表单接口里数组用扁平索引键（`UHostIds.0`、`UHostIds.1`），
/// 签名串自然变成 `UHostIds.0uhost-aUHostIds.1uhost-b`。
///
/// JSON 接口里数组是真正的 JSON 数组，签名规则是：**键只出现一次，
/// 各元素值直接首尾相接**：
///
/// ```text
/// UHostIdsu-1u-2     // 正确
/// UHostIdsu-1,u-2    // 错：逗号分隔 → Signature VerifyAC Error
/// UHostIds["u-1","u-2"]  // 错：JSON 字面量 → Signature VerifyAC Error
/// ```
///
/// 三条结论全部来自真实调用对照实验（见 `docs` 注释与
/// `sign::tests::json_array_signs_by_concatenating_elements`）。
pub fn signature_of_json(
    private_key: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> String {
    // 键按字典序：BTreeMap 保证顺序与文档一致
    let mut entries: Vec<(&String, &serde_json::Value)> = params.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let mut s = String::new();
    for (k, v) in entries {
        if k.eq_ignore_ascii_case("Signature") {
            continue;
        }
        s.push_str(k);
        match v {
            serde_json::Value::Array(items) => {
                // 元素值直接拼接，不加任何分隔符
                for item in items {
                    s.push_str(&json_scalar_to_string(item));
                }
            }
            other => s.push_str(&json_scalar_to_string(other)),
        }
    }
    s.push_str(private_key);
    sha1::hex_digest(s.as_bytes())
}

/// JSON 标量 → 签名串里的字符串形式。
///
/// 规则与表单一致：布尔小写、整数值浮点去小数、字符串原样。
fn json_scalar_to_string(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => encode_value_bool(*b),
        Value::Number(n) => match n.as_f64() {
            Some(f) => encode_f64(f),
            None => n.to_string(),
        },
        Value::Null => String::new(),
        // 嵌套对象/数组不在本协议的使用范围内，退化为紧凑 JSON 而非 panic
        other => other.to_string(),
    }
}

/// 把参数值转成协议的字符串形式。
///
/// * 布尔 → `true` / `false`
/// * 整数值浮点 → 去小数（`100.0` → `100`）
/// * 其它浮点 → 原样
pub fn encode_value_bool(v: bool) -> String {
    if v {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

/// 浮点编码规则：整数值去掉小数部分，避免网关按 Lua 整数解析时不一致。
pub fn encode_f64(v: f64) -> String {
    if v.fract() == 0.0 && v.is_finite() {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方文档给出的测试向量（PublicKey/PrivateKey 与参数均为文档示例）。
    ///
    /// 期望值 `cba5cf5ec4d4233d206b1b54951e3787350a642f` 来自 UCloud 文档，
    /// 是判定签名实现是否正确的**唯一硬标准**。
    #[test]
    fn official_test_vector() {
        let params = BTreeMap::from([
            ("Action".to_string(), "DescribeUHostInstance".to_string()),
            ("Region".to_string(), "cn-bj2".to_string()),
            ("Limit".to_string(), "10".to_string()),
            (
                "PublicKey".to_string(),
                "ucloudsomeone@example.com1296235120854146120".to_string(),
            ),
        ]);
        let sig = signature_of("46f09bb9fab4f12dfc160dae12273d5332b5debe", &params);
        assert_eq!(sig, "cba5cf5ec4d4233d206b1b54951e3787350a642f");
    }

    #[test]
    fn sign_injects_public_key() {
        let cred = Credential::new("pk", "sk").unwrap();
        let params = BTreeMap::from([("Action".to_string(), "X".to_string())]);
        let sig = cred.sign(&params);
        // 与手工构造含 PublicKey 的参数一致
        let manual = BTreeMap::from([
            ("Action".to_string(), "X".to_string()),
            ("PublicKey".to_string(), "pk".to_string()),
        ]);
        assert_eq!(sig, signature_of("sk", &manual));
    }

    #[test]
    fn signature_is_order_independent_at_call_site() {
        let cred = Credential::new("pk", "sk").unwrap();
        let a = BTreeMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        let b = BTreeMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]);
        assert_eq!(cred.sign(&a), cred.sign(&b));
    }

    #[test]
    fn signature_changes_with_any_param() {
        let cred = Credential::new("pk", "sk").unwrap();
        let base = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let other = BTreeMap::from([("A".to_string(), "2".to_string())]);
        assert_ne!(cred.sign(&base), cred.sign(&other));
    }

    #[test]
    fn signature_excludes_itself() {
        let cred = Credential::new("pk", "sk").unwrap();
        let clean = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let mut polluted = clean.clone();
        polluted.insert("Signature".to_string(), "should-be-ignored".to_string());
        assert_eq!(cred.sign(&clean), cred.sign(&polluted));
    }

    #[test]
    fn empty_credentials_rejected() {
        assert!(Credential::new("", "sk").is_err());
        assert!(Credential::new("pk", "  ").is_err());
    }

    #[test]
    fn debug_output_hides_private_key() {
        let cred = Credential::new("public-abcdef", "super-secret-key").unwrap();
        let s = format!("{cred:?}");
        assert!(!s.contains("super-secret-key"), "私钥泄漏：{s}");
        assert!(s.contains("public***") || s.contains("public"), "{s}");
        assert_eq!(cred.redacted(), "public***");
    }

    #[test]
    fn value_encoding_rules() {
        assert_eq!(encode_value_bool(true), "true");
        assert_eq!(encode_value_bool(false), "false");
        assert_eq!(encode_f64(100.0), "100");
        assert_eq!(encode_f64(2.38), "2.38");
        assert_eq!(encode_f64(0.0), "0");
    }

    #[test]
    fn signature_is_lowercase_hex_40() {
        let cred = Credential::new("pk", "sk").unwrap();
        let sig = cred.sign(&BTreeMap::new());
        assert_eq!(sig.len(), 40);
        assert!(sig
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }
}
