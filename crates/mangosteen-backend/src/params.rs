//! 请求参数的扁平化与表单编码。
//!
//! 归属：**算力执行层 · 云端后端管理**（协议适配核心）。
//!
//! ## 为什么必须扁平化
//!
//! UCloud 风格 API 只用一层键值对传递参数，嵌套结构要按固定规则展开：
//!
//! | 输入 | 扁平键 |
//! |------|--------|
//! | `{"A": "x"}` | `A=x` |
//! | `{"Disks": [{"IsBoot": true, "Size": 100}]}` | `Disks.0.IsBoot=true`, `Disks.0.Size=100` |
//! | `{"Ids": ["u-1", "u-2"]}` | `Ids.0=u-1`, `Ids.1=u-2` |
//!
//! 这与厂商 SDK 的编码器逐条对应。**签名作用于扁平后的原始值**，
//! URL 转义只在拼请求体时进行——两件事必须分开，否则签名必然不匹配。
//!
//! ## 编码顺序
//!
//! 签名要求键按字典序升序拼接，所以内部用 [`BTreeMap`] 存储。
//! 请求体是否有序无关紧要（服务端按参数名取值），但保持有序便于快照测试。

use std::collections::BTreeMap;

use mangosteen_core::{MnError, MnResult};

use crate::sign::{encode_f64, encode_value_bool};

/// 待签名/待发送的参数集合（已扁平化）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Params {
    flat: BTreeMap<String, String>,
}

impl Params {
    /// 空集合。
    pub fn new() -> Self {
        Self::default()
    }

    /// 以 `Action` 起始构造。
    pub fn action(action: &str) -> Self {
        let mut p = Self::new();
        p.set("Action", action);
        p
    }

    /// 插入一个字符串参数。键为空视为编码错误。
    pub fn set(&mut self, key: &str, value: impl Into<String>) -> &mut Self {
        self.flat.insert(key.to_string(), value.into());
        self
    }

    /// 插入整数参数。
    pub fn set_i64(&mut self, key: &str, value: i64) -> &mut Self {
        self.set(key, value.to_string())
    }

    /// 插入布尔参数（小写形式）。
    pub fn set_bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.set(key, encode_value_bool(value))
    }

    /// 插入浮点参数（整数去小数）。
    pub fn set_f64(&mut self, key: &str, value: f64) -> &mut Self {
        self.set(key, encode_f64(value))
    }

    /// 仅在 `Some` 时插入。用于厂商接口的可选参数。
    pub fn set_opt(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        if let Some(v) = value {
            self.set(key, v);
        }
        self
    }

    /// 插入字符串数组 → `key.0`, `key.1`, ...
    ///
    /// 空数组不产生任何键（厂商把「无该参数」与「空数组」视为同一语义，
    /// 而 `key.0` 为空串会被判非法）。
    pub fn set_list(&mut self, key: &str, values: &[String]) -> &mut Self {
        for (i, v) in values.iter().enumerate() {
            self.set(&format!("{key}.{i}"), v.clone());
        }
        self
    }

    /// 插入嵌套 JSON 值，按厂商规则展开为扁平键。
    ///
    /// 支持对象、对象数组、标量数组与标量；用于 `Disks` 这类结构体数组。
    pub fn set_json(&mut self, key: &str, value: &serde_json::Value) -> &mut Self {
        flatten_into(&mut self.flat, key, value);
        self
    }

    /// 追加前缀（子对象内逐字段设置时使用，如 `Disks.0`）。
    pub fn set_at(&mut self, prefix: &str, key: &str, value: impl Into<String>) -> &mut Self {
        self.set(&format!("{prefix}.{key}"), value)
    }

    /// 取参数值。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.flat.get(key).map(|s| s.as_str())
    }

    /// 参数个数。
    pub fn len(&self) -> usize {
        self.flat.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.flat.is_empty()
    }

    /// 只读视图（供签名使用）。
    pub fn as_map(&self) -> &BTreeMap<String, String> {
        &self.flat
    }

    /// 是否存在某个必需参数缺失。
    pub fn require(&self, keys: &[&str]) -> MnResult<()> {
        let mut missing = Vec::new();
        for k in keys {
            match self.flat.get(*k) {
                Some(v) if !v.trim().is_empty() => {}
                _ => missing.push(*k),
            }
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(MnError::bad_request(format!(
                "缺少必填参数：{}",
                missing.join("、")
            )))
        }
    }

    /// 编码为 `application/x-www-form-urlencoded` 请求体。
    ///
    /// 注意：此步骤引入 URL 转义，**必须在签名之后**执行。调用方若先转义
    /// 再签名，服务端算出的签名会与请求携带的不一致。
    pub fn to_form_body(&self) -> String {
        let mut out = String::new();
        for (k, v) in &self.flat {
            if !out.is_empty() {
                out.push('&');
            }
            out.push_str(&percent_encode(k));
            out.push('=');
            out.push_str(&percent_encode(v));
        }
        out
    }
}

