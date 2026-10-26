//! 统一协议适配层。
//!
//! 归属：**API 网关层 · 统一协议适配层**（网关三大组件之一）。
//!
//! ## 职责
//!
//! 屏蔽「本地 llama.cpp」与「云端实例」的接口差异，向上输出两套统一接口：
//!
//! 1. **部署管理接口**：由网关门面 `Gateway` 提供（create/start/stop/destroy/status）；
//! 2. **OpenAI 兼容推理接口**：由本模块负责**请求规范化 + 上游转发**。
//!
//! ## 为什么转发而不是自己实现推理
//!
//! 网关不碰推理逻辑，只做协议转换与路由。这样：
//!
//! * 本地与云端走同一份转发代码，行为一致（**推理引擎唯一性**的延伸）；
//! * 上游升级 llama.cpp 时，网关无需改动；
//! * 网关不可用时，用户仍可直连后端排障。
//!
//! ## 请求规范化要点
//!
//! * `model = "auto"` → 替换为当前已部署的模型 ID（上层应用无需知道实际模型）；
//! * 采样参数缺失 → 不补默认值（交给 llama.cpp 用它自己的默认值，避免双重默认）；
//! * 未知字段 → 原样透传（OpenAI 兼容性的关键）。

use mangosteen_core::openai::{
    ChatRequest, ChatResponse, CompletionRequest, EmbeddingRequest, ErrorResponse, ModelList,
    ModelObject, AUTO_MODEL_ID,
};
use mangosteen_core::{MnError, MnResult};

/// 后端算力目标：网关转发时的目的地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendTarget {
    /// 监听地址（本地为 `127.0.0.1`，云端经隧道后也是 `127.0.0.1`）
    pub host: String,
    /// 端口
    pub port: u16,
    /// 该后端的模型 ID（用于 `auto` 替换与响应回填）
    pub model_id: String,
    /// 算力来源描述（写入响应与健康检查）
    pub origin_label: String,
}

