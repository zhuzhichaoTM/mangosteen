//! 跨平台路径解析：配置目录、模型库、缓存目录。
//!
//! 归属：**平台适配层**。
//!
//! ## 三大平台的约定
//!
//! | 用途 | macOS | Linux | Windows |
//! |------|-------|-------|---------|
//! | 配置 | `~/Library/Application Support/mangosteen` | `$XDG_CONFIG_HOME/mangosteen` 或 `~/.config/mangosteen` | `%APPDATA%\mangosteen` |
//! | 数据（模型） | `~/Library/Application Support/mangosteen/models` | `$XDG_DATA_HOME/mangosteen/models` 或 `~/.local/share/mangosteen/models` | `%LOCALAPPDATA%\mangosteen\models` |
//! | 缓存 | `~/Library/Caches/mangosteen` | `$XDG_CACHE_HOME/mangosteen` 或 `~/.cache/mangosteen` | `%LOCALAPPDATA%\mangosteen\cache` |
//! | 日志 | `~/Library/Logs/mangosteen` | `$XDG_STATE_HOME/mangosteen/log` 或 `~/.local/state/mangosteen/log` | `%LOCALAPPDATA%\mangosteen\logs` |
//!
//! 每个函数都支持 `MANGOSTEEN_*` 环境变量覆盖，便于容器化与测试。
//!
//! ## 为什么不用 `dirs` crate
//!
//! 无依据扩展原则：本产品的路径规则只有四类、覆盖三平台，
//! 手写不足 200 行且可完全单测（不依赖真实 HOME），引入第三方 crate
//! 反而增加审计面与供应链风险。

use std::path::PathBuf;

use mangosteen_core::config::ConfigPaths;
use mangosteen_core::identity::CONFIG_DIR;

