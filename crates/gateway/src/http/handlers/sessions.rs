//! Session HTTP handlers（第一层·用户会话管理的 HTTP 入口）。
//!
//! 仍以实现于 `GatewayServer` 的 impl 块存在，保持与私有状态同模块的访问约束；
//! 通过 `pub(crate)` 字段与共享响应类型与其他 handler 组隔离。

use mangosteen_core::openai::ErrorResponse;

use crate::http::server::{GatewayServer, ParsedRequest, Response};

impl GatewayServer {
    pub(crate) fn list_sessions(&self, req: &ParsedRequest) -> Response {
        let kind = req
            .query_param("kind")
            .and_then(|k| mangosteen_session::SessionKind::parse(&k));
        let res = match kind {
            Some(k) => self.sessions.list_by_kind(k),
            None => self.sessions.list(),
        };
        match res {
            Ok(list) => Response::json(200, &serde_json::json!({ "sessions": list })),
            Err(e) => Response::error(&e),
        }
    }

    /// 新建会话。`POST /api/sessions`
    ///
    /// 请求体：`{ "kind": "chat"|"deploy", "title": "...", "model": "..." }`
    /// 三者都可省略——`kind` 默认 `chat`，`title` 留空则由首条用户消息自动生成。
    pub(crate) fn create_session(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let kind = match parsed.get("kind").and_then(|v| v.as_str()) {
            Some(s) => match mangosteen_session::SessionKind::parse(s) {
                Some(k) => k,
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知会话种类 `{s}`，可选 chat / deploy"),
                            Some("kind"),
                        ),
                    )
                }
            },
            None => mangosteen_session::SessionKind::Chat,
        };
        let title = parsed
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let model = parsed
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match self.sessions.create(kind, title, model) {
            Ok(s) => Response::json(201, &serde_json::json!({ "session": s })),
            Err(e) => Response::error(&e),
        }
    }

    /// 读取会话详情（含消息与操作记录）。`GET /api/sessions/{id}`
    pub(crate) fn get_session(&self, id: &str) -> Response {
        match self.sessions.get(id) {
            Ok(Some(s)) => Response::json(200, &serde_json::json!({ "session": s })),
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 删除会话。`DELETE /api/sessions/{id}`
    pub(crate) fn delete_session(&self, id: &str) -> Response {
        match self.sessions.delete(id) {
            Ok(true) => Response::json(200, &serde_json::json!({ "ok": true, "id": id })),
            Ok(false) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 追加一条消息。`POST /api/sessions/{id}/messages`
    ///
    /// 请求体：`{ "role": "user"|"assistant"|"system", "content": "..." }`
    pub(crate) fn append_session_message(&self, id: &str, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(content) = parsed.get("content").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 content 字段", Some("content")),
            );
        };
        let role = parsed
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user");
        if !matches!(role, "user" | "assistant" | "system") {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    format!("未知角色 `{role}`，可选 user / assistant / system"),
                    Some("role"),
                ),
            );
        }
        match self.sessions.append_message(id, role, content) {
            Ok(Some(s)) => Response::json(200, &serde_json::json!({ "session": s })),
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }

    /// 记录一次操作。`POST /api/sessions/{id}/operations`
    ///
    /// 请求体：`{ "action": "deploy", "detail": "...", "ok": true }`
    ///
    /// 失败的操作也要记（`ok: false`）——操作记录的价值就在于
    /// 「当时到底发生了什么」，只记成功等于自欺。
    pub(crate) fn record_session_operation(&self, id: &str, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(action) = parsed.get("action").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 action 字段", Some("action")),
            );
        };
        let detail = parsed
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ok = parsed.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
        match self.sessions.record_operation(id, action, &detail, ok) {
            Ok(Some(s)) => Response::json(200, &serde_json::json!({ "session": s })),
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }
}
