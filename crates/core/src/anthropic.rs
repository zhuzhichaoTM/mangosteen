//! Anthropic 兼容协议数据结构。
//!
//! 归属：**API 网关层 · 统一协议适配层**。
//!
//! ## 设计目标
//!
//! 支持 Anthropic Claude API 的客户端接入 Mangosteen 网关，与 OpenAI 接口并存。
//! 网关会自动将 Anthropic 请求转换为 OpenAI 格式，转发给后端（llama.cpp），
//! 再将响应转换回 Anthropic 格式返回。
//!
//! ## 覆盖范围
//!
//! | 端点 | 结构 |
//! |------|------|
//! | `POST /v1/messages` | [`MessagesRequest`] / [`MessagesResponse`] |
//!
//! 流式响应（SSE）的 chunk 结构见 [`MessageStreamEvent`]。

use serde::{Deserialize, Serialize};

/// Anthropic 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicRole {
    /// 用户
    User,
    /// 助手
    Assistant,
}

impl AnthropicRole {
    pub fn as_str(self) -> &'static str {
        match self {
            AnthropicRole::User => "user",
            AnthropicRole::Assistant => "assistant",
        }
    }
}

/// Anthropic 消息内容块类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// 文本块
    #[serde(rename = "text")]
    Text {
        text: String,
    },
    /// 工具使用块（简化实现，不支持工具调用）
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// 工具结果块
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

/// Anthropic 消息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicMessage {
    /// 角色
    pub role: AnthropicRole,
    /// 内容（字符串或内容块数组）
    pub content: serde_json::Value, // 支持字符串或 ContentBlock 数组
}

/// Anthropic 消息请求。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesRequest {
    /// 模型 ID
    pub model: String,
    /// 消息列表
    pub messages: Vec<AnthropicMessage>,
    /// 系统提示（独立字段，不在 messages 中）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// 最大生成 token 数
    pub max_tokens: u32,
    /// 是否流式返回
    #[serde(default)]
    pub stream: bool,
    /// 采样参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 核采样
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// 停止序列
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    /// 元数据（保留字段）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// 保留的额外字段
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Anthropic 消息响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessagesResponse {
    /// 消息 ID
    pub id: String,
    /// 固定为 "message"
    #[serde(rename = "type")]
    pub kind: String,
    /// 角色（固定为 assistant）
    pub role: String,
    /// 内容块数组
    pub content: Vec<ContentBlock>,
    /// 模型 ID
    pub model: String,
    /// 停止原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// token 用量
    pub usage: Usage,
}

/// token 用量统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// 输入 token 数
    pub input_tokens: u32,
    /// 输出 token 数
    pub output_tokens: u32,
}

/// Anthropic 流式事件类型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageStreamEvent {
    /// 消息开始
    #[serde(rename = "message_start")]
    MessageStart {
        message: MessagesResponse,
    },
    /// 内容块开始
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    /// 内容块增量
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: u32,
        delta: Delta,
    },
    /// 内容块结束
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        index: u32,
    },
    /// 消息增量
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDelta,
        usage: Usage,
    },
    /// 消息结束
    #[serde(rename = "message_stop")]
    MessageStop,
    /// 错误事件
    #[serde(rename = "error")]
    Error {
        error: AnthropicError,
    },
    /// ping 事件
    #[serde(rename = "ping")]
    Ping,
}

/// 内容块增量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Delta {
    /// 文本增量
    #[serde(rename = "text_delta")]
    TextDelta {
        text: String,
    },
    /// 输入 JSON 增量（工具调用）
    #[serde(rename = "input_json_delta")]
    InputJsonDelta {
        partial_json: String,
    },
}

/// 消息增量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageDelta {
    /// 停止原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// 停止序列
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
}

/// Anthropic 错误响应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicError {
    /// 错误类型
    #[serde(rename = "type")]
    pub kind: String,
    /// 错误消息
    pub message: String,
}

/// Anthropic 错误响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnthropicErrorResponse {
    /// 错误详情
    pub error: AnthropicError,
}

impl AnthropicErrorResponse {
    /// 由内部错误构造。
    pub fn from_mn(e: &crate::MnError) -> Self {
        let kind = match e {
            crate::MnError::BadRequest { .. } => "invalid_request_error",
            crate::MnError::ModelNotFound { .. } => "not_found_error",
            crate::MnError::AccessDenied { .. } => "permission_error",
            _ => "api_error",
        };
        Self {
            error: AnthropicError {
                kind: kind.to_string(),
                message: e.to_string(),
            },
        }
    }

    /// 构造请求参数错误。
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            error: AnthropicError {
                kind: "invalid_request_error".to_string(),
                message: message.into(),
            },
        }
    }
}

impl MessagesRequest {
    /// 校验请求。
    pub fn validate(&self) -> Result<(), (&'static str, String)> {
        if self.model.trim().is_empty() {
            return Err(("model", "不能为空".to_string()));
        }
        if self.messages.is_empty() {
            return Err(("messages", "至少需要一条消息".to_string()));
        }
        if self.max_tokens == 0 {
            return Err(("max_tokens", "不能为 0".to_string()));
        }
        Ok(())
    }
}