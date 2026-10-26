//! 基于系统 `curl` 的真实 HTTP 传输实现。
//!
//! 归属：**算力执行层 · 多云统一适配层**（驱动的生产传输后端）。
//!
//! ## 为什么用系统 curl 而不是引入 HTTP 客户端库
//!
//! 按「禁止无依据扩展」原则，这里没有引入 reqwest / ureq：
//!
//! | 需求 | curl 是否满足 |
//! |------|--------------|
//! | HTTPS（云厂商 API 必需） | 自带 TLS |
//! | 三平台可用 | macOS/Linux 自带；Windows 10 1803+ 自带 `curl.exe` |
//! | 超时 | `--max-time` |
//! | 自定义头 | `--header` / `--config` |
//! | 拿状态码 | `--write-out "%{http_code}"` |
//! | 无新增供应链 | 是 |
//!
//! 云端驱动是**低频**操作（分钟级），进程启动开销（~10ms）可忽略。
//!
//! ## 安全要点
//!
//! 认证头若用 `-H` 传入，会出现在进程参数列表（同机其它用户可见）。
//! 因此优先把请求头写进**权限 0600 的临时配置文件**，用 `--config` 传给 curl，
//! 用完立即删除。

use std::io::Write;
use std::process::{Command, Stdio};

use mangosteen_core::{MnError, MnResult};

use super::transport::{HttpRequest, HttpResponse, Transport};

/// 基于 curl 的传输实现。
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

    /// 构建 curl 命令行参数（不含 `--config`）。
    pub fn build_args(&self, req: &HttpRequest) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "--silent".into(),
            "--show-error".into(),
            "--max-time".into(),
            self.timeout_secs.to_string(),
            "--request".into(),
            req.method.clone(),
            // 不跟随重定向：云 API 不该重定向，跟随可能把认证头带到别的域
            "--location".into(),
            "--max-redirs".into(),
            "0".into(),
            "--write-out".into(),
            "\n%{http_code}".into(),
        ];
        if let Some(body) = &req.body {
            args.push("--data-binary".into());
            args.push(body.clone());
        }
        args.push(req.url.clone());
        args
    }

    /// 解析 curl 输出。
    ///
    /// 约定：`--write-out "\n%{http_code}"` 使状态码位于输出的最后一行。
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
        // 请求头写进私有临时配置，避免出现在进程参数列表
        let mut config = String::new();
        for (k, v) in &req.headers {
            config.push_str(&format!(
                "header = \"{}: {}\"\n",
                escape_curl_config(k),
                escape_curl_config(v)
            ));
        }
        // ⚠ URL **只**通过 argv 传给 curl（见 `build_args` 的最后一项）。
        //
        // 这里曾经也写进 config，导致 curl 收到两个 URL、把同一请求发了
        // **两次**：响应体变成 `{json}\n200{json}\n200`，调用方解析时报
        // `trailing characters at line 2`。对于 POST（创建实例）尤其危险——
        // 重复提交可能创建出两台实例并持续计费。
        //
        // 该缺陷长期未被发现，是因为此前只有 GET 类查询走过这条桥接路径，
        // 而它们的重复响应看起来「无害」。修 CompShare 监控接入时才暴露。
        let tmp = TempConfig::create(&config)?;

        let mut cmd = Command::new(&self.curl_bin);
        cmd.args(self.build_args(req));
        cmd.arg("--config").arg(tmp.path());
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let out = cmd.output().map_err(|e| MnError::Network {
            stage: "启动 curl".to_string(),
            reason: format!(
                "无法执行 `{}`：{e}。Mangosteen 依赖系统 curl 访问云厂商 API。\
                 macOS / Linux 通常自带；Windows 10 1803+ 自带 curl.exe。",
                self.curl_bin
            ),
        })?;
        drop(tmp);

        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let code = out.status.code().unwrap_or(-1);
        CurlTransport::parse_output(&stdout, code, &stderr)
    }
}