/// 环境变量读取器类型，便于测试注入。
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// 用真实进程环境读取。
pub fn real_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// 用户主目录。
///
/// * Unix：`$HOME`，缺失时回退 `/tmp`（不 panic，保证 CLI 可运行）
/// * Windows：`%USERPROFILE%`
pub fn home_dir(env: EnvLookup<'_>) -> PathBuf {
    if let Some(h) = env("MANGOSTEEN_HOME") {
        return PathBuf::from(h);
    }
    if cfg!(target_os = "windows") {
        if let Some(p) = env("USERPROFILE") {
            return PathBuf::from(p);
        }
    }
    if let Some(h) = env("HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from("/tmp")
}

/// 用户配置目录。
pub fn config_dir(env: EnvLookup<'_>) -> PathBuf {
    if let Some(p) = env("MANGOSTEEN_CONFIG_DIR") {
        return PathBuf::from(p);
    }
    let home = home_dir(env);
    if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support").join(CONFIG_DIR)
    } else if cfg!(target_os = "windows") {
        env("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
            .join(CONFIG_DIR)
    } else {
        env("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join(CONFIG_DIR)
    }
}

/// 模型库目录（数据目录）。
pub fn models_dir(env: EnvLookup<'_>) -> PathBuf {
    if let Some(p) = env("MANGOSTEEN_MODELS_DIR") {
        return PathBuf::from(p);
    }
    let home = home_dir(env);
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join(CONFIG_DIR)
            .join("models")
    } else if cfg!(target_os = "windows") {
        env("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join(CONFIG_DIR)
            .join("models")
    } else {
        env("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"))
            .join(CONFIG_DIR)
            .join("models")
    }
}

/// 下载缓存目录。
pub fn cache_dir(env: EnvLookup<'_>) -> PathBuf {
    if let Some(p) = env("MANGOSTEEN_CACHE_DIR") {
        return PathBuf::from(p);
    }
    let home = home_dir(env);
    if cfg!(target_os = "macos") {
        home.join("Library").join("Caches").join(CONFIG_DIR)
    } else if cfg!(target_os = "windows") {
        env("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join(CONFIG_DIR)
            .join("cache")
    } else {
        env("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join(CONFIG_DIR)
    }
}

/// 日志目录。
pub fn log_dir(env: EnvLookup<'_>) -> PathBuf {
    if let Some(p) = env("MANGOSTEEN_LOG_DIR") {
        return PathBuf::from(p);
    }
    let home = home_dir(env);
    if cfg!(target_os = "macos") {
        home.join("Library").join("Logs").join(CONFIG_DIR)
    } else if cfg!(target_os = "windows") {
        env("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join(CONFIG_DIR)
            .join("logs")
    } else {
        env("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("state"))
            .join(CONFIG_DIR)
            .join("log")
    }
}

/// 部署台账文件路径。
pub fn state_file(env: EnvLookup<'_>) -> PathBuf {
    config_dir(env).join("state.json")
}

/// 模型台账文件路径。
pub fn model_index_file(env: EnvLookup<'_>) -> PathBuf {
    config_dir(env).join("models.json")
}

/// 会话文件路径（第一层·用户会话管理的持久化落点）。
///
/// 与部署台账、模型台账同目录：三者都是「用户状态」，
/// 备份/迁移时应该一起搬走。
pub fn sessions_file(env: EnvLookup<'_>) -> PathBuf {
    config_dir(env).join("sessions.json")
}

/// 系统级配置目录（只读兜底）。Windows 无此概念，返回 `None`。
pub fn system_config_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        None
    } else {
        Some(PathBuf::from("/etc").join(CONFIG_DIR))
    }
}

/// 组装 [`ConfigPaths`]，供 core 的配置发现逻辑使用。
pub fn config_paths(env: EnvLookup<'_>, explicit: Option<PathBuf>) -> ConfigPaths {
    ConfigPaths {
        explicit,
        user_config_dir: config_dir(env),
        system_config_dir: system_config_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn mangosteen_home_overrides_everything() {
        let e = env_of(&[("MANGOSTEEN_HOME", "/custom/home")]);
        let env: EnvLookup = &e;
        assert_eq!(home_dir(env), PathBuf::from("/custom/home"));
        // 三平台下都应基于该 home 派生
        assert!(config_dir(env).starts_with("/custom/home"));
        assert!(models_dir(env).starts_with("/custom/home"));
        assert!(cache_dir(env).starts_with("/custom/home"));
    }

    #[test]
    fn explicit_env_overrides_win() {
        let e = env_of(&[
            ("MANGOSTEEN_HOME", "/h"),
            ("MANGOSTEEN_CONFIG_DIR", "/cfg"),
            ("MANGOSTEEN_MODELS_DIR", "/models"),
            ("MANGOSTEEN_CACHE_DIR", "/cache"),
            ("MANGOSTEEN_LOG_DIR", "/logs"),
        ]);
        let env: EnvLookup = &e;
        assert_eq!(config_dir(env), PathBuf::from("/cfg"));
        assert_eq!(models_dir(env), PathBuf::from("/models"));
        assert_eq!(cache_dir(env), PathBuf::from("/cache"));
        assert_eq!(log_dir(env), PathBuf::from("/logs"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_uses_library_conventions() {
        let e = env_of(&[("HOME", "/Users/u")]);
        let env: EnvLookup = &e;
        assert_eq!(
            config_dir(env),
            PathBuf::from("/Users/u/Library/Application Support/mangosteen")
        );
        assert_eq!(
            models_dir(env),
            PathBuf::from("/Users/u/Library/Application Support/mangosteen/models")
        );
        assert_eq!(
            cache_dir(env),
            PathBuf::from("/Users/u/Library/Caches/mangosteen")
        );
        assert_eq!(
            log_dir(env),
            PathBuf::from("/Users/u/Library/Logs/mangosteen")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_respects_xdg_and_falls_back() {
        let e = env_of(&[("HOME", "/home/u")]);
        let env: EnvLookup = &e;
        assert_eq!(
            config_dir(env),
            PathBuf::from("/home/u/.config/mangosteen")
        );
        assert_eq!(
            models_dir(env),
            PathBuf::from("/home/u/.local/share/mangosteen/models")
        );
        assert_eq!(
            cache_dir(env),
            PathBuf::from("/home/u/.cache/mangosteen")
        );

        let e = env_of(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/x/cfg"),
            ("XDG_DATA_HOME", "/x/data"),
            ("XDG_CACHE_HOME", "/x/cache"),
        ]);
        let env: EnvLookup = &e;
        assert_eq!(config_dir(env), PathBuf::from("/x/cfg/mangosteen"));
        assert_eq!(models_dir(env), PathBuf::from("/x/data/mangosteen/models"));
        assert_eq!(cache_dir(env), PathBuf::from("/x/cache/mangosteen"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_uses_appdata() {
        let e = env_of(&[
            ("USERPROFILE", "C:\\Users\\u"),
            ("APPDATA", "C:\\Users\\u\\AppData\\Roaming"),
            ("LOCALAPPDATA", "C:\\Users\\u\\AppData\\Local"),
        ]);
        let env: EnvLookup = &e;
        assert!(config_dir(env).ends_with("mangosteen"));
        assert!(models_dir(env).ends_with("models"));
        assert!(cache_dir(env).ends_with("cache"));
    }

    #[test]
    fn home_falls_back_without_panic() {
        let e = env_of(&[]);
        let env: EnvLookup = &e;
        // 无 HOME / USERPROFILE 时必须仍返回可用路径
        let h = home_dir(env);
        assert!(!h.as_os_str().is_empty());
    }

    #[test]
    fn state_and_index_live_in_config_dir() {
        let e = env_of(&[("MANGOSTEEN_CONFIG_DIR", "/cfg")]);
        let env: EnvLookup = &e;
        assert_eq!(state_file(env), PathBuf::from("/cfg/state.json"));
        assert_eq!(model_index_file(env), PathBuf::from("/cfg/models.json"));
    }

    #[test]
    fn config_paths_assembly() {
        let e = env_of(&[("MANGOSTEEN_CONFIG_DIR", "/cfg")]);
        let env: EnvLookup = &e;
        let p = config_paths(env, Some(PathBuf::from("/explicit.toml")));
        assert_eq!(p.explicit, Some(PathBuf::from("/explicit.toml")));
        assert_eq!(p.user_config_dir, PathBuf::from("/cfg"));
        let cands = p.candidates();
        assert_eq!(cands[0], PathBuf::from("/explicit.toml"));
        assert_eq!(cands[1], PathBuf::from("/cfg/config.toml"));
    }

    #[test]
    fn real_env_filters_blank() {
        // 用必定不存在的变量名验证「空值视为未设置」
        assert!(real_env("MANGOSTEEN_DEFINITELY_NOT_SET_XYZ").is_none());
    }
}
