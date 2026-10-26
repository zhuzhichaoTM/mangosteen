//! 云端实例内的**推理服务就绪**：等 HTTP 起来、把模型加载进远端引擎。
//!
//! ## 为什么需要这一层
//!
//! 建实例 + 建 SSH 隧道只解决了「网络能通」，不等于「能推理」：
//! 镜像里的推理服务需要几秒到几十秒才监听端口，模型也要先落到远端
//! 引擎自己的存储里。少了这一步，台账会标 `Ready` 而 `/v1/chat/completions`
//! 直接 502——用户看到的是「部署成功但对话报错」，最难排查的一类症状。
//!
//! ## 为什么走隧道而不是 SSH
//!
//! 隧道已经是现成能力（[`crate::net::TunnelManager`]），而 SSH 命令执行
//! 需要处理密码/密钥分发、`BatchMode`、`StrictHostKeyChecking` 等一堆
//! 与业务无关的细节。Ollama 镜像本身就带 HTTP 管理接口（`/api/pull`
//! 等），从隧道过去就能把模型装好——**不引入 SSH 依赖**。
//!
//! ## 为什么用 curl 而不是 HTTP 客户端库
//!
//! 工作区的依赖极简约束：HTTP 一律走系统 `curl`。这里沿用同一约定，
//! 顺带复用用户的代理配置（`--noproxy` 显式关闭，因为目标是 127.0.0.1）。

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mangosteen_core::{MnError, MnResult};

/// 远端 Ollama 的默认监听端口。
///
/// 不用 `CloudPlan.engine_port` 的自由值：Ollama 镜像是**常驻服务**，
/// 端口由镜像固定为 11434，改不了。把端口当可配参数只会让人以为
/// 能改，实际改了也不生效。
pub const OLLAMA_PORT: u16 = 11434;

/// 云端 `llama-server` 监听端口。
///
/// 与 [`OLLAMA_PORT`] 分开：ollama 是常驻服务、端口被它占死；llama.cpp 是
/// 我们自己拉起的子进程，端口由我们定。混用会让「云端跑 llama.cpp」的部署
/// 把隧道转发到 11434——那里根本没有服务，隧道建好了也连不上。
pub const LLAMA_CPP_PORT: u16 = 8080;

/// 一次 HTTP 调用的结果。
#[derive(Debug, Clone)]
pub struct HttpReply {
    /// HTTP 状态码（0 表示连接失败）
    pub status: u16,
    /// 响应体
    pub body: String,
}

impl HttpReply {
    /// 是否 2xx。
    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 用 curl 发一个请求。
///
/// `timeout_secs` 是**总时长**上限：`/api/pull` 要下载几 GB 模型，
/// 必须给足；而 `/api/version` 这种探测就该短。
pub fn http(
    method: &str,
    url: &str,
    json_body: Option<&str>,
    timeout_secs: u64,
) -> MnResult<HttpReply> {
    let mut cmd = Command::new("curl");
    cmd.arg("--silent")
        .arg("--show-error")
        // 目标是本机隧道端口：任何代理介入都会 502，必须显式绕开
        .arg("--noproxy")
        .arg("*")
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("-X")
        .arg(method)
        // 只取状态码与体，头丢掉
        .arg("--write-out")
        .arg("\n%{http_code}")
        .arg("--output")
        .arg("-");

    if let Some(b) = json_body {
        cmd.arg("--header")
            .arg("Content-Type: application/json")
            .arg("--data-binary")
            .arg(b);
    }
    cmd.arg(url);

    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| MnError::Network {
            stage: "云端推理服务".to_string(),
            reason: format!("无法启动 curl：{e}"),
        })?;

    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // 末尾一行是状态码
    let (body, status) = match text.rsplit_once('\n') {
        Some((b, s)) => (b.to_string(), s.trim().parse::<u16>().unwrap_or(0)),
        None => (String::new(), 0),
    };

