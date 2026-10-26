//! 模型管理模块：模型台账、GGUF 解析、哈希校验、删除。
//!
//! 归属：**基础能力层**。
//!
//! ## 职责边界
//!
//! 本模块**不负责下载**（模型获取走 `mangosteen-local` 的拉取流程或用户手动放置），
//! 只负责：
//!
//! * 把磁盘上的 GGUF 文件**登记**进台账（解析头部 → 生成 [`ModelSpec`]）；
//! * 台账的持久化与并发安全写入；
//! * 哈希校验（检测文件被替换/损坏）；
//! * 删除（同时清理台账与磁盘文件）。
//!
//! ## 台账为什么存 JSON 而不是数据库
//!
//! 个人级场景模型数量在几十量级，JSON 足以胜任；而且**用户可以直接手改**
//! 排障，符合「配置驱动」的产品调性。写操作走原子替换，不会写坏。

use std::path::{Path, PathBuf};

use mangosteen_core::gguf::{self, GgufInfo};
use mangosteen_core::model::{ModelEntry, ModelLocation, ModelSpec};
use mangosteen_core::{MnError, MnResult};
use serde::{Deserialize, Serialize};

use crate::guard::AccessGuard;

/// 台账文件格式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelIndex {
    /// 格式版本
    #[serde(default = "one")]
    pub version: u32,
    /// 模型条目
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

fn one() -> u32 {
    1
}

impl Default for ModelIndex {
    fn default() -> Self {
        Self {
            version: 1,
            models: Vec::new(),
        }
    }
}

/// 注册结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Registration {
    /// 登记后的条目
    pub entry: ModelEntry,
    /// 解析出的 GGUF 信息
    pub info: GgufInfo,
    /// 是否为重复注册（同 ID 已存在，仅更新）
    pub replaced: bool,
}

/// 模型存储。
#[derive(Debug, Clone)]
pub struct ModelStore {
    index_path: PathBuf,
    guard: AccessGuard,
    /// 是否在注册时计算 SHA-256（大文件耗时）
    verify_hash: bool,
}

impl ModelStore {
    /// 构造。
    pub fn new(index_path: PathBuf, guard: AccessGuard, verify_hash: bool) -> Self {
        Self {
            index_path,
            guard,
            verify_hash,
        }
    }

    /// 台账路径。
    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// 读取台账。文件不存在时返回空台账。
    pub fn load_index(&self) -> MnResult<ModelIndex> {
        match mangosteen_platform::fs::read_to_string_opt(&self.index_path)? {
            None => Ok(ModelIndex::default()),
            Some(text) if text.trim().is_empty() => Ok(ModelIndex::default()),
            Some(text) => serde_json::from_str(&text).map_err(|e| MnError::ConfigIo {
                path: self.index_path.clone(),
                reason: format!("模型台账 JSON 解析失败：{e}"),
            }),
        }
    }

    /// 写入台账（原子替换）。
    pub fn save_index(&self, index: &ModelIndex) -> MnResult<()> {
        let text = serde_json::to_string_pretty(index)
            .map_err(|e| MnError::config("models_index", format!("序列化失败：{e}")))?;
        mangosteen_platform::fs::atomic_write_str(&self.index_path, &text)
    }

    /// 列出全部模型。
    pub fn list(&self) -> MnResult<Vec<ModelEntry>> {
        Ok(self.load_index()?.models)
    }

    /// 按 ID 查找。
    pub fn get(&self, id: &str) -> MnResult<Option<ModelEntry>> {
        Ok(self.load_index()?.models.into_iter().find(|m| m.id == id))
    }

