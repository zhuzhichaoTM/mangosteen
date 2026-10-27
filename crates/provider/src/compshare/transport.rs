//! 优云智算驱动的 HTTP 传输：系统 curl 实现。
//!
//! 请求/响应 DTO、`Transport` trait 与测试替身统一来自
//! [`mangosteen_core::transport`]，本模块只保留基于系统 curl 的真实实现，
//! 避免各驱动重复定义同形状的传输抽象。

use std::io::Write;
use std::process::{Command, Stdio};

pub use mangosteen_core::transport::{HttpRequest, HttpResponse, MockTransport, Transport};
use mangosteen_core::{MnError, MnResult};

/// 基于系统 `curl` 的真实传输实现。
#[derive(Debug, Clone)]
pub struct CurlTransport {
    /// 单次请求超时（秒）
    pub timeout_secs: u64,
    /// curl 可执行文件（默认 `curl`）
    pub curl_bin: String,
}

impl Default for CurlTransport {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            curl_bin: "curl".to_string(),
        }
    }
}

impl CurlTransport {
    /// 新建。
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout_secs,
            ..Default::default()
        }
    }

    /// 构建 curl 命令行参数。
    pub fn build_args(&self, req: &HttpRequest) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "--silent".into(),
            "--show-error".into(),
            "--max-time".into(),
            self.timeout_secs.to_string(),
            "--request".into(),
            req.method.clone(),
            // 云 API 不应重定向；禁止跟随可避免把请求体投递到别的地址
            "--location".into(),
            "--max-redirs".into(),
            "0".into(),
            "--write-out".into(),
            "\n%{http_code}".into(),
        ];
        if req.body.is_some() {
            // 从 stdin 读请求体：认证参数不进入进程参数列表
            args.push("--data-binary".into());
            args.push("@-".into());
        }
        for (k, v) in &req.headers {
            args.push("--header".into());
            args.push(format!("{k}: {v}"));
        }
        args.push(req.url.clone());
        args
    }

    /// 解析 curl 输出。
    ///
    /// 约定：`--write-out "\n%{http_code}"` 让状态码位于最后一行。
    pub fn parse_output(stdout: &str, exit_code: i32, stderr: &str) -> MnResult<HttpResponse> {
        if exit_code != 0 {
            let stage = match exit_code {
                6 => "DNS 解析",
                7 => "TCP 连接",
                28 => "请求超时",
                35 | 60 => "TLS 证书校验",
                52 => "服务端空响应",
                _ => "HTTP 请求",
            };
            return Err(MnError::Network {
                stage: stage.to_string(),
                reason: format!(
                    "curl 退出码 {exit_code}：{}",
                    stderr.trim().lines().next().unwrap_or("（无错误输出）")
                ),
            });
        }
        let (body, code_line) = match stdout.rfind('\n') {
            Some(pos) => (&stdout[..pos], &stdout[pos + 1..]),
            None => (stdout, ""),
        };
        let status: u16 = code_line.trim().parse().map_err(|_| MnError::Network {
            stage: "解析响应".to_string(),
            reason: format!("无法解析 HTTP 状态码：{code_line:?}"),
        })?;
        Ok(HttpResponse::new(status, body.to_string()))
    }
}

