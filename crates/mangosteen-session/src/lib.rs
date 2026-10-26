//! # Mangosteen Session
//!
//! **第一层 · 用户会话与交互层：用户会话管理**。
//!
//! ```text
//! ┌───────────────────────────────────────────────────────┐
//! │ 第一层  用户会话与交互层                                │
//! │                                                        │
//! │  · 用户会话（Chat）：推理对话的消息、模型、上下文        │
//! │  · 部署任务会话（Deploy）：一次部署的全过程与操作记录    │
//! │                                                        │
//! │  职责：维护任务状态、上下文、操作记录                    │
//! │  约束：**只与 API 网关层通信**，不直接访问引擎或云 API   │
//! └───────────────────────────────────────────────────────┘
//!        │  HTTP / CLI
//!        ▼
//!   API 网关层（mangosteen-gateway）
//! ```
//!
//! ## 为什么单独一层
//!
//! 基线要求会话是「最上层交互入口」，要管理**三类会话的生命周期**：
//! 用户会话、部署任务会话、推理会话。在此之前这些状态散落在各处——
//! 对话历史只在浏览器内存（刷新即丢），部署任务只在网关的内存表里
//! （进程重启即丢），没有任何操作记录可追溯。
//!
//! 现在统一收敛到本 crate：
//!
//! | 能力 | 说明 |
//! |------|------|
//! | 会话 CRUD | 新建 / 列出 / 读取 / 删除 |
//! | 消息追加 | 推理会话的上下文（role + content + 时间） |
//! | 操作记录 | 部署会话里每一步动作与结果，便于事后追溯 |
//! | 持久化 | 单文件 JSON + 原子写，跨进程可见 |
//!
//! ## 边界
//!
//! 本层**不判断**能不能部署、**不启动**引擎、**不访问**云厂商。
//! 那些都是下层职责。这里只记录「谁、在什么时候、做了什么、结果如何」。

use std::path::{Path, PathBuf};

use mangosteen_core::time::now_unix_millis;
use mangosteen_core::{MnError, MnResult};
use serde::{Deserialize, Serialize};

/// 会话种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// 推理会话（对话）
    Chat,
    /// 部署任务会话
    Deploy,
}

impl SessionKind {
    /// 稳定标识（HTTP 入参）。
    pub fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Deploy => "deploy",
        }
    }

    /// 从字符串解析。
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "chat" => Some(Self::Chat),
            "deploy" => Some(Self::Deploy),
            _ => None,
        }
    }
}

/// 一条消息（推理会话的上下文单元）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMessage {
    /// `user` / `assistant` / `system`
    pub role: String,
    /// 内容
    pub content: String,
    /// 写入时间（Unix 毫秒）
    pub at_ms: u64,
}

/// 一条操作记录（部署任务会话的过程追溯）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// 动作名（如 `deploy` / `stop` / `pull`）
    pub action: String,
    /// 人类可读说明
    pub detail: String,
    /// 是否成功
    pub ok: bool,
    /// 发生时间（Unix 毫秒）
    pub at_ms: u64,
}

/// 一个会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// 会话 ID
    pub id: String,
    /// 种类
    pub kind: SessionKind,
    /// 标题（对话取首条消息前若干字；部署取模型 ID）
    pub title: String,
    /// 关联模型 ID（部署会话必有；对话会话在首次发送后确定）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// 消息（仅 `Chat`）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<SessionMessage>,
    /// 操作记录（仅 `Deploy`）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<Operation>,
    /// 创建时间
    pub created_ms: u64,
    /// 最后更新时间
    pub updated_ms: u64,
}

/// 会话摘要（列表用，不含消息体，避免列表接口过大）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 会话 ID
    pub id: String,
    /// 种类
    pub kind: SessionKind,
    /// 标题
    pub title: String,
    /// 关联模型
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// 消息条数
    pub message_count: usize,
    /// 操作条数
    pub operation_count: usize,
    /// 创建时间
    pub created_ms: u64,
    /// 最后更新时间
    pub updated_ms: u64,
}