    if !out.status.success() && status == 0 {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(MnError::Network {
            stage: "云端推理服务".to_string(),
            reason: format!(
                "{url} 请求失败：{}",
                err.lines().next().unwrap_or("无错误输出").trim()
            ),
        });
    }
    Ok(HttpReply { status, body })
}

/// 轮询直到 `url` 返回 2xx（或超时）。
///
/// 镜像首次启动要拉起容器/服务，几十秒很常见，所以这里必须等而不是
/// 探一次就判死。返回实际耗时，便于把「等了多久」如实写进阶段说明。
pub fn wait_http_ready(url: &str, budget: Duration, interval: Duration) -> MnResult<Duration> {
    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < budget {
        match http("GET", url, None, 5) {
            Ok(r) if r.is_ok() => return Ok(started.elapsed()),
            Ok(r) => last = format!("HTTP {}", r.status),
            Err(e) => last = format!("{e}"),
        }
        std::thread::sleep(interval);
    }
    Err(MnError::Network {
        stage: "等待云端推理服务".to_string(),
        reason: format!(
            "{} 在 {}s 内未就绪（最后状态：{}）。可能原因：镜像内服务未随实例启动、\
             隧道端口未转发到 {}，或实例仍在初始化",
            url,
            budget.as_secs(),
            if last.is_empty() { "无响应" } else { &last },
            OLLAMA_PORT
        ),
    })
}

/// 确认远端引擎里已有指定模型；没有就让它自己下载。
///
/// ## 为什么是「远端自己下载」而不是上传本地文件
///
/// 本地 GGUF 有几百 MB 到几 GB，从家用宽带上行传到云端既慢又容易中断，
/// 而且 Ollama 用自己的 blob 存储，传过去还要再 `create` 一次。
/// 让远端按名字拉取官方权重，链路上只有几百字节的 JSON。
///
/// 代价是**量化版本可能与本地不同**（远端按官方 tag 拉）。这一点必须
/// 在阶段说明里写清楚，不能让用户以为跑的是他本地那个 q2_k。
pub fn ensure_ollama_model(base_url: &str, model: &str, budget: Duration) -> MnResult<Duration> {
    let started = Instant::now();

    // 1. 已存在就直接用（重复部署不重复下载）
    if ollama_has_model(base_url, model) {
        return Ok(started.elapsed());
    }

    // 2. 触发拉取。`stream:false` 让服务端拉完才返回，避免我们还要
    //    解析事件流；超时按总预算给足。
    let body = serde_json::json!({ "name": model, "stream": false }).to_string();
    let reply = http(
        "POST",
        &format!("{base_url}/api/pull"),
        Some(&body),
        budget.as_secs().max(60),
    )?;
    if !reply.is_ok() {
        return Err(MnError::Network {
            stage: "云端加载模型".to_string(),
            reason: format!(
                "远端拉取 `{model}` 失败（HTTP {}）：{}",
                reply.status,
                first_line(&reply.body)
            ),
        });
    }

    // 3. 复核：拉取接口成功但模型名可能被规范化过，必须以 tags 为准
    if !ollama_has_model(base_url, model) {
        return Err(MnError::Network {
            stage: "云端加载模型".to_string(),
            reason: format!(
                "远端拉取 `{model}` 返回成功，但 `/api/tags` 里查不到它。\
                 若该名称不在 Ollama 官方库，请用 `cloud.model_map` 配置正确名称"
            ),
        });
    }
    Ok(started.elapsed())
}