    /// 注册本地 GGUF 文件。
    ///
    /// 步骤：访问控制校验 → 解析 GGUF 头部 → 可选哈希 → 写入台账。
    ///
    /// 重复注册同一 `name+quant` 会**更新**已有条目（幂等），
    /// 便于「同一模型换了文件位置」这类操作。
    pub fn register_local(&self, path: &Path, version: Option<&str>) -> MnResult<Registration> {
        // 1. 访问控制（含 .gguf 扩展名与体积上限）
        let real = self.guard.check_model_file(path)?;

        // 2. 解析 GGUF 头部
        let info = gguf::parse_file(&real)?;

        // 3. 构造规格
        let spec = info.to_model_spec();
        spec.validate().map_err(|reason| MnError::GgufFormat {
            path: real.clone(),
            reason,
        })?;

        // 4. 可选哈希
        let sha256 = if self.verify_hash {
            Some(gguf::sha256_file(&real)?)
        } else {
            None
        };

        let id = ModelEntry::derive_id(&spec.name, spec.quant);
        let now = mangosteen_core::time::now_unix();
        let entry = ModelEntry {
            id: id.clone(),
            spec,
            location: ModelLocation::Local { path: real },
            sha256,
            version: version.map(|s| s.to_string()),
            verified_at: Some(now),
        };

        // 5. 写入台账
        let mut index = self.load_index()?;
        let replaced = if let Some(slot) = index.models.iter_mut().find(|m| m.id == id) {
            *slot = entry.clone();
            true
        } else {
            index.models.push(entry.clone());
            false
        };
        index.models.sort_by(|a, b| a.id.cmp(&b.id));
        self.save_index(&index)?;

        Ok(Registration {
            entry,
            info,
            replaced,
        })
    }

    /// 登记云端实例上的模型（不做本地文件校验）。
    pub fn register_cloud(
        &self,
        spec: ModelSpec,
        instance_id: &str,
        remote_path: &str,
    ) -> MnResult<ModelEntry> {
        spec.validate().map_err(MnError::bad_request)?;
        let id = ModelEntry::derive_id(&spec.name, spec.quant);
        let entry = ModelEntry {
            id: id.clone(),
            spec,
            location: ModelLocation::Cloud {
                instance_id: instance_id.to_string(),
                path: remote_path.to_string(),
            },
            sha256: None,
            version: None,
            verified_at: Some(mangosteen_core::time::now_unix()),
        };
        let mut index = self.load_index()?;
        if let Some(slot) = index.models.iter_mut().find(|m| m.id == id) {
            *slot = entry.clone();
        } else {
            index.models.push(entry.clone());
        }
        index.models.sort_by(|a, b| a.id.cmp(&b.id));
        self.save_index(&index)?;
        Ok(entry)
    }

    /// 校验模型文件哈希。
    ///
    /// 返回 `Ok(true)` 表示一致；台账无哈希记录时返回 `Ok(false)` 并**补写**哈希。
    pub fn verify(&self, id: &str) -> MnResult<bool> {
        let mut index = self.load_index()?;
        let entry = index
            .models
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| MnError::bad_request(format!("模型 `{id}` 未登记")))?;

        let path = match &entry.location {
            ModelLocation::Local { path } => path.clone(),
            ModelLocation::Cloud { .. } => {
                return Err(MnError::bad_request(
                    "云端模型的哈希校验需在实例内执行，本地无法校验".to_string(),
                ))
            }
        };