impl BackendTarget {
    /// base URL（不含 `/v1`）。
    pub fn base_url(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// OpenAI 兼容 base URL。
    pub fn openai_base_url(&self) -> String {
        format!("{}/v1", self.base_url())
    }

    /// 完整上游 URL。
    pub fn url_for(&self, path: &str) -> String {
        format!(
            "{}/v1/{}",
            self.base_url().trim_end_matches('/'),
            path.trim_start_matches("/v1/").trim_start_matches('/')
        )
    }
}

/// 规范化对话请求。
///
/// **只改 `model` 字段**，其余一律原样保留以维持 OpenAI 兼容性。
pub fn normalize_chat(mut req: ChatRequest, target: &BackendTarget) -> MnResult<ChatRequest> {
    // 先把 auto/空 model 收敛掉，再校验——否则空 model 会被 validate 直接拒绝，
    // 而「不写 model」在 Mangosteen 里是合法用法（等于 auto）。
    if req.model == AUTO_MODEL_ID || req.model.trim().is_empty() {
        req.model = target.model_id.clone();
    }

    req.validate()
        .map_err(|(param, reason)| MnError::bad_request(format!("参数 `{param}` 非法：{reason}")))?;

    Ok(req)
}

/// 规范化文本补全请求。
pub fn normalize_completion(
    mut req: CompletionRequest,
    target: &BackendTarget,
) -> MnResult<CompletionRequest> {
    if req.model == AUTO_MODEL_ID || req.model.trim().is_empty() {
        req.model = target.model_id.clone();
    }
    req.sampling
        .validate()
        .map_err(|(param, reason)| MnError::bad_request(format!("参数 `{param}` 非法：{reason}")))?;
    Ok(req)
}

/// 规范化向量化请求。
pub fn normalize_embedding(
    mut req: EmbeddingRequest,
    target: &BackendTarget,
) -> MnResult<EmbeddingRequest> {
    if req.model == AUTO_MODEL_ID || req.model.trim().is_empty() {
        req.model = target.model_id.clone();
    }
    if req.input.as_slice().iter().all(|s| s.trim().is_empty()) {
        return Err(MnError::bad_request("input 不能全为空字符串"));
    }
    Ok(req)
}

/// 构造模型列表响应。
///
/// `owned_by` 用算力来源表达：`local` 或 `cloud:<provider>`，
/// 让上层应用在需要时能区分，但不影响其调用方式。
pub fn build_model_list(entries: &[(String, String)], created: i64) -> ModelList {
    ModelList::new(
        entries
            .iter()
            .map(|(id, owned_by)| ModelObject::new(id.clone(), created, owned_by.clone()))
            .collect(),
    )
}

/// 从上游响应文本解析对话响应。
///
/// 上游报错时把它的错误体**归一化**成 Mangosteen 的错误格式，
/// 确保上层应用只需处理一种错误结构。
pub fn parse_chat_response(body: &str, status: u16) -> MnResult<ChatResponse> {
    if !(200..300).contains(&status) {
        return Err(upstream_error(body, status));
    }
    serde_json::from_str(body).map_err(|e| MnError::Network {
        stage: "解析上游响应".to_string(),
        reason: format!("上游返回的不是合法 chat.completion JSON：{e}"),
    })
}

/// 把上游响应体里的 `model` 字段改写为统一的模型 ID。
///
/// ## 为什么必须改写
///
/// llama.cpp 在响应里填的是**模型文件的绝对路径**
/// （如 `/Users/x/models/qwen2.5-0.5b.gguf`），而 `/v1/models` 返回的是
/// Mangosteen 的模型 ID（`qwen2.5-0.5b-instruct:q4_k_m`）。两者不一致时：
///
/// * 上层应用做「请求模型 == 响应模型」的校验会失败；
/// * 日志/Tracing 里出现本机绝对路径，泄漏部署细节且难以聚合统计。
///
/// 这正是**统一协议适配层**该做的事：屏蔽本地/云端的上游差异，
/// 向上只暴露一套一致的标识。
///
/// 实现上只改 `model` 一个字段，其余字段原样保留——不用重新序列化整个
/// 结构体，避免因 `ChatResponse` 字段增减而丢字段。
pub fn rewrite_response_model(body: &str, model_id: &str) -> String {
    // 流式响应是 SSE：每个 `data: {...}` 行都是一个独立 chunk，
    // 每个 chunk 都带 model 字段，必须逐行处理。
    if looks_like_sse(body) {
        return rewrite_sse_model(body, model_id);
    }

    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(body) else {
        // 不是 JSON（例如上游返回纯文本错误）→ 原样返回，由调用方判断
        return body.to_string();
    };
    let Some(obj) = v.as_object_mut() else {
        return body.to_string();
    };
    if obj.contains_key("model") {
        obj.insert(
            "model".to_string(),
            serde_json::Value::String(model_id.to_string()),
        );
    }
    // 上游（llama.cpp）额外塞了一些非标准字段（timings / system_fingerprint 等），
    // 保留它们：OpenAI 客户端都容忍未知字段，删掉反而丢了性能观测信息。
    serde_json::to_string(&v).unwrap_or_else(|_| body.to_string())
}

/// 响应体是否为 SSE（Server-Sent Events）。
pub fn looks_like_sse(body: &str) -> bool {
    body.trim_start().starts_with("data:")
}

/// 逐行改写 SSE 里的 `model` 字段。
///
/// 改写**单行** SSE 的 `model` 字段（流式直通用）。
///
/// 与 [`rewrite_sse_model`] 的区别：那个处理整段 body，这个处理一行。
/// 流式转发不能等整段——必须来一行改一行、立刻发给客户端，
/// 否则就退化成「全部读完再返回」，流式语义全丢。
///
/// 非 `data:` 行（空行、`event:`、`[DONE]`）原样返回：
/// 破坏 SSE 帧结构会让标准客户端直接解析失败。
pub fn rewrite_sse_line(line: &str, model_id: &str) -> String {
    let Some(payload) = line.strip_prefix("data:") else {
        return line.to_string();
    };
    let payload = payload.trim_start();
    if payload == "[DONE]" || payload.is_empty() {
        return line.to_string();
    }
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(mut v) => {
            if let Some(obj) = v.as_object_mut() {
                if obj.contains_key("model") {
                    obj.insert(
                        "model".to_string(),
                        serde_json::Value::String(model_id.to_string()),
                    );
                }
            }
            // 序列化失败就退回原文：宁可多暴露一个路径，也不能丢帧
            match serde_json::to_string(&v) {
                Ok(j) => format!("data: {j}"),
                Err(_) => line.to_string(),
            }
        }
        Err(_) => line.to_string(),
    }
}

