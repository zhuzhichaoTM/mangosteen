//! SHA-1 摘要实现（RFC 3174）。
//!
//! 归属：**算力执行层 · 云端后端管理**（内部工具）。
//!
//! ## 为什么自己实现
//!
//! 优云智算（UCloud V1 签名）要求 `SHA1(参数串 + 私钥)` 的十六进制结果，
//! 属于 SHA-1 家族而非 SHA-2。工作区现有依赖只有 `sha2`（SHA-224/256/384/512），
//! 不覆盖 SHA-1；而按「依赖极简」原则，为单一 60 行算法引入一个 crate
//! 并不划算。SHA-1 的计算过程完全确定，可用官方测试向量逐位校验。
//!
//! ## 安全边界（重要）
//!
//! SHA-1 已被证明存在碰撞攻击，**不可用于数字签名/证书/口令存储**。
//! 本项目只把它当作「与厂商协议对齐的校验和」，其安全性由 HTTPS 传输层
//! 与厂商侧的密钥体系保证，而不是由 SHA-1 的抗碰撞性保证。
//!
//! 由于用途单一且明确，本实现不含任何可配置项，避免被误用于其它场景。

/// SHA-1 计算的初始状态（FIPS 180-1 定义的常量）。
const H0: [u32; 5] = [
    0x6745_2301,
    0xEFCD_AB89,
    0x98BA_DCFE,
    0x1032_5476,
    0xC3D2_E1F0,
];

/// 计算 SHA-1 摘要。
pub fn digest(data: &[u8]) -> [u8; 20] {
    let mut h = H0;

    // ---- 1. 填充：追加 0x80，补零至 56 mod 64，末尾 8 字节存比特长度 ----
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity(data.len() + 72);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // ---- 2. 逐块压缩 ----
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            // chunks_exact(4) 保证正好 4 字节；用 fold 避免索引越界的 unwrap
            let mut v = 0u32;
            for b in word {
                v = (v << 8) | u32::from(*b);
            }
            w[i] = v;
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// 计算 SHA-1 并转成小写十六进制字符串（厂商签名要求小写）。
pub fn hex_digest(data: &[u8]) -> String {
    let d = digest(data);
    let mut s = String::with_capacity(40);
    for b in d {
        s.push(hex_char(b >> 4));
        s.push(hex_char(b & 0x0F));
    }
    s
}

/// 单个半字节 → 小写十六进制字符。
fn hex_char(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'a' + (nibble - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_test_vectors() {
        // FIPS 180-1 / RFC 3174 附录给出的标准向量
        assert_eq!(hex_digest(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex_digest(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn long_input_crosses_block_boundary() {
        // 100 万个 'a' 也应有确定结果（RFC 3174 的经典向量）
        let million = vec![b'a'; 1_000_000];
        assert_eq!(
            hex_digest(&million),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn length_boundaries_are_stable() {
        // 55/56/63/64 字节是填充逻辑的关键边界
        for n in [55usize, 56, 57, 63, 64, 65, 119, 120] {
            let d = digest(&vec![0u8; n]);
            assert_eq!(d.len(), 20);
            // 与自身一致（确定性）
            assert_eq!(d, digest(&vec![0u8; n]));
        }
        // 不同长度不应碰撞到同一摘要
        assert_ne!(hex_digest(&[0u8; 55]), hex_digest(&[0u8; 56]));
    }

    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        let s = hex_digest(b"mangosteen");
        assert_eq!(s.len(), 40);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(s, s.to_lowercase());
    }
}