/// 转义 curl 配置文件字符串值。
fn escape_curl_config(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 临时配置文件，`Drop` 时删除。
struct TempConfig {
    path: std::path::PathBuf,
}

impl TempConfig {
    fn create(content: &str) -> MnResult<Self> {
        // 文件名必须全局唯一：同一毫秒内并发创建会撞名，
        // 导致后创建者 A 覆盖前者 B 的文件，B 的 Drop 又删掉 A 的文件。
        // 用 pid + 毫秒 + 进程内单调序号，三者组合保证唯一。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let mut path = std::env::temp_dir();
        path.push(format!(
            "mangosteen-curl-{}-{}-{}.conf",
            std::process::id(),
            mangosteen_core::time::now_unix_millis(),
            seq
        ));
        {
            let mut f = std::fs::File::create(&path)
                .map_err(|e| MnError::io(format!("创建 curl 配置 {}", path.display()), &e))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| MnError::io("设置 curl 配置权限", &e))?;
            }
            f.write_all(content.as_bytes())
                .map_err(|e| MnError::io("写入 curl 配置", &e))?;
        }
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_body_and_status() {
        let r = CurlTransport::parse_output("{\"a\":1}\n200", 0, "").unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"a\":1}");
        assert!(r.is_success());
    }

    #[test]
    fn parses_multiline_body() {
        let body = "line1\nline2\nline3";
        let out = format!("{body}\n404");
        let r = CurlTransport::parse_output(&out, 0, "").unwrap();
        assert_eq!(r.status, 404);
        assert_eq!(r.body, body);
        assert!(!r.is_success());
    }

    #[test]
    fn empty_body_with_status() {
        let r = CurlTransport::parse_output("\n204", 0, "").unwrap();
        assert_eq!(r.status, 204);
        assert_eq!(r.body, "");
    }

    #[test]
    fn network_exit_codes_map_to_stage() {
        let e = CurlTransport::parse_output("", 7, "Connection refused").unwrap_err();
        assert!(matches!(e, MnError::Network { .. }));
        assert!(e.to_string().contains("TCP 连接"));
        assert!(e.is_retryable());

        let e = CurlTransport::parse_output("", 28, "timed out").unwrap_err();
        assert!(e.to_string().contains("超时"));

        let e = CurlTransport::parse_output("", 6, "could not resolve").unwrap_err();
        assert!(e.to_string().contains("DNS"));

        let e = CurlTransport::parse_output("", 60, "certificate verify failed").unwrap_err();
        assert!(e.to_string().contains("TLS"));
    }

    #[test]
    fn unparseable_status_is_an_error() {
        let e = CurlTransport::parse_output("body\nnotacode", 0, "").unwrap_err();
        assert!(e.to_string().contains("状态码"));
    }

    #[test]
    fn args_include_method_and_body() {
        let t = CurlTransport::new(15);
        let req = HttpRequest::post("https://api/x", "{\"k\":1}");
        let args = t.build_args(&req);
        assert!(args.contains(&"POST".to_string()));
        assert!(args.contains(&"15".to_string()));
        assert!(args.iter().any(|a| a == "{\"k\":1}"));
        assert_eq!(args.last().unwrap(), "https://api/x");
        // 认证头不应出现在命令行参数里（安全要求）
        assert!(!args.iter().any(|a| a.contains("Authorization")));
    }

    /// URL 只能出现一次，否则 curl 会把同一请求发两遍。
    ///
    /// ## 这个测试守护的是一个真实发生过的事故
    ///
    /// `execute` 曾经把 URL **同时**写进 `--config` 文件和 argv，
    /// curl 于是把同一请求发送两次，响应体变成 `{json}\n200{json}\n200`，
    /// 调用方解析报 `trailing characters at line 2`。
    ///
    /// 对 GET 类查询只是「看起来怪」，但对 POST 创建实例意味着
    /// **可能创建出两台并持续计费**。修复方式：URL 只走 argv。
    #[test]
    fn url_must_appear_exactly_once() {
        let t = CurlTransport::new(10);
        let req = HttpRequest::post("https://api.compshare.cn/v1/x", "{\"k\":1}");
        let args = t.build_args(&req);

        // argv 里恰好一次（最后一项），这是唯一的 URL 来源
        assert_eq!(
            args.iter()
                .filter(|a| a.as_str() == "https://api.compshare.cn/v1/x")
                .count(),
            1,
            "URL 在 argv 里必须恰好出现一次：{args:?}"
        );
        assert_eq!(
            args.last().map(|s| s.as_str()),
            Some("https://api.compshare.cn/v1/x")
        );
    }

    #[test]
    fn curl_config_escaping() {
        assert_eq!(escape_curl_config(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_curl_config(r"a\b"), r"a\\b");
    }

    #[test]
    fn temp_config_is_private_and_cleaned_up() {
        let p;
        {
            let t = TempConfig::create("header = \"A: b\"\n").unwrap();
            p = t.path().to_path_buf();
            assert!(p.exists());
            assert!(std::fs::read_to_string(&p).unwrap().contains("A: b"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "含认证头的临时文件必须 0600");
            }
        }
        assert!(!p.exists(), "临时配置文件必须被清理");
    }

    #[test]
    fn missing_curl_gives_actionable_error() {
        let t = CurlTransport {
            timeout_secs: 5,
            curl_bin: "/nonexistent/curl-xyz".to_string(),
        };
        let e = t.execute(&HttpRequest::get("https://x/y")).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("无法执行"), "got {msg}");
        assert!(msg.contains("curl"), "应提示依赖 curl：{msg}");
    }

    /// 真实联网：仅在显式开关下运行，避免常规测试依赖外网。
    #[test]
    fn live_request_when_enabled() {
        if std::env::var("MANGOSTEEN_LIVE_NETWORK_TESTS").is_err() {
            return;
        }
        let t = CurlTransport::new(20);
        let r = t
            .execute(&HttpRequest::get("https://httpbin.org/status/200"))
            .expect("live request");
        assert_eq!(r.status, 200);
    }
}