/// 非 `data:` 行（空行、`event:`、`[DONE]`）原样保留——破坏 SSE 帧结构
/// 会让标准客户端直接解析失败。
fn rewrite_sse_model(body: &str, model_id: &str) -> String {
    let mut out = String::with_capacity(body.len());
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let payload = payload.trim_start();
        if payload == "[DONE]" || payload.is_empty() {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    if obj.contains_key("model") {
                        obj.insert(
                            "model".to_string(),
                            serde_json::Value::String(model_id.to_string()),
                        );
                    }
                }
                out.push_str("data: ");
                out.push_str(&serde_json::to_string(&v).unwrap_or_else(|_| payload.to_string()));
                out.push('\n');
            }
            // 解析失败的行原样透传，不吞数据
            Err(_) => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// 解析向量化响应。
pub fn parse_embedding_response(
    body: &str,
    status: u16,
) -> MnResult<mangosteen_core::openai::EmbeddingResponse> {
    if !(200..300).contains(&status) {
        return Err(upstream_error(body, status));
    }
    serde_json::from_str(body).map_err(|e| MnError::Network {
        stage: "解析上游响应".to_string(),
        reason: format!("上游返回的不是合法 embeddings JSON：{e}"),
    })
}

/// 把上游错误归一化为 Mangosteen 错误。
fn upstream_error(body: &str, status: u16) -> MnError {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("message")
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_else(|| body.chars().take(200).collect());

    MnError::Network {
        stage: format!("上游推理服务（HTTP {status}）"),
        reason: detail,
    }
}

/// 生成请求 ID（不引入 UUID 依赖：时间戳 + 序号）。
pub fn new_request_id(prefix: &str, seq: u64) -> String {
    format!(
        "{prefix}-{:x}{:04x}",
        mangosteen_core::time::now_unix_millis(),
        seq & 0xffff
    )
}

