//! OpenAI 兼容协议数据结构。
//!
//! 归属：**API 网关层 · 统一协议适配层**。
//!
//! ## 设计目标
//!
//! 上层应用（IDE 插件、脚本、任意 OpenAI SDK 客户端）对接网关即可，
//! 完全不感知底层是本地 llama.cpp 还是云端实例。因此本模块的结构体
//! **必须与 OpenAI 官方 schema 字段级对齐**，不添加私有字段到必填位置，
//! 私有扩展只放在标记为 `#[serde(flatten)]` 的 `extra` 里。
//!
//! ## 覆盖范围
//!
//! | 端点 | 结构 |
//! |------|------|
//! | `GET /v1/models` | [`ModelList`] |
//! | `GET /v1/models/{id}` | [`ModelObject`] |
//! | `POST /v1/chat/completions` | [`ChatRequest`] / [`ChatResponse`] |
//! | `POST /v1/completions` | [`CompletionRequest`] / [`CompletionResponse`] |
//! | `POST /v1/embeddings` | [`EmbeddingRequest`] / [`EmbeddingResponse`] |
//! | `GET /health` | [`HealthResponse`] |
//!
//! 流式响应（SSE）的 chunk 结构见 [`ChatChunk`]。

use serde::{Deserialize, Serialize};

/// 统一错误响应体，对齐 OpenAI `error` 包装。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// 错误详情
    pub error: ErrorDetail,
}

/// 错误详情。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorDetail {
    /// 人类可读消息
    pub message: String,
    /// 错误类型（`invalid_request_error` / `server_error` / ...）
    #[serde(rename = "type")]
    pub kind: String,
    /// 细分错误码
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 出错参数名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
}

impl ErrorResponse {
    /// 由内部错误构造。网关是唯一调用点，保证错误语义对外统一。
    pub fn from_mn(e: &crate::MnError) -> Self {
        let kind = match e {
            crate::MnError::BadRequest { .. } => "invalid_request_error",
            crate::MnError::ModelNotFound { .. } => "invalid_request_error",
            crate::MnError::AccessDenied { .. } => "permission_error",
            crate::MnError::Config { .. } | crate::MnError::ConfigIo { .. } => "server_error",
            _ => "server_error",
        };
        Self {
            error: ErrorDetail {
                message: e.to_string(),
                kind: kind.to_string(),
                code: Some(e.code().to_string()),
                param: None,
            },
        }
    }

    /// 构造请求参数错误。
    pub fn bad_request(message: impl Into<String>, param: Option<&str>) -> Self {
        Self {
            error: ErrorDetail {
                message: message.into(),
                kind: "invalid_request_error".to_string(),
                code: Some("bad_request".to_string()),
                param: param.map(|s| s.to_string()),
            },
        }
    }

    /// 该错误应返回的 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        match self.error.kind.as_str() {
            "invalid_request_error" => 400,
            "permission_error" => 403,
            _ => match self.error.code.as_deref() {
                Some("model_not_found") => 404,
                Some("insufficient_local_resource") => 503,
                Some("no_viable_candidate") => 503,
                _ => 500,
            },
        }
    }
}

/// 模型对象。对齐 `GET /v1/models` 的数组元素。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelObject {
    /// 模型 ID
    pub id: String,
    /// 固定为 `model`
    pub object: String,
    /// 创建时间（Unix 秒）
    pub created: i64,
    /// 归属方。Mangosteen 用 `local` / `cloud:<provider>` 表达算力来源。
    pub owned_by: String,
}

impl ModelObject {
    /// 构造。
    pub fn new(id: impl Into<String>, created: i64, owned_by: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            object: "model".to_string(),
            created,
            owned_by: owned_by.into(),
        }
    }
}

/// 模型列表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelList {
    /// 固定为 `list`
    pub object: String,
    /// 模型数组
    pub data: Vec<ModelObject>,
}

impl ModelList {
    /// 构造。
    pub fn new(data: Vec<ModelObject>) -> Self {
        Self {
            object: "list".to_string(),
            data,
        }
    }
}

