//! 云端驱动的内置模板注册点。
//!
//! 归属：**第四层 · 云供应设备能力（provider 内部驱动设施）**。
//!
//! ## 当前支持范围
//!
//! 产品当前**只验证并支持优云智算**（`driver = "compshare"`，由
//! [`crate::compshare`] 专用驱动实现）。其余云厂商的 REST 字段映射尚未逐一
//! 核对，若作为「内置可用」发布，会在真实部署时产生难排查的字段错位，
//! 因此内置模板清单刻意留空。
//!
//! ## 动态扩展能力保留
//!
//! 「空清单」不等于「不支持扩展」。新增云厂商有两条路径，都不改动调度骨架：
//!
//! 1. **配置路径（无需改代码）**：在 `config.toml` 的
//!    `providers.<name>.profile` 写入端点、认证方式、字段映射与状态映射，
//!    由 [`super::rest::RestDriver`] 直接执行。
//! 2. **代码路径（协议特殊时）**：像 `compshare` 那样新增一个
//!    `impl ProviderDriver`，并在 [`crate::registry`] 注册。
//!
//! 核对完成的内置模板由这里返回，`registry` 与上层无需改动。

/// 内置 REST 驱动模板清单。
///
/// 当前为空：唯一受支持的 `compshare` 走专用驱动，其余厂商通过配置里的
/// `profile` 段接入。
pub fn builtin_profile(_name: &str) -> Option<super::rest::RestDriverProfile> {
    None
}

/// 当前内置的 REST 模板名列表（为空，见模块文档）。
pub fn builtin_driver_names() -> &'static [&'static str] {
    &[]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_unverified_builtin_templates() {
        // 未核对的生产配置不得作为「内置可用」发布：宁可让用户显式配置，
        // 也不要在真实部署时给出字段错位的假就绪驱动。
        assert!(builtin_driver_names().is_empty());
        assert!(builtin_profile("autodl").is_none());
        assert!(builtin_profile("ucloud").is_none());
        assert!(builtin_profile("aliyun").is_none());
    }
}