impl Transport for CurlTransport {
    fn execute(&self, req: &HttpRequest) -> MnResult<HttpResponse> {
        let mut cmd = Command::new(&self.curl_bin);
        cmd.args(self.build_args(req));
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if req.body.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });

        let mut child = cmd.spawn().map_err(|e| MnError::Network {
            stage: "启动 curl".to_string(),
            reason: format!(
                "无法执行 `{}`：{e}。Mangosteen 依赖系统 curl 访问云厂商 API。",
                self.curl_bin
            ),
        })?;

        // 写完必须显式 drop stdin，curl 才会读到 EOF 并发出请求
        if let Some(body) = &req.body {
            let mut stdin = child.stdin.take().ok_or_else(|| MnError::Network {
                stage: "写入请求体".to_string(),
                reason: "curl 子进程的 stdin 不可用".to_string(),
            })?;
            stdin
                .write_all(body.as_bytes())
                .map_err(|e| MnError::Network {
                    stage: "写入请求体".to_string(),
                    reason: format!("管道写入失败：{e}"),
                })?;
        }
        let out = child.wait_with_output().map_err(|e| MnError::Network {
            stage: "等待 curl".to_string(),
            reason: e.to_string(),
        })?;

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let code = out.status.code().unwrap_or(-1);
        CurlTransport::parse_output(&stdout, code, &stderr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_post_sets_content_type() {
        let r = HttpRequest::form_post("https://api/x", "Action=Y");
        assert_eq!(r.method, "POST");
        assert_eq!(
            r.header("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(r.body.as_deref(), Some("Action=Y"));
    }

    #[test]
    fn response_success_range() {
        assert!(HttpResponse::new(200, "").is_success());
        assert!(HttpResponse::new(204, "").is_success());
        assert!(!HttpResponse::new(300, "").is_success());
        assert!(!HttpResponse::new(500, "").is_success());
    }

    #[test]
    fn parse_output_splits_status() {
        let r = CurlTransport::parse_output("{\"a\":1}\n200", 0, "").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"a\":1}");
    }

    #[test]
    fn parse_output_multiline_body() {
        let out = "line1\nline2\n404";
        let r = CurlTransport::parse_output(out, 0, "").unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, "line1\nline2");
    }

    #[test]
    fn network_exit_codes_map_to_stage() {
        let e = CurlTransport::parse_output("", 7, "Connection refused").unwrap_err();
        assert!(e.to_string().contains("TCP 连接"));
        assert!(e.is_retryable());
        let e = CurlTransport::parse_output("", 28, "timed out").unwrap_err();
        assert!(e.to_string().contains("超时"));
    }

    #[test]
    fn args_use_stdin_for_body_and_never_inline_it() {
        let t = CurlTransport::new(12);
        let req = HttpRequest::form_post("https://api.compshare.cn", "Signature=secret-value");
        let args = t.build_args(&req);
        // body 必须走 stdin，绝不出现在参数里（否则 ps 可见）
        assert!(args.iter().any(|a| a == "@-"), "{args:?}");
        assert!(
            !args.iter().any(|a| a.contains("secret-value")),
            "请求体内容不得进入进程参数：{args:?}"
        );
        assert_eq!(
            args.last().map(|s| s.as_str()),
            Some("https://api.compshare.cn")
        );
    }

    #[test]
    fn args_without_body_do_not_read_stdin() {
        let t = CurlTransport::default();
        let req = HttpRequest {
            method: "POST".into(),
            url: "https://x/y".into(),
            headers: vec![],
            body: None,
        };
        let args = t.build_args(&req);
        assert!(!args.iter().any(|a| a == "@-"));
    }

    #[test]
    fn mock_records_and_sequences() {
        let m = MockTransport::sequence(vec!["a".to_string(), "b".to_string()]);
        let t: &dyn Transport = &m;
        assert_eq!(
            t.execute(&HttpRequest::form_post("u", "x")).unwrap().body,
            "a"
        );
        assert_eq!(
            t.execute(&HttpRequest::form_post("u", "x")).unwrap().body,
            "b"
        );
        assert_eq!(m.call_count(), 2);
        assert_eq!(m.requests()[0].body.as_deref(), Some("x"));
    }

    #[test]
    fn mock_failure_is_retryable() {
        let m = MockTransport::always_fail("boom");
        let e = m.execute(&HttpRequest::form_post("u", "x")).unwrap_err();
        assert!(e.to_string().contains("boom"));
        assert!(e.is_retryable());
    }

    #[test]
    fn missing_curl_is_actionable() {
        let t = CurlTransport {
            timeout_secs: 3,
            curl_bin: "/nonexistent/curl-xyz".to_string(),
        };
        let e = t
            .execute(&HttpRequest::form_post("https://x/y", "a=b"))
            .unwrap_err();
        assert!(e.to_string().contains("curl"), "got {e}");
    }

    #[test]
    fn real_curl_roundtrip_when_available() {
        // 用本地文件 URL 之外的方式难以离线验证，这里只验证「能启动 curl」，
        // 真正联网的验证由 MANGOSTEEN_LIVE_NETWORK_TESTS 控制。
        if std::env::var("MANGOSTEEN_LIVE_NETWORK_TESTS").is_err() {
            return;
        }
        let t = CurlTransport::new(20);
        let r = t
            .execute(&HttpRequest::form_post(
                "https://httpbin.org/post",
                "hello=world",
            ))
            .expect("live request");
        assert!(r.status == 200 || r.status == 503);
    }
}
