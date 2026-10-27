//! # Mangosteen Base
//!
//! **兼容基础支撑层**（配置、台账与文件安全）。为上层提供四类与算力来源无关的支撑能力：
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`models`] | 模型台账：注册、解析 GGUF、哈希校验、删除、版本管理 |
//! | [`config`] | 配置的发现、加载、写入（密钥权限收紧） |
//! | [`state`] | 部署台账持久化：跨会话恢复部署状态 |
//! | [`guard`] | 文件访问控制：白名单 + 符号链接逃逸防护 |
//! | [`download`] | 模型下载：`.part` 临时文件 + 续传 + SHA-256 校验 + 原子改名 |
//! | [`quantize`] | 模型量化（**可选工具能力**，非部署主链路必经流程） |
//! | [`extract`] | 文档文本提取：纯文本直读 + 富文档走系统工具 |
//! | [`webfetch`] | 网页抓取：URL → 正文纯文本（联网搜索的兜底路径） |
//!
//! ## 分层约束
//!
//! 本层可以依赖 `mangosteen-core`（领域模型）与 `mangosteen-platform`
//! （平台能力），但**不得**依赖 `gateway` / `local` / `cloud`。
//! 它是一个叶子能力提供者，不感知调度与算力来源。

pub mod config;
pub mod download;
pub mod extract;
pub mod guard;
pub mod models;
pub mod quantize;
pub mod state;
pub mod webfetch;

pub use config::ConfigStore;
pub use download::{download_model, DownloadOptions, Downloaded};
pub use extract::{extract, ExtractedText};
pub use guard::AccessGuard;
pub use models::ModelStore;
pub use quantize::{QuantType, QuantizeRequest, QuantizerProbe};
pub use state::StateStore;
pub use webfetch::{fetch, FetchOptions, FetchedPage};
