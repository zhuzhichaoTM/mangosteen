//! Base64 编解码（RFC 4648 标准字母表 + `=` 填充）。
//!
//! 归属：**算力执行层 · 云端后端管理**（协议辅助）。
//!
//! ## 为什么自己实现而不引入 `base64` crate
//!
//! 遵循工作区「依赖极简」原则：本 crate 只需要**标准字母表的编码**，
//! 用在两处——
//!
//! 1. 重置实例密码（厂商要求 `Password` 为 Base64）；
//! 2. 解码 `DescribeCompShareInstance` 返回的 `Password` 字段（也是 Base64）。
//!
//! 一个 60 行的实现换掉一个传递依赖树，符合本项目「少依赖、可审计」的取向
//! （同类先例：`sha1.rs` 也是手写的）。
//!
//! ## 不做的事
//!
//! 不支持 URL-safe 字母表、不支持无填充变体——厂商两处都用标准形式。
//! 需要时再加，避免现在写一堆没有调用点的分支。

/// Base64 编码表（RFC 4648 §4）。
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 编码为 Base64 字符串（带 `=` 填充）。
///
/// 输入按字节处理，不做任何字符集转换——密码里的中文等非 ASCII
/// 会按其 UTF-8 字节编码，与厂商 SDK 行为一致。
pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);

    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        // 剩余字节数决定填充：1 字节 → 2 个 `=`，2 字节 → 1 个 `=`
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }

    out
}

/// 解码 Base64 字符串。
///
/// 宽松处理：忽略空白与换行（厂商返回的字段里可能带 `\n`），
/// 遇到非法字符返回 `None` 而不是 panic——调用方需要区分
/// 「不是 Base64」与「解码成功但内容不对」。
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    let mut out = Vec::with_capacity(input.len() / 4 * 3);

    for ch in input.bytes() {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            // 填充结束
            b'=' => break,
            // 空白与换行：厂商字段里可能夹带，忽略
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            _ => return None,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }

    Some(out)
}

/// 解码为 UTF-8 字符串；非法 Base64 或非 UTF-8 都返回 `None`。
pub fn decode_str(input: &str) -> Option<String> {
    String::from_utf8(decode(input)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10 官方测试向量。
    #[test]
    fn rfc4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded, "encode({plain:?})");
            assert_eq!(
                decode_str(encoded).as_deref(),
                Some(plain),
                "decode({encoded:?})"
            );
        }
    }

    #[test]
    fn roundtrip_binary_and_utf8() {
        // 全字节范围
        let all: Vec<u8> = (0u8..=255).collect();
        let enc = encode(&all);
        assert_eq!(decode(&enc).unwrap(), all);

        // 中文密码（按 UTF-8 字节编码）
        let cn = "密码Abc123!";
        assert_eq!(decode_str(&encode(cn.as_bytes())).as_deref(), Some(cn));
    }

    #[test]
    fn padding_is_exact() {
        // 长度 1/2/3 的倍数分别对应 2/1/0 个填充符
        assert!(encode(b"a").ends_with("=="));
        assert!(encode(b"ab").ends_with('='));
        assert!(!encode(b"abc").contains('='));
        assert_eq!(encode(b""), "");
    }

    #[test]
    fn decode_tolerates_whitespace() {
        // 厂商字段可能带换行
        assert_eq!(decode_str("Zm9v\nYmFy").as_deref(), Some("foobar"));
        assert_eq!(decode_str(" Zm9v YmFy ").as_deref(), Some("foobar"));
    }

    #[test]
    fn invalid_input_is_none_not_panic() {
        assert!(decode("!!!").is_none());
        assert!(decode_str("不是base64").is_none());
        // 截断的多字节（非法 UTF-8）也要返回 None
        assert!(decode_str("/w==").is_none());
    }
}
