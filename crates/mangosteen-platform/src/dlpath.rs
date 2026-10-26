//! 动态库搜索路径探测。
//!
//! 归属：**平台适配层**。
//!
//! ## 解决什么问题
//!
//! Homebrew / 手工安装的 `llama-server` 常常依赖 `@rpath/libllama-server-impl.dylib`
//! 这类相对路径，但二进制本身**没有写入 `LC_RPATH`**。于是：
//!
//! ```text
//! $ llama-server --version
//! dyld[...]: Library not loaded: @rpath/libllama-server-impl.dylib
//!            Referenced from: /usr/local/bin/llama-server
//!            Reason: no LC_RPATH's found
//! ```
//!
//! 而 `.dylib` 其实就躺在 `/usr/local/lib` 里。
//!
//! 这是**安装布局的常见缺陷**，不该让用户自己去查 `otool -L` 然后手写
//! 环境变量。因此 Mangosteen 在启动引擎前自动做三件事：
//!
//! 1. 从可执行文件路径推导出候选库目录（`../lib`、`../lib64`、同级目录）；
//! 2. 补充该平台的系统默认库目录；
//! 3. 把命中的目录拼成环境变量注入子进程。
//!
//! 用户仍可在配置里用 `local.engine_env` 显式覆盖，或用
//! `local.auto_library_path = false` 完全关闭自动注入。

use std::path::{Path, PathBuf};

/// 返回本平台用于指定「动态库回退搜索路径」的环境变量名。
///
/// * macOS：`DYLD_FALLBACK_LIBRARY_PATH`（`DYLD_LIBRARY_PATH` 会被 SIP 剥离，
///   回退变量更可靠，且不影响已被 dyld 缓存的系统库）
/// * Linux：`LD_LIBRARY_PATH`
/// * Windows：`PATH`（DLL 搜索用 PATH）
pub const fn library_path_var() -> &'static str {
    if cfg!(target_os = "macos") {
        "DYLD_FALLBACK_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        "PATH"
    } else {
        "LD_LIBRARY_PATH"
    }
}

/// 平台默认的库目录（按优先级）。
fn system_library_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/usr/local/lib"),
            PathBuf::from("/opt/homebrew/lib"),
            PathBuf::from("/opt/local/lib"),
            PathBuf::from("/usr/lib"),
        ]
    } else if cfg!(target_os = "linux") {
        vec![
            PathBuf::from("/usr/local/lib"),
            PathBuf::from("/usr/lib"),
            PathBuf::from("/usr/lib/x86_64-linux-gnu"),
            PathBuf::from("/usr/lib/aarch64-linux-gnu"),
            PathBuf::from("/lib"),
        ]
    } else {
        Vec::new()
    }
}

/// 从可执行文件位置推导候选库目录。
///
/// 覆盖三种常见安装布局：
/// * `bin/llama-server` + `lib/*.dylib`（Homebrew、Makefile install）
/// * 可执行文件与库**同目录**（便携包、解压即用的发布件）
/// * `bin/` 下的 `../lib64`（部分 Linux 发行版）
pub fn sibling_library_dirs(exe: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let dir = match exe.parent() {
        // 相对路径（如 "llama-server"）的 parent 是空路径，没有推导价值
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => return out,
    };

    // 1. 可执行文件同目录（便携包最常见）
    out.push(dir.to_path_buf());

    // 2. ../lib、../lib64
    if let Some(parent) = dir.parent() {
        out.push(parent.join("lib"));
        out.push(parent.join("lib64"));
    }

    // 3. 若可执行文件在 bin/ 下，再上一层找 lib（如 /usr/local/bin → /usr/local/lib）
    if dir.file_name().map(|n| n == "bin").unwrap_or(false) {
        if let Some(grand) = dir.parent() {
            out.push(grand.join("lib"));
            out.push(grand.join("lib64"));
        }
    }
    out
}

/// 计算需要注入的动态库搜索路径列表。
///
/// 只包含**真实存在**的目录，避免把不存在的路径塞进环境变量
/// （dyld 遇到不存在的目录会静默跳过，但会让排查变复杂）。
///
/// `extra` 为用户显式追加的目录，排在自动探测结果**之后**
/// （用户指定应当优先，但由调用方决定拼接顺序；此处保持"探测在前、追加在后"，
/// 因为探测到的正是引擎本体的库，优先级更高）。
pub fn detect_library_dirs(exe: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    for d in sibling_library_dirs(exe) {
        if d.is_dir() {
            dirs.push(d);
        }
    }
    for d in system_library_dirs() {
        if d.is_dir() {
            dirs.push(d);
        }
    }
    for d in extra {
        if d.is_dir() {
            dirs.push(d.clone());
        }
    }

    // 去重并保持顺序
    let mut seen = Vec::new();
    dirs.retain(|d| {
        if seen.contains(d) {
            false
        } else {
            seen.push(d.clone());
            true
        }
    });
    dirs
}