        let actual = gguf::sha256_file(&path)?;
        match &entry.sha256 {
            Some(expected) => {
                if expected == &actual {
                    entry.verified_at = Some(mangosteen_core::time::now_unix());
                    self.save_index(&index)?;
                    Ok(true)
                } else {
                    Err(MnError::HashMismatch {
                        path,
                        expected: expected.clone(),
                        actual,
                    })
                }
            }
            None => {
                // 首次校验：记录哈希
                entry.sha256 = Some(actual);
                entry.verified_at = Some(mangosteen_core::time::now_unix());
                self.save_index(&index)?;
                Ok(false)
            }
        }
    }

    /// 从台账移除条目。`delete_file` 为真时同时删除磁盘文件。
    ///
    /// 删除磁盘文件前会再次走访问控制，避免借用已登记路径删除授权范围外的文件。
    pub fn remove(&self, id: &str, delete_file: bool) -> MnResult<bool> {
        let mut index = self.load_index()?;
        let pos = match index.models.iter().position(|m| m.id == id) {
            Some(p) => p,
            None => return Ok(false),
        };
        let entry = index.models.remove(pos);

        if delete_file {
            if let ModelLocation::Local { path } = &entry.location {
                // 只删除确实位于授权范围内且仍是该 GGUF 的文件
                if self.guard.check_model_file(path).is_ok() {
                    std::fs::remove_file(path)
                        .map_err(|e| MnError::io(format!("删除 {}", path.display()), &e))?;
                }
            }
        }
        self.save_index(&index)?;
        Ok(true)
    }

    /// 按 `name` 模糊匹配（用于 CLI 允许用户只输入 `qwen2.5` 这类短名）。
    pub fn find_by_name(&self, needle: &str) -> MnResult<Vec<ModelEntry>> {
        let needle = needle.to_ascii_lowercase();
        Ok(self
            .load_index()?
            .models
            .into_iter()
            .filter(|m| m.id.to_ascii_lowercase().contains(&needle))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::model::Quantization;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-models-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_in(d: &Path, verify: bool) -> ModelStore {
        ModelStore::new(
            d.join("models.json"),
            AccessGuard::new(vec![d.to_path_buf()], 1 << 30),
            verify,
        )
    }

    /// 写一个可解析的 GGUF 文件（带 4KB 权重区，模拟真实体积）。
    fn write_gguf(dir: &Path, name: &str) -> PathBuf {
        let mut bytes = gguf::build_test_gguf("qwen2", 28, 28, 4, 3584, 32768);
        bytes.extend(std::iter::repeat(0u8).take(4096));
        let p = dir.join(name);
        std::fs::write(&p, &bytes).unwrap();
        p
    }

    #[test]
    fn empty_index_when_absent() {
        let d = tmpdir("empty");
        let s = store_in(&d, false);
        assert_eq!(s.load_index().unwrap(), ModelIndex::default());
        assert!(s.list().unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn register_parses_and_persists() {
        let d = tmpdir("register");
        let f = write_gguf(&d, "qwen2.5-7b-q4.gguf");
        let s = store_in(&d, false);

        let reg = s.register_local(&f, Some("2024-11")).unwrap();
        assert!(!reg.replaced);
        assert_eq!(reg.info.n_layers, 28);
        assert_eq!(reg.info.n_kv_heads, 4);
        assert_eq!(reg.entry.spec.n_layers, 28);
        assert_eq!(reg.entry.version.as_deref(), Some("2024-11"));
        assert!(reg.entry.is_local());

        // 重新加载确认已落盘
        let list = s.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, reg.entry.id);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn register_is_idempotent_by_id() {
        let d = tmpdir("idem");
        let f = write_gguf(&d, "m.gguf");
        let s = store_in(&d, false);
        let r1 = s.register_local(&f, None).unwrap();
        let r2 = s.register_local(&f, Some("v2")).unwrap();
        assert!(!r1.replaced);
        assert!(r2.replaced);
        assert_eq!(s.list().unwrap().len(), 1);
        assert_eq!(
            s.list().unwrap()[0].version.as_deref(),
            Some("v2"),
            "重复注册应更新版本"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn register_rejects_non_gguf() {
        let d = tmpdir("non-gguf");
        let f = d.join("model.bin");
        std::fs::write(&f, b"x").unwrap();
        let s = store_in(&d, false);
        let e = s.register_local(&f, None).unwrap_err();
        assert!(matches!(e, MnError::GgufFormat { .. }));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn register_rejects_corrupt_gguf() {
        let d = tmpdir("corrupt");
        let f = d.join("bad.gguf");
        std::fs::write(&f, b"NOTGGUF-not-a-model-at-all-padding-padding").unwrap();
        let s = store_in(&d, false);
        let e = s.register_local(&f, None).unwrap_err();
        assert!(matches!(e, MnError::GgufFormat { .. }));
        assert!(e.to_string().contains("magic"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hash_recorded_when_verify_enabled() {
        let d = tmpdir("hash");
        let f = write_gguf(&d, "m.gguf");
        let s = store_in(&d, true);
        let reg = s.register_local(&f, None).unwrap();
        let sha = reg.entry.sha256.expect("应记录哈希");
        assert_eq!(sha.len(), 64);
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn verify_detects_untracked_then_matches() {
        let d = tmpdir("verify");
        let f = write_gguf(&d, "m.gguf");
        let s = store_in(&d, false); // 注册时不记账哈希
        let reg = s.register_local(&f, None).unwrap();
        assert!(reg.entry.sha256.is_none());

        // 首次 verify 记录哈希，返回 false（表示「此前未知」）
        assert!(!s.verify(&reg.entry.id).unwrap());
        // 再次 verify 应匹配
        assert!(s.verify(&reg.entry.id).unwrap());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn verify_detects_tampering() {
        let d = tmpdir("tamper");
        let f = write_gguf(&d, "m.gguf");
        let s = store_in(&d, true);
        let reg = s.register_local(&f, None).unwrap();

        // 改动文件内容
        let mut content = std::fs::read(&f).unwrap();
        let last = content.len() - 1;
        content[last] ^= 0xFF;
        std::fs::write(&f, &content).unwrap();

        let e = s.verify(&reg.entry.id).unwrap_err();
        assert!(matches!(e, MnError::HashMismatch { .. }));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn verify_unknown_model_errors() {
        let d = tmpdir("verify-unknown");
        let s = store_in(&d, false);
        assert!(s.verify("nope").is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_without_file_keeps_disk() {
        let d = tmpdir("remove");
        let f = write_gguf(&d, "m.gguf");
        let s = store_in(&d, false);
        let reg = s.register_local(&f, None).unwrap();

        assert!(s.remove(&reg.entry.id, false).unwrap());
        assert!(f.exists(), "未要求删文件时磁盘文件必须保留");
        assert!(s.list().unwrap().is_empty());
        assert!(
            !s.remove(&reg.entry.id, false).unwrap(),
            "重复删除返回 false"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_with_file_deletes_disk() {
        let d = tmpdir("remove-file");
        let f = write_gguf(&d, "m.gguf");
        let s = store_in(&d, false);
        let reg = s.register_local(&f, None).unwrap();

        assert!(s.remove(&reg.entry.id, true).unwrap());
        assert!(!f.exists(), "要求删文件时必须删除");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn find_by_name_is_substring_case_insensitive() {
        let d = tmpdir("find");
        let a = write_gguf(&d, "a.gguf");
        let s = store_in(&d, false);
        let reg = s.register_local(&a, None).unwrap();

        let hits = s.find_by_name("QWEN").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, reg.entry.id);
        assert!(s.find_by_name("llama").unwrap().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cloud_registration_skips_file_checks() {
        let d = tmpdir("cloud");
        let s = store_in(&d, false);
        let spec = ModelSpec {
            name: "qwen2.5-7b-instruct".into(),
            params_b: 7.615,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 0,
        };
        let e = s
            .register_cloud(spec, "i-abc", "/root/models/q.gguf")
            .unwrap();
        assert!(!e.is_local());
        assert!(matches!(e.location, ModelLocation::Cloud { .. }));
        // 云端模型无法本地校验哈希
        assert!(s.verify(&e.id).is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn index_is_sorted_and_stable() {
        let d = tmpdir("sorted");
        let a = write_gguf(&d, "a.gguf");
        let s = store_in(&d, false);
        let r1 = s.register_local(&a, None).unwrap();
        let r2 = s.register_local(&d.join("b.gguf"), None).unwrap_err();
        assert!(r2.to_string().contains("不存在") || matches!(r2, MnError::ModelNotFound { .. }));
        let idx = s.load_index().unwrap();
        assert_eq!(idx.models.len(), 1);
        assert_eq!(idx.models[0].id, r1.entry.id);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn corrupt_index_reports_path() {
        let d = tmpdir("corrupt-index");
        std::fs::write(d.join("models.json"), "{not json").unwrap();
        let s = store_in(&d, false);
        let e = s.load_index().unwrap_err();
        assert!(e.to_string().contains("JSON"));
        std::fs::remove_dir_all(&d).ok();
    }
}