impl Session {
    fn new(kind: SessionKind, title: impl Into<String>, model_id: Option<String>) -> Self {
        let now = now_unix_millis().max(0) as u64;
        Self {
            id: derive_id(kind, now),
            kind,
            title: title.into(),
            model_id,
            messages: Vec::new(),
            operations: Vec::new(),
            created_ms: now,
            updated_ms: now,
        }
    }

    /// 摘要。
    pub fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id.clone(),
            kind: self.kind,
            title: self.title.clone(),
            model_id: self.model_id.clone(),
            message_count: self.messages.len(),
            operation_count: self.operations.len(),
            created_ms: self.created_ms,
            updated_ms: self.updated_ms,
        }
    }

    /// 转成 [OI] 兼容的 messages 数组（供推理请求直接使用）。
    pub fn to_chat_messages(&self) -> Vec<serde_json::Value> {
        self.messages
            .iter()
            .map(|m| {
                serde_json::json!({
                    "role": m.role,
                    "content": m.content,
                })
            })
            .collect()
    }
}

/// 会话 ID：`<kind>-<毫秒时间戳>`。
///
/// 不用 UUID：项目刻意不引第三方 crate，而时间戳 + 种类已经足够区分
/// （同一毫秒内不可能建两个会话——建会话是人工操作）。
fn derive_id(kind: SessionKind, at_ms: u64) -> String {
    format!("{}-{at_ms}", kind.id())
}

/// 落盘结构。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionFile {
    #[serde(default)]
    sessions: Vec<Session>,
}

