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
                config: Config::load(&user)?,
                source: ConfigSource::User(user),
            });
        }
        // 4. 系统级
        if let Some(sys_dir) = &self.paths.system_config_dir {
            let sys = sys_dir.join("config.toml");
            if sys.is_file() {
                return Ok(LoadedConfig {
                    config: Config::load(&sys)?,
                    source: ConfigSource::System(sys),
                });
            }
        }
        // 5. 内置默认
        Ok(LoadedConfig {
            config: Config::sample(
                mangosteen_platform::paths::models_dir(&mangosteen_platform::paths::real_env),
                mangosteen_platform::paths::cache_dir(&mangosteen_platform::paths::real_env),
            ),
            source: ConfigSource::Builtin,
        })
    }

    /// 写配置到默认位置（权限收紧到 0600，因为可能含密钥）。
    pub fn save(&self, cfg: &Config) -> MnResult<PathBuf> {
        let path = self.default_write_path();
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
                models_dir: PathBuf::from("/m"),
                cache_dir: PathBuf::from("/c"),
                allowed_read_roots: vec![],
                verify_hash: false,
                max_model_bytes: 1 << 30,
            },
            providers: Default::default(),
            model_sources: Default::default(),
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
        assert_eq!(l.config.storage.models_dir, PathBuf::from("/m"));
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
        cfg.storage.models_dir = PathBuf::from("/explicit-models");
        s.save_to(&cfg, &explicit).unwrap();

        let s2 = ConfigStore::new(ConfigPaths {
            explicit: Some(explicit.clone()),
            user_config_dir: d.clone(),
            system_config_dir: None,
        });
        let l = s2.load().unwrap();
        assert_eq!(l.source, ConfigSource::Explicit(explicit));
        assert_eq!(
            l.config.storage.models_dir,
            PathBuf::from("/explicit-models")
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
        std::fs::write(&bad, "[storage]\nmodels_dir = ''\ncache_dir = '/c'\n").unwrap();
        let s = store_in(&d);
        let e = s.load().unwrap_err();
        assert!(e.to_string().contains("storage.models_dir"), "got {e}");
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
}