/// 对话消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// 系统提示
    System,
    /// 用户
    User,
    /// 助手
    Assistant,
    /// 工具结果
    Tool,
}

impl Role {
    /// 字符串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// 对话消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// 角色
    pub role: Role,
    /// 文本内容。多模态内容在个人级部署中不展开，仅支持纯文本。
    pub content: String,
    /// 工具调用 ID（`role=tool` 时使用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// 用户消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
        }
    }

    /// 系统消息。
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
        }
    }
}

/// 采样参数。字段与 OpenAI 对齐，全部可选。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SamplingParams {
    /// 温度
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 核采样
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// 最大生成 token 数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 停止序列
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// 随机种子
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// 频率惩罚
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    /// 存在惩罚
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
}

impl SamplingParams {
    /// 校验取值范围。违反时返回参数名。
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        if let Some(t) = self.temperature {
            if !(0.0..=2.0).contains(&t) {
                return Err(("temperature", format!("必须在 0..=2 之间，实际 {t}")));
            }
        }
        if let Some(p) = self.top_p {
            if !(0.0..=1.0).contains(&p) {
                return Err(("top_p", format!("必须在 0..=1 之间，实际 {p}")));
            }
        }
        if let Some(m) = self.max_tokens {
            if m == 0 || m > 131_072 {
                return Err(("max_tokens", format!("必须在 1..=131072 之间，实际 {m}")));
            }
        }
        Ok(())
    }
}

/// 对话补全请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 模型 ID。特殊值 `auto` 表示由网关按调度策略自动选择。
    pub model: String,
    /// 消息列表
    pub messages: Vec<ChatMessage>,
    /// 是否流式返回
    #[serde(default)]
    pub stream: bool,
    /// 采样参数
    #[serde(flatten)]
    pub sampling: SamplingParams,
    /// 保留的额外字段（透传给后端，但不参与网关逻辑）
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 自动选择模型的保留 ID。
pub const AUTO_MODEL_ID: &str = "auto";

impl ChatRequest {
    /// 校验请求。
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        if self.model.trim().is_empty() {
            return Err(("model", "不能为空".to_string()));
        }
        if self.messages.is_empty() {
            return Err(("messages", "至少需要一条消息".to_string()));
        }
        let total: usize = self.messages.iter().map(|m| m.content.len()).sum();
        if total > 16 * 1024 * 1024 {
            return Err(("messages", "总内容超过 16MiB 上限".to_string()));
        }
        self.sampling.validate()
    }

    /// 从最后一条用户消息取提示文本（用于 token 估算）。
    pub fn last_user_content(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
    }
}

/// token 用量统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// 提示 token 数
    pub prompt_tokens: u32,
    /// 生成 token 数
    pub completion_tokens: u32,
    /// 合计
    pub total_tokens: u32,
}

impl Usage {
    /// 由提示/生成数构造并自动求和。
    pub fn new(prompt_tokens: u32, completion_tokens: u32) -> Self {
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
        }
    }
}

/// 生成结束原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// 自然结束
    Stop,
    /// 达到 max_tokens
    Length,
    /// 命中工具调用
    ToolCalls,
    /// 内容过滤
    ContentFilter,
}

impl FinishReason {
    /// 字符串形式。
    pub fn as_str(self) -> &'static str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::ContentFilter => "content_filter",
        }
    }
}

/// 对话补全的单个选项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChoice {
    /// 序号
    pub index: u32,
    /// 生成的消息
    pub message: ChatMessage,
    /// 结束原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// 对话补全响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 响应 ID
    pub id: String,
    /// 固定为 `chat.completion`
    pub object: String,
    /// 创建时间
    pub created: i64,
    /// 实际使用的模型
    pub model: String,
    /// 选项数组
    pub choices: Vec<ChatChoice>,
    /// 用量
    pub usage: Usage,
}

