//! OpenAI 兼容推理转发处理器（`/v1/*`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use mangosteen_core::openai::{ChatRequest, ErrorResponse};
use mangosteen_core::{MnError, MnResult};

use crate::http::server::*;
use crate::http::upstream::{http_post_json, summarize_body};
use crate::protocol::{self, BackendTarget};

impl GatewayServer {
    pub(crate) fn chat(&self, req: &ParsedRequest) -> Response {
        let parsed: ChatRequest = match serde_json::from_slice(&req.body) {
            Ok(r) => r,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 ChatRequest：{e}"), None),
                )
            }
        };
        let target = match self.backend_for_model(Some(parsed.model.as_str())) {
            Ok(t) => t,
            Err(e) => return Response::error(&e),
        };
        let normalized = match protocol::normalize_chat(parsed, &target) {
            Ok(n) => n,
            Err(e) => return Response::error(&e),
        };
        let body = match serde_json::to_vec(&normalized) {
            Ok(b) => b,
            Err(e) => return Response::error(&MnError::bad_request(format!("序列化失败：{e}"))),
        };
        self.forward_json("chat/completions", &body, &target)
    }
    pub(crate) fn completion(&self, req: &ParsedRequest) -> Response {
        let parsed: mangosteen_core::openai::CompletionRequest =
            match serde_json::from_slice(&req.body) {
                Ok(r) => r,
                Err(e) => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("请求体不是合法 CompletionRequest：{e}"),
                            None,
                        ),
                    )
                }
            };
        let target = match self.backend() {
            Ok(t) => t,
            Err(e) => return Response::error(&e),
        };
        let normalized = match protocol::normalize_completion(parsed, &target) {
            Ok(n) => n,
            Err(e) => return Response::error(&e),
        };
        let body = match serde_json::to_vec(&normalized) {
            Ok(b) => b,
            Err(e) => return Response::error(&MnError::bad_request(format!("序列化失败：{e}"))),
        };
        self.forward_json("completions", &body, &target)
    }
    pub(crate) fn embeddings(&self, req: &ParsedRequest) -> Response {
        let parsed: mangosteen_core::openai::EmbeddingRequest =
            match serde_json::from_slice(&req.body) {
                Ok(r) => r,
                Err(e) => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("请求体不是合法 EmbeddingRequest：{e}"),
                            None,
                        ),
                    )
                }
            };
        let target = match self.backend() {
            Ok(t) => t,
            Err(e) => return Response::error(&e),
        };
        let normalized = match protocol::normalize_embedding(parsed, &target) {
            Ok(n) => n,
            Err(e) => return Response::error(&e),
        };
        let body = match serde_json::to_vec(&normalized) {
            Ok(b) => b,
            Err(e) => return Response::error(&MnError::bad_request(format!("序列化失败：{e}"))),
        };
        self.forward_json("embeddings", &body, &target)
    }
    /// 取当前后端。
    pub(crate) fn backend(&self) -> MnResult<BackendTarget> {
        let mut gw = self
            .gateway
            .lock()
            .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
        gw.active_backend()
    }
    /// 按请求里的 `model` 选择后端。
    ///
    /// 客户端传 `auto` 或缺省 → 自动（本地优先）；
    /// 传了具体模型 ID → 必须路由到那个模型，未运行则明确报错。
    ///
    /// 这样 UI「模型信息」下拉的选择才真正生效——否则用户选了 A
    /// 却被路由到 B，回答不对劲却查不出原因。
    pub(crate) fn backend_for_model(&self, model: Option<&str>) -> MnResult<BackendTarget> {
        let mut gw = self
            .gateway
            .lock()
            .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
        gw.active_backend_for(model)
    }
    /// 转发 JSON 请求到上游，并把上游响应归一化。
    pub(crate) fn forward_json(&self, path: &str, body: &[u8], target: &BackendTarget) -> Response {
        let url = target.url_for(path);
        match http_post_json(&url, body, self.upstream_timeout) {
            Ok((status, resp_body)) => {
                // 上游错误 → 归一化
                if !(200..300).contains(&status) {
                    let e = MnError::Network {
                        stage: format!("上游推理服务（HTTP {status}）"),
                        reason: summarize_body(&resp_body),
                    };
                    return Response::error(&e);
                }
                // 上游会把 model 填成模型文件绝对路径（llama.cpp 行为），
                // 这里统一改写为 Mangosteen 的模型 ID，保证与 /v1/models
                // 一致，且不向调用方泄漏本机路径。SSE 流也走同一函数
                // （内部按 data: 行逐条改写）。
                let body = protocol::rewrite_response_model(&resp_body, &target.model_id);
                // stream=true 时上游返回 SSE，Content-Type 必须是
                // text/event-stream，否则标准 OpenAI SDK 的流式解析器不认。
                let content_type = if protocol::looks_like_sse(&body) {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                Response {
                    status,
                    content_type: content_type.to_string(),
                    body: body.into_bytes(),
                }
            }
            Err(e) => Response::error(&e),
        }
    }
}
