//! HTTP 路由分发与鉴权。
//!
//! 归属：**第二层 · API Server**。路由只负责把请求映射到职责化 handler，
//! 不承载部署、模型或推理业务。

use super::*;
use mangosteen_core::openai::ErrorResponse;
use mangosteen_core::MnError;

/// 恒定时间密钥比较（防时序侧信道）：比较长度与内容均不提前退出。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let ba = a.as_bytes();
    let bb = b.as_bytes();
    let mut diff: u8 = (ba.len() ^ bb.len()) as u8;
    for i in 0..ba.len().max(bb.len()) {
        diff |= ba.get(i).copied().unwrap_or(0) ^ bb.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// UI 登录页（2026-11-19 用户要求）：开启鉴权且未登录时 GET / 返回。
/// 输入 API Key → POST /api/auth/login → 成功写 HttpOnly Cookie 后刷新。
/// 样式内联（CSP 已放宽），无外部依赖。
const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh">
<head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Mangosteen 登录</title>
<meta http-equiv="Content-Security-Policy" content="default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'">
<style>
  body { font-family: -apple-system, sans-serif; max-width: 340px; margin: 15vh auto; padding: 0 20px; color: #333; }
  h2 { text-align: center; font-weight: 600; }
  input { width: 100%; box-sizing: border-box; padding: 10px; font-size: 14px; border: 1px solid #ddd; border-radius: 6px; }
  button { width: 100%; padding: 10px; margin-top: 12px; font-size: 14px; background: #1a73e8; color: #fff; border: 0; border-radius: 6px; cursor: pointer; }
  button:hover { background: #1765cc; }
  #err { color: #d93025; font-size: 13px; text-align: center; min-height: 20px; margin-top: 10px; }
  .hint { font-size: 12px; color: #888; text-align: center; margin-top: 16px; }
  code { background: #f1f3f4; padding: 1px 4px; border-radius: 3px; }
</style></head>
<body>
<h2>Mangosteen 控制台</h2>
<form id="f" autocomplete="off">
  <input id="k" type="password" placeholder="网关 API Key" autofocus>
  <button type="submit">登 录</button>
</form>
<div id="err"></div>
<div class="hint">API Key 查看：<code>mangosteen status</code> 或配置文件 gateway.key</div>
<script>
document.getElementById('f').addEventListener('submit', async function (ev) {
  ev.preventDefault();
  var err = document.getElementById('err');
  err.textContent = '';
  try {
    var r = await fetch('/api/auth/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ api_key: document.getElementById('k').value })
    });
    if (r.ok) {
      try { localStorage.setItem("gw_token", document.getElementById("k").value); } catch (e) {}
      location.reload();
    }
    else { err.textContent = 'API Key 错误，请重试（' + r.status + '）'; }
  } catch (e) { err.textContent = '网络错误：' + e; }
});
</script>
</body></html>"#;

impl GatewayServer {
    /// 检查请求是否携带有效 UI 会话 Cookie（HttpOnly ui_session）。
    fn check_ui_session(&self, req: &ParsedRequest) -> bool {
        if !self.require_key {
            return false;
        }
        let cookie = req.header("cookie").unwrap_or("");
        for part in cookie.split(';') {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("ui_session=") {
                return constant_time_eq(v, &self.ui_session_token);
            }
        }
        false
    }

    /// POST /api/auth/login — Body: {api_key} → 校验后写 HttpOnly Cookie。
    /// 登录失败返回 401（不回显 key 内容）。
    fn api_auth_login(&self, req: &ParsedRequest) -> Response {
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let key = body.get("api_key").and_then(|v| v.as_str()).unwrap_or("");
        if key.is_empty() || !constant_time_eq(key, &self.api_key) {
            let e = MnError::AccessDenied {
                path: std::path::PathBuf::from("/api/auth/login"),
                reason: "API Key 错误".to_string(),
            };
            return Response::json(401, &ErrorResponse::from_mn(&e));
        }
        let mut resp = Response::json(200, &serde_json::json!({ "ok": true }));
        resp.headers.push((
            "Set-Cookie".to_string(),
            format!(
                "ui_session={}; HttpOnly; SameSite=Lax; Path=/; Max-Age=86400",
                self.ui_session_token
            ),
        ));
        resp
    }

    /// GET /api/auth/session — 返回当前会话登录态（供前端判断）。
    fn api_auth_session(&self, authed: bool) -> Response {
        Response::json(
            200,
            &serde_json::json!({ "authenticated": authed && self.require_key }),
        )
    }

    /// 路由分发。
    pub fn route(&self, req: &ParsedRequest) -> Response {
        // 鉴权（可选）。豁免：`/`（页面本体，无数据）、`/index.html`、
        // `/health`（探活）——数据与操作全部在 /v1/* 与 /api/* 后面，
        // 页面 JS 调 API 时带 token（URL ?token= 或 localStorage）。
        //
        // UI 会话中间件（2026-11-19 用户要求）：Web UI 登录后凭
        // HttpOnly Cookie `ui_session` 访问 /api/* 与 /v1/*；未登录时
        // GET / 返回登录页（输入 API Key），登录成功写 Cookie 回主页。
        // 登录端点 /api/auth/login 与 /api/auth/session 免鉴权（否则
        // 鸡生蛋）。Authorization: Bearer <api_key> 与会话 Cookie 任一
        // 有效即放行（API 客户端仍可用 Bearer）。
        let has_valid_session = self.check_ui_session(req);

        if matches!((req.method.as_str(), req.path.as_str()), ("POST", "/api/auth/login")) {
            return self.api_auth_login(req);
        }
        if matches!((req.method.as_str(), req.path.as_str()), ("GET", "/api/auth/session")) {
            return self.api_auth_session(has_valid_session);
        }

        // 页面本体：开启鉴权且无有效会话 → 返回登录页（200，含登录表单）
        if matches!(req.path.as_str(), "/" | "/index.html") {
            if self.require_key && !has_valid_session {
                return Response {
                    status: 200,
                    content_type: "text/html; charset=utf-8".to_string(),
                    body: LOGIN_HTML.as_bytes().to_vec(),
                    // 登录页需要内联 script——放宽 CSP（仅本页）
                    headers: vec![(
                        "Content-Security-Policy".to_string(),
                        "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'".to_string(),
                    )],
                };
            }
            return Response {
                status: 200,
                content_type: "text/html; charset=utf-8".to_string(),
                body: mangosteen_webui::INDEX_HTML.as_bytes().to_vec(),
                // P1-3 修复（2026-11-20 实测）：控制台页面有 67KB 内联
                // <script>（UI 全部逻辑），to_bytes 全局严格 CSP
                //（script-src 'self'）会拦截内联 JS——页面显示但所有
                // 按钮/轮询/模型加载全部失效（表现为模型下拉永远
                // “加载中”）。与登录页同款：自定义宽松 CSP 覆盖全局。
                headers: vec![(
                    "Content-Security-Policy".to_string(),
                    "default-src 'self'; script-src 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'".to_string(),
                )],
            };
        }

        // 数据/操作接口：Bearer key 或会话 Cookie 任一有效即放行
        if self.require_key
            && !self.authorized(req)
            && !has_valid_session
            && req.path.as_str() != "/health"
        {
            let e = MnError::AccessDenied {
                path: std::path::PathBuf::from(&req.path),
                reason: "缺少或错误的 API Key（Authorization: Bearer <key> 或登录后访问）".to_string(),
            };
            return Response::json(401, &ErrorResponse::from_mn(&e));
        }

        match (req.method.as_str(), req.path.as_str()) {
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
            // CLI 部署完成后通知网关节扭重建并持有（CLI 退出后隧道依然有效）
            ("POST", p)
                if p.starts_with("/api/deployments/") && p.ends_with("/restore-tunnel") =>
            {
                let id = &p["/api/deployments/".len()..p.len() - "/restore-tunnel".len()];
                self.restore_tunnel(id)
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
