//! 文件访问控制模块。
//!
//! 归属：**基础能力层**。
//!
//! ## 威胁模型
//!
//! 本客户端会接受用户输入的模型路径。若不做约束，以下情况会造成危害：
//!
//! 1. 误把系统文件当模型读（`/etc/shadow`）→ 信息泄露到日志/云端；
//! 2. 允许目录内的符号链接指向外部 → 前缀判断被绕过；
//! 3. `../../` 路径穿越；
//! 4. 读取超大文件耗尽内存/磁盘。
//!
//! ## 防御策略
//!
//! * **真实路径解析**：`fs::canonicalize` 后再做前缀比较，符号链接无处遁形；
//! * **词法归约兜底**：目标尚不存在时用词法归一化，仍能拦住 `..`；
//! * **体积上限**：单文件超过配置上限直接拒绝；
//! * **扩展名白名单**：只接受 `.gguf`（模型格式统一原则）。

use std::path::{Path, PathBuf};

use mangosteen_core::config::StorageConfig;
use mangosteen_core::{MnError, MnResult};

/// 文件访问检查器。
#[derive(Debug, Clone)]
pub struct AccessGuard {
    /// 允许读取的根目录（已展开的真实路径）
    roots: Vec<PathBuf>,
    /// 单文件体积上限
    max_bytes: u64,
}

impl AccessGuard {
    /// 由存储配置构造。
    ///
    /// 授权范围 = `models_dir` ∪ `cache_dir` ∪ `allowed_read_roots`。
    pub fn from_config(cfg: &StorageConfig) -> Self {
        let mut roots = vec![cfg.models_dir.clone(), cfg.cache_dir.clone()];
        roots.extend(cfg.allowed_read_roots.iter().cloned());
        Self {
            roots,
            max_bytes: cfg.max_model_bytes,
        }
    }

    /// 直接指定授权根目录（测试与特殊场景用）。
    pub fn new(roots: Vec<PathBuf>, max_bytes: u64) -> Self {
        Self { roots, max_bytes }
    }