/// 会话存储。
///
/// 单文件 JSON + 原子写，与部署台账、模型台账的风格一致：
/// 跨进程可见（`mangosteen ui` 与 CLI 看到同一份），不需要额外数据库。
#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// 指定路径。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> MnResult<SessionFile> {
        if !self.path.is_file() {
            return Ok(SessionFile::default());
        }
        let text = std::fs::read_to_string(&self.path).map_err(|e| MnError::ConfigIo {
            path: self.path.clone(),
            reason: format!("会话文件读取失败：{e}"),
        })?;
        if text.trim().is_empty() {
            return Ok(SessionFile::default());
        }
        serde_json::from_str(&text).map_err(|e| MnError::ConfigIo {
            path: self.path.clone(),
            reason: format!("会话文件 JSON 解析失败：{e}"),
        })
    }

    fn save(&self, file: &SessionFile) -> MnResult<()> {
        let text = serde_json::to_string_pretty(file)
            .map_err(|e| MnError::config("session", format!("序列化失败：{e}")))?;
        mangosteen_platform::fs::atomic_write_str(&self.path, &text)
    }

    /// 新建会话。
    pub fn create(
        &self,
        kind: SessionKind,
        title: impl Into<String>,
        model_id: Option<String>,
    ) -> MnResult<Session> {
        let mut file = self.load()?;
        let mut s = Session::new(kind, title, model_id);
        // 同一毫秒重复创建时自增后缀，保证 ID 唯一
        let mut n = 1;
        while file.sessions.iter().any(|x| x.id == s.id) {
            s.id = format!("{}-{n}", derive_id(kind, s.created_ms));
            n += 1;
        }
        file.sessions.push(s.clone());
        self.save(&file)?;
        Ok(s)
    }

    /// 列出会话（按最后更新时间倒序）。
    pub fn list(&self) -> MnResult<Vec<SessionSummary>> {
        let mut out: Vec<SessionSummary> =
            self.load()?.sessions.iter().map(|s| s.summary()).collect();
        out.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        Ok(out)
    }

    /// 按种类列出。
    pub fn list_by_kind(&self, kind: SessionKind) -> MnResult<Vec<SessionSummary>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|s| s.kind == kind)
            .collect())
    }

    /// 读取一个会话。
    pub fn get(&self, id: &str) -> MnResult<Option<Session>> {
        Ok(self.load()?.sessions.into_iter().find(|s| s.id == id))
    }

    /// 删除一个会话。返回是否真的删掉了。
    pub fn delete(&self, id: &str) -> MnResult<bool> {
        let mut file = self.load()?;
        let before = file.sessions.len();
        file.sessions.retain(|s| s.id != id);
        let removed = before != file.sessions.len();
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
    }

    /// 追加一条消息。会话不存在时返回 `None`。
    ///
    /// 标题为空时用首条用户消息自动生成——用户不需要先给对话起名字。
    pub fn append_message(&self, id: &str, role: &str, content: &str) -> MnResult<Option<Session>> {
        let mut file = self.load()?;
        let Some(s) = file.sessions.iter_mut().find(|x| x.id == id) else {
            return Ok(None);
        };
        let now = now_unix_millis().max(0) as u64;
        s.messages.push(SessionMessage {
            role: role.to_string(),
            content: content.to_string(),
            at_ms: now,
        });
        // 首条用户消息作为标题（截断，避免列表里出现整段话）
        if s.title.trim().is_empty() && role == "user" {
            s.title = truncate(content, 24);
        }
        s.updated_ms = now;
        let out = s.clone();
        self.save(&file)?;
        Ok(Some(out))
    }

    /// 记录一次操作。
    pub fn record_operation(
        &self,
        id: &str,
        action: &str,
        detail: &str,
        ok: bool,
    ) -> MnResult<Option<Session>> {
        let mut file = self.load()?;
        let Some(s) = file.sessions.iter_mut().find(|x| x.id == id) else {
            return Ok(None);
        };
        let now = now_unix_millis().max(0) as u64;
        s.operations.push(Operation {
            action: action.to_string(),
            detail: detail.to_string(),
            ok,
            at_ms: now,
        });
        s.updated_ms = now;
        let out = s.clone();
        self.save(&file)?;
        Ok(Some(out))
    }

    /// 关联模型（首次对话确定模型时写回）。
    pub fn set_model(&self, id: &str, model_id: &str) -> MnResult<Option<Session>> {
        let mut file = self.load()?;
        let Some(s) = file.sessions.iter_mut().find(|x| x.id == id) else {
            return Ok(None);
        };
        s.model_id = Some(model_id.to_string());
        s.updated_ms = now_unix_millis().max(0) as u64;
        let out = s.clone();
        self.save(&file)?;
        Ok(Some(out))
    }

    /// 只保留最近 `keep` 个会话（按更新时间）。
    ///
    /// 会话文件会随对话增长，需要上限保护——否则长期使用后
    /// 每次读写都要解析几 MB JSON，UI 会变慢。
    pub fn prune(&self, keep: usize) -> MnResult<usize> {
        let mut file = self.load()?;
        if file.sessions.len() <= keep {
            return Ok(0);
        }
        file.sessions
            .sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        let removed = file.sessions.len() - keep;
        file.sessions.truncate(keep);
        self.save(&file)?;
        Ok(removed)
    }
}