impl ChatResponse {
    /// 构造单选项响应。
    pub fn single(
        id: impl Into<String>,
        model: impl Into<String>,
        created: i64,
        content: impl Into<String>,
        finish_reason: FinishReason,
        usage: Usage,
    ) -> Self {
        Self {
            id: id.into(),
            object: "chat.completion".to_string(),
            created,
            model: model.into(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: Role::Assistant,
                    content: content.into(),
                    tool_call_id: None,
                },
                finish_reason: Some(finish_reason),
            }],
            usage,
        }
    }
}

/// 流式 chunk 中的 delta。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChatDelta {
    /// 增量角色（仅首个 chunk 携带）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// 增量文本
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// 流式 chunk 的选项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChunkChoice {
    /// 序号
    pub index: u32,
    /// 增量
    pub delta: ChatDelta,
    /// 结束原因（仅末个 chunk 携带）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// 流式 chunk。对齐 `data: {...}` 的载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatChunk {
    /// chunk ID
    pub id: String,
    /// 固定为 `chat.completion.chunk`
    pub object: String,
    /// 创建时间
    pub created: i64,
    /// 模型
    pub model: String,
    /// 选项
    pub choices: Vec<ChatChunkChoice>,
}

impl ChatChunk {
    /// 首个 chunk（携带 role）。
    pub fn first(id: &str, model: &str, created: i64) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    role: Some(Role::Assistant),
                    content: None,
                },
                finish_reason: None,
            }],
        }
    }

    /// 内容 chunk。
    pub fn content(id: &str, model: &str, created: i64, text: &str) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta {
                    role: None,
                    content: Some(text.to_string()),
                },
                finish_reason: None,
            }],
        }
    }

    /// 末个 chunk（携带 finish_reason）。
    pub fn last(id: &str, model: &str, created: i64, reason: FinishReason) -> Self {
        Self {
            id: id.to_string(),
            object: "chat.completion.chunk".to_string(),
            created,
            model: model.to_string(),
            choices: vec![ChatChunkChoice {
                index: 0,
                delta: ChatDelta::default(),
                finish_reason: Some(reason),
            }],
        }
    }

    /// 序列化为 SSE 行（含 `data: ` 前缀与双换行）。
    pub fn to_sse(&self) -> String {
        format!(
            "data: {}\n\n",
            serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
        )
    }

    /// SSE 终止标记。
    pub fn sse_done() -> &'static str {
        "data: [DONE]\n\n"
    }
}

/// 文本补全请求（legacy `/v1/completions`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// 模型 ID
    pub model: String,
    /// 提示文本
    pub prompt: String,
    /// 是否流式
    #[serde(default)]
    pub stream: bool,
    /// 采样参数
    #[serde(flatten)]
    pub sampling: SamplingParams,
    /// 额外字段
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// 文本补全的选项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionChoice {
    /// 序号
    pub index: u32,
    /// 生成文本
    pub text: String,
    /// 结束原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

/// 文本补全响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// ID
    pub id: String,
    /// 固定为 `text_completion`
    pub object: String,
    /// 创建时间
    pub created: i64,
    /// 模型
    pub model: String,
    /// 选项
    pub choices: Vec<CompletionChoice>,
    /// 用量
    pub usage: Usage,
}

/// 向量化请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    /// 模型 ID
    pub model: String,
    /// 输入文本（单个或多个）
    pub input: EmbeddingInput,
}

/// 向量化输入，支持单串或数组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    /// 单条
    Single(String),
    /// 多条
    Multiple(Vec<String>),
}