    /// 当前授权根目录。
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// 校验一个**可读**路径。
    ///
    /// 成功后返回其真实路径，调用方应使用返回值而非原始输入，
    /// 以免检查与使用之间的路径发生变化（TOCTOU）。
    pub fn check_read(&self, path: &Path) -> MnResult<PathBuf> {
        // 1. 授权范围
        let real = mangosteen_platform::fs::canonicalize_for_access(path);
        if !self.roots.iter().any(|r| {
            let rr = mangosteen_platform::fs::canonicalize_for_access(r);
            real.starts_with(&rr)
        }) {
            return Err(MnError::AccessDenied {
                path: path.to_path_buf(),
                reason: format!(
                    "不在允许读取的目录内。允许范围：{}",
                    self.roots
                        .iter()
                        .map(|r| r.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }

        // 2. 必须存在且为普通文件
        let meta = std::fs::metadata(&real).map_err(|_| MnError::ModelNotFound {
            path: path.to_path_buf(),
        })?;
        if !meta.is_file() {
            return Err(MnError::AccessDenied {
                path: path.to_path_buf(),
                reason: "不是普通文件".to_string(),
            });
        }

        // 3. 体积上限
        if meta.len() > self.max_bytes {
            return Err(MnError::AccessDenied {
                path: path.to_path_buf(),
                reason: format!(
                    "文件 {} 超过上限 {}",
                    mangosteen_platform::fs::human_bytes(meta.len()),
                    mangosteen_platform::fs::human_bytes(self.max_bytes)
                ),
            });
        }
        Ok(real)
    }

    /// 校验可读路径，并额外要求扩展名为 `.gguf`。
    ///
    /// 部署路径必须用此方法（模型格式统一原则）。
    pub fn check_model_file(&self, path: &Path) -> MnResult<PathBuf> {
        let real = self.check_read(path)?;
        let is_gguf = real
            .extension()
            .map(|e| e.eq_ignore_ascii_case("gguf"))
            .unwrap_or(false);
        if !is_gguf {
            return Err(MnError::GgufFormat {
                path: path.to_path_buf(),
                reason: "Mangosteen 仅支持 .gguf 格式模型".to_string(),
            });
        }
        Ok(real)
    }

    /// 校验一个**可写**目标路径。
    ///
    /// 写入目标的父目录必须已存在且位于授权范围内；
    /// 目标本身可以不存在（新建文件场景）。
    pub fn check_write(&self, path: &Path) -> MnResult<PathBuf> {
        let parent = path.parent().ok_or_else(|| MnError::AccessDenied {
            path: path.to_path_buf(),
            reason: "路径无父目录".to_string(),
        })?;
        let real_parent = mangosteen_platform::fs::canonicalize_for_access(parent);
        if !self.roots.iter().any(|r| {
            let rr = mangosteen_platform::fs::canonicalize_for_access(r);
            real_parent.starts_with(&rr)
        }) {
            return Err(MnError::AccessDenied {
                path: path.to_path_buf(),
                reason: format!("目标目录 {} 不在允许写入的范围内", real_parent.display()),
            });
        }
        let name = path.file_name().ok_or_else(|| MnError::AccessDenied {
            path: path.to_path_buf(),
            reason: "路径无文件名".to_string(),
        })?;
        // 拒绝隐式相对路径段落
        if name == "." || name == ".." {
            return Err(MnError::AccessDenied {
                path: path.to_path_buf(),
                reason: "非法文件名".to_string(),
            });
        }
        Ok(real_parent.join(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-guard-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn guard_for(d: &Path, max: u64) -> AccessGuard {
        AccessGuard::new(vec![d.to_path_buf()], max)
    }

    #[test]
    fn allows_file_in_authorized_dir() {
        let d = tmpdir("ok");
        let f = d.join("m.gguf");
        fs::write(&f, b"GGUF").unwrap();
        let g = guard_for(&d, 1 << 20);
        let real = g.check_read(&f).unwrap();
        assert!(real.is_absolute());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn blocks_file_outside_authorized_dir() {
        let d = tmpdir("outside");
        let outside = d.join("secret.txt");
        fs::write(&outside, b"x").unwrap();
        let sub = d.join("models");
        fs::create_dir_all(&sub).unwrap();
        let g = AccessGuard::new(vec![sub], 1 << 20);
        let e = g.check_read(&outside).unwrap_err();
        assert!(matches!(e, MnError::AccessDenied { .. }));
        assert!(e.to_string().contains("不在允许读取的目录内"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn blocks_path_traversal() {
        let d = tmpdir("traversal");
        let models = d.join("models");
        fs::create_dir_all(&models).unwrap();
        let g = AccessGuard::new(vec![models.clone()], 1 << 20);
        // ../secret 会被词法归约到 d 下，不在 models 内
        let sneaky = models.join("..").join("secret.txt");
        fs::write(&sneaky, b"x").unwrap();
        assert!(g.check_read(&sneaky).is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape() {
        let d = tmpdir("symlink");
        let models = d.join("models");
        fs::create_dir_all(&models).unwrap();
        let secret = d.join("passwd-copy");
        fs::write(&secret, b"root:x:0:0").unwrap();
        let link = models.join("evil.gguf");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        let g = AccessGuard::new(vec![models], 1 << 20);
        let e = g.check_model_file(&link).unwrap_err();
        assert!(
            matches!(e, MnError::AccessDenied { .. }),
            "符号链接指向授权目录外必须被拒绝，实际：{e:?}"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn enforces_size_limit() {
        let d = tmpdir("size");
        let f = d.join("big.gguf");
        fs::write(&f, vec![0u8; 2048]).unwrap();
        let g = guard_for(&d, 1024);
        let e = g.check_read(&f).unwrap_err();
        assert!(e.to_string().contains("超过上限"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn rejects_directory_as_file() {
        let d = tmpdir("dir");
        let sub = d.join("sub.gguf");
        fs::create_dir_all(&sub).unwrap();
        let g = guard_for(&d, 1 << 20);
        let e = g.check_read(&sub).unwrap_err();
        assert!(matches!(e, MnError::AccessDenied { .. }));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_file_maps_to_model_not_found() {
        let d = tmpdir("missing");
        let g = guard_for(&d, 1 << 20);
        let e = g.check_read(&d.join("nope.gguf")).unwrap_err();
        assert!(matches!(e, MnError::ModelNotFound { .. }));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn model_file_requires_gguf_extension() {
        let d = tmpdir("ext");
        let txt = d.join("model.txt");
        fs::write(&txt, b"x").unwrap();
        let g = guard_for(&d, 1 << 20);
        let e = g.check_model_file(&txt).unwrap_err();
        assert!(matches!(e, MnError::GgufFormat { .. }));
        assert!(e.to_string().contains(".gguf"));

        // 大小写不敏感
        let upper = d.join("MODEL.GGUF");
        fs::write(&upper, b"x").unwrap();
        assert!(g.check_model_file(&upper).is_ok());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn write_check_allows_new_file_in_authorized_dir() {
        let d = tmpdir("write");
        let target = d.join("new.gguf");
        let g = guard_for(&d, 1 << 20);
        assert!(g.check_write(&target).is_ok());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn write_check_blocks_escape() {
        let d = tmpdir("write-escape");
        let models = d.join("models");
        fs::create_dir_all(&models).unwrap();
        let g = AccessGuard::new(vec![models], 1 << 20);
        assert!(g.check_write(&d.join("evil.gguf")).is_err());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn from_config_builds_union_of_roots() {
        let d = tmpdir("fromcfg");
        let cfg = StorageConfig {
            default_source: "modelscope".to_string(),
            models_dir: d.join("models"),
            cache_dir: d.join("cache"),
            allowed_read_roots: vec![d.join("extra")],
            verify_hash: false,
            max_model_bytes: 42,
        };
        let g = AccessGuard::from_config(&cfg);
        assert_eq!(g.roots().len(), 3);
        assert_eq!(g.max_bytes, 42);
        fs::remove_dir_all(&d).ok();
    }
}
