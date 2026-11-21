//! OpenAI 兼容推理转发处理器（`/v1/*`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use mangosteen_core::openai::{ChatRequest, ErrorResponse};
use mangosteen_core::{MnError, MnResult};

use crate::http::server::*;
use crate::http::upstream::{http_post_json_with_key_tls, summarize_body};
use crate::protocol::{self, BackendTarget};

impl GatewayServer {
    pub(crate) fn chat(&self, req: &ParsedRequest) -> Response {
        let mut parsed: ChatRequest = match serde_json::from_slice(&req.body) {
            Ok(r) => r,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 ChatRequest：{e}"), None),
                )
            }
        };
        // P1-4 max_tokens 上限（2026-11-19）：客户端可请求极大值耗尽 GPU
        // 时长/成本（成本型 DoS）。clamp 到 1..=4096，未指定则不注入
        //（保持 OpenAI 原义，llama-server 使用自身默认）。
        if let Some(m) = parsed.sampling.max_tokens {
            if m == 0 || m > 4096 {
                parsed.sampling.max_tokens = Some(m.clamp(1, 4096));
            }
        }
        // P2 速率限制（2026-11-21）：按模型分桶 token bucket，防成本型 DoS。
        if let Err(retry) = self.rate_limiter.lock().unwrap().try_acquire(&parsed.model) {
            let mut resp = Response::json(
                429,
                &ErrorResponse::bad_request(
                    format!("请求过于频繁，请 {retry}s 后重试"),
                    None,
                ),
            );
            resp.headers
                .push(("Retry-After".to_string(), retry.to_string()));
            return resp;
        }
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
        // P2 速率限制（2026-11-21）：按模型分桶 token bucket，防成本型 DoS。
        if let Err(retry) = self.rate_limiter.lock().unwrap().try_acquire(&parsed.model) {
            let mut resp = Response::json(
                429,
                &ErrorResponse::bad_request(
                    format!("请求过于频繁，请 {retry}s 后重试"),
                    None,
                ),
            );
            resp.headers
                .push(("Retry-After".to_string(), retry.to_string()));
            return resp;
        }
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
        // P1 熔断/限流（2026-11-19）：熔断打开时直接拒绝（503 + Retry-After），
        // 避免异常流量打爆远端 llama-server；5xx/429 重试最多 2 次（退避）。
        {
            let guard = self.upstream_guard.lock().unwrap();
            if let Some(retry_after) = guard.retry_after() {
                let payload = mangosteen_core::openai::ErrorResponse::bad_request(
                    format!("上游推理服务暂时不可用（熔断中），请 {retry_after}s 后重试"),
                    None,
                );
                let mut resp = Response::json(503, &payload);
                resp.headers
                    .push(("Retry-After".to_string(), retry_after.to_string()));
                return resp;
            }
        }

        let mut attempt = 0u32;
        loop {
            let r = http_post_json_with_key_tls(
                &url,
                body,
                self.upstream_timeout,
                target.api_key.as_deref(),
                target.tls,
                target.tls_cert_path.as_deref().map(std::path::Path::new),
            );
            match r {
                // 503（模型加载期）/429（限流）：退避重试，最多 2 次
                Ok((status, resp_body)) if status == 503 || status == 429 => {
                    if attempt < 2 {
                        attempt += 1;
                        self.upstream_guard.lock().unwrap().on_failure();
                        std::thread::sleep(std::time::Duration::from_millis(
                            500 * attempt as u64,
                        ));
                        continue;
                    }
                    // 重试用尽：503 + Retry-After（模型加载估算 30s；429 取 5s）
                    self.upstream_guard.lock().unwrap().on_failure();
                    let ra = if status == 503 { 30u64 } else { 5u64 };
                    let payload = mangosteen_core::openai::ErrorResponse::bad_request(
                        format!("上游推理服务暂时不可用（HTTP {status}），请稍后重试"),
                        None,
                    );
                    let mut resp = Response::json(503, &payload);
                    resp.headers.push(("Retry-After".to_string(), ra.to_string()));
                    return resp;
                }
                Ok((status, resp_body)) => {
                    self.upstream_guard.lock().unwrap().on_success();
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
                    return Response {
                        status,
                        content_type: content_type.to_string(),
                        body: body.into_bytes(),
                        headers: Vec::new(),
                    };
                }
                Err(e) => return Response::error(&e),
            }
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

        // P2 速率限制（2026-11-21）：Anthropic 兼容接口也接入 token bucket。
        if let Err(retry) = self.rate_limiter.lock().unwrap().try_acquire(&parsed.model) {
            let mut resp = Response::json(
                429,
                &mangosteen_core::anthropic::AnthropicErrorResponse::bad_request(format!(
                    "请求过于频繁，请 {retry}s 后重试"
                )),
            );
            resp.headers
                .push(("Retry-After".to_string(), retry.to_string()));
            return resp;
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
                // 证书 CN=llama-server（引导链自签），tls 时 URL host 必须
                // 与之匹配才能过 curl hostname 校验（upstream/relay 已加
                // --resolve llama-server:port:127.0.0.1 映射回环）。
                host: if record.plan.cloud.as_ref().map(|c| c.tls).unwrap_or(false) {
                    "llama-server".to_string()
                } else {
                    "127.0.0.1".to_string()
                },
                port,
                model_id,
                origin_label: record.plan.origin.label(),
                api_key: record.plan.cloud.as_ref().and_then(|c| c.api_key.clone()),
                tls: record.plan.cloud.as_ref().map(|c| c.tls).unwrap_or(false),
                tls_cert_path: record
                    .plan
                    .origin
                    .instance_id()
                    .map(|id| {
                        let p = mangosteen_platform::paths::config_dir(
                            &mangosteen_platform::paths::real_env,
                        )
                        .join("tls")
                        .join(format!("{id}.crt"));
                        p.is_file()
                            .then(|| p.to_string_lossy().to_string())
                    })
                    .flatten(),
            },
        );
    }
}