impl EmbeddingInput {
    /// 归一化为切片视图。
    pub fn as_slice(&self) -> Vec<&str> {
        match self {
            EmbeddingInput::Single(s) => vec![s.as_str()],
            EmbeddingInput::Multiple(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

/// 单条向量结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingItem {
    /// 固定为 `embedding`
    pub object: String,
    /// 向量
    pub embedding: Vec<f32>,
    /// 序号
    pub index: u32,
}

/// 向量化响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    /// 固定为 `list`
    pub object: String,
    /// 结果数组
    pub data: Vec<EmbeddingItem>,
    /// 模型
    pub model: String,
    /// 用量
    pub usage: Usage,
}

/// 健康检查响应。
///
/// **私有扩展**：OpenAI 无此端点，这里额外暴露算力来源与降级信息，
/// 便于 CLI / TUI 展示而不必再打一个私有端点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// 固定 `ok`（网关自身存活即 ok，不代表后端就绪）
    pub status: String,
    /// 产品版本
    pub version: String,
    /// 当前后端算力来源描述
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// 后端推理服务是否已就绪
    pub backend_ready: bool,
    /// 当前模型 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl HealthResponse {
    /// 网关存活但后端未就绪。
    pub fn degraded(version: impl Into<String>, model: Option<String>) -> Self {
        Self {
            status: "ok".to_string(),
            version: version.into(),
            backend: None,
            backend_ready: false,
            model,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_maps_statuses() {
        let e = crate::MnError::bad_request("x");
        let r = ErrorResponse::from_mn(&e);
        assert_eq!(r.http_status(), 400);

        let e = crate::MnError::ModelNotFound {
            path: std::path::PathBuf::from("/m"),
        };
        assert_eq!(ErrorResponse::from_mn(&e).http_status(), 400);

        // 本地资源不足属于「暂时无法服务」，语义上应对应 503 而非 500
        let e = crate::MnError::InsufficientLocalResource { detail: "x".into() };
        assert_eq!(ErrorResponse::from_mn(&e).http_status(), 503);

        // 无可用云端节点同样是 503
        let e = crate::MnError::NoViableCandidate { reason: "x".into() };
        assert_eq!(ErrorResponse::from_mn(&e).http_status(), 503);

        // 配置类错误是真正的服务端问题
        let e = crate::MnError::config("a.b", "x");
        assert_eq!(ErrorResponse::from_mn(&e).http_status(), 500);
    }

    #[test]
    fn chat_request_parses_openai_payload() {
        let json = r#"{
            "model": "qwen2.5-7b",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hi"}
            ],
            "temperature": 0.7,
            "max_tokens": 128,
            "stream": true
        }"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "qwen2.5-7b");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, Role::System);
        assert!(req.stream);
        assert_eq!(req.sampling.temperature, Some(0.7));
        assert_eq!(req.sampling.max_tokens, Some(128));
        assert!(req.validate().is_ok());
        assert_eq!(req.last_user_content(), Some("hi"));
    }

    #[test]
    fn unknown_fields_are_preserved_not_rejected() {
        let json = r#"{
            "model": "m",
            "messages": [{"role": "user", "content": "x"}],
            "logprobs": true,
            "some_future_field": 42
        }"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.extra.get("logprobs").unwrap(), &serde_json::json!(true));
        assert_eq!(
            req.extra.get("some_future_field").unwrap(),
            &serde_json::json!(42)
        );
    }

    #[test]
    fn validation_catches_bad_ranges() {
        let mut req = ChatRequest {
            model: "m".into(),
            messages: vec![ChatMessage::user("hi")],
            stream: false,
            sampling: SamplingParams::default(),
            extra: Default::default(),
        };
        assert!(req.validate().is_ok());

        req.sampling.temperature = Some(3.0);
        let (param, _) = req.validate().unwrap_err();
        assert_eq!(param, "temperature");

        req.sampling.temperature = None;
        req.sampling.top_p = Some(1.5);
        assert_eq!(req.validate().unwrap_err().0, "top_p");

        req.sampling.top_p = None;
        req.sampling.max_tokens = Some(0);
        assert_eq!(req.validate().unwrap_err().0, "max_tokens");

        req.sampling.max_tokens = None;
        req.messages.clear();
        assert_eq!(req.validate().unwrap_err().0, "messages");

        req.messages = vec![ChatMessage::user("x")];
        req.model = "  ".into();
        assert_eq!(req.validate().unwrap_err().0, "model");
    }

