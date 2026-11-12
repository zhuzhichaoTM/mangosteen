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

    /// 按台账重建业务转发快照（**整体替换**）。
    ///
    /// 部署成功、卸载、停止服务之后都必须调用：只增不减会让已停止的模型
    /// 继续留在快照里，对话被路由到一个已经关闭的隧道端口，表现为
    /// 「模型已卸载但对话仍报连接失败」。
    ///
    /// 后台探测线程翻转台账状态后也要重建（经 [`rebuild_snapshot`]），
    /// 否则探测自愈的 `ready` 记录进不了对话路由。
    pub(crate) fn rebuild_backend_snapshot(&self) {
        rebuild_snapshot(&self.gateway, &self.backend_snapshot);
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

    /// Anthropic Messages API 处理器（`/v1/messages`）。
    ///
    /// 将 Anthropic 请求转换为 OpenAI 格式，转发给后端，再将响应转换回 Anthropic 格式。
    pub(crate) fn anthropic_messages(&self, req: &ParsedRequest) -> Response {
        use mangosteen_core::anthropic::MessagesRequest;

        let parsed: MessagesRequest = match serde_json::from_slice(&req.body) {
            Ok(r) => r,
            Err(e) => {
                return Response::json(
                    400,
                    &mangosteen_core::anthropic::AnthropicErrorResponse::bad_request(format!(
                        "请求体不是合法 MessagesRequest：{e}"
                    )),
                )
            }
        };

        // 校验请求
        if let Err((param, reason)) = parsed.validate() {
            return Response::json(
                400,
                &mangosteen_core::anthropic::AnthropicErrorResponse::bad_request(format!(
                    "参数 `{param}` 非法：{reason}"
                )),
            );
        }

        // 选择后端
        let target = match self.backend_for_model(Some(parsed.model.as_str())) {
            Ok(t) => t,
            Err(e) => {
                return Response::json(
                    400,
                    &mangosteen_core::anthropic::AnthropicErrorResponse::from_mn(&e),
                )
            }
        };

        // 转换为 OpenAI 请求
        let openai_req = match protocol::normalize_anthropic_to_openai(&parsed, &target) {
            Ok(r) => r,
            Err(e) => {
                return Response::json(
                    400,
                    &mangosteen_core::anthropic::AnthropicErrorResponse::from_mn(&e),
                )
            }
        };

        // 序列化请求体
        let body = match serde_json::to_vec(&openai_req) {
            Ok(b) => b,
            Err(e) => {
                return Response::json(
                    500,
                    &mangosteen_core::anthropic::AnthropicErrorResponse::bad_request(format!(
                        "序列化失败：{e}"
                    )),
                )
            }
        };

        // 转发请求
        let response = self.forward_json("chat/completions", &body, &target);

        // 转换响应为 Anthropic 格式
        if response.status >= 200 && response.status < 300 {
            let body_str = String::from_utf8_lossy(&response.body);
            match protocol::parse_openai_to_anthropic_response(
                &body_str,
                response.status,
                &target.model_id,
            ) {
                Ok(anthropic_resp) => Response::json(200, &anthropic_resp),
                Err(e) => Response::json(
                    500,
                    &mangosteen_core::anthropic::AnthropicErrorResponse::from_mn(&e),
                ),
            }
        } else {
            // 错误响应也需要转换为 Anthropic 格式
            let error = mangosteen_core::anthropic::AnthropicErrorResponse::from_mn(
                &MnError::Network {
                    stage: format!("上游推理服务（HTTP {}）", response.status),
                    reason: String::from_utf8_lossy(&response.body).to_string(),
                },
            );
            Response::json(response.status, &error)
        }
    }
}

/// 按台账整体重建业务转发快照。
///
/// 与 [`GatewayServer::rebuild_backend_snapshot`] 同一逻辑的自由函数
/// 形态：后台探测线程（`crate::runprobe`）没有 `GatewayServer` 实例，
/// 只持有 `gateway` 与 `backend_snapshot` 两个 Arc，翻转台账状态后
/// 通过它同步对话路由，保证探测自愈的模型立刻可对话。
pub(crate) fn rebuild_snapshot(
    gateway: &std::sync::Arc<crate::gateway::Gateway>,
    snapshot: &std::sync::RwLock<std::collections::BTreeMap<String, BackendTarget>>,
) {
    let gw = gateway.as_ref();
    let Ok(records) = gw.list_deployments() else {
        return;
    };
    let Ok(mut snapshot) = snapshot.write() else {
        return;
    };
    snapshot.clear();
    for record in records {
        if record.status != mangosteen_base::state::DeploymentStatus::Ready {
            continue;
        }
        let Some(port) = record.plan.local_access_port() else {
            continue;
        };
        let model_id = mangosteen_core::model::ModelEntry::derive_id(
            &record.plan.model.name,
            record.plan.model.quant,
            record.plan.model.alias.as_deref(),
        );
        snapshot.insert(
            model_id.clone(),
            BackendTarget {
                host: "127.0.0.1".to_string(),
                port,
                model_id,
                origin_label: record.plan.origin.label(),
            },
        );
    }
}