/// 远端是否已有该模型。
fn ollama_has_model(base_url: &str, model: &str) -> bool {
    let Ok(r) = http("GET", &format!("{base_url}/api/tags"), None, 10) else {
        return false;
    };
    if !r.is_ok() {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&r.body) else {
        return false;
    };
    let want = normalize_tag(model);
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter().any(|m| {
                m.get("name")
                    .and_then(|n| n.as_str())
                    .map(normalize_tag)
                    .map(|n| n == want)
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// `qwen2.5:0.5b` 与 `qwen2.5:0.5b-instruct` 视为不同模型，但
/// `qwen2.5` 与 `qwen2.5:latest` 是同一个。
fn normalize_tag(name: &str) -> String {
    let n = name.trim();
    if n.contains(':') {
        n.to_string()
    } else {
        format!("{n}:latest")
    }
}

/// 取错误体第一行（Ollama 的 `error` 字段通常就一句话）。
fn first_line(s: &str) -> String {
    // 优先解析 JSON 的 error 字段
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            return e.to_string();
        }
    }
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("无响应体")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_tag_adds_latest() {
        assert_eq!(normalize_tag("qwen2.5"), "qwen2.5:latest");
        assert_eq!(normalize_tag("qwen2.5:0.5b"), "qwen2.5:0.5b");
        assert_eq!(normalize_tag("  llama3  "), "llama3:latest");
    }

    #[test]
    fn first_line_prefers_json_error_field() {
        assert_eq!(
            first_line(r#"{"error":"model not found"}"#),
            "model not found"
        );
        assert_eq!(first_line("plain\ntext"), "plain");
        assert_eq!(first_line(""), "无响应体");
    }

    /// 连不上时必须报错而不是 panic，且错误里带上是哪个 URL。
    #[test]
    fn wait_http_ready_reports_unreachable() {
        // 端口 1 不可能有服务
        let e = wait_http_ready(
            "http://127.0.0.1:1/api/version",
            Duration::from_millis(300),
            Duration::from_millis(50),
        )
        .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("127.0.0.1:1"), "错误应带上目标地址：{msg}");
    }

    /// 本地起一个必然 200 的端口，验证等待能成功返回。
    #[test]
    fn wait_http_ready_succeeds_when_serving() {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = match s {
                    Ok(s) => s,
                    Err(_) => break,
                };
                use std::io::{Read, Write};
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            }
        });
        let d = wait_http_ready(
            &format!("http://127.0.0.1:{port}/api/version"),
            Duration::from_secs(5),
            Duration::from_millis(50),
        )
        .expect("应能等到服务就绪");
        assert!(d < Duration::from_secs(5));
    }
}

// ------------------------------------------------------------------ SSH 执行
//
// 云端 llama.cpp 链路需要「在远端跑命令、把模型传上去」。这两件事都只能
// 走 SSH，而 SSH 的密码认证没有命令行参数，只能靠 `SSH_ASKPASS`
// （OpenSSH 8.4+ 的 `SSH_ASKPASS_REQUIRE=force` 才允许无 tty 时使用）。

use std::path::PathBuf;

/// SSH 目标。
#[derive(Debug, Clone)]
pub struct SshTarget {
    /// 主机（域名或 IP）
    pub host: String,
    /// 端口
    pub port: u16,
    /// 用户
    pub user: String,
    /// 密码（`None` 表示走密钥/agent）
    pub password: Option<String>,
}

