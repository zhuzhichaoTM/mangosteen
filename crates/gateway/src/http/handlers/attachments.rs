//! 附件与网页抓取处理器（`/api/extract`、`/api/fetch-url`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;

impl GatewayServer {
    pub(crate) fn extract_attachment(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };

        let max_chars = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            // 字符上限按模型上下文折算：一半留给对话历史与输出
            ((gw.config().local.default_context as usize) * 2)
                .min(mangosteen_base::extract::MAX_TEXT_CHARS)
        };

        // ---- 方式一：内容上传（UI）----
        if let Some(b64) = parsed.get("content_base64").and_then(|v| v.as_str()) {
            let filename = parsed
                .get("filename")
                .and_then(|v| v.as_str())
                .unwrap_or("attachment.txt");

            let bytes = match base64_decode(b64) {
                Ok(b) => b,
                Err(e) => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("content_base64 解码失败：{e}"),
                            Some("content_base64"),
                        ),
                    )
                }
            };

            return match mangosteen_base::extract::extract_bytes(filename, &bytes, max_chars) {
                Ok(r) => Response::json(
                    200,
                    &serde_json::json!({
                        "filename": filename,
                        "text": r.text,
                        "chars": r.text.chars().count(),
                        "original_bytes": r.original_bytes,
                        "method": r.method,
                        "truncated": r.truncated,
                        "warnings": r.warnings,
                    }),
                ),
                Err(e) => Response::error(&e),
            };
        }

        // ---- 方式二：路径引用（CLI，受访问控制约束）----
        let Some(path) = parsed.get("path").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "需要 `content_base64` + `filename`（上传）或 `path`（引用本地文件）",
                    None,
                ),
            );
        };

        let guard = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            mangosteen_base::AccessGuard::from_config(&gw.config().storage)
        };

        // 访问控制：越权直接拒绝
        let allowed = match guard.check_read(std::path::Path::new(path)) {
            Ok(p) => p,
            Err(e) => return Response::error(&e),
        };

        match mangosteen_base::extract::extract(&allowed, max_chars) {
            Ok(r) => Response::json(
                200,
                &serde_json::json!({
                    "path": allowed.display().to_string(),
                    "text": r.text,
                    "chars": r.text.chars().count(),
                    "original_bytes": r.original_bytes,
                    "method": r.method,
                    "truncated": r.truncated,
                    "warnings": r.warnings,
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }
    /// 抓取 URL 正文（联网搜索的兜底路径）。
    pub(crate) fn fetch_url(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(url) = parsed.get("url").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 url 字段", Some("url")),
            );
        };

        let mut opts = mangosteen_base::webfetch::FetchOptions::default();
        if let Some(n) = parsed.get("max_chars").and_then(|v| v.as_u64()) {
            opts.max_chars =
                (n as usize).min(mangosteen_base::webfetch::FetchOptions::default().max_chars);
        }

        match mangosteen_base::webfetch::fetch(url, &opts) {
            Ok(p) => Response::json(
                200,
                &serde_json::json!({
                    "url": p.final_url,
                    "title": p.title,
                    "text": p.text,
                    "chars": p.text.chars().count(),
                    "status": p.status,
                    "content_type": p.content_type,
                    "bytes": p.bytes,
                    "truncated": p.truncated,
                    "warnings": p.warnings,
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }
}
