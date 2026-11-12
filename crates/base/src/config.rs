//! 配置管理模块：配置的发现、加载、写入。
//!
//! 归属：**基础能力层**。
//!
//! ## 搜索顺序（优先级从高到低）
//!
//! 1. `--config <path>` 显式指定（必须存在，否则报错）
//! 2. `MANGOSTEEN_CONFIG` 环境变量
//! 3. 用户配置目录（平台相关）
//! 4. 系统级配置目录（`/etc/mangosteen`，Windows 无）
//! 5. 全部缺失 → 使用内置默认（首次运行场景，`mangosteen init` 会落盘）
//!
//! 找到配置文件后会记录**实际来源路径**，供 `mangosteen config path` 展示，
//! 也让「配置改了没生效」这类问题一眼可查。

use std::path::{Path, PathBuf};

use mangosteen_core::config::{Config, ConfigPaths};
use mangosteen_core::{MnError, MnResult};

/// 配置来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// 显式指定的文件
    Explicit(PathBuf),
    /// 环境变量指定的文件
    Env(PathBuf),
    /// 用户配置目录下的文件
    User(PathBuf),
    /// 系统级配置目录下的文件
    System(PathBuf),
    /// 无配置文件，使用内置默认（尚未落盘）
    Builtin,
}

impl ConfigSource {
    /// 来源文件路径（`Builtin` 时为 `None`）。
    pub fn path(&self) -> Option<&Path> {
        match self {
            ConfigSource::Explicit(p)
            | ConfigSource::Env(p)
            | ConfigSource::User(p)
            | ConfigSource::System(p) => Some(p),
            ConfigSource::Builtin => None,
        }
    }

    /// 人类可读描述。
    pub fn describe(&self) -> String {
        match self {
            ConfigSource::Explicit(p) => format!("显式指定：{}", p.display()),
            ConfigSource::Env(p) => format!("环境变量指定：{}", p.display()),
            ConfigSource::User(p) => format!("用户配置：{}", p.display()),
            ConfigSource::System(p) => format!("系统配置：{}", p.display()),
            ConfigSource::Builtin => "内置默认（尚未写入磁盘）".to_string(),
        }
    }
}

/// 加载结果。
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// 配置内容
    pub config: Config,
    /// 来源
    pub source: ConfigSource,
}

/// 配置存储。封装路径发现与读写。
#[derive(Debug, Clone)]
pub struct ConfigStore {
    paths: ConfigPaths,
}

impl ConfigStore {
    /// 使用给定路径集合构造。
    pub fn new(paths: ConfigPaths) -> Self {
        Self { paths }
    }

    /// 使用平台默认路径构造。
    pub fn platform_default(explicit: Option<PathBuf>) -> Self {
        let paths = mangosteen_platform::paths::config_paths(
            &mangosteen_platform::paths::real_env,
            explicit,
        );
        Self::new(paths)
    }

    /// 候选路径列表。
    pub fn candidates(&self) -> Vec<PathBuf> {
        self.paths.candidates()
    }

    /// 默认应当写入的位置（用户配置目录）。
    pub fn default_write_path(&self) -> PathBuf {
        self.paths.user_config_dir.join("config.toml")
    }

    /// 加载配置。
    ///
    /// 关键行为：显式指定或环境变量指定的路径**必须存在**，否则报错——
    /// 因为那是用户明确要求的，静默回退到默认值会让人误以为配置生效了。
    pub fn load(&self) -> MnResult<LoadedConfig> {
        // 1. 显式
        if let Some(p) = &self.paths.explicit {
            if !p.is_file() {
                return Err(MnError::ConfigIo {
                    path: p.clone(),
                    reason: "显式指定的配置文件不存在".to_string(),
                });
            }
            return Ok(LoadedConfig {
                config: Config::load(p)?,
                source: ConfigSource::Explicit(p.clone()),
            });
        }
        // 2. 环境变量
        if let Some(p) = mangosteen_platform::paths::real_env("MANGOSTEEN_CONFIG") {
            let p = PathBuf::from(p);
            if !p.is_file() {
                return Err(MnError::ConfigIo {
                    path: p.clone(),
                    reason: "MANGOSTEEN_CONFIG 指向的文件不存在".to_string(),
                });
            }
            return Ok(LoadedConfig {
                config: Config::load(&p)?,
                source: ConfigSource::Env(p),
            });
        }
        // 3. 用户配置目录
        let user = self.paths.user_config_dir.join("config.toml");
        if user.is_file() {
            return Ok(LoadedConfig {
                config: load_or_quarantine(&user)?,
                source: ConfigSource::User(user),
            });
        }
        // 4. 系统级
        if let Some(sys_dir) = &self.paths.system_config_dir {
            let sys = sys_dir.join("config.toml");
            if sys.is_file() {
                return Ok(LoadedConfig {
                    config: load_or_quarantine(&sys)?,
                    source: ConfigSource::System(sys),
                });
            }
        }
        // 5. 内置默认
        Ok(LoadedConfig {
            config: Config::sample(),
            source: ConfigSource::Builtin,
        })
    }