impl SshTarget {
    /// 生成一个临时 askpass 脚本，返回路径。
    ///
    /// 脚本权限 0700、内容只有 `printf '%s\n' '<密码>'`。调用方必须在
    /// 命令结束后**立刻删除**——明文密码不该在 /tmp 里多留一秒。
    fn write_askpass(&self) -> MnResult<Option<PathBuf>> {
        let Some(pw) = self.password.as_deref() else {
            return Ok(None);
        };
        let path = std::env::temp_dir().join(format!(
            "mn-askpass-{}-{}.sh",
            std::process::id(),
            mangosteen_core::time::now_unix_millis()
        ));
        let escaped = pw.replace('\'', r#"'\''"#);
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{escaped}'\n"))
            .map_err(|e| MnError::io(format!("写入 askpass 脚本 {}", path.display()), &e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Some(path))
    }

    /// 公共 ssh 选项。
    fn base_opts(&self) -> Vec<String> {
        let mut a = vec![
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            "UserKnownHostsFile=/dev/null".to_string(),
            "-o".to_string(),
            "ConnectTimeout=15".to_string(),
            "-p".to_string(),
            self.port.to_string(),
        ];
        if self.password.is_some() {
            a.push("-o".to_string());
            a.push("PreferredAuthentications=password".to_string());
            a.push("-o".to_string());
            a.push("PubkeyAuthentication=no".to_string());
        }
        a
    }
}

/// 在远端执行一条命令，返回 stdout。
///
/// 命令用 `sh -c` 包一层，避免引号/管道在本地被解释成别的东西。
pub fn ssh_exec(target: &SshTarget, cmd: &str, timeout_secs: u64) -> MnResult<String> {
    let askpass = target.write_askpass()?;
    let mut args = target.base_opts();
    args.push(format!("{}@{}", target.user, target.host));
    args.push(cmd.to_string());

    let mut c = Command::new("ssh");
    c.args(&args).stdin(Stdio::null());
    if let Some(p) = &askpass {
        c.env("SSH_ASKPASS", p.display().to_string())
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", ":0");
    }
    let out = c.output();
    if let Some(p) = &askpass {
        let _ = std::fs::remove_file(p);
    }
    let out = out.map_err(|e| MnError::Network {
        stage: "远端执行命令".to_string(),
        reason: format!("无法启动 ssh：{e}"),
    })?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(MnError::Network {
            stage: "远端执行命令".to_string(),
            reason: format!(
                "`{}` 失败（退出码 {:?}）：{}",
                truncate_cmd(cmd),
                out.status.code(),
                err.lines().take(3).collect::<Vec<_>>().join(" / ")
            ),
        });
    }
    let _ = timeout_secs;
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 把本地文件传到远端（用 `scp`）。
///
/// 不用 rsync：目标机不一定装了 rsync，而 `scp` 是 OpenSSH 自带的。
fn truncate_cmd(cmd: &str) -> String {
    let one_line = cmd.replace('\n', " ");
    if one_line.chars().count() > 120 {
        format!("{}…", one_line.chars().take(120).collect::<String>())
    } else {
        one_line
    }
}

// ------------------------------------------------------ 云端 llama.cpp 引导

/// 在远端准备好 `llama-server`，返回其绝对路径。
///
/// ## 为什么要有「已存在就跳过」
///
/// AutoDL 的实例可以用「无卡模式」长期保留系统盘，同一台机器上反复部署
/// 是常态。每次都重新下载/编译一次 llama.cpp 纯属浪费几分钟，所以先探测。
///
/// ## 三种来源，按成本从低到高
///
/// 1. `PATH` 里已有 `llama-server`（用户自己装过 / 自定义镜像）；
/// 2. 我们上次装的位置（`<workdir>/bin/llama-server`）；
/// 3. 从 `download_url` 下载官方预编译包并解压。
///
/// 三种都不成，返回明确的错误——**不尝试在远端编译**：那要十几分钟且
/// 依赖 cmake/nvcc 齐全，失败面太大，不如让用户配一个可用的下载地址。
pub fn ensure_llama_server(
    target: &SshTarget,
    workdir: &str,
    download_url: &str,
) -> MnResult<String> {
    let installed = format!("{workdir}/bin/llama-server");

    // 1 + 2：一次命令同时探测两处
    let probe = ssh_exec(
        target,
        &format!(
            "if command -v llama-server >/dev/null 2>&1; then command -v llama-server; \
             elif [ -x {installed} ]; then echo {installed}; fi"
        ),
        60,
    )?;
    let found = probe.trim();
    if !found.is_empty() {
        return Ok(found.to_string());
    }

    // 3. 下载预编译包
    if download_url.trim().is_empty() {
        return Err(MnError::Network {
            stage: "准备云端 llama.cpp".to_string(),
            reason: format!(
                "远端没有 llama-server，且未配置下载地址。\
                 请在 [cloud] 里设置 llama_server_url（llama.cpp 官方 release 的 \
                 ubuntu-x64 zip 地址），或换一个自带 llama-server 的镜像"
            ),
        });
    }

    // 用 curl -L 跟随跳转（GitHub release 会 302 到对象存储）。
    //
    // 官方发布包有两种格式：老的 `...-ubuntu-x64.zip` 和现在主流的
    // `...-ubuntu-cuda-12.8-x64.tar.gz`。按 URL 后缀选解压命令，
    // 别硬编码一种——用错会得到「解压成功但找不到 llama-server」。
    let is_tar = download_url.ends_with(".tar.gz") || download_url.ends_with(".tgz");
    let (file, extract) = if is_tar {
        ("llama.tar.gz", "tar -xzf llama.tar.gz")
    } else {
        ("llama.zip", "unzip -o -q llama.zip")
    };
    let script = format!(
        "set -e; \
         mkdir -p {workdir}/bin {workdir}/tmp; \
         cd {workdir}/tmp; \
         curl -fL --retry 2 -o {file} '{download_url}'; \
         {extract}; \
         d=$(dirname $(find . -name llama-server -type f | head -1)); \
         cp -a \"$d\"/. {workdir}/bin/; \
         chmod +x {workdir}/bin/llama-server; \
         rm -rf {workdir}/tmp; \
         echo {installed}"
    );
    let out = ssh_exec(target, &script, 900)?;
    let path = out.trim().lines().last().unwrap_or("").trim().to_string();
    if path.is_empty() {
        return Err(MnError::Network {
            stage: "准备云端 llama.cpp".to_string(),
            reason: "下载解压后没有找到 llama-server 可执行文件".to_string(),
        });
    }
    Ok(path)
}

/// 在远端后台启动 `llama-server`。
///
/// ## 为什么要 `setsid` + `nohup`
///
/// SSH 会话一断开，普通子进程会收到 SIGHUP 被杀。`setsid` 让它脱离
/// 控制终端、`nohup` 忽略 HUP，这样隧道重连、本地关掉 mangosteen 都不会
/// 把云端推理服务带走——它本来就该独立于客户端活着。
///
/// 启动后**不等**健康检查：调用方紧接着会用隧道做 HTTP 探活，
/// 在这里再等一遍只会让超时逻辑重复两套。
pub fn start_llama_server(
    target: &SshTarget,
    bin: &str,
    args: &[String],
    workdir: &str,
) -> MnResult<()> {
    let log = format!("{workdir}/llama-server.log");
    let pid_file = format!("{workdir}/llama-server.pid");
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    let script = format!(
        "set -e; mkdir -p {workdir}; \
         pkill -f '{bin}' 2>/dev/null || true; \
         setsid nohup {bin} {args} > {log} 2>&1 < /dev/null & \
         echo $! > {pid_file}; \
         sleep 1; \
         if ! kill -0 $(cat {pid_file}) 2>/dev/null; then \
           echo '--- 启动失败，日志尾部 ---' >&2; tail -20 {log} >&2; exit 1; \
         fi; \
         echo started",
        args = quoted.join(" ")
    );
    ssh_exec(target, &script, 120)?;
    Ok(())
}

/// 给 shell 参数加单引号（模型路径里可能有空格）。
fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r#"'\''"#))
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_only_when_needed() {
        assert_eq!(shell_quote("/root/a.gguf"), "/root/a.gguf");
        assert_eq!(shell_quote("--ctx-size"), "--ctx-size");
        assert_eq!(shell_quote("/root/a b.gguf"), "'/root/a b.gguf'");
        // 单引号必须转义，否则会截断参数
        assert_eq!(shell_quote("a'b"), r#"'a'\''b'"#);
    }
}

/// 远端 `llama-server` 是否在跑。
pub fn llama_server_running(target: &SshTarget, workdir: &str) -> MnResult<bool> {
    let out = ssh_exec(
        target,
        &format!(
            "if pgrep -f '{workdir}/bin/llama-server' >/dev/null 2>&1 \
                || pgrep -x llama-server >/dev/null 2>&1; then echo yes; else echo no; fi"
        ),
        60,
    )?;
    Ok(out.trim() == "yes")
}

/// 停掉远端 `llama-server`（**保留实例**）。
///
/// ## 与「销毁实例」的区别
///
/// 这是「停止服务」：进程没了，GPU 显存释放，但机器还在——所以**仍在计费**。
/// 好处是再启动只要几秒（模型文件已在盘上）。真正停止计费要走
/// `Orchestrator::teardown`（销毁实例）。
///
/// 调用方必须把「实例仍在计费」这件事告诉用户，否则会有人以为点了停止就
/// 不花钱了。
pub fn stop_llama_server(target: &SshTarget, workdir: &str) -> MnResult<()> {
    // 先 TERM 给机会优雅退出（它会自己释放显存），等不到再 KILL。
    // 只 TERM 就返回是不行的：端口没释放，重启会报 address in use。
    let out = ssh_exec(
        target,
        &format!(
            "pkill -TERM -f '{workdir}/bin/llama-server' 2>/dev/null; \
             pkill -TERM -x llama-server 2>/dev/null; \
             for i in $(seq 1 20); do \
               if ! pgrep -x llama-server >/dev/null 2>&1; then echo stopped; exit 0; fi; \
               sleep 0.5; \
             done; \
             pkill -KILL -x llama-server 2>/dev/null; sleep 1; \
             if pgrep -x llama-server >/dev/null 2>&1; then echo still-running; exit 1; fi; \
             echo stopped",
        ),
        60,
    )?;
    if out.contains("still-running") {
        return Err(MnError::Network {
            stage: "停止云端推理服务".to_string(),
            reason: "llama-server 在 TERM 与 KILL 之后仍在运行".to_string(),
        });
    }
    Ok(())
}

// ------------------------------------------------------------ 远端下载模型

/// 在远端直接下载模型文件（**云端部署的主链路**）。
///
/// ## 为什么下载要在远端发生
///
/// 设备选「云端」时，模型本就该落在云上。本地是否已有这个文件**与云端任务
/// 无关**——云端要用的就是它自己盘上那份。把本地文件传上去既浪费本地带宽
/// 与磁盘，又让「这台机器没下过」变成云端部署的前置条件。
///
/// ## 为什么用「后台下载 + 轮询」而不是一条 SSH 连接挂着
///
/// 实测踩到：`ssh 'curl ...'` 直接跑几 GB 下载会被厂商的 SSH 网关切断
/// （`Connection closed` / 退出码 255）。网关对单连接时长与空闲有阈值，
/// 而大文件下载恰好两者都占满。
///
/// 所以改成：`nohup curl ... &` 起后台任务，主连接立刻返回；之后用**独立的
/// 短连接**轮询远端文件大小。每条连接只活一两秒，不会被判定超时。
/// 断点续传靠 `curl -C -`：轮询发现停滞或连接断了，重发一次就能接着下。
///
/// ## 大小校验
///
/// `expected_bytes` 已知时必须核对；未知时只检查「文件非空」并**如实说明
/// 未校验**——不假装校验过。校验失败会删掉半截文件，避免下次误用它。
pub fn download_model_remote(
    target: &SshTarget,
    url: &str,
    dest: &str,
    expected_bytes: Option<u64>,
) -> MnResult<u64> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(MnError::Network {
            stage: "云端下载模型".to_string(),
            reason: format!("下载地址不是 HTTP(S)：{url}"),
        });
    }
    let workdir = dest
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| "/root".to_string());
    let log = format!("{dest}.curl.log");
    let pid_file = format!("{dest}.curl.pid");

    // 先清掉上次可能残留的半截文件？**不清**——`-C -` 要靠它续传。
    // 但要把日志清空，否则这次看不到新错误。
    let start_cmd = format!(
        "set -e; mkdir -p '{workdir}';          : > '{log}';          pkill -f 'curl.*{dest}' 2>/dev/null || true;          nohup curl -fL -C - --retry 5 --retry-delay 3 --connect-timeout 20               --speed-time 60 --speed-limit 1024               -o '{dest}' '{url}' > '{log}' 2>&1 < /dev/null &          echo $! > '{pid_file}';          echo started"
    );
    ssh_exec(target, &start_cmd, 60)?;

    // 轮询：每条连接都是新的短连接，不受网关时长限制
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let mut last_size = 0u64;
    let mut stalled = 0u32;
    loop {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let size = remote_file_size(target, dest)?;
        if let Some(exp) = expected_bytes {
            if exp > 0 && size == exp {
                return Ok(size);
            }
        }
        // 下载进程还活着吗
        let alive = ssh_exec(
            target,
            &format!(
                "if kill -0 $(cat '{pid_file}' 2>/dev/null) 2>/dev/null; then echo alive; else echo dead; fi"
            ),
            60,
        )?;
        if alive.contains("dead") {
            // 进程结束但大小没到 → 看日志、决定是否续传一次
            if size > last_size {
                // 还在长，可能只是刚退出，再等一轮
                last_size = size;
                continue;
            }
            let tail = ssh_exec(target, &format!("tail -5 '{log}' 2>/dev/null"), 60)?;
            if stalled >= 2 {
                // 连续两次没有进展 → 判失败（不清文件，保留续传可能）
                return Err(MnError::Network {
                    stage: "云端下载模型".to_string(),
                    reason: format!(
                        "下载中断且无法续传（已下 {} 字节）：{}",
                        size,
                        tail.lines().take(3).collect::<Vec<_>>().join(" / ")
                    ),
                });
            }
            // 重新拉起续传
            stalled += 1;
            last_size = size;
            let resume = format!(
                "nohup curl -fL -C - --retry 5 --retry-delay 3 --connect-timeout 20                       -o '{dest}' '{url}' >> '{log}' 2>&1 < /dev/null &                  echo $! > '{pid_file}'; echo resumed"
            );
            ssh_exec(target, &resume, 60)?;
            continue;
        }
        // 活着但长时间不增长 → 网络卡死，杀掉重来（`-C -` 会续传）
        if size == last_size {
            stalled += 1;
            if stalled >= 6 {
                ssh_exec(
                    target,
                    &format!("kill $(cat '{pid_file}') 2>/dev/null || true"),
                    30,
                )?;
                let resume = format!(
                    "nohup curl -fL -C - --retry 5 --retry-delay 3 --connect-timeout 20                           -o '{dest}' '{url}' >> '{log}' 2>&1 < /dev/null &                      echo $! > '{pid_file}'; echo resumed"
                );
                ssh_exec(target, &resume, 60)?;
                stalled = 0;
            }
        } else {
            stalled = 0;
        }
        last_size = size;

        if std::time::Instant::now() >= deadline {
            return Err(MnError::Network {
                stage: "云端下载模型".to_string(),
                reason: format!("下载超时（已下 {} 字节）", size),
            });
        }
    }
}

/// 查远端文件大小；不存在返回 `Ok(0)`。
pub fn remote_file_size(target: &SshTarget, path: &str) -> MnResult<u64> {
    let out = ssh_exec(
        target,
        &format!("if [ -f '{path}' ]; then stat -c %s '{path}'; else echo 0; fi"),
        60,
    )?;
    Ok(out
        .trim()
        .lines()
        .last()
        .unwrap_or("0")
        .trim()
        .parse()
        .unwrap_or(0))
}
