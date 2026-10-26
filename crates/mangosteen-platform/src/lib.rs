//! # Mangosteen Platform
//!
//! **平台适配层**。跨平台兼容原则的唯一落地点：
//! 所有 `#[cfg]` 分支、所有 OS 系统调用、所有硬件探测命令都收敛在这个 crate 里。
//!
//! ## 铁律
//!
//! 上层（core / gateway / local / cloud / cli）**禁止**出现 `cfg(target_os)`。
//! 需要平台能力时，从这里拿一个 `impl` 抽象 trait 的实例。
//! `cargo test -p mangosteen-platform` 与 `crates/mangosteen-cli/tests/layering.rs`
//! 会检查这条约束。
//!
//! ## 模块地图
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`paths`] | 配置目录、模型目录、缓存目录的跨平台解析 |
//! | [`hardware`] | CPU / 内存 / GPU 采集（`nvidia-smi`、`system_profiler`、`/proc`、`WMI`） |
//! | [`process`] | 子进程拉起、健康探测、优雅终止 |
//! | [`fs`] | 文件权限、符号链接、原子写 |
//! | [`dlpath`] | 动态库搜索路径探测（解决 llama-server 找不到 .dylib/.so） |
//! | [`resmon`] | 实时资源采样（CPU 占用率、可用内存、进程 RSS） |
//! | [`docconv`] | 富文档转换命令选择（各平台工具差异收敛点） |

pub mod dlpath;
pub mod docconv;
pub mod fs;
pub mod hardware;
pub mod paths;
pub mod process;
pub mod resmon;

/// 当前平台标识。用于日志与诊断输出，不参与业务决策。
pub const fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

/// 当前 CPU 架构标识。
pub const fn arch_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        "unknown"
    }
}

/// 本平台 llama-server 可执行文件名。Windows 需要 `.exe` 后缀。
pub const fn engine_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "llama-server.exe"
    } else {
        "llama-server"
    }
}

/// 本平台 `llama-quantize` 可执行文件名。
///
/// 与 `llama-server` 同目录发布（llama.cpp 的 release 包一起给），
/// 所以量化工具的定位复用同一套搜索目录。
///
/// 放在平台层而不是 base：`.exe` 后缀是平台差异，
/// 按基线「跨平台兼容原则」必须收敛在平台适配层。
pub const fn quantizer_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "llama-quantize.exe"
    } else {
        "llama-quantize"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_and_arch_are_known() {
        assert_ne!(os_name(), "unknown");
        assert_ne!(arch_name(), "unknown");
        assert!(
            ["macos", "windows", "linux"].contains(&os_name()),
            "unexpected os {}",
            os_name()
        );
    }

    #[test]
    fn engine_binary_suffix_matches_platform() {
        if cfg!(target_os = "windows") {
            assert!(engine_binary_name().ends_with(".exe"));
        } else {
            assert!(!engine_binary_name().ends_with(".exe"));
        }
    }
}