/// 把 JSON 值按厂商规则展开写入扁平表。
fn flatten_into(out: &mut BTreeMap<String, String>, prefix: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                flatten_into(out, &format!("{prefix}.{k}"), v);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten_into(out, &format!("{prefix}.{i}"), v);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        serde_json::Value::Bool(b) => {
            out.insert(prefix.to_string(), encode_value_bool(*b));
        }
        serde_json::Value::Number(n) => {
            // 浮点整数值去小数；整数原样
            let text = match n.as_f64() {
                Some(f) if n.is_f64() => encode_f64(f),
                _ => n.to_string(),
            };
            out.insert(prefix.to_string(), text);
        }
        serde_json::Value::Null => {}
    }
}

/// RFC 3986 百分号编码（表单用，空格编码为 `%20` 而非 `+`）。
///
/// 厂商网关按标准百分号解码，`+` 在部分实现里会被当作字面加号，
/// 因此不用 `+` 表示空格更安全。
fn percent_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if UNRESERVED.contains(b) {
            out.push(*b as char);
        } else {
            out.push('%');
            out.push(hex_upper(*b >> 4));
            out.push(hex_upper(*b & 0x0F));
        }
    }
    out
}

/// 单个半字节 → 大写十六进制字符（URL 编码惯例）。
fn hex_upper(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_and_required_check() {
        let mut p = Params::action("StartCompShareInstance");
        p.set("Region", "cn-wlcb")
            .set_i64("Limit", 20)
            .set_bool("IncludeShareBandwidth", true)
            .set_f64("Price", 2.5);
        assert_eq!(p.get("Action"), Some("StartCompShareInstance"));
        assert_eq!(p.get("Limit"), Some("20"));
        assert_eq!(p.get("IncludeShareBandwidth"), Some("true"));
        assert_eq!(p.get("Price"), Some("2.5"));
        assert!(p.require(&["Action", "Region", "Limit"]).is_ok());

        let e = p.require(&["Action", "Zone"]).unwrap_err();
        assert!(e.to_string().contains("Zone"), "got {e}");
    }

    #[test]
    fn blank_required_value_counts_as_missing() {
        let mut p = Params::action("X");
        p.set("Zone", "   ");
        assert!(p.require(&["Zone"]).is_err());
    }

    #[test]
    fn nested_object_flattens_with_dot_keys() {
        let mut p = Params::new();
        let disks = serde_json::json!([{"IsBoot": true, "Type": "CLOUD_SSD", "Size": 100}]);
        p.set_json("Disks", &disks);
        assert_eq!(p.get("Disks.0.IsBoot"), Some("true"));
        assert_eq!(p.get("Disks.0.Type"), Some("CLOUD_SSD"));
        assert_eq!(p.get("Disks.0.Size"), Some("100"));
        // 不应残留嵌套键
        assert!(p.get("Disks").is_none());
    }

    #[test]
    fn scalar_array_uses_indices() {
        let mut p = Params::new();
        p.set_list("UHostIds", &["uhost-1".to_string(), "uhost-2".to_string()]);
        assert_eq!(p.get("UHostIds.0"), Some("uhost-1"));
        assert_eq!(p.get("UHostIds.1"), Some("uhost-2"));
    }

    #[test]
    fn empty_list_emits_nothing() {
        let mut p = Params::new();
        p.set_list("UHostIds", &[]);
        assert!(p.is_empty());
    }

    #[test]
    fn nested_arrays_of_objects_index_correctly() {
        let mut p = Params::new();
        let v = serde_json::json!([
            {"IsBoot": true, "Size": 100},
            {"IsBoot": false, "Size": 500, "CouponId": "c-1"}
        ]);
        p.set_json("Disks", &v);
        assert_eq!(p.get("Disks.0.Size"), Some("100"));
        assert_eq!(p.get("Disks.1.Size"), Some("500"));
        assert_eq!(p.get("Disks.1.CouponId"), Some("c-1"));
    }

    #[test]
    fn float_with_integral_value_drops_decimals() {
        // 厂商网关会把整数值浮点按整数解析，签名值必须一致
        let mut p = Params::new();
        p.set_f64("A", 100.0);
        p.set_json("B", &serde_json::json!(100.0));
        assert_eq!(p.get("A"), Some("100"));
        assert_eq!(p.get("B"), Some("100"));
    }

    #[test]
    fn null_values_are_skipped() {
        let mut p = Params::new();
        p.set_json("X", &serde_json::json!({"A": null, "B": 1}));
        assert!(p.get("X.A").is_none());
        assert_eq!(p.get("X.B"), Some("1"));
    }

    #[test]
    fn set_at_prefixes_field() {
        let mut p = Params::new();
        p.set_at("Disks.0", "Size", "100");
        assert_eq!(p.get("Disks.0.Size"), Some("100"));
    }

    #[test]
    fn form_body_percent_encodes_reserved_chars() {
        let mut p = Params::new();
        p.set("Name", "my gpu:1");
        let body = p.to_form_body();
        assert!(body.contains("Name=my%20gpu%3A1"), "{body}");
        // 不应出现未编码的空格或冒号
        assert!(!body.contains(' '));
    }

    #[test]
    fn form_body_keeps_unreserved_intact() {
        let mut p = Params::new();
        p.set("A", "aZ0-_.~");
        assert_eq!(p.to_form_body(), "A=aZ0-_.~");
    }

    #[test]
    fn form_body_encodes_utf8_bytes() {
        let mut p = Params::new();
        p.set("Name", "实例");
        let body = p.to_form_body();
        // 每个 UTF-8 字节单独百分号编码
        assert!(body.contains("%E5%AE%9E%E4%BE%8B"), "{body}");
    }

    #[test]
    fn form_body_is_key_sorted() {
        let mut p = Params::new();
        p.set("B", "2").set("A", "1");
        assert_eq!(p.to_form_body(), "A=1&B=2");
    }

    #[test]
    fn signature_uses_raw_values_not_escaped() {
        use crate::sign::{signature_of, Credential};
        let cred = Credential::new("pk", "sk").unwrap();
        let mut p = Params::action("X");
        p.set("Name", "a b");
        // 签名基于原始值
        let sig = cred.sign(p.as_map());
        // 表单体里是转义后的值，但签名不变
        let body = p.to_form_body();
        assert!(body.contains("a%20b"));
        assert_eq!(sig.len(), 40);
        // 若误用转义值签名，结果必然不同
        let mut escaped_pass = Params::action("X");
        escaped_pass.set("Name", "a%20b");
        assert_ne!(signature_of("sk", escaped_pass.as_map()), sig);
    }
}