/// 统一错误 → (HTTP 状态码, OpenAI 错误响应)。
pub fn error_payload(e: &MnError) -> (u16, ErrorResponse) {
    let r = ErrorResponse::from_mn(e);
    (r.http_status(), r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::openai::{ChatMessage, FinishReason, SamplingParams, Usage};

    fn target() -> BackendTarget {
        BackendTarget {
            host: "127.0.0.1".into(),
            port: 11435,
            model_id: "qwen2.5-7b-instruct:q4_k_m".into(),
            origin_label: "本地算力".into(),
        }
    }

    fn chat_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![ChatMessage::user("hi")],
            stream: false,
            sampling: SamplingParams::default(),
            extra: Default::default(),
        }
    }

    #[test]
    fn auto_model_is_replaced() {
        let n = normalize_chat(chat_req("auto"), &target()).unwrap();
        assert_eq!(n.model, "qwen2.5-7b-instruct:q4_k_m");
    }

    #[test]
    fn empty_model_is_treated_as_auto() {
        let n = normalize_chat(chat_req("  "), &target()).unwrap();
        assert_eq!(n.model, "qwen2.5-7b-instruct:q4_k_m");
    }

    #[test]
    fn explicit_model_is_preserved() {
        let n = normalize_chat(chat_req("my-own-model"), &target()).unwrap();
        assert_eq!(n.model, "my-own-model", "上层指定的模型名不应被改写");
    }

    #[test]
    fn normalization_preserves_unknown_fields() {
        let mut r = chat_req("auto");
        r.extra.insert("logprobs".into(), serde_json::json!(true));
        r.extra.insert("custom".into(), serde_json::json!({"k": 1}));
        let n = normalize_chat(r, &target()).unwrap();
        assert_eq!(n.extra.get("logprobs").unwrap(), &serde_json::json!(true));
        assert_eq!(n.extra.get("custom").unwrap()["k"], 1);
    }

    #[test]
    fn normalization_validates_sampling() {
        let mut r = chat_req("auto");
        r.sampling.temperature = Some(9.0);
        let e = normalize_chat(r, &target()).unwrap_err();
        assert!(matches!(e, MnError::BadRequest { .. }));
        assert!(e.to_string().contains("temperature"));
    }

    #[test]
    fn empty_messages_rejected() {
        let mut r = chat_req("auto");
        r.messages.clear();
        assert!(normalize_chat(r, &target()).is_err());
    }

    #[test]
    fn completion_normalization() {
        let r = CompletionRequest {
            model: "auto".into(),
            prompt: "hello".into(),
            stream: false,
            sampling: SamplingParams::default(),
            extra: Default::default(),
        };
        let n = normalize_completion(r, &target()).unwrap();
        assert_eq!(n.model, "qwen2.5-7b-instruct:q4_k_m");
    }

    #[test]
    fn embedding_normalization_rejects_blank_input() {
        let r = EmbeddingRequest {
            model: "auto".into(),
            input: mangosteen_core::openai::EmbeddingInput::Multiple(vec![
                "  ".into(),
                "".into(),
            ]),
        };
        assert!(normalize_embedding(r, &target()).is_err());

        let ok = EmbeddingRequest {
            model: "auto".into(),
            input: mangosteen_core::openai::EmbeddingInput::Single("text".into()),
        };
        assert_eq!(
            normalize_embedding(ok, &target()).unwrap().model,
            "qwen2.5-7b-instruct:q4_k_m"
        );
    }

    #[test]
    fn backend_target_urls() {
        let t = target();
        assert_eq!(t.base_url(), "http://127.0.0.1:11435");
        assert_eq!(t.openai_base_url(), "http://127.0.0.1:11435/v1");
        assert_eq!(
            t.url_for("/v1/chat/completions"),
            "http://127.0.0.1:11435/v1/chat/completions"
        );
        assert_eq!(
            t.url_for("chat/completions"),
            "http://127.0.0.1:11435/v1/chat/completions"
        );
    }

    #[test]
    fn model_list_marks_origin() {
        let l = build_model_list(
            &[
                ("qwen2.5-7b:q4_k_m".into(), "local".into()),
                ("qwen2.5-32b:q4_k_m".into(), "cloud:autodl".into()),
            ],
            1_700_000_000,
        );
        assert_eq!(l.data.len(), 2);
        assert_eq!(l.data[0].owned_by, "local");
        assert_eq!(l.data[1].owned_by, "cloud:autodl");
        assert_eq!(l.object, "list");
    }

    #[test]
    fn parses_valid_chat_response() {
        let r = ChatResponse::single(
            "id1",
            "m",
            0,
            "hello",
            FinishReason::Stop,
            Usage::new(3, 1),
        );
        let body = serde_json::to_string(&r).unwrap();
        let parsed = parse_chat_response(&body, 200).unwrap();
        assert_eq!(parsed.choices[0].message.content, "hello");
    }

    #[test]
    fn upstream_error_is_normalized() {
        let body =
            r#"{"error":{"message":"context length exceeded","type":"invalid_request_error"}}"#;
        let e = parse_chat_response(body, 400).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("context length exceeded"), "got {msg}");
        assert!(msg.contains("400"), "got {msg}");
        assert!(e.is_retryable(), "上游错误应可重试/降级");
    }

    #[test]
    fn upstream_error_without_json_falls_back_to_body() {
        let e = parse_chat_response("gateway timeout", 504).unwrap_err();
        assert!(e.to_string().contains("gateway timeout"));
    }

    #[test]
    fn malformed_success_body_is_reported() {
        let e = parse_chat_response("<html>", 200).unwrap_err();
        assert!(e.to_string().contains("不是合法"), "got {e}");
    }

    #[test]
    fn embedding_response_parsing_and_errors() {
        let ok = r#"{"object":"list","data":[{"object":"embedding","embedding":[0.1],"index":0}],"model":"m","usage":{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}}"#;
        assert!(parse_embedding_response(ok, 200).is_ok());
        assert!(parse_embedding_response("{}", 500).is_err());
    }

    #[test]
    fn error_payload_maps_status() {
        let (code, body) = error_payload(&MnError::bad_request("x"));
        assert_eq!(code, 400);
        assert_eq!(body.error.kind, "invalid_request_error");

        let (code, _) = error_payload(&MnError::ModelNotFound {
            path: std::path::PathBuf::from("/m"),
        });
        assert_eq!(code, 400);

        let (code, _) = error_payload(&MnError::InsufficientLocalResource {
            detail: "x".into(),
        });
        assert_eq!(code, 503);
    }

    #[test]
    fn request_ids_are_unique_and_prefixed() {
        let a = new_request_id("chatcmpl", 1);
        let b = new_request_id("chatcmpl", 2);
        assert!(a.starts_with("chatcmpl-"));
        assert_ne!(a, b);
    }
}
