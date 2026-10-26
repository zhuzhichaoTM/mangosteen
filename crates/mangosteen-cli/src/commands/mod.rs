//! 子命令实现集合。
//!
//! 归属：**用户交互层**。
//!
//! 每个子命令都遵循同一模式：
//!
//! ```text
//! 1. 装载配置（ConfigStore）
//! 2. 构造网关门面（GatewayBuilder）——这是与后端交互的唯一边界
//! 3. 调用网关的语义化方法
//! 4. 用 output 模块格式化结果
//! ```

pub mod assess;
pub mod cloud;
pub mod config;
pub mod deploy;
pub mod engine;
pub mod doctor;
pub mod models;
pub mod serve;
pub mod status;
pub mod stop;
pub mod ui;

use mangosteen_core::config::Config;
use mangosteen_core::{MnError, MnResult};
use mangosteen_gateway::{Gateway, GatewayBuilder};

use crate::cli::Args;

/// 装载配置：解析 `--config` 并加载。
pub fn load_config(args: &Args) -> MnResult<(Config, String)> {
    let explicit = args
        .get("config")
        .map(std::path::PathBuf::from);
    let store = mangosteen_base::ConfigStore::platform_default(explicit);
    let loaded = store.load()?;
    let source = loaded.source.describe();
    Ok((loaded.config, source))
}

/// 构造网关门面。
pub fn build_gateway(args: &Args) -> MnResult<Gateway> {
    let (config, _src) = load_config(args)?;
    GatewayBuilder::new(config).build()
}

/// 读取模型台账（不启动网关，用于只读命令）。
pub fn model_store(args: &Args) -> MnResult<mangosteen_base::ModelStore> {
    let (config, _) = load_config(args)?;
    let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
    let index = mangosteen_platform::paths::model_index_file(env);
    let guard = mangosteen_base::AccessGuard::from_config(&config.storage);
    Ok(mangosteen_base::ModelStore::new(
        index,
        guard,
        config.storage.verify_hash,
    ))
}

/// 读取部署台账（不启动网关）。
pub fn state_store(args: &Args) -> MnResult<mangosteen_base::StateStore> {
    let _ = load_config(args)?;
    let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
    Ok(mangosteen_base::StateStore::new(
        mangosteen_platform::paths::state_file(env),
    ))
}

/// 把 `Option<T>` 转成展示字符串。
pub fn opt_str<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref().map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

/// 校验参数是否只包含已知选项，未知则拒绝（防止用户拼错选项被静默忽略）。
///
/// 同时检查 `options` 与 `flags`：布尔开关若不在允许列表中，同样视为拼错。
/// 全局开关（`help` / `version` / `json` / `quiet`）在所有子命令中都合法。
pub fn reject_unknown_options(args: &Args, allowed: &[&str]) -> MnResult<()> {
    const GLOBAL: &[&str] = &["help", "version", "json", "quiet"];
    for k in args.options.keys() {
        if !allowed.contains(&k.as_str()) && !GLOBAL.contains(&k.as_str()) {
            return Err(MnError::bad_request(format!(
                "命令不支持选项 --{k}。可用选项：{}",
                list_options(allowed)
            )));
        }
    }
    for f in &args.flags {
        if !allowed.contains(&f.as_str()) && !GLOBAL.contains(&f.as_str()) {
            return Err(MnError::bad_request(format!(
                "命令不支持选项 --{f}。可用选项：{}",
                list_options(allowed)
            )));
        }
    }
    Ok(())
}

/// 把允许选项列表格式化为提示文本。
fn list_options(allowed: &[&str]) -> String {
    if allowed.is_empty() {
        return "（无）".to_string();
    }
    let mut items: Vec<String> = allowed.iter().map(|s| format!("--{s}")).collect();
    items.push("--help".into());
    items.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Args {
        crate::cli::parse_args(&items.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn opt_str_formats_none_as_dash() {
        assert_eq!(opt_str(&None::<u32>), "-");
        assert_eq!(opt_str(&Some(42u32)), "42");
    }

    #[test]
    fn reject_unknown_options_accepts_known() {
        let a = args(&["m", "--context", "1", "--json"]);
        assert!(reject_unknown_options(&a, &["context"]).is_ok());
    }

    #[test]
    fn reject_unknown_options_rejects_typo() {
        let a = args(&["m", "--contex", "1"]);
        let e = reject_unknown_options(&a, &["context"]).unwrap_err();
        assert!(e.to_string().contains("--contex"), "got {e}");
        assert!(e.to_string().contains("--context"), "应提示正确选项：{e}");
    }

    #[test]
    fn reject_all_with_empty_allowlist() {
        let a = args(&["m", "--anything", "1"]);
        let e = reject_unknown_options(&a, &[]).unwrap_err();
        assert!(e.to_string().contains("--anything"), "got {e}");
        assert!(e.to_string().contains("（无）"), "空列表应显式标注：{e}");
    }

    #[test]
    fn global_flags_always_allowed() {
        let a = args(&["m", "--json", "--quiet"]);
        assert!(reject_unknown_options(&a, &[]).is_ok());
    }

    #[test]
    fn unknown_boolean_flag_is_also_rejected() {
        // --force 是全局已知的布尔开关，但不应在 stop 以外的小列表里放行
        let a = args(&["m", "--force"]);
        let e = reject_unknown_options(&a, &["all"]).unwrap_err();
        assert!(e.to_string().contains("--force"), "got {e}");
    }
}