    /// 写配置到默认位置（权限收紧到 0600，因为可能含密钥）。
    pub fn save(&self, cfg: &Config) -> MnResult<PathBuf> {        let path = self.default_write_path();
        let text = cfg.to_toml()?;
        mangosteen_platform::fs::ensure_dir(&self.paths.user_config_dir)?;
        mangosteen_platform::fs::atomic_write_private(&path, text.as_bytes())?;
        Ok(path)
    }

    /// 写到指定路径。
    pub fn save_to(&self, cfg: &Config, path: &Path) -> MnResult<()> {
        let text = cfg.to_toml()?;
        mangosteen_platform::fs::atomic_write_private(path, text.as_bytes())
    }
}

/// 加载配置文件；若文件**语法损坏**，隔离它并回落到内置默认。
///
/// ## 为什么不让损坏配置直接报错
///
/// 配置文件是**可重建**的资产（真正不可恢复的是模型与台账）。一旦它损坏，
/// 若直接 `Err` 返回，网关起不来、CLI 全线失败，用户被困住——而程序本身
/// 明明有能力继续跑。这里的取舍是：**启动优先**，但绝不静默——
/// 把损坏文件重命名保留（`.corrupt-<时间戳>`）并打印醒目警告，
/// 然后按「配置文件不存在」的路径继续（回落内置样例）。
///
/// 隔离而非删除：损坏文件可能含用户手写的自定义模板，直接删等于替用户
/// 做了不可撤销的决定。
///
/// ## 为什么只隔离「语法损坏」，不隔离「语义错误」
///
/// 两者必须分开处理：
/// * **语法损坏**（非法 UTF-8、半截文件、TOML 解析失败）是程序自身的
///   写入缺陷或磁盘问题造成的，用户无从修复，只能重建 → 隔离 + 回退；
/// * **语义错误**（如 `storage.models_dir` 为空串、端口越界）是用户
///   手改配置写错了，属于「明确的输入错误」，必须原样报出来让他去改。
///   静默回退会让用户以为配置生效了，实则跑在默认值上。
fn load_or_quarantine(path: &Path) -> MnResult<Config> {
    match Config::load(path) {
        Ok(cfg) => Ok(cfg),
        Err(e) => {
            if !is_syntax_corruption(&e) {
                // 语义错误：原样上报，不隔离、不回退
                return Err(e);
            }
            let stamp = mangosteen_core::time::now_unix();
            let broken = path.with_extension(format!("toml.corrupt-{stamp}"));
            let moved = std::fs::rename(path, &broken).is_ok();
            eprintln!(
                "[config] ⚠ 配置文件损坏，已{}：{}。原因：{e}",
                if moved {
                    format!("隔离为 {}", broken.display())
                } else {
                    "读取失败（无法重命名，文件保留原处）".to_string()
                },
                path.display()
            );
            eprintln!("[config] 本次启动将使用内置默认配置；请在设置页重新保存以生成新配置");
            Ok(Config::sample())
        }
    }
}