/// 把目录列表拼成环境变量值。
///
/// 保留调用方已存在的环境变量内容（追加而非覆盖），
/// 这样用户自己设置的路径不会失效。
pub fn join_library_path(dirs: &[PathBuf], existing: Option<&str>) -> String {
    let sep = if cfg!(target_os = "windows") { ";" } else { ":" };
    let mut parts: Vec<String> = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect();
    if let Some(e) = existing {
        if !e.trim().is_empty() {
            parts.push(e.to_string());
        }
    }
    parts.join(sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_path_var_is_platform_correct() {
        let v = library_path_var();
        if cfg!(target_os = "macos") {
            assert_eq!(v, "DYLD_FALLBACK_LIBRARY_PATH");
        } else if cfg!(target_os = "windows") {
            assert_eq!(v, "PATH");
        } else {
            assert_eq!(v, "LD_LIBRARY_PATH");
        }
    }

    #[test]
    fn sibling_dirs_include_same_and_parent_lib() {
        let exe = Path::new("/opt/mangosteen/bin/llama-server");
        let dirs = sibling_library_dirs(exe);
        assert!(dirs.contains(&PathBuf::from("/opt/mangosteen/bin")));
        assert!(dirs.contains(&PathBuf::from("/opt/mangosteen/lib")));
        assert!(dirs.contains(&PathBuf::from("/opt/mangosteen/lib64")));
    }

    #[test]
    fn sibling_dirs_handle_root_level_exe() {
        // 没有父目录时不应 panic
        let dirs = sibling_library_dirs(Path::new("llama-server"));
        assert!(dirs.is_empty());
    }

    #[test]
    fn detect_only_returns_existing_dirs() {
        // 用真实存在的临时目录验证「只返回存在目录」
        let tmp = std::env::temp_dir();
        let fake_exe = tmp.join("mn-nodir").join("llama-server");
        let dirs = detect_library_dirs(&fake_exe, &[]);
        // 探测结果中所有项必须真实存在
        for d in &dirs {
            assert!(d.is_dir(), "返回了不存在的目录：{}", d.display());
        }
    }

    #[test]
    fn detect_respects_extra_dirs() {
        let tmp = std::env::temp_dir();
        let dirs = detect_library_dirs(Path::new("/nonexistent/bin/llama-server"), &[tmp.clone()]);
        assert!(
            dirs.iter().any(|d| d == &tmp),
            "显式追加的目录应被包含"
        );
    }

    #[test]
    fn detect_deduplicates() {
        let tmp = std::env::temp_dir();
        // 同一目录传两次（既在 extra，又是系统目录的可能不做假设）
        let dirs = detect_library_dirs(Path::new("/nonexistent/x"), &[tmp.clone(), tmp.clone()]);
        let count = dirs.iter().filter(|d| **d == tmp).count();
        assert_eq!(count, 1, "重复目录应被去重");
    }

    #[test]
    fn join_preserves_existing_value() {
        let dirs = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let joined = join_library_path(&dirs, Some("/existing"));
        assert!(joined.contains("/a"));
        assert!(joined.contains("/b"));
        assert!(
            joined.contains("/existing"),
            "不应丢弃用户原有的路径：{joined}"
        );
    }

    #[test]
    fn join_handles_empty_existing() {
        let dirs = vec![PathBuf::from("/a")];
        assert_eq!(join_library_path(&dirs, None), "/a");
        assert_eq!(join_library_path(&dirs, Some("")), "/a");
        assert_eq!(join_library_path(&dirs, Some("   ")), "/a");
    }

    #[test]
    fn join_handles_empty_dirs() {
        assert_eq!(join_library_path(&[], Some("/x")), "/x");
        assert_eq!(join_library_path(&[], None), "");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_finds_homebrew_lib() {
        // 本机验证：Homebrew 的 /usr/local/lib 应存在
        let dirs = detect_library_dirs(Path::new("/usr/local/bin/llama-server"), &[]);
        assert!(
            dirs.iter().any(|d| d == Path::new("/usr/local/lib")),
            "macOS 上应探测到 /usr/local/lib"
        );
    }
}
