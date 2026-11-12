//! HTTP 路由分发与鉴权。
//!
//! 归属：**第二层 · API Server**。路由只负责把请求映射到职责化 handler，
//! 不承载部署、模型或推理业务。

use super::*;
use mangosteen_core::openai::ErrorResponse;
use mangosteen_core::MnError;

impl GatewayServer {
    /// 路由分发。
    pub fn route(&self, req: &ParsedRequest) -> Response {
        // 鉴权（可选）
        if self.require_key && !self.authorized(req) {
            let e = MnError::AccessDenied {
                path: std::path::PathBuf::from(&req.path),
                reason: "缺少或错误的 API Key（Authorization: Bearer <key>）".to_string(),
            };
            return Response::json(401, &ErrorResponse::from_mn(&e));
        }

        match (req.method.as_str(), req.path.as_str()) {
            // 内置 Web 控制台。挂在网关同一端口：同源即可调 /api 与 /v1，
            // 不需要 CORS，也不需要额外的静态文件服务。
            ("GET", "/") | ("GET", "/index.html") => Response {
                status: 200,
                content_type: "text/html; charset=utf-8".to_string(),
                body: mangosteen_webui::INDEX_HTML.as_bytes().to_vec(),
            },
            ("GET", "/health") => self.health(),
            ("GET", "/v1/models") => self.models(),
            ("POST", "/v1/chat/completions") => self.chat(req),
            ("POST", "/v1/completions") => self.completion(req),
            ("POST", "/v1/embeddings") => self.embeddings(req),
            // Anthropic 兼容接口
            ("POST", "/v1/messages") => self.anthropic_messages(req),
            ("GET", "/api/status") => self.status(),
            ("POST", "/api/deploy") => self.deploy(req),
            ("GET", "/api/deployments") => self.deployments(),
            ("POST", "/api/assess") => self.assess(req),
            ("POST", "/api/preview") => self.preview(req),
            ("GET", "/api/sysinfo") => self.sysinfo(),
            ("GET", "/api/settings") => self.get_settings(),
            ("PUT", "/api/settings") => self.put_settings(req),
            // 模型配置模板：设置页是唯一维护入口，部署页据此填充下拉框
            ("GET", "/api/model-templates") => self.list_model_templates(),
            ("PUT", "/api/model-templates") => self.put_model_template(req),
            ("DELETE", p) if p.starts_with("/api/model-templates/") => {
                // /api/model-templates/{model}/{slot}
                let rest = &p["/api/model-templates/".len()..];
                match rest.split_once('/') {
                    Some((m, s)) => self.delete_model_template(m, s),
                    None => Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            "缺少模板名，格式：/api/model-templates/{模型名}/{模板名}",
                            None,
                        ),
                    ),
                }
            }
            ("GET", "/api/devices") => self.get_devices(),
            ("POST", "/api/devices") => self.add_device(req),
            // ---- 设置页「云端节点」管理：实例列表 / 关机 / 开机 / 删除 ----
            //
            // 与 /api/devices（用户自登记的 SSH 机器）完全独立：这里是
            // 云厂商 API 托管的实例，动作直接走厂商驱动。
            ("GET", "/api/cloud/instances") => self.cloud_instances_list(),
            ("POST", p) if p.starts_with("/api/cloud/instances/") && p.ends_with("/power-off") => {
                let rest = &p["/api/cloud/instances/".len()..p.len() - "/power-off".len()];
                match rest.split_once('/') {
                    Some((prov, id)) => self.cloud_instance_power_off(prov, id),
                    None => Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            "路径格式：/api/cloud/instances/{provider}/{instance_id}/power-off",
                            None,
                        ),
                    ),
                }
            }
            ("POST", p) if p.starts_with("/api/cloud/instances/") && p.ends_with("/power-on") => {
                let rest = &p["/api/cloud/instances/".len()..p.len() - "/power-on".len()];
                match rest.split_once('/') {
                    Some((prov, id)) => self.cloud_instance_power_on(prov, id),
                    None => Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            "路径格式：/api/cloud/instances/{provider}/{instance_id}/power-on",
                            None,
                        ),
                    ),
                }
            }
            ("DELETE", p) if p.starts_with("/api/cloud/instances/") => {
                let rest = &p["/api/cloud/instances/".len()..];
                match rest.split_once('/') {
                    Some((prov, id)) => self.cloud_instance_destroy(prov, id),
                    None => Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            "路径格式：/api/cloud/instances/{provider}/{instance_id}",
                            None,
                        ),
                    ),
                }
            }
            ("POST", p) if p.starts_with("/api/devices/") && p.ends_with("/test") => {
                let id = &p["/api/devices/".len()..p.len() - "/test".len()];
                self.test_device(id)
            }
            ("DELETE", p) if p.starts_with("/api/devices/") => {
                self.remove_device(&p["/api/devices/".len()..])
            }
            ("POST", "/api/extract") => self.extract_attachment(req),
            ("POST", "/api/fetch-url") => self.fetch_url(req),
            ("POST", "/api/models/search") => self.search_models(req),
            ("POST", "/api/models/pull") => self.pull_model(req),
            ("GET", "/api/models") => self.list_models(),
            ("GET", "/api/models/jobs") => self.list_pull_jobs(req),
            ("DELETE", "/api/models/jobs") => {
                let n = self.pull_jobs.clear_finished();
                Response::json(200, &serde_json::json!({ "ok": true, "cleared": n }))
            }
            ("POST", p) if p.starts_with("/api/models/jobs/") && p.ends_with("/cancel") => {
                let id = &p["/api/models/jobs/".len()..p.len() - "/cancel".len()];
                self.cancel_pull_job(id)
            }
            ("DELETE", p) if p.starts_with("/api/models/") => {
                self.remove_model(&p["/api/models/".len()..], req)
            }
            // ---- 第一层·用户会话管理 ----
            //
            // 会话是「最上层交互入口」的状态载体：推理对话的上下文、
            // 部署任务的操作记录。放在网关侧而非浏览器内存，
            // 是为了跨进程可见（关掉 UI 再开，历史还在）。
            ("GET", "/api/sessions") => self.list_sessions(req),
            ("POST", "/api/sessions") => self.create_session(req),
            ("POST", p) if p.starts_with("/api/sessions/") && p.ends_with("/messages") => {
                let id = &p["/api/sessions/".len()..p.len() - "/messages".len()];
                self.append_session_message(id, req)
            }
            ("POST", p) if p.starts_with("/api/sessions/") && p.ends_with("/operations") => {
                let id = &p["/api/sessions/".len()..p.len() - "/operations".len()];
                self.record_session_operation(id, req)
            }
            ("DELETE", p) if p.starts_with("/api/sessions/") => {
                self.delete_session(&p["/api/sessions/".len()..])
            }
            ("GET", p) if p.starts_with("/api/sessions/") => {
                self.get_session(&p["/api/sessions/".len()..])
            }
            ("GET", "/api/doctor") => self.doctor(),
            // 生命周期：停止服务（保留实例，关机停计费）／启动服务（开机并拉起）
            //
            // 必须排在 `DELETE /api/deployments/{id}` 之前不是语法要求，
            // 而是可读性要求：这里三条路由语义相近，放在一起便于对照。
            ("POST", p) if p.starts_with("/api/deployments/") && p.ends_with("/stop-service") => {
                let id = &p["/api/deployments/".len()..p.len() - "/stop-service".len()];
                self.stop_service(id)
            }
            ("POST", p) if p.starts_with("/api/deployments/") && p.ends_with("/start-service") => {
                let id = &p["/api/deployments/".len()..p.len() - "/start-service".len()];
                self.start_service(id)
            }
            ("POST", p) if p.starts_with("/api/deployments/") && p.ends_with("/unload") => {
                let id = &p["/api/deployments/".len()..p.len() - "/unload".len()];
                self.unload(id)
            }
            // 兼容：前端/旧版本可能用 DELETE 语义表达「删除部署」，但把
            // 动作后缀（/unload）也带上了。DELETE /api/deployments/{id}/unload
            // 必须路由到 unload，而不是掉进 DELETE catch-all 把 "id/unload"
            // 整段当部署 ID 查台账（报「部署 xxx/unload 不存在」或 404）。
            ("DELETE", p) if p.starts_with("/api/deployments/") && p.ends_with("/unload") => {
                let id = &p["/api/deployments/".len()..p.len() - "/unload".len()];
                self.unload(id)
            }
            ("DELETE", p) if p.starts_with("/api/deployments/") => {
                self.stop(&p["/api/deployments/".len()..])
            }
            ("GET", p)
                if p.starts_with("/api/deployments/") && p.ends_with("/ssh-info") =>
            {
                let id = &p["/api/deployments/".len()..p.len() - "/ssh-info".len()];
                self.ssh_info(id)
            }
            ("GET", p) if p.starts_with("/api/deployments/") => {
                self.deployment(&p["/api/deployments/".len()..])
            }
            (m, p) => {
                let mut r = Response::not_found(p);
                if p.starts_with("/api/") || p.starts_with("/v1/") {
                    r = Response::text(405, format!("方法 {m} 不被 {p} 支持"));
                }
                r
            }
        }
    }
}