    #[test]
    fn chat_response_serializes_with_openai_shape() {
        let r = ChatResponse::single(
            "chatcmpl-1",
            "qwen2.5-7b",
            1_700_000_000,
            "hello",
            FinishReason::Stop,
            Usage::new(5, 1),
        );
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["choices"][0]["message"]["role"], "assistant");
        assert_eq!(v["choices"][0]["message"]["content"], "hello");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["total_tokens"], 6);
    }

    #[test]
    fn usage_sums_safely() {
        let u = Usage::new(u32::MAX, 1);
        assert_eq!(u.total_tokens, u32::MAX); // 饱和而非溢出
        assert_eq!(Usage::new(10, 5).total_tokens, 15);
    }

    #[test]
    fn stream_chunks_have_correct_sequence() {
        let first = ChatChunk::first("c1", "m", 100);
        assert!(first.choices[0].delta.role.is_some());
        assert!(first.choices[0].delta.content.is_none());
        assert!(first.choices[0].finish_reason.is_none());

        let mid = ChatChunk::content("c1", "m", 100, "tok");
        assert!(mid.choices[0].delta.role.is_none());
        assert_eq!(mid.choices[0].delta.content.as_deref(), Some("tok"));

        let last = ChatChunk::last("c1", "m", 100, FinishReason::Length);
        assert!(last.choices[0].delta.content.is_none());
        assert_eq!(last.choices[0].finish_reason, Some(FinishReason::Length));

        let sse = mid.to_sse();
        assert!(sse.starts_with("data: {"));
        assert!(sse.ends_with("\n\n"));
        assert_eq!(ChatChunk::sse_done(), "data: [DONE]\n\n");
    }

    #[test]
    fn chunk_omits_null_delta_fields() {
        let last = ChatChunk::last("c", "m", 0, FinishReason::Stop);
        let v = serde_json::to_value(&last).unwrap();
        assert!(v["choices"][0]["delta"].get("content").is_none());
        assert!(v["choices"][0]["delta"].get("role").is_none());
    }

    #[test]
    fn model_list_shape() {
        let l = ModelList::new(vec![ModelObject::new("a", 1, "local")]);
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][0]["object"], "model");
        assert_eq!(v["data"][0]["owned_by"], "local");
    }

    #[test]
    fn embedding_input_variants() {
        let single: EmbeddingRequest =
            serde_json::from_str(r#"{"model":"m","input":"hello"}"#).unwrap();
        assert_eq!(single.input.as_slice(), vec!["hello"]);

        let multi: EmbeddingRequest =
            serde_json::from_str(r#"{"model":"m","input":["a","b"]}"#).unwrap();
        assert_eq!(multi.input.as_slice(), vec!["a", "b"]);
    }

    #[test]
    fn role_roundtrip() {
        for r in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let j = serde_json::to_string(&r).unwrap();
            let back: Role = serde_json::from_str(&j).unwrap();
            assert_eq!(back, r);
            assert_eq!(j, format!("\"{}\"", r.as_str()));
        }
    }

    #[test]
    fn finish_reason_serialization() {
        assert_eq!(
            serde_json::to_string(&FinishReason::ContentFilter).unwrap(),
            "\"content_filter\""
        );
        assert_eq!(FinishReason::ToolCalls.as_str(), "tool_calls");
    }

    #[test]
    fn health_response_degraded_shape() {
        let h = HealthResponse::degraded("0.1.0", Some("m".into()));
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["backend_ready"], false);
        assert!(v.get("backend").is_none());
    }

    #[test]
    fn completion_request_parses() {
        let r: CompletionRequest =
            serde_json::from_str(r#"{"model":"m","prompt":"hi","max_tokens":8}"#).unwrap();
        assert_eq!(r.prompt, "hi");
        assert_eq!(r.sampling.max_tokens, Some(8));
    }

    #[test]
    fn error_bad_request_carries_param() {
        let e = ErrorResponse::bad_request("bad", Some("messages"));
        assert_eq!(e.error.param.as_deref(), Some("messages"));
        assert_eq!(e.http_status(), 400);
    }
}