/// 按字符（不是字节）截断，避免切坏 UTF-8。
fn truncate(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "mn-session-{}-{}-{}",
            tag,
            std::process::id(),
            now_unix_millis()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let p = dir.join("sessions.json");
        (SessionStore::new(&p), dir)
    }

    #[test]
    fn create_list_get_delete() {
        let (store, dir) = temp_store("crud");

        let s = store
            .create(SessionKind::Chat, "新对话", None)
            .expect("create");
        assert!(s.id.starts_with("chat-"));

        let list = store.list().expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "新对话");

        assert!(store.get(&s.id).expect("get").is_some());
        assert!(store.delete(&s.id).expect("delete"));
        assert!(store.get(&s.id).expect("get").is_none());
        // 重复删除返回 false，不报错（幂等）
        assert!(!store.delete(&s.id).expect("delete again"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一毫秒内连续创建两个会话，ID 必须不同。
    ///
    /// 建会话是人工操作，但测试/脚本可能瞬间连发两次；
    /// ID 冲突会让第二个会话覆盖第一个。
    #[test]
    fn same_millisecond_creates_unique_ids() {
        let (store, dir) = temp_store("uniq");
        let a = store.create(SessionKind::Chat, "a", None).expect("a");
        let b = store.create(SessionKind::Chat, "b", None).expect("b");
        assert_ne!(a.id, b.id, "同一毫秒建的两个会话必须有不同 ID");
        assert_eq!(store.list().expect("list").len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首条用户消息自动成为标题，且按**字符**截断（不切坏中文）。
    #[test]
    fn first_user_message_becomes_title() {
        let (store, dir) = temp_store("title");
        let s = store.create(SessionKind::Chat, "", None).expect("create");

        let long = "这是一段很长的中文内容用来测试标题截断是否按字符进行而不是按字节";
        store.append_message(&s.id, "user", long).expect("append");

        let got = store.get(&s.id).expect("get").expect("exists");
        assert!(!got.title.is_empty(), "标题应自动生成");
        assert!(
            got.title.chars().count() <= 25,
            "标题应截断到 24 字符 + 省略号，实际：{}",
            got.title
        );
        assert!(got.title.ends_with('…'));
        assert_eq!(got.messages.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 部署会话的操作记录按时间累积，可追溯。
    #[test]
    fn deploy_session_records_operations() {
        let (store, dir) = temp_store("ops");
        let s = store
            .create(
                SessionKind::Deploy,
                "qwen2.5:q4_k_m",
                Some("qwen2.5:q4_k_m".into()),
            )
            .expect("create");

        store
            .record_operation(&s.id, "deploy", "评估通过，本地可部署", true)
            .expect("op1");
        store
            .record_operation(&s.id, "start", "引擎就绪，端口 11435", true)
            .expect("op2");
        store
            .record_operation(&s.id, "stop", "已停止", false)
            .expect("op3");

        let got = store.get(&s.id).expect("get").expect("exists");
        assert_eq!(got.operations.len(), 3);
        assert_eq!(got.operations[0].action, "deploy");
        assert!(!got.operations[2].ok, "失败的操作也要记录（如实）");
        assert_eq!(got.model_id.as_deref(), Some("qwen2.5:q4_k_m"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 按种类过滤，两种会话互不干扰。
    #[test]
    fn list_by_kind_filters() {
        let (store, dir) = temp_store("kind");
        store.create(SessionKind::Chat, "c", None).expect("c");
        store.create(SessionKind::Deploy, "d", None).expect("d");

        assert_eq!(store.list_by_kind(SessionKind::Chat).expect("x").len(), 1);
        assert_eq!(store.list_by_kind(SessionKind::Deploy).expect("x").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// prune 保留最近的，且不误删未超限的。
    #[test]
    fn prune_keeps_recent() {
        let (store, dir) = temp_store("prune");
        for i in 0..5 {
            store
                .create(SessionKind::Chat, format!("s{i}"), None)
                .expect("create");
        }
        assert_eq!(store.prune(3).expect("prune"), 2);
        assert_eq!(store.list().expect("list").len(), 3);
        // 未超限时不删
        assert_eq!(store.prune(10).expect("prune"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 转 [OI] messages：只保留 role/content，不带内部字段。
    #[test]
    fn to_chat_messages_is_protocol_clean() {
        let (store, dir) = temp_store("msg");
        let s = store.create(SessionKind::Chat, "", None).expect("create");
        store.append_message(&s.id, "user", "你好").expect("a");
        store
            .append_message(&s.id, "assistant", "你好呀")
            .expect("b");

        let got = store.get(&s.id).expect("get").expect("exists");
        let msgs = got.to_chat_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "你好");
        assert!(msgs[0].get("at_ms").is_none(), "内部时间戳不应进入协议消息");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话不存在时，追加/记录都返回 None 而不是报错。
    #[test]
    fn missing_session_returns_none() {
        let (store, dir) = temp_store("missing");
        assert!(store
            .append_message("nope", "user", "x")
            .expect("a")
            .is_none());
        assert!(store
            .record_operation("nope", "deploy", "x", true)
            .expect("b")
            .is_none());
        assert!(store.set_model("nope", "m").expect("c").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
