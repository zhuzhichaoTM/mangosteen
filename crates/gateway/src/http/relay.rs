//! 流式推理直通（SSE relay）。
//!
//! 归属：**第二层 · API Server**。慢生成请求必须边读边转发；这条路径
//! 与普通缓冲响应完全分开，避免把流式退化成「全部生成完再返回」。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use mangosteen_core::{MnError, MnResult};

use super::{GatewayServer, ParsedRequest, Response};
use crate::http::upstream::{summarize_body, write_chunk};
use crate::protocol;

/// 是否为**流式推理请求**（需要边生成边转发）。
///
/// 判定三件事：POST、推理路由、body 里 `stream: true`。
///
/// 为什么必须单独识别：`Response.body` 是 `Vec<u8>`，天然是
/// 「全部读完再返回」。流式请求若走那条路径，用户要等整段生成完
/// 才看到第一个字——这正是「输出卡顿」的直接原因。
pub(crate) fn is_stream_request(req: &ParsedRequest) -> bool {
    if req.method != "POST" {
        return false;
    }
    if !matches!(
        req.path.as_str(),
        "/v1/chat/completions" | "/v1/completions"
    ) {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&req.body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false)
}

impl GatewayServer {
    /// 流式直通：边从上游读、边改写、边写给客户端。
    ///
    /// ## 为什么用 curl 而不是裸 TcpStream
    ///
    /// 上游（llama-server）流式响应是 `Transfer-Encoding: chunked`，
    /// 自己解 chunked 要维护跨块状态机（块头、块尾 CRLF、跨块的块头），
    /// 很容易在边界上出错。`curl --no-buffer` 已经把 chunked 解开、
    /// 按到达顺序输出，而且与项目「HTTP 走系统 curl」的约定一致。
    ///
    /// ## 关键点
    ///
    /// 1. `--no-buffer`：不加这个，curl 会攒满缓冲区才输出，
    ///    流式又变回「等全部完成」；
    /// 2. `--noproxy '*'`：本机常有 `HTTP_PROXY`，回环请求不能走代理；
    /// 3. 每个事件写完立刻 `flush` + `TCP_NODELAY`，否则小包被
    ///    Nagle 算法攒着，看起来像「一顿一顿地出」；
    /// 4. 客户端断开（用户点「停止」/关页面）→ 立刻 kill curl，
    ///    上游写失败就会停止生成，不白烧 CPU。
    pub(crate) fn relay_stream(&self, mut client: TcpStream, req: &ParsedRequest) -> MnResult<()> {
        // 错误必须**回给客户端**。
        //
        // 踩过的坑：早先直接 `?` 把错误抛给 handle_connection，
        // 而那边只是 `return`——客户端看到的是「连接被关闭、没有任何响应」，
        // 前端只能报「网络错误」，完全不知道真正原因是「没有就绪实例」。
        match self.relay_stream_inner(&mut client, req) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = client.write_all(&Response::error(&e).to_bytes());
                let _ = client.flush();
                Ok(())
            }
        }
    }

    /// 流式直通的实际实现（错误由 [`Self::relay_stream`] 统一回写）。
    fn relay_stream_inner(&self, client: &mut TcpStream, req: &ParsedRequest) -> MnResult<()> {
        // ---- 选后端 ----
        let model = serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
        let target = {
            let mut gw = self
                .gateway
                .lock()
                .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
            gw.active_backend_for(model.as_deref())?
        };
        let url = target.url_for(&req.path);

        // ---- 拉起 curl ----
        let timeout = self.upstream_timeout.max(Duration::from_secs(600));
        let mut child = std::process::Command::new("curl")
            .arg("--no-buffer")
            .arg("--silent")
            .arg("--show-error")
            .arg("--noproxy")
            .arg("*")
            .arg("--max-time")
            .arg(timeout.as_secs().to_string())
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("--data-binary")
            .arg("@-")
            .arg(&url)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| MnError::Network {
                stage: "启动上游转发".to_string(),
                reason: format!("无法启动 curl：{e}"),
            })?;

        // 写请求体后立即关闭 stdin（否则上游一直等 body）
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(&req.body);
        }

        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            return Err(MnError::Network {
                stage: "上游转发".to_string(),
                reason: "无法读取 curl 输出".to_string(),
            });
        };
        let mut reader = BufReader::new(stdout);

        // ---- 先探第一段内容，区分「正常 SSE」与「上游报错」----
        //
        // 不能一上来就写 200 头：如果上游返回的是错误 JSON，
        // 写死 200 会让客户端把错误当成正常流解析，症状是「界面空白」。
        let mut head_lines: Vec<String> = Vec::new();
        let mut is_sse = false;
        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            let probe = text.trim_start();
            if probe.is_empty() {
                head_lines.push(text);
                continue;
            }
            // SSE 的有效帧：`data:` / `event:` / 注释 `:`
            if probe.starts_with("data:") || probe.starts_with("event:") || probe.starts_with(':') {
                is_sse = true;
                head_lines.push(text);
                break;
            }
            // 非 SSE 内容（多半是错误 JSON）→ 再多读几行看看，但别无限读
            head_lines.push(text);
            if head_lines.len() > 8 {
                break;
            }
        }

        if !is_sse {
            // 上游返回的不是 SSE：读完整段，按普通响应回错误
            let mut rest = String::new();
            let _ = reader.read_to_string(&mut rest);
            let _ = child.kill();
            let body = format!("{}{}", head_lines.join("\n"), rest);
            let e = MnError::Network {
                stage: "上游推理服务".to_string(),
                reason: summarize_body(&body),
            };
            let _ = client.write_all(&Response::error(&e).to_bytes());
            let _ = client.flush();
            return Ok(());
        }

        // ---- 写响应头（chunked，长度未知）----
        client.set_nodelay(true).ok();
        let head = format!(
            "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream; charset=utf-8\r\n\
         Cache-Control: no-cache, no-transform\r\n\
         X-Accel-Buffering: no\r\n\
         Transfer-Encoding: chunked\r\n\
         Server: mangosteen/{}\r\n\
         Connection: close\r\n\r\n",
            mangosteen_core::identity::VERSION
        );
        if client.write_all(head.as_bytes()).is_err() {
            let _ = child.kill();
            return Ok(());
        }
        let _ = client.flush();

        // ---- 逐行转发 ----
        let relay_line = |c: &mut TcpStream, line: &str| -> bool {
            let rewritten = protocol::rewrite_sse_line(line, &target.model_id);
            let payload = format!("{rewritten}\n");
            write_chunk(c, payload.as_bytes())
        };

        for l in &head_lines {
            if !relay_line(client, l) {
                let _ = child.kill();
                return Ok(());
            }
        }

        loop {
            let mut line = Vec::new();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break, // 超时或被杀
            }
            let text = String::from_utf8_lossy(&line)
                .trim_end_matches(['\r', '\n'])
                .to_string();
            if !relay_line(client, &text) {
                // 客户端断开（用户点「停止」/关页面）→ 立刻停上游，
                // 否则引擎会继续把剩下的 token 全部生成完，白烧 CPU
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
        }

        // ---- 结束 chunked 流 ----
        let _ = client.write_all(b"0\r\n\r\n");
        let _ = client.flush();
        let _ = child.wait();
        Ok(())
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use crate::http::upstream::write_chunk;
    use std::collections::HashMap;

    fn req(method: &str, path: &str, body: &str) -> ParsedRequest {
        ParsedRequest {
            method: method.to_string(),
            path: path.to_string(),
            query: String::new(),
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    /// 只有 `stream: true` 的推理请求才走直通路径。
    ///
    /// 判定错了后果很严重：非流式请求走流式路径会破坏 `Content-Length`
    /// 语义；流式请求走缓冲路径则退化成「等全部生成完才返回」。
    #[test]
    fn detects_stream_requests() {
        // 正常流式
        assert!(is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            r#"{"model":"m","stream":true,"messages":[]}"#
        )));
        assert!(is_stream_request(&req(
            "POST",
            "/v1/completions",
            r#"{"model":"m","stream":true}"#
        )));

        // 非流式
        assert!(!is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            r#"{"model":"m","stream":false,"messages":[]}"#
        )));
        // 缺省 stream → 非流式（协议默认）
        assert!(!is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            r#"{"model":"m","messages":[]}"#
        )));
        // 非推理路由
        assert!(!is_stream_request(&req(
            "POST",
            "/api/deploy",
            r#"{"stream":true}"#
        )));
        // 非 POST
        assert!(!is_stream_request(&req(
            "GET",
            "/v1/chat/completions",
            "{}"
        )));
        // body 不是 JSON → 不能崩，按非流式处理
        assert!(!is_stream_request(&req(
            "POST",
            "/v1/chat/completions",
            "not json"
        )));
    }

    /// SSE 单行改写：只改 `model`，帧结构必须原样保留。
    #[test]
    fn rewrites_single_sse_line() {
        let line =
            r#"data: {"model":"/abs/path/model.gguf","choices":[{"delta":{"content":"你"}}]}"#;
        let out = protocol::rewrite_sse_line(line, "qwen2.5:q4_k_m");
        assert!(out.starts_with("data: "), "帧前缀必须保留：{out}");
        let payload = out.trim_start_matches("data: ");
        let v: serde_json::Value = serde_json::from_str(payload).expect("仍是合法 JSON");
        assert_eq!(v["model"], "qwen2.5:q4_k_m", "model 必须改写为模型 ID");
        assert_eq!(v["choices"][0]["delta"]["content"], "你", "内容不能丢");

        // [DONE] 与非 data 行原样返回
        assert_eq!(
            protocol::rewrite_sse_line("data: [DONE]", "x"),
            "data: [DONE]"
        );
        assert_eq!(protocol::rewrite_sse_line("", "x"), "");
        assert_eq!(
            protocol::rewrite_sse_line("event: ping", "x"),
            "event: ping"
        );
    }

    /// chunked 帧格式必须严格符合 HTTP/1.1：`<hex len>\r\n<data>\r\n`。
    ///
    /// 格式错一位浏览器就解析失败，症状是「流式完全没有输出」。
    #[test]
    fn chunk_encoding_format_is_correct() {
        // 用一个本地 listener 验证真实写出的字节
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            write_chunk(&mut s, b"hello")
        });

        let mut client = TcpStream::connect(addr).unwrap();
        assert!(h.join().unwrap(), "写 chunk 应成功");

        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got),
            "5\r\nhello\r\n",
            "chunked 帧格式错误：{got:?}"
        );
    }
}