/// 判断错误是否属于「文件本身损坏」（而非用户写错字段）。
///
/// `Config::load` 会把字段语义错误包装成 `ConfigIo { reason: "<字段>: <原因>" }`，
/// 而 TOML 解析失败来自 `from_toml` 的 `Config { field: "toml" }`。据此区分：
/// 只有 `toml` 字段的失败才是语法损坏。
fn is_syntax_corruption(e: &MnError) -> bool {
    match e {
        MnError::Config { field, .. } => field == "toml",
        MnError::ConfigIo { reason, .. } => {
            // from_toml 的解析错误会被包装成 `toml: ...`
            reason.starts_with("toml:")
                // 非法 UTF-8 在 read_to_string 阶段就失败，没有被包装
                || reason.contains("stream did not contain valid UTF-8")
                || reason.contains("invalid utf-8")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::config::StorageConfig;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-cfgstore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_in(dir: &Path) -> ConfigStore {
        ConfigStore::new(ConfigPaths {
            explicit: None,
            user_config_dir: dir.to_path_buf(),
            system_config_dir: None,
        })
    }

    fn minimal_config() -> Config {
        Config {
            cloud: Default::default(),
            config_version: 1,
            gateway: Default::default(),
            scheduler: Default::default(),
            local: Default::default(),
            storage: StorageConfig {
                default_source: "modelscope".to_string(),
                allowed_read_roots: vec![],
                verify_hash: false,
                max_model_bytes: 1 << 30,
            },
            providers: Default::default(),
            model_sources: Default::default(),
            remote_devices: Default::default(),
            model_whitelist: Default::default(),
            remote_inference: None,
            remote_inferences: Default::default(),
        }
    }

    #[test]
    fn builtin_default_when_nothing_exists() {
        let d = tmpdir("builtin");
        let s = store_in(&d);
        let l = s.load().unwrap();
        assert_eq!(l.source, ConfigSource::Builtin);
        assert!(l.config.scheduler.local_first);
        assert_eq!(l.source.describe(), "内置默认（尚未写入磁盘）");
        assert!(l.source.path().is_none());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn save_then_load_roundtrip() {
        let d = tmpdir("save");
        let s = store_in(&d);
        let cfg = minimal_config();
        let p = s.save(&cfg).unwrap();
        assert_eq!(p, d.join("config.toml"));
        assert!(p.is_file());

        let l = s.load().unwrap();
        assert!(matches!(l.source, ConfigSource::User(_)));
        assert_eq!(l.config.storage.default_source, "modelscope");
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("perm");
        let s = store_in(&d);
        let p = s.save(&minimal_config()).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "含密钥的配置必须 0600");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn explicit_missing_is_an_error_not_silent_fallback() {
        let d = tmpdir("explicit");
        let s = ConfigStore::new(ConfigPaths {
            explicit: Some(d.join("nope.toml")),
            user_config_dir: d.clone(),
            system_config_dir: None,
        });
        let e = s.load().unwrap_err();
        assert!(matches!(e, MnError::ConfigIo { .. }));
        assert!(e.to_string().contains("不存在"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn explicit_existing_takes_priority_over_user() {
        let d = tmpdir("priority");
        let s = store_in(&d);
        // 用户配置写一个值
        s.save(&minimal_config()).unwrap();
        // 显式配置写另一个值
        let explicit = d.join("explicit.toml");
        let mut cfg = minimal_config();
        cfg.storage.allowed_read_roots = vec![PathBuf::from("/explicit-models")];
        s.save_to(&cfg, &explicit).unwrap();

        let s2 = ConfigStore::new(ConfigPaths {
            explicit: Some(explicit.clone()),
            user_config_dir: d.clone(),
            system_config_dir: None,
        });
        let l = s2.load().unwrap();
        assert_eq!(l.source, ConfigSource::Explicit(explicit));
        assert_eq!(
            l.config.storage.allowed_read_roots,
            vec![PathBuf::from("/explicit-models")]
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn system_config_used_when_user_absent() {
        let d = tmpdir("system");
        let user = d.join("user");
        let sys = d.join("sys");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&sys).unwrap();

        let s = ConfigStore::new(ConfigPaths {
            explicit: None,
            user_config_dir: user.clone(),
            system_config_dir: Some(sys.clone()),
        });
        s.save_to(&minimal_config(), &sys.join("config.toml"))
            .unwrap();

        let l = s.load().unwrap();
        assert!(matches!(l.source, ConfigSource::System(_)));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn invalid_config_surfaces_field_path() {
        let d = tmpdir("invalid");
        let bad = d.join("config.toml");
        // storage.max_model_bytes = 0 触发校验错误（含字段路径）
        std::fs::write(&bad, "[storage]\nmax_model_bytes = 0\n").unwrap();
        let s = store_in(&d);
        let e = s.load().unwrap_err();
        assert!(e.to_string().contains("storage.max_model_bytes"), "got {e}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn default_write_path_is_under_user_dir() {
        let d = tmpdir("writepath");
        let s = store_in(&d);
        assert_eq!(s.default_write_path(), d.join("config.toml"));
        assert_eq!(s.candidates().len(), 1); // 无 explicit、无 system
        std::fs::remove_dir_all(&d).ok();
    }

    /// 损坏的配置文件必须被隔离，且**不阻止启动**。
    ///
    /// 实测踩到：`config.toml` 末尾因并发写残留半截 UTF-8，整个文件解析
    /// 失败，于是网关起不来、CLI 全线报错——用户分不清是自己配错了还是
    /// 程序坏了。配置文件是「可重建」的：隔离后的新文件用内置样例重建，
    /// 用户至少还能进 UI 改回来。
    #[test]
    fn corrupted_config_is_quarantined_and_boot_continues() {
        let d = tmpdir("corrupt");
        let p = d.join("config.toml");
        // 精确复现现场：末尾是截断在多字节字符中间的内容
        let mut bytes = b"[gateway]\nport = 11516\nhost = \"127.0.0.1\"\n".to_vec();
        bytes.extend_from_slice(b"# \xe5\x88\x86\xe9\x85\x8d"); // 合法
        bytes.extend_from_slice(b"\x86\xe9\x85\x8d"); // 非法：半个字符
        std::fs::write(&p, &bytes).unwrap();

        let s = store_in(&d);
        let loaded = s.load().expect("损坏配置不应阻止启动");
        // 回落到内置样例（Config::sample）：模板等取内置默认值，
        // 而不是损坏文件里的内容（损坏文件本身没有这些段）
        assert!(
            loaded
                .config
                .model_whitelist
                .contains_key("qwen2.5-0.5b"),
            "应回落到内置样例的模型模板"
        );
        assert!(
            loaded.config.providers.contains_key("compshare"),
            "应回落到内置样例的 compshare 服务商"
        );
        assert_eq!(loaded.config.gateway.port, 11516, "应使用内置默认端口");

        // 损坏文件必须被保留下来供排查，而不是被静默删掉
        let quarantined: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("corrupt"))
            .collect();
        assert_eq!(quarantined.len(), 1, "损坏文件应被重命名保留：{quarantined:?}");
        assert!(!p.exists(), "原路径上的损坏文件应已移走");
        std::fs::remove_dir_all(&d).ok();
    }
}
