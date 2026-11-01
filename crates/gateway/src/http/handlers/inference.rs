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

    /// 取当前后端。业务转发只读启动/刷新后的快照，不等待部署控制锁。
    pub(crate) fn backend(&self) -> MnResult<BackendTarget> {
        if let Ok(snapshot) = self.backend_snapshot.read() {
            if let Some(target) = snapshot.values().next() {
                return Ok(target.clone());
            }
        }
        Err(MnError::bad_request(
            "当前没有业务转发后端；请先部署并等待模型就绪",
        ))
    }
    /// 按请求里的 `model` 选择后端。
    ///
    /// 业务转发路径只读 `backend_snapshot`。若快照中已有对应模型，
    /// **不再回退到 Gateway 锁**；这样部署线程进行长时间节点配置、
    /// 下载或启动时，已就绪模型的对话仍可直接转发。
    pub(crate) fn backend_for_model(&self, model: Option<&str>) -> MnResult<BackendTarget> {
        let requested = model.filter(|m| !m.is_empty() && *m != "auto");
        if let Ok(snapshot) = self.backend_snapshot.read() {
            if let Some(id) = requested {
                if let Some(target) = snapshot.get(id) {
                    return Ok(target.clone());
                }
                // 兼容调用方只传裸模型名：仍只读快照，不碰部署控制锁。
                if let Some(target) = snapshot
                    .values()
                    .find(|target| target.model_id.split(':').next() == Some(id))
                {
                    return Ok(target.clone());
                }
                return Err(MnError::bad_request(format!(
                    "模型 `{id}` 未在业务转发快照中运行，部署控制不会阻塞对话；请等待该模型就绪"
                )));
            }
            if let Some(target) = snapshot.values().next() {
                return Ok(target.clone());
            }
        }
        Err(MnError::bad_request(
            "当前没有业务转发后端；请先部署并等待模型就绪",
        ))
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
