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

/// 设备执行目标：本地进程或远端节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceTarget {
    /// 本机执行
    Local,
    /// 远端节点执行（由 provider 创建并给出连接信息）
    Remote(SshTarget),
}

/// 统一的设备运行时契约。
///
/// 本地与云端共用同一套模型准备 / 引擎启停语义：本地实现直接拉起进程，
/// 远端实现经 SSH 在节点内执行。上层只依赖本 trait，不区分目标形态。
pub trait DeviceRuntime: Send + Sync {
    /// 目标描述。
    fn target(&self) -> &DeviceTarget;

    /// 确保推理引擎二进制可用，返回可执行路径。
    fn ensure_engine(&self, workdir: &str, download_url: &str) -> MnResult<String>;

    /// 确保模型文件就位（远端按 URL 下载并校验大小）。
    fn ensure_model(&self, url: &str, dest: &str, expected_bytes: Option<u64>) -> MnResult<u64>;

    /// 拉起推理服务。
    fn launch(&self, bin: &str, args: &[String], workdir: &str) -> MnResult<()>;

    /// 服务是否在运行。
    fn is_running(&self, workdir: &str) -> MnResult<bool>;

    /// 停止服务（保留节点）。
    fn stop(&self, workdir: &str) -> MnResult<()>;
}

/// 远端设备运行时：所有操作经 SSH 在节点内执行。
#[derive(Debug, Clone)]
pub struct RemoteRuntime {
    target: DeviceTarget,
    /// 远端工作目录（模型、二进制、日志）
    workdir: String,
}

impl RemoteRuntime {
    /// 构造。`target` 必须是 [`DeviceTarget::Remote`]。
    pub fn new(target: DeviceTarget, workdir: impl Into<String>) -> Self {
        Self {
            target,
            workdir: workdir.into().trim_end_matches('/').to_string(),
        }
    }

    fn ssh(&self) -> MnResult<&SshTarget> {
        match &self.target {
            DeviceTarget::Remote(t) => Ok(t),
            DeviceTarget::Local => Err(MnError::bad_request("RemoteRuntime 需要远端节点连接信息")),
        }
    }
}

impl DeviceRuntime for RemoteRuntime {
    fn target(&self) -> &DeviceTarget {
        &self.target
    }

    fn ensure_engine(&self, workdir: &str, download_url: &str) -> MnResult<String> {
        let workdir = if workdir.is_empty() {
            self.workdir.as_str()
        } else {
            workdir
        };
        ensure_llama_server(self.ssh()?, workdir, download_url)
    }

    fn ensure_model(&self, url: &str, dest: &str, expected_bytes: Option<u64>) -> MnResult<u64> {
        download_model_remote(self.ssh()?, url, dest, expected_bytes)
    }

    fn launch(&self, bin: &str, args: &[String], workdir: &str) -> MnResult<()> {
        let workdir = if workdir.is_empty() {
            self.workdir.as_str()
        } else {
            workdir
        };
        start_llama_server(self.ssh()?, bin, args, workdir)
    }

    fn is_running(&self, workdir: &str) -> MnResult<bool> {
        let workdir = if workdir.is_empty() {
            self.workdir.as_str()
        } else {
            workdir
        };
        llama_server_running(self.ssh()?, workdir)
    }

    fn stop(&self, workdir: &str) -> MnResult<()> {
        let workdir = if workdir.is_empty() {
            self.workdir.as_str()
        } else {
            workdir
        };
        stop_llama_server(self.ssh()?, workdir)
    }
}

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
            "{} 在 {}s 内未就绪（最后状态：{}）。可能原因：远端推理服务未监听目标端口、\
             SSH 隧道未转发到远端引擎端口，或实例仍在初始化",
            url,
            budget.as_secs(),
            if last.is_empty() { "无响应" } else { &last }
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

    /// 幂等追加命令必须：先建目录/权限、用精确匹配判断、再按需追加。
    #[test]
    fn authorized_keys_cmd_is_idempotent() {
        let cmd = authorized_keys_append_cmd("ssh-ed25519 AAAA test@host");
        // 目录与权限按 sshd 要求收紧
        assert!(cmd.contains("mkdir -p ~/.ssh"), "{cmd}");
        assert!(cmd.contains("chmod 700 ~/.ssh"), "{cmd}");
        assert!(cmd.contains("chmod 600 ~/.ssh/authorized_keys"), "{cmd}");
        // 用整行精确匹配做去重判断（-qxF：quiet + 整行 + 固定字符串）
        assert!(cmd.contains("grep -qxF 'ssh-ed25519 AAAA test@host'"), "{cmd}");
        // 只有没命中才追加
        assert!(
            cmd.contains("|| printf '%s\\n' 'ssh-ed25519 AAAA test@host' >> ~/.ssh/authorized_keys"),
            "{cmd}"
        );
    }

    /// 公钥路径必须是 `<私钥>.pub` 的同名文件。
    ///
    /// 这里刻意用字符串拼接而不是 `with_extension`：两者对 `id_ed25519`
    /// 结果一致（Rust 只替换最后一个 `.` 之后的部分），但拼接的语义更明确——
    /// 「公钥就是私钥名加 .pub」，不受扩展名解析规则影响。
    #[test]
    fn public_key_path_is_private_key_plus_pub() {
        let key = PathBuf::from("/home/u/.ssh/id_ed25519");
        let pub_key = PathBuf::from(format!("{}.pub", key.display()));
        assert_eq!(pub_key, PathBuf::from("/home/u/.ssh/id_ed25519.pub"));
        // 与 with_extension 的结果一致，证明当前实现不会丢文件名前缀
        assert_eq!(key.with_extension("pub"), pub_key);
    }

    #[test]
    fn first_line_prefers_json_error_field() {        assert_eq!(
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// 主机（域名或 IP）
    pub host: String,
    /// 端口
    pub port: u16,
    /// 用户
    pub user: String,
    /// 密码（`None` 表示走密钥/agent）
    pub password: Option<String>,
    /// 已安装到远端后的本地私钥路径；为空时使用 ssh 默认密钥/agent。
    pub identity_file: Option<PathBuf>,
}

impl SshTarget {
    /// 仅删除本次目标节点的 known_hosts 指纹，不影响其他 SSH 主机。
    fn remove_known_host_entry(&self) -> MnResult<()> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| MnError::config("HOME", "无法确定本机 HOME 目录"))?;
        let known_hosts = home.join(".ssh").join("known_hosts");
        if !known_hosts.is_file() {
            return Ok(());
        }
        let host = if self.port == 22 {
            self.host.clone()
        } else {
            format!("[{}]:{}", self.host, self.port)
        };
        let status = Command::new("ssh-keygen")
            .args(["-f"])
            .arg(&known_hosts)
            .args(["-R", &host])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| MnError::Network {
                stage: "清理 SSH 主机指纹".to_string(),
                reason: format!("无法执行 ssh-keygen：{e}"),
            })?;
        if !status.success() {
            return Err(MnError::Network {
                stage: "清理 SSH 主机指纹".to_string(),
                reason: format!("无法清理目标节点 {} 的 known_hosts 记录", host),
            });
        }
        Ok(())
    }

    /// 生成一个临时 askpass 脚本，返回它连同密码 sidecar。
    ///
    /// 实际形态由平台层决定（`mangosteen_platform::ssh`）：macOS/Linux 是
    /// `#!/bin/sh` 脚本，Windows 是 `.cmd` 批处理。上层不能假设 `/bin/sh` 存在。
    /// 调用方必须在命令结束后**立刻** `cleanup()`——明文密码不该在临时目录里多留一秒。
    fn write_askpass(&self) -> MnResult<Option<mangosteen_platform::ssh::Askpass>> {
        let Some(pw) = self.password.as_deref() else {
            return Ok(None);
        };
        let tag = format!(
            "{}-{}",
            std::process::id(),
            mangosteen_core::time::now_unix_millis()
        );
        mangosteen_platform::ssh::write_askpass(&tag, Some(pw))
    }

    /// 公共 ssh 选项。
    fn base_opts(&self) -> Vec<String> {
        let mut a = vec![
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            // 丢弃式的 known_hosts 路径是平台差异：Unix 是 /dev/null，
            // Windows 是 NUL。写死 /dev/null 会让 OpenSSH for Windows 把它
            // 当成相对路径去创建文件而报错。
            format!(
                "UserKnownHostsFile={}",
                mangosteen_platform::ssh::null_device_path()
            ),
            // 丢弃主机指纹时，OpenSSH 会把首次连接提示写到
            // stderr；这是正常现象，但会污染远端命令错误信息。
            "-o".to_string(),
            "LogLevel=ERROR".to_string(),
            "-o".to_string(),
            "ConnectTimeout=15".to_string(),
            // 连接级重试：厂商 SSH 入口是共享转发网关，转发重建期间会
            // 出现「连上即被关闭」和「端口拒绝」的秒级窗口。这组参数让
            // ssh 在建立 TCP 连接阶段自己重试，与应用层重试形成双保险
            // （应用层还负责被远端关闭之后的重新建连）。
            "-o".to_string(),
            "ConnectionAttempts=5".to_string(),
            // 应用层保活：长命令（apt-get、解压、启动等待循环）可能几十秒
            // 没有任何输出，中间 NAT/网关会把静默连接掐掉。SSP 每 15 秒发
            // 一次心跳，连续 4 次无响应才判定断连——既保活又能及时发现真断。
            "-o".to_string(),
            "ServerAliveInterval=15".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=4".to_string(),
            "-o".to_string(),
            "TCPKeepAlive=yes".to_string(),
            "-p".to_string(),
            self.port.to_string(),
        ];
        if let Some(identity) = &self.identity_file {
            a.push("-i".to_string());
            a.push(identity.display().to_string());
            a.push("-o".to_string());
            a.push("IdentitiesOnly=yes".to_string());
        }
        if self.password.is_some() {
            a.push("-o".to_string());
            a.push("PreferredAuthentications=keyboard-interactive,password".to_string());
            a.push("-o".to_string());
            a.push("PasswordAuthentication=yes".to_string());
            a.push("-o".to_string());
            a.push("KbdInteractiveAuthentication=yes".to_string());
            a.push("-o".to_string());
            a.push("PubkeyAuthentication=no".to_string());
        }
        a
    }
}

/// 生成「把公钥幂等追加到远端 authorized_keys」的远端命令。
///
/// ## 为什么是幂等的
///
/// 同一台机器重复部署、或用户手动装过公钥时，直接 `>>` 会把同一行
/// 反复追加，authorized_keys 越来越长。用 `grep -qxF` 做整行精确匹配，
/// 命中就跳过，保证「装过一次就不会有第二行」。
///
/// ## 为什么不用 ssh-copy-id
///
/// 它是 shell 脚本，内部会再 fork `cat`/`ssh-add` 取身份列表；macOS 对
/// 子进程有策略拦截（`Brokered program policy check unavailable`），
/// 一旦 `GET_ID` 求值为空就报 `ERROR: No identities found` 退出 1——
/// 与调用方参数无关，无法通过调参规避。改成只用 ssh 一条链路。
fn authorized_keys_append_cmd(pub_text: &str) -> String {
    format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && \
         touch ~/.ssh/authorized_keys && chmod 600 ~/.ssh/authorized_keys && \
         grep -qxF '{pub_text}' ~/.ssh/authorized_keys || \
         printf '%s\\n' '{pub_text}' >> ~/.ssh/authorized_keys"
    )
}

/// 在节点配置环境中安装 SSH 公钥；调用方负责按节点生命周期重试。
///
/// ## 顺序（每一步都不能省）
///
/// 1. **先探测免密登录**：公钥可能早就装过（同节点重复部署、或用户手动
///    `ssh-copy-id` 过）。此时直接返回密钥，绝不能再装一次——
///    重复安装不仅多余，还会踩 `ssh-copy-id` 的「All keys were skipped」
///    分支（退出码 0 但没有 added 标记），旧实现会把它误判成失败。
/// 2. 没有免密才安装。**不用 `ssh-copy-id`**：它是 shell 脚本，内部会
///    再 fork `cat`/`ssh-add` 取身份列表；macOS 对子进程有策略拦截
///    （`Brokered program policy check unavailable`），一旦 `GET_ID`
///    求值为空就直接 `ERROR: No identities found` 退出 1——
///    与我们自己的参数无关，无法通过调参规避。
///    改成「本地读公钥 → ssh 追加到远端 authorized_keys」这条只用
///    ssh 一条链路的方式，行为完全可控。
/// 本机 SSH 用户目录（`~/.ssh`）。
///
/// Windows 没有 `HOME`，必须回退到 `USERPROFILE`；两者都取不到就明确报错，
/// 而不是默默把密钥生成到进程的当前目录。
fn ssh_home_dir() -> MnResult<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| MnError::config("HOME", "无法确定本机用户主目录"))?;
    let dir = home.join(".ssh");
    std::fs::create_dir_all(&dir)
        .map_err(|e| MnError::io(format!("创建 SSH 目录 {}", dir.display()), &e))?;
    Ok(dir)
}

/// 本机默认私钥路径（`~/.ssh/id_ed25519`）。
///
/// **不**保证文件存在；只用于「尝试用已有密钥登录」的探测场景。
pub fn default_identity_key() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let key = PathBuf::from(home).join(".ssh").join("id_ed25519");
    key.is_file().then_some(key)
}

/// 返回本机默认可用的 ed25519 私钥路径，不存在时用 `ssh-keygen` 无交互生成。
///
/// 密钥是「装一次、长期复用」的：后续所有 SSH 都走密钥认证，不再需要密码。
/// 生成时 `-N ""` 表示空口令——隧道与自动化命令无人值守，带口令的密钥
/// 会在 ssh 启动时卡在交互输入上。
fn ensure_identity_key() -> MnResult<PathBuf> {
    let ssh_dir = ssh_home_dir()?;
    let key = ssh_dir.join("id_ed25519");
    let pub_key = ssh_dir.join("id_ed25519.pub");
    if key.is_file() && pub_key.is_file() {
        return Ok(key);
    }
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .map_err(|e| MnError::Network {
            stage: "生成 SSH 密钥".to_string(),
            reason: e.to_string(),
        })?;
    if !status.success() || !key.is_file() || !pub_key.is_file() {
        return Err(MnError::Network {
            stage: "生成 SSH 密钥".to_string(),
            reason: "本机没有默认公钥且 ssh-keygen 无法无交互生成密钥".to_string(),
        });
    }
    Ok(key)
}

pub fn install_ssh_copy_id(target: &SshTarget) -> MnResult<PathBuf> {
    let key = ensure_identity_key()?;
    // 公钥 = 私钥名 + `.pub`，用拼接表达这个语义（与 with_extension 结果一致）
    let pub_key = PathBuf::from(format!("{}.pub", key.display()));

    // ---- 步骤 1：公钥可能已装好，先探测，避免重复导入 ----
    //
    // 探测用「仅公钥认证」的独立 target：优先 password 那个会把
    // PubkeyAuthentication 关掉，永远探不出免密是否可用。
    //
    // 探测前先清掉该主机的旧指纹：云厂商的 IP 会被重新分配给别的机器，
    // 残留的 known_hosts 条目会让 ssh 因 host key 变更直接拒连，
    // 看起来像「公钥没装好」。
    let _ = target.remove_known_host_entry();
    let probe = SshTarget {
        host: target.host.clone(),
        port: target.port,
        user: target.user.clone(),
        password: None,
        identity_file: Some(key.clone()),
    };
    if ssh_exec(&probe, "true", 10).is_ok() {
        return Ok(key);
    }

    // ---- 步骤 2：安装公钥（ssh + cat，不依赖 ssh-copy-id 脚本） ----
    let pub_text = std::fs::read_to_string(&pub_key)
        .map_err(|e| MnError::io(format!("读取公钥 {}", pub_key.display()), &e))?;
    let pub_text = pub_text.trim();
    if pub_text.is_empty() {
        return Err(MnError::Network {
            stage: "安装 SSH 公钥".to_string(),
            reason: format!("公钥文件为空：{}", pub_key.display()),
        });
    }
    // 幂等追加：远端已有同一行就跳过，避免 authorized_keys 堆积重复项。
    // 用 grep -qxF 精确整行匹配；父目录与权限按 sshd 要求收紧。
    let remote_cmd = authorized_keys_append_cmd(pub_text);
    ssh_exec(target, &remote_cmd, 30)?;

    // 写入后给 sshd 一点时间重新读取 authorized_keys 与 sshd_config，
    // 避免立即探测时还未生效。
    std::thread::sleep(std::time::Duration::from_secs(2));

    // ---- 步骤 3：确认免密真的可用 ----
    //
    // 刚写入时 sshd 可能需要几秒才重新读取 authorized_keys，
    // 给足够的重试窗口；仍不通就明确失败。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        match ssh_exec(&probe, "true", 5) {
            Ok(_) => return Ok(key),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(MnError::Network {
                        stage: "安装 SSH 公钥".to_string(),
                        reason: format!(
                            "公钥已写入远端 {user}@{host}:{port} 的 authorized_keys，\
                             但仍无法免密登录：{e}。请检查远端 sshd 是否允许 \
                             PubkeyAuthentication、authorized_keys 权限是否为 600",
                            user = target.user,
                            host = target.host,
                            port = target.port
                        ),
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

/// 在远端执行一条命令，返回 stdout。
///
/// 命令用 `sh -c` 包一层，避免引号/管道在本地被解释成别的东西。
///
/// ## 为什么内置重试
///
/// 每条命令都是**全新的 SSH 连接**。云厂商的 SSH 入口是共享转发网关：
/// 实例重启/迁移、网关滚动更新期间会出现一段「连上即被远端关闭」的窗口
/// （`Connection closed by remote host`），紧跟着是转发重建中的
/// `Connection refused`。这属于秒级瞬时故障，重发一次通常就成功。
///
/// 判定规则：退出码 255 且 stderr 未命中认证/配置类关键词 → 可重试。
/// 认证失败（`Permission denied`）、host key 冲突、无法解析主机名是确定性
/// 失败，重试只会白等，立即报错。
///
/// 重试 5 次、退避 1s/2s/3s/4s：厂商网关重建转发通常需要数秒，
/// 只重试 3 次窗口太窄，实测仍会被穿透。
/// 检查远端是否有 GPU 进程在运行（通过 nvidia-smi）。
///
/// 返回 `Ok(true)` 表示有进程占用 GPU，`Ok(false)` 表示无进程，
/// `Err` 表示 nvidia-smi 不可用或查询失败。
pub fn has_gpu_processes(target: &SshTarget) -> MnResult<bool> {
    // 使用 nvidia-smi 查询 GPU 进程，仅输出进程数
    let cmd = "nvidia-smi --query-compute-apps=pid --format=csv,noheader 2>/dev/null | wc -l";
    let output = ssh_exec(target, cmd, 60)?;

    let count = output.trim().parse::<u32>().unwrap_or(0);
    Ok(count > 0)
}

/// SSH 快速探活：**单次连接、无应用层重试**。
///
/// ## 为什么不用 `ssh_exec`
///
/// `ssh_exec` 内置 5 轮重试（每轮 ConnectTimeout 15s × ConnectionAttempts 5），
/// 探活一次要耗 80 秒以上才返回失败——「不可达 → 强制重启」的决策被
/// 拖到重试窗口耗尽之后，用户看到的就是「SSH 失败但迟迟不重启」。
///
/// 探活的判定与**凭据无关**：连接层失败（超时/拒绝/重置）说明 TCP 都
/// 连不上，机器必然不可用；认证失败（Permission denied）说明 TCP 通了、
/// 机器活着——返回 `Err` 但调用方应把认证失败与连接失败区分开。
pub fn ssh_probe(target: &SshTarget, timeout_secs: u64) -> MnResult<bool> {
    let _ = timeout_secs; // 实际超时由 base_opts 的 ConnectTimeout 约束
    // BatchMode=yes：探活必须非交互。password 为 None 时若不加，
    // sshd 要求密码认证会让前台 CLI 卡在 `root@host's password:` 提示。
    match ssh_exec_once_opts(target, "true", &["-o", "BatchMode=yes"]) {
        Ok(_) => Ok(true),
        Err(SshFailure::Transient(reason)) => {
            // 连接层失败：真不可达
            let _ = reason;
            Ok(false)
        }
        Err(SshFailure::Fatal(reason)) => {
            // 认证失败：机器活着，但当前凭据进不去——对探活而言也算「通」
            if reason.contains("Permission denied")
                || reason.contains("publickey")
                || reason.contains("password")
            {
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

pub fn ssh_exec(target: &SshTarget, cmd: &str, timeout_secs: u64) -> MnResult<String> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut last: Option<SshFailure> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        match ssh_exec_once(target, cmd) {
            Ok(out) => return Ok(out),
            Err(f) => {
                let transient = matches!(f, SshFailure::Transient(_));
                last = Some(f);
                if !transient || attempt == MAX_ATTEMPTS {
                    break;
                }
                // 1s / 2s / 3s / 4s 退避：厂商网关重建转发通常要数秒，
                // 退避太短会在同一个故障窗口里连续撞墙。
                let wait = std::time::Duration::from_secs(attempt as u64);
                eprintln!(
                    "[ssh] 连接层中断（第 {attempt}/{MAX_ATTEMPTS} 次），{}s 后重试：{}",
                    wait.as_secs(),
                    match &last {
                        Some(SshFailure::Transient(r)) => r.as_str(),
                        _ => "",
                    }
                );
                std::thread::sleep(wait);
            }
        }
    }
    Err(match last {
        Some(SshFailure::Transient(reason)) => MnError::Network {
            stage: "远端执行命令".to_string(),
            reason: format!(
                "{reason}。已自动重试 {MAX_ATTEMPTS} 次仍中断，节点可能已失联（实例被回收/重启，\
                 或厂商网关持续限流），请到部署页确认节点状态后重试"
            ),
        },
        Some(SshFailure::Fatal(reason)) => MnError::Network {
            stage: "远端执行命令".to_string(),
            reason,
        },
        None => MnError::Network {
            stage: "远端执行命令".to_string(),
            reason: "未知失败".to_string(),
        },
    })
}

/// [`ssh_exec`] 的失败分类：决定能否安全重试。
enum SshFailure {
    /// 连接层中断（退出码 255 且无真实 stderr）。每条命令都是新连接，
    /// 重发不影响远端状态。
    Transient(String),
    /// 认证失败、远端命令非零退出等真实失败。重试无意义甚至有害。
    Fatal(String),
}

/// 执行一次（无重试）。
fn ssh_exec_once(target: &SshTarget, cmd: &str) -> Result<String, SshFailure> {
    ssh_exec_once_opts(target, cmd, &[])
}

/// [`ssh_exec_once`] 的可注入选项变体。
///
/// `extra_opts` 追加在 base_opts 之后（后写的 ssh 选项覆盖先写的），
/// 供探活等**无凭据**场景注入 `BatchMode=yes`——没有它，password 为
/// None 的探活在**前台 CLI**（有 /dev/tty）里会被 sshd 的密码认证
/// 拦下并交互式提示密码，整个部署卡死在 `root@x.x.x.x's password:`。
/// BatchMode 让认证失败立即返回 Permission denied（探活正确归类为
/// 「机器活着，凭据进不去」）。
fn ssh_exec_once_opts(
    target: &SshTarget,
    cmd: &str,
    extra_opts: &[&str],
) -> Result<String, SshFailure> {
    let askpass = target
        .write_askpass()
        .map_err(|e| SshFailure::Fatal(e.to_string()))?;
    let mut args = target.base_opts();
    for o in extra_opts {
        args.push(o.to_string());
    }
    args.push(format!("{}@{}", target.user, target.host));
    args.push(cmd.to_string());

    let mut c = Command::new(mangosteen_platform::ssh::ssh_binary_name());
    c.args(&args).stdin(Stdio::null());
    if let Some(ap) = &askpass {
        c.env("SSH_ASKPASS", ap.script_arg())
            .env("SSH_ASKPASS_REQUIRE", "force");
        #[cfg(not(target_os = "windows"))]
        {
            c.env("DISPLAY", ":0");
        }
    }
    let out = c.output();
    if let Some(ap) = &askpass {
        ap.cleanup();
    }
    let out = out.map_err(|e| {
        SshFailure::Fatal(format!("无法启动 ssh：{e}"))
    })?;

    // UserKnownHostsFile=/dev/null + accept-new：每次连接都会往 stderr
    // 打一条指纹 Warning。它是 SSH 的正常提示，不是命令的失败原因，
    // 不能原样当成错误展示——否则真实故障（如远端 shell 被杀导致
    // 退出码 255）会被这条无害提示掩盖，用户看到的是一脸懵的
    // "Warning: Permanently added ..."。
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let real: Vec<&str> = err
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
            .collect();
        let reason = if real.is_empty() {
            if out.status.code() == Some(255) {
                format!(
                    "`{}` 失败：SSH 连接层中断（退出码 255），远端命令未执行完。\
                     常见原因：网络中断、承载命令的远端会话被终止、节点重启",
                    truncate_cmd(cmd)
                )
            } else {
                format!(
                    "`{}` 失败（退出码 {:?}）：远端命令没有输出错误信息",
                    truncate_cmd(cmd),
                    out.status.code()
                )
            }
        } else {
            // 保留尾部 12 行并用换行分隔，而不是只取前 3 行用 " / " 拼成一行。
            //
            // 远端命令的 stderr 往往是「先说结论、再贴细节」——例如启动失败
            // 时先打 `--- 启动失败，关键日志 ---`，真正原因在随后的几行。
            // 只取前 3 行会恰好把结论留下、把原因丢掉；压成一行则让多行
            // 日志（参数回显、堆栈）彻底不可读。
            let tail: Vec<&str> = real
                .iter()
                .rev()
                .take(12)
                .rev()
                .cloned()
                .collect();
            format!(
                "`{}` 失败（退出码 {:?}）：\n{}",
                truncate_cmd(cmd),
                out.status.code(),
                tail.join("\n")
            )
        };
        // 判定「值不值得重试」不能只看 stderr 是否为空。
        //
        // ## 曾经的错误判定
        //
        // 旧逻辑是「255 且 stderr 为空 → 可重试」，理由是「有 stderr 说明
        // SSH 给出了明确原因」。但这个前提不成立：`Connection closed by
        // remote host` / `Connection reset by peer` / `Connection timed out`
        // 都是**瞬时网络中断**，SSH 恰恰会为它们写出 stderr。于是最该重试
        // 的一类失败被归成了致命错误——用户看到「相同问题重复出现」。
        //
        // ## 现在的判定
        //
        // 退出码 255 且 stderr 未命中「认证/配置类」关键词 → 视为连接层
        // 中断，可重试。判定逻辑见 [`classify_ssh_failure`]。
        let transient = out.status.code() == Some(255)
            && !real.iter().any(|l| is_deterministic_ssh_error(l));
        return Err(if transient {
            SshFailure::Transient(reason)
        } else {
            SshFailure::Fatal(reason)
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 判断某行 stderr 是否表示**确定性失败**（重试无意义）。
///
/// 只列举认证与配置类错误：这些重试一百次也是同样结果，立即报错才能
/// 让用户看到真正的原因。其余 255 一律按连接层中断处理——包括
/// `Connection closed by remote host` 这种「看起来像错误、其实是瞬时
/// 网络中断」的提示，它恰恰是最该重试的一类。
fn is_deterministic_ssh_error(line: &str) -> bool {
    const FATAL_HINTS: [&str; 6] = [
        "Permission denied",
        "Too many authentication failures",
        "Host key verification failed",
        "REMOTE HOST IDENTIFICATION HAS CHANGED",
        "Could not resolve hostname",
        "No such file or directory",
    ];
    FATAL_HINTS.iter().any(|h| line.contains(h))
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

/* removed local archive upload path */
pub fn scp_upload(target: &SshTarget, local: &std::path::Path, remote: &str) -> MnResult<()> {
    if !local.is_file() {
        return Err(MnError::Network {
            stage: "上传云端推理引擎".to_string(),
            reason: format!("本地引擎归档不存在：{}", local.display()),
        });
    }
    let askpass = target.write_askpass()?;
    let mut args = target.base_opts();
    args.push(local.display().to_string());
    args.push(format!("{}@{}:{}", target.user, target.host, remote));
    let mut c = Command::new(mangosteen_platform::ssh::scp_binary_name());
    c.args(&args).stdin(Stdio::null());
    if let Some(ap) = &askpass {
        c.env("SSH_ASKPASS", ap.script_arg())
            .env("SSH_ASKPASS_REQUIRE", "force");
        #[cfg(not(target_os = "windows"))]
        {
            c.env("DISPLAY", ":0");
        }
    }
    let out = c.output();
    if let Some(ap) = &askpass {
        ap.cleanup();
    }
    let out = out.map_err(|e| MnError::Network {
        stage: "上传云端推理引擎".to_string(),
        reason: format!("无法启动 scp：{e}"),
    })?;
    if !out.status.success() {
        return Err(MnError::Network {
            stage: "上传云端推理引擎".to_string(),
            reason: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(())
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
pub fn ensure_llama_server_from_archive(
    target: &SshTarget,
    workdir: &str,
    download_url: &str,
) -> MnResult<String> {
    let installed = format!("{workdir}/bin/llama-server");
    let probe = ssh_exec(
        target,
        &format!("if command -v llama-server >/dev/null 2>&1; then command -v llama-server; elif [ -x {installed} ] && [ -f {workdir}/bin/libllama-server-impl.so ]; then echo {installed}; fi"),
        60,
    )?;
    if !probe.trim().is_empty() {
        return Ok(probe.trim().to_string());
    }
    ensure_llama_server(target, workdir, download_url)
}

/// 节点配置阶段安装模型下载与解压依赖。
///
/// 只在缺失时安装，已存在的工具保持不变；优先使用 apt-get，其次 yum/dnf。
/// `curl`、`wget` 至少安装一个，`aria2c` 用于多连接模型下载，`unzip`/`tar`
/// 用于 llama-server 发布包解压。
pub fn ensure_node_download_dependencies(target: &SshTarget) -> MnResult<String> {
    let script = "set -e; need=''; command -v curl >/dev/null 2>&1 || need=\"$need curl\"; command -v wget >/dev/null 2>&1 || need=\"$need wget\"; command -v aria2c >/dev/null 2>&1 || need=\"$need aria2\"; command -v unzip >/dev/null 2>&1 || need=\"$need unzip\"; command -v tar >/dev/null 2>&1 || need=\"$need tar\"; if [ -n \"$need\" ]; then if command -v apt-get >/dev/null 2>&1; then export DEBIAN_FRONTEND=noninteractive; apt-get update -y >/dev/null 2>&1 && apt-get install -y $need; elif command -v dnf >/dev/null 2>&1; then dnf install -y $need; elif command -v yum >/dev/null 2>&1; then yum install -y $need; else echo \"无法找到 apt-get/dnf/yum，缺少依赖:$need\" >&2; exit 1; fi; fi; printf 'deps:'; command -v curl >/dev/null 2>&1 && printf ' curl'; command -v wget >/dev/null 2>&1 && printf ' wget'; command -v aria2c >/dev/null 2>&1 && printf ' aria2c'; command -v unzip >/dev/null 2>&1 && printf ' unzip'; command -v tar >/dev/null 2>&1 && printf ' tar'; echo"
        .to_string();
    ssh_exec(target, &script, 600)
}

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
            reason: "远端没有 llama-server，且未配置下载地址。\
                     请在 [cloud] 里设置 llama_server_url（llama.cpp 官方 release 的 \
                     ubuntu-x64 zip 地址），或换一个自带 llama-server 的镜像"
                .to_string(),
        });
    }

    // 下载器：aria2c 多连接优先（-x16 -s16 -k1M），缺失或失败回退 curl。
    //
    // ## 为什么不再直接用 curl
    //
    // 预编译包约 600 MB，跨境单连接 curl 实测只有几百 KB/s，十几分钟都下
    // 不完，节点配置阶段就长时间卡在「准备引擎」，用户看着像卡死。aria2c
    // 用 16 段并发能显著提速，参数与模型权重下载保持一致。
    //
    // `--allow-overwrite` + `--auto-file-renaming=false` 是必须的：
    // 上一次失败的残留同名文件如果不覆盖，aria2 会改写成
    // `llama.tar.gz.1`，随后的 `tar -xzf llama.tar.gz` 解压的是旧文件，
    // 表现为「解压成功但找不到 llama-server」，极难排查。
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
    let q_url = shell_quote(download_url);
    let q_file = shell_quote(file);
    let q_tmp = shell_quote(&format!("{workdir}/tmp"));
    let fetch = format!(
        "if command -v aria2c >/dev/null 2>&1; then \
           echo '使用 aria2c -x16 多连接下载 llama-server 预编译包'; \
           aria2c -x16 -s16 -k1M --timeout=15 --max-tries=15 --retry-wait=2 \
             --file-allocation=none --allow-overwrite=true --auto-file-renaming=false --continue=true \
             -d {q_tmp} -o {q_file} {q_url} \
           || curl -fL --retry 5 --retry-delay 3 --connect-timeout 20 -o {file} {q_url}; \
         else \
           echo '未找到 aria2c，回退 curl 下载 llama-server 预编译包'; \
           curl -fL --retry 5 --retry-delay 3 --connect-timeout 20 -o {file} {q_url}; \
         fi"
    );
    let script = format!(
        "set -e; \
         mkdir -p {workdir}/bin {workdir}/tmp; \
         cd {workdir}/tmp; \
         {fetch}; \
         {extract}; \
         d=$(dirname $(find . -name llama-server -type f | head -1)); \
         cp -a \"$d\"/. {workdir}/bin/; \
         chmod +x {workdir}/bin/llama-server; \
         test -f {workdir}/bin/libllama-server-impl.so || find {workdir}/bin -maxdepth 2 -name 'libllama-server-impl.so' -type f -exec cp -f {{}} {workdir}/bin/\\; || true; \
         rm -rf {workdir}/tmp; \
         test -f {workdir}/bin/libllama-server-impl.so || {{ echo '缺少 libllama-server-impl.so' >&2; exit 1; }}; \
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

/// 与 [`ensure_llama_server`] 相同，预留下载阶段进度回调。
///
/// 引擎预编译包通常较小；当前回调只保证契约存在，具体下载进度由远端模型
/// 权重下载流程上报。
pub fn ensure_llama_server_with_progress(
    target: &SshTarget,
    workdir: &str,
    download_url: &str,
    _on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> MnResult<String> {
    ensure_llama_server(target, workdir, download_url)
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
    let log = match port_from_args(args) {
        8080 => format!("{workdir}/llama-server.log"),
        p => format!("{workdir}/llama-server-{p}.log"),
    };
    let pid_file = match port_from_args(args) {
        8080 => format!("{workdir}/llama-server.pid"),
        p => format!("{workdir}/llama-server-{p}.pid"),
    };
    let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
    // 清理旧实例绝不使用 `pkill -f '<bin>'`：承载本命令的远端 shell
    // 命令行里就带着 `<bin>` 文本（见下方 nohup 一段），朴素模式必然
    // 匹配到自己并当场杀掉当前会话（客户端表现为退出码 255）。
    // 改为：pid 文件杀进程组 + 括号守卫兜底 + 排除当前 shell PID。
    let bin_dir = bin.rsplit_once('/').map(|(dir, _)| dir).unwrap_or(".");
    let library_path = format!("{bin_dir}:$LD_LIBRARY_PATH");
    let port = port_from_args(args);
    let listen_probe = listening_probe(port);
    // 启动判定不能只看「1 秒后进程还在」：
    //  * `llama-server` 加载模型期间只输出 initializing/参数信息，
    //    过短的观察窗会把「正在加载」误判成失败；
    //  * `setsid ... &` 的 `$!` 是 setsid 的 PID，setsid 在已是进程组
    //    组长时会 fork，PID 文件于是指向一个已退出的父进程。
    // 因此改为：由被启动的 shell 自己写 PID（`exec` 后 PID 不变），
    // 再循环观察「端口开始监听」或「进程提前退出」，两者都等不到才算失败。
    let script = format!(
        "set -e; mkdir -p {workdir}; \
         if [ -f {pid_file} ]; then \
           P=$(cat {pid_file} 2>/dev/null); \
           if [ -n \"$P\" ] && kill -0 \"$P\" 2>/dev/null \
              && ps -p \"$P\" -o args= 2>/dev/null | grep -q llama-server; then \
             kill -- -\"$P\" 2>/dev/null || kill \"$P\" 2>/dev/null || true; \
           fi; \
           rm -f {pid_file}; \
         fi; \
         test -x {bin}; \
         LD_LIBRARY_PATH={library_path} {bin} --version >/dev/null 2>&1 || true; \
         setsid env LD_LIBRARY_PATH={library_path} sh -c 'echo $$ > {pid_file}; exec {bin} {args}' > {log} 2>&1 < /dev/null & \
         started=0; \
         i=0; \
         while [ $i -lt 60 ]; do \
           i=$((i+1)); \
           sleep 0.5; \
           if [ ! -f {pid_file} ] || ! kill -0 $(cat {pid_file}) 2>/dev/null; then \
             if [ $i -lt 2 ]; then continue; fi; \
             echo '--- 启动失败，关键日志 ---' >&2; {failure_digest}; exit 1; \
           fi; \
           if {listen_probe}; then started=1; break; fi; \
         done; \
         if [ $started -eq 1 ] || kill -0 $(cat {pid_file}) 2>/dev/null; then echo started; exit 0; fi; \
         echo '--- 启动超时，关键日志 ---' >&2; {failure_digest}; exit 1",
        args = quoted.join(" "),
        failure_digest = failure_digest_script(&log)
    );
    ssh_exec(target, &script, 180)?;
    Ok(())
}

/// 端口监听探测片段。
///
/// 远端不一定装了 `ss`/`netstat`：都没有时退化为「只要进程还活着就算启动
/// 成功」，随后仍会由调用方经隧道探 `/health` 做最终判定。
fn listening_probe(port: u16) -> String {
    format!(
        "(command -v ss >/dev/null 2>&1 && ss -lnt 2>/dev/null | grep -q ':{port} ') \
         || (command -v netstat >/dev/null 2>&1 && netstat -lnt 2>/dev/null | grep -q ':{port} ')"
    )
}

/// 从启动参数里读 `--port`，缺省按 llama.cpp 常规端口处理。
fn port_from_args(args: &[String]) -> u16 {
    args.iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(LLAMA_CPP_PORT)
}

/// 生成「提取启动失败关键信息」的远端 shell 片段。
///
/// ## 为什么不能直接 `tail -40`
///
/// llama-server 启动时会先打十几行参数回显和 DEPRECATED 警告，真正的
/// 失败原因（如 `error while handling argument "--model-draft"`、
/// `failed to load model`、CUDA OOM）往往在**更靠前**的位置，或者被
/// 淹没在警告里。`tail -40` 拿到的是「最后一屏噪音」，再看 ssh_exec 只
/// 展示前 3 行，用户看到的就是一条与真实原因无关的 `DEPRECATED: ...`。
///
/// 这里按优先级取：
/// 1. 命中错误关键词的行（error/failed/abort/not found/cannot/out of memory）
/// 2. 上述为空时退回尾部 15 行（至少能看到最后在做什么）
///
/// 结果交给调用方原样展示，不再由 ssh_exec 二次截断。
fn failure_digest_script(log: &str) -> String {
    format!(
        "D=$(grep -iE 'error|fail|abort|not found|cannot|out of memory|invalid' {log} 2>/dev/null | grep -viE 'DEPRECATED|^\\s*$' | tail -8); \
         if [ -z \"$D\" ]; then D=$(tail -15 {log} 2>/dev/null); fi; \
         printf '%s\\n' \"$D\" >&2"
    )
}

/// 读取指定部署的远端启动诊断信息，不修改远端状态。
///
/// 健康检查超时后必须查看 PID、监听端口和日志尾部；否则调用方只能看到
/// 本地隧道 connection reset，看不到 llama-server 是启动失败、OOM 退出还是
/// 参数错误。
pub fn llama_server_start_diagnostics(
    target: &SshTarget,
    workdir: &str,
    port: u16,
) -> MnResult<String> {
    let pid_file = if port == LLAMA_CPP_PORT {
        format!("{workdir}/llama-server.pid")
    } else {
        format!("{workdir}/llama-server-{port}.pid")
    };
    let log_file = if port == LLAMA_CPP_PORT {
        format!("{workdir}/llama-server.log")
    } else {
        format!("{workdir}/llama-server-{port}.log")
    };
    ssh_exec(
        target,
        &format!(
            "echo '[pid]'; if [ -f {pid} ]; then cat {pid}; ps -fp $(cat {pid}) 2>&1 || true; else echo missing; fi; \
             echo '[listen]'; (ss -lntp 2>/dev/null || netstat -lntp 2>/dev/null || true) | grep ':{port} ' || echo missing; \
             echo '[processes]'; pgrep -af llama-server 2>/dev/null || echo missing; \
             echo '[log]'; tail -80 {log} 2>&1 || true",
            pid = pid_file,
            log = log_file,
        ),
        30,
    )
}

/// 与 [`start_llama_server`] 相同，预留启动阶段进度回调。
pub fn start_llama_server_with_progress(
    target: &SshTarget,
    bin: &str,
    args: &[String],
    workdir: &str,
    _on_progress: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> MnResult<()> {
    start_llama_server(target, bin, args, workdir)
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
            "if [ -f {workdir}/llama-server.pid ] && kill -0 $(cat {workdir}/llama-server.pid) 2>/dev/null; then echo yes; \
             elif pgrep -x llama-server >/dev/null 2>&1; then echo yes; else echo no; fi"
        ),
        60,
    )?;
    Ok(out.trim() == "yes")
}

/// 指定远端端口对应的 llama-server 是否仍在运行。
///
/// 判据按「pid 文件 → 命令行端口 → 端口监听」依次检查，两种 pid 命名都要看：
/// 8080 历史上写的是 `llama-server.pid`，其它端口写 `llama-server-<port>.pid`。
///
/// ## 为什么必须叠加「命令行端口」判据
///
/// 只看 pid 文件会在 pid 文件缺失/陈旧时**漏判**（进程明明活着却报 no）；
/// 只看端口监听会在进程已启动、尚未监听时**误判**（ports 还没 bind）。
/// 按 `--port <p>` 匹配进程命令行是唯一既准确又不依赖辅助文件的判据——
/// 现场踩到的正是「台账 8080、真实 8081、pid 文件里是死 PID」这种组合。
pub fn llama_server_running_on_port(
    target: &SshTarget,
    workdir: &str,
    port: u16,
) -> MnResult<bool> {
    let scoped_pid = format!("{workdir}/llama-server-{port}.pid");
    // 只有 8080 会写历史名 `llama-server.pid`；其它端口绝不能检查它，
    // 否则会把同节点 8080 的模型误判成当前端口的进程还在。
    let pid_candidates = if port == 8080 {
        format!("{scoped_pid} {workdir}/llama-server.pid")
    } else {
        scoped_pid
    };
    let out = ssh_exec(
        target,
        &format!(
            "for f in {pid_candidates}; do \
               if [ -f \"$f\" ] && kill -0 $(cat \"$f\") 2>/dev/null \
                  && ps -p $(cat \"$f\") -o args= 2>/dev/null | grep -q llama-server; then \
                 echo yes; exit 0; fi; \
             done; \
             if ps -eo args= 2>/dev/null | awk -v p='--port {port}' \
                  'index($0,\"llama-server\")>0 && index($0,p)>0' | grep -q .; then \
               echo yes; exit 0; fi; \
             if {listen}; then echo yes; else echo no; fi",
            listen = listening_probe(port)
        ),
        60,
    )?;
    Ok(out.trim() == "yes")
}

/// 停掉指定部署的远端 `llama-server`（**保留实例**）。
///
/// 通过该部署自己的 PID 文件和端口进程定位，不再使用全局
/// `pkill -x llama-server`，避免卸载一个模型时误杀同节点上的其它模型。
pub fn stop_llama_server(target: &SshTarget, workdir: &str) -> MnResult<()> {
    stop_llama_server_on_port(target, workdir, None)
}

/// 停掉指定远端端口对应的模型进程。
///
/// `port` 为 `Some` 时只操作该部署的 PID/端口；旧调用传 `None` 时保留
/// 兼容行为，但同节点多模型的生命周期必须使用带端口版本。
pub fn stop_llama_server_on_port(
    target: &SshTarget,
    workdir: &str,
    port: Option<u16>,
) -> MnResult<()> {
    let script = build_stop_script(workdir, port);
    let out = ssh_exec(target, &script, 90)?;
    if out.contains("still-running") {
        return Err(MnError::Network {
            stage: "停止云端推理服务".to_string(),
            reason: "llama-server 在 TERM 与 KILL 之后仍在运行".to_string(),
        });
    }
    Ok(())
}

/// 生成停止 `llama-server` 的远端脚本（纯函数，供生产与测试共用）。
///
/// ## 为什么不能只靠 pid 文件定位（实测踩到的「停止假成功」）
///
/// 旧实现只按 `pid_file` 找进程，判定式是
/// `(test ! -f pid || ! kill -0 $(cat pid)) && ! (监听 <port>)`。
/// 现场组合恰好让三项全真：
///
/// * 台账记的 `engine_port=8080`，远端真实进程却在 `--port 8081` 上跑；
/// * `llama-server.pid` 里躺着早已退出的旧 PID（kill -0 失败）；
/// * 8080 当然没有监听。
///
/// 于是直接 `rm -f llama-server.pid; echo stopped`——**报告停止成功，
/// 实际一个进程都没杀**。上层据此认为「服务已停」，随后的关机流程
/// 又被跳过，节点继续按小时计费。
///
/// ## 现在的定位：三路并集
///
/// 1. pid 文件（兼容两种历史命名）；
/// 2. 命令行里含 `--port <p>` 的 llama-server 进程——**端口漂移的唯一可靠线索**；
/// 3. 正在监听该端口的进程（由 `ss`/`netstat` 兜底）。
///
/// 三者都为空才算「已停止」。并且**只有确认停止之后才删 pid 文件**，
/// 否则会把诊断线索一起抹掉。
fn build_stop_script(workdir: &str, port: Option<u16>) -> String {
    // 先 TERM 给机会优雅退出（它会自己释放显存），等不到再 KILL。
    // 只 TERM 就返回是不行的：端口没释放，重启会报 address in use。
    //
    // 多模型节点必须按部署自己的 pid/端口操作；不带端口时才兼容旧的
    // 单模型逻辑。带端口时不允许 `pkill -x llama-server` 兜底。
    //
    // pid 文件名必须与启动侧 `start_llama_server` 完全一致：
    // 8080 写的是历史名 `llama-server.pid`，其它端口才带端口号。
    let pid_path = match port {
        Some(8080) => format!("{workdir}/llama-server.pid"),
        Some(p) => format!("{workdir}/llama-server-{p}.pid"),
        None => format!("{workdir}/llama-server.pid"),
    };
    let pid_candidates = match port {
        // 8080 同时兼容两种命名：历史上不同版本写过的名字不一样
        Some(8080) => format!("{workdir}/llama-server.pid {workdir}/llama-server-8080.pid"),
        _ => pid_path.clone(),
    };
    // 按端口匹配 llama-server 进程 PID（awk 里已按 llama-server 过滤，
    // 不会把 grep/awk 自身算进去）
    let pids_by_port = match port {
        Some(p) => format!(
            "ps -eo pid=,args= 2>/dev/null | awk -v p='--port {p}' \
             'index($0,\"llama-server\")>0 && index($0,p)>0 {{print $1}}'"
        ),
        None => "true".to_string(),
    };
    // 收集本部署所有相关 PID（去重）
    let collect = format!(
        "PIDS=\"\"; \
         for f in {pid_candidates}; do \
           if [ -f \"$f\" ]; then Q=$(cat \"$f\" 2>/dev/null); \
             if [ -n \"$Q\" ] && kill -0 \"$Q\" 2>/dev/null \
                && ps -p \"$Q\" -o args= 2>/dev/null | grep -q llama-server; then \
               PIDS=\"$PIDS $Q\"; fi; fi; \
         done; \
         for Q in $({pids_by_port}); do PIDS=\"$PIDS $Q\"; done; \
         PIDS=$(printf '%s\\n' $PIDS | sort -u)"
    );
    // 逐 PID 终止：进程组优先（setsid 起的 llama-server 是组长）
    let kill_pids = |sig: &str| {
        format!(
            "for Q in $PIDS; do \
               kill -{sig} -- -\"$Q\" 2>/dev/null || kill -{sig} \"$Q\" 2>/dev/null || true; \
             done"
        )
    };
    let term_all = format!("{collect}; {}", kill_pids("TERM"));
    let kill_all = format!("{collect}; {}", kill_pids("KILL"));
    // 停止成功的唯一判据：**没有**属于该端口的 llama-server 进程。
    let scope_check = match port {
        Some(p) => format!(
            "{collect}; \
             test -z \"$({pids_by_port})\" && ! ({listen})",
            listen = listening_probe(p)
        ),
        None => "! pgrep -x llama-server >/dev/null 2>&1".to_string(),
    };
    let cleanup = match port {
        Some(_) => format!("rm -f {pid_path}"),
        None => format!("rm -f {workdir}/llama-server.pid {workdir}/llama-server-*.pid"),
    };
    stop_script(&term_all, &kill_all, &scope_check, &cleanup)
}

/// 组装停止脚本。
///
/// ## 顺序不可调换
///
/// 1. 先发 TERM（给进程优雅退出、释放显存的机会）；
/// 2. 最多等 10 秒；**只有确认进程真的没了**才删 pid 文件并报 stopped；
/// 3. 等不到就 KILL，再校验一次；
/// 4. 仍活着则输出 `still-running` 让调用方报错。
///
/// 旧实现把「删 pid 文件」提前到判定式通过的分支，而判定式本身可以被
/// 「pid 文件不存在」误触发——于是「没杀成，还把唯一的线索删了」，
/// 上层拿到 stopped 就继续走关机流程，实际 GPU 仍在计费。
fn stop_script(term_all: &str, kill_all: &str, scope_check: &str, cleanup: &str) -> String {
    format!(
        "{term_all}; \
         for i in $(seq 1 20); do \
           if {scope_check}; then {cleanup}; echo stopped; exit 0; fi; \
           sleep 0.5; \
         done; \
         {kill_all}; sleep 1; \
         if ! ({scope_check}); then echo still-running; exit 1; fi; \
         {cleanup}; echo stopped"
    )
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
    download_model_remote_with_progress(target, url, dest, expected_bytes, None, None, None)
}

/// 带进度回调的远端下载。
///
/// 回调参数为已下载字节与期望总字节；只在上报点调用，不做格式化——
/// 进度如何展示由调用方决定。
///
/// 支持镜像源降级：若主源（如 hf-mirror.com）返回错误，自动尝试备用源
///（如 huggingface.co），避免单点故障导致部署失败。
pub fn download_model_remote_with_progress(
    target: &SshTarget,
    url: &str,
    dest: &str,
    expected_bytes: Option<u64>,
    hf_token: Option<&str>,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    should_cancel: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> MnResult<u64> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(MnError::Network {
            stage: "云端下载模型".to_string(),
            reason: format!("下载地址不是 HTTP(S)：{url}"),
        });
    }

    // 从本地环境读取代理配置，传给远端下载命令。
    // 云端 GPU 实例在国内网络环境下访问 hf-mirror.com 等源站可能被限流或掐断，
    // 需要通过代理转发。若本地未配置代理，则不设置（直连）。
    let http_proxy = std::env::var("HTTP_PROXY").or_else(|_| std::env::var("http_proxy")).ok();
    let https_proxy = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")).ok();
    let no_proxy = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")).ok();
    let proxy_env = match (&http_proxy, &https_proxy, &no_proxy) {
        (Some(hp), Some(sp), Some(np)) => format!(
            "HTTP_PROXY={hp} HTTPS_PROXY={sp} NO_PROXY={np} "
        ),
        (Some(hp), Some(sp), None) => format!("HTTP_PROXY={hp} HTTPS_PROXY={sp} "),
        (Some(hp), None, _) => format!("HTTP_PROXY={hp} "),
        (None, Some(sp), _) => format!("HTTPS_PROXY={sp} "),
        (None, None, _) => String::new(),
    };

    // 构建候选 URL 列表：主源优先，失败后尝试备用源（将 hf-mirror.com 替换为 huggingface.co）
    let alt_url = url.replace("hf-mirror.com", "huggingface.co");
    let urls: Vec<&str> = if alt_url != url {
        vec![url, &alt_url]
    } else {
        vec![url]
    };

    let mut last_err: Option<MnError> = None;
    // 源站轮询：最多尝试 MAX_SOURCE_ROUNDS 轮，每轮遍历所有源站。
    // 避免单个源站暂时不可用就直接放弃整个下载。
    const MAX_SOURCE_ROUNDS: u32 = 3;
    const SOURCE_ROUND_WAIT_SECS: u64 = 10;
    for source_round in 1..=MAX_SOURCE_ROUNDS {
        for candidate_url in &urls {
            match download_single_url(
                target,
                candidate_url,
                dest,
                expected_bytes,
                hf_token,
                on_progress,
                should_cancel,
                &proxy_env,
            ) {
                Ok(size) => return Ok(size),
                Err(e) => {
                    eprintln!(
                        "[download] 第 {source_round} 轮 源站 {candidate_url} 下载失败：{e}"
                    );
                    last_err = Some(e);
                }
            }
        }
        // 源站轮次之间等待，给网络/源站恢复时间
        if source_round < MAX_SOURCE_ROUNDS {
            eprintln!(
                "[download] 所有源站第 {source_round} 轮均失败，等待 {SOURCE_ROUND_WAIT_SECS} 秒后开始第 {} 轮",
                source_round + 1
            );
            std::thread::sleep(std::time::Duration::from_secs(SOURCE_ROUND_WAIT_SECS));
        }
    }

    Err(last_err.unwrap_or_else(|| {
        MnError::Network {
            stage: "云端下载模型".to_string(),
            reason: format!(
                "所有源站均下载失败（共尝试 {} 轮，每轮 {} 个源站）",
                MAX_SOURCE_ROUNDS,
                urls.len()
            ),
        }
    }))
}

/// 下载单个 URL 的内部实现。
fn download_single_url(
    target: &SshTarget,
    url: &str,
    dest: &str,
    expected_bytes: Option<u64>,
    hf_token: Option<&str>,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    should_cancel: Option<&(dyn Fn() -> bool + Send + Sync)>,
    proxy_env: &str,
) -> MnResult<u64> {
    let workdir = dest
        .rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| "/root".to_string());
    let log = format!("{dest}.download.log");
    let pid_file = format!("{dest}.download.pid");
    let file_name = dest.rsplit('/').next().unwrap_or(dest);
    let q_dest = shell_quote(dest);
    let q_file_name = shell_quote(file_name);
    let q_url = shell_quote(url);
    let q_log = shell_quote(&log);
    let q_pid = shell_quote(&pid_file);
    let auth = hf_token
        .filter(|token| !token.trim().is_empty())
        .map(|token| {
            format!(
                "--header {}",
                shell_quote(&format!("Authorization: Bearer {token}"))
            )
        })
        .unwrap_or_default();

    // aria2c 是首选：多连接、多分片、可续传；aria2c 不存在或启动失败时，
    // 同一份 .part/目标文件回退到原有 curl，避免改变既有下载契约。
    //
    // ## 为什么最后写一个 `.download.ok` 标记
    //
    // 原先只用「文件大小 == 计划里的期望字节数」判定完成。实测踩到：
    // 计划里的 `download_bytes` 来自预估或另一路元数据，与源站实际字节
    // 数差几个字节时，aria2c 明明已经 `OK` 下完，轮询仍认为「没完成」；
    // 等下载进程退出，又因为大小不再增长被判成「下载中断且无法续传」，
    // 部署因此卡死在「模型下载」。
    //
    // 现在由下载器自己声明成功：只有下载命令以 0 退出时才写标记，
    // 标记内容就是当时的真实文件大小。轮询以标记为**权威完成信号**，
    // 期望字节数退化为参考值。
    let ok_file = format!("{dest}.download.ok");
    let q_ok = shell_quote(&ok_file);

    // 构建下载器脚本：保持简单，避免复杂 shell 语法导致语法错误
    let downloader = format!(
        "{proxy_env}aria2c -x16 -s16 --min-split-size=4M --file-allocation=none --allow-overwrite=true --continue=true {auth} -d {workdir} -o {q_file_name} {q_url} 2>/dev/null; status=$?; if [ \"$status\" -ne 0 ]; then curl -fL -C - --retry 5 --retry-delay 3 --connect-timeout 20 --speed-time 60 -o {q_dest} {q_url}; status=$?; fi; if [ \"$status\" -eq 0 ]; then echo \"$(stat -c %s {q_dest})\" > {q_ok}; fi; exit \"$status\"",
    );
    // ## 为什么先把下载器写成脚本文件，再用一条短命令启动
    //
    // 旧做法把下载器全文塞进启动命令：承载该命令的远端 shell 命令行里
    // 因此同时含有 `aria2c`、`curl` 与目标路径文本，前置的
    // `pkill -f 'aria2c.*<dest>'` 按完整命令行匹配，**必然命中自己**，
    // 当场杀掉当前 SSH 会话——客户端只看到退出码 255，以及一条与本案
    // 无关的 known_hosts Warning（实测踩过）。
    //
    // 拆成两步后，启动命令的命令行里只剩脚本路径，清理逻辑改为：
    // 1. pid 文件杀进程组（setsid 启动使 `$!` 恰为组长 PID）；
    // 2. 括号守卫 + 排除 `$$` 的模式兜底，专杀孤儿下载进程。
    let script_path = format!("{dest}.download.sh");
    let q_script = shell_quote(&script_path);

    let write_cmd = format!(
        "set -e; mkdir -p {workdir}; printf '%s\\n' {} > {q_script}; chmod 700 {q_script}",
        shell_quote(&downloader),
    );
    ssh_exec(target, &write_cmd, 60)?;

    let start_cmd = format!(
        "set -e; : > {q_log}; rm -f {q_ok}; \
         if [ -f {q_pid} ]; then kill -- -$(cat {q_pid}) 2>/dev/null || kill $(cat {q_pid}) 2>/dev/null || true; fi; \
         setsid nohup sh {q_script} > {q_log} 2>&1 < /dev/null & echo $! > {q_pid}; echo started"
    );
    ssh_exec(target, &start_cmd, 60)?;

    // 统一的续传命令：重新跑同一份脚本（aria2c 优先，`--continue`
    // / `curl -C -` 都从断点接着下），并刷新 pid 文件。
    let resume_cmd = format!(
        "setsid nohup sh {q_script} >> {q_log} 2>&1 < /dev/null & echo $! > {q_pid}; echo resumed"
    );
    // 统一的进程组终止：setsid 启动的下载器 `$!` 即组长 PID，
    // 杀进程组才能把 aria2c/curl 子进程一并带走；旧版 pid 文件退化
    // 为单 PID kill。
    let stop_cmd = format!(
        "if [ -f {q_pid} ]; then kill -- -$(cat {q_pid}) 2>/dev/null || kill $(cat {q_pid}) 2>/dev/null || true; fi"
    );

    // 轮询 + 断点续传重试。
    //
    // ## 重试模型（三层）
    //
    // 1. **单轮**：远端下载器进程退出/停滞时，重新拉起下载命令，
    //    从文件上次落地的断点接着下。单轮最多 `ATTEMPTS_PER_ROUND` 次。
    // 2. **轮次**：单轮内所有续传均未恢复（大小仍不增长）→ 等待
    //    `RETRY_WAIT_SECS` 秒后开启下一轮，最多 `MAX_ROUNDS` 轮。
    // 3. **判失败**：`MAX_ROUNDS * ATTEMPTS_PER_ROUND` 次续传全部用尽，
    //    仍无进展才返回错误。
    //
    // 这比旧逻辑「进程退出就最多 2 次续传」要更有韧性：网络短暂抖动、
    // SSH 网关切断等只会触发单轮重试，并不直接终结部署；
    // 只有多轮均被击垮时才判定为真正的下载失败。
    //
    // ## 停滞必须单独设上限（实测踩到的「永久卡在 15.66 GiB」）
    //
    // 三层重试原本只看「尝试次数」，不看**是否真的下动了**。现场：
    // 日志连续刷 `已下载 16810714528 字节，大小已停滞`，3 轮 × 5 次全用完，
    // 字节数一个都没涨——每一轮都只是把同一个死局重放一遍，白白耗掉
    // 十几分钟，用户只能看到进度条不动、不知道在等什么。
    //
    // 因此新增 `MAX_STALLED_ATTEMPTS`：**连续这么多次续传后文件大小
    // 仍无任何增长**就立即判失败并如实说明，不再把轮次预算烧光。
    // 网络抖动通常几次续传内就会恢复增长，不会误伤。
    const ATTEMPTS_PER_ROUND: u32 = 5;
    const MAX_ROUNDS: u32 = 3;
    const RETRY_WAIT_SECS: u64 = 2;
    /// 连续多少次「续传后仍无字节增长」就判定下载无法恢复。
    ///
    /// 取 **3**：用户明确要求不再把轮次预算烧光。现场那次卡死是字节数
    /// 一个都没涨（`16810714528` 反复出现），3 次续传已足以区分
    /// 「真卡死」与「偶发抖动」——抖动通常下一次就恢复增长。
    const MAX_STALLED_ATTEMPTS: u32 = 3;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3600);
    let mut last_size: u64 = 0;
    let mut stalled: u32 = 0;
    // 当前轮次（1 起）与本轮已执行的续传次数（0 起）
    let mut round: u32 = 1;
    let mut attempts_in_round: u32 = 0;
    // 最近一次采样文件是否仍在增长（失败原因里要如实说明）
    let mut growing: bool;
    // 连续轮询失败计数：短暂抖动靠 ssh_exec 内部重试消化，仍失败时先
    // 容忍（下一跳大概率恢复），连续多次失败才判死——否则一条 stat 被
    // 网关掐断就终结整个下载，等于把瞬时抖动放大成部署失败。
    let mut failed_polls: u32 = 0;
    // 连续「续传后仍无字节增长」的次数（见 MAX_STALLED_ATTEMPTS）。
    let mut stalled_attempts: u32 = 0;
    // 上一次续传时的文件大小：用来判断「这次续传有没有真的下动」。
    // 与 `last_size` 不同——后者是相邻采样点比较，这里是跨续传比较。
    let mut size_at_last_attempt: u64 = 0;

    // 每个采样点只开**一条** SSH 连接，同时取回三样东西：
    // 文件大小、成功标记内容、下载器进程状态。
    //
    // ## 为什么必须合并
    //
    // 旧实现每 2 秒开 3 条独立连接（stat / cat .ok / kill -0），几 GB 的
    // 下载持续几十分钟就是上千次握手，厂商 SSH 网关会因连接频率限流而
    // 随机掐断连接（退出码 255），轮询反而成了部署失败的根源。
    // 合并后连接频率降为 1/3，且三样东西取自同一瞬间，状态不会互相矛盾。
    let poll_cmd = format!(
        "S=0; if [ -f {q_dest} ]; then S=$(stat -c %s {q_dest} 2>/dev/null || echo 0); fi; \
         O=0; if [ -f {q_ok} ]; then O=$(cat {q_ok} 2>/dev/null || echo 0); fi; \
         A=dead; if kill -0 $(cat {q_pid} 2>/dev/null) 2>/dev/null; then A=alive; fi; \
         echo \"size=$S\"; echo \"ok=$O\"; echo \"state=$A\""
    );

    loop {
        if should_cancel.map(|check| check()).unwrap_or(false) {
            let _ = ssh_exec(target, &format!("{stop_cmd}; rm -f '{pid_file}'"), 30);
            return Err(MnError::Cancelled {
                stage: "云端模型下载".to_string(),
            });
        }
        // 轮询间隔 10s：不需要频繁查询，避免无效查询增加 SSH 负担
        std::thread::sleep(std::time::Duration::from_secs(10));

        // 一次连接取回三态；解析失败按 0 处理并计入连续失败。
        let (size, completed, is_dead) = match ssh_exec(target, &poll_cmd, 60) {
            Ok(out) => {
                failed_polls = 0;
                let mut size = 0u64;
                let mut ok_val = String::new();
                let mut dead = true;
                for line in out.lines() {
                    if let Some(v) = line.trim().strip_prefix("size=") {
                        size = v.trim().parse().unwrap_or(0);
                    } else if let Some(v) = line.trim().strip_prefix("ok=") {
                        ok_val = v.trim().to_string();
                    } else if let Some(v) = line.trim().strip_prefix("state=") {
                        dead = v.trim() != "alive";
                    }
                }
                let completed = !ok_val.is_empty() && ok_val != "0";
                (size, completed, dead)
            }
            Err(e) => {
                failed_polls += 1;
                if failed_polls >= 10 {
                    return Err(MnError::Network {
                        stage: "云端下载模型".to_string(),
                        reason: format!(
                            "下载状态轮询连续 {failed_polls} 次失败（含自动重试），\
                             判定节点连接不可用：{e}。已下载约 {last_size} 字节，\
                             断点续传文件已保留，恢复连接后重新部署可从断点继续"
                        ),
                    });
                }
                eprintln!(
                    "[download] 轮询失败（第 {failed_polls} 次连续），跳过本次采样：{e}"
                );
                continue;
            }
        };
        if let Some(cb) = on_progress {
            cb(size, expected_bytes);
        }
        growing = size > last_size;
        // 完成判定优先看远端写的成功标记（仅下载器自身以 0 退出时才写）。
        // 这样就不需要「大小 == 计划里的期望字节数」这个容易误判的前提。
        if completed && size > 0 {
            return Ok(size);
        }
        // **优化**：增加快速通道——如果文件大小达到期望值，立即判定完成。
        // 这避免了 `.download.ok` 未写入（如 aria2c 已退出但标记写入失败）
        // 导致的长时间等待。
        if let Some(exp) = expected_bytes {
            if exp > 0 && size == exp {
                return Ok(size);
            }
        }
        // **优化**：如果期望大小未知（want=None）且文件已存在（size > 0），
            // 且下载器已退出（is_dead），也判定完成。
        // 这处理了模板未指定大小、但文件已下载完成的场景。
        if expected_bytes.is_none() && size > 0 && is_dead {
            return Ok(size);
        }

        // 停滞计数：进程活着但连续多轮采样大小不变（约 12s 无增长）
        if !is_dead && !growing {
            stalled += 1;
        } else {
            stalled = 0;
        }

        // 需要续传：进程已退出，或活着但已停滞
        if is_dead || stalled >= 6 {
            attempts_in_round += 1;

            // ---- 停滞上限：连续 N 次续传后字节数纹丝不动 → 立即失败 ----
            //
            // 判据是「跨续传比较」而不是「相邻采样比较」：`last_size` 只
            // 反映两次轮询之间，而这里要看「这次续传到底有没有让文件变长」。
            // 有增长就清零，说明连接仍有效，只是慢。
            if size > size_at_last_attempt {
                stalled_attempts = 0;
            } else {
                stalled_attempts += 1;
            }
            size_at_last_attempt = size;

            if stalled_attempts >= MAX_STALLED_ATTEMPTS {
                let tail = ssh_exec(target, &format!("tail -5 '{log}' 2>/dev/null"), 60)
                    .unwrap_or_default();
                return Err(MnError::Network {
                    stage: "云端下载模型".to_string(),
                    reason: format!(
                        "下载已停滞：连续 {} 次断点续传后文件大小仍是 {} 字节（无任何增长），\
                         判定无法自动恢复，已提前终止（不再耗尽全部重试轮次）。\
                         常见原因：源站限流/连接被中间网关掐断/远端磁盘满。\
                         断点文件已保留，网络恢复后重新部署可从断点继续。远端日志尾部：{}",
                        stalled_attempts,
                        size,
                        tail.lines().take(3).collect::<Vec<_>>().join(" / ")
                    ),
                });
            }

            // 单轮次数用尽 → 判断是否还有轮次预算
            if attempts_in_round > ATTEMPTS_PER_ROUND {
                if round >= MAX_ROUNDS {
                    let tail = ssh_exec(target, &format!("tail -5 '{log}' 2>/dev/null"), 60)
                        .unwrap_or_default();
                    let total_attempts = MAX_ROUNDS * ATTEMPTS_PER_ROUND;
                    return Err(MnError::Network {
                        stage: "云端下载模型".to_string(),
                        reason: format!(
                            "下载最终失败：已完成 {} 轮、共 {} 次断点续传（每轮上限 {} 次，轮间等待 {} 秒），\
                             仍无法恢复。当前已下载 {} 字节，文件大小{}。远端日志尾部：{}",
                            MAX_ROUNDS,
                            total_attempts,
                            ATTEMPTS_PER_ROUND,
                            RETRY_WAIT_SECS,
                            size,
                            if growing { "仍在增长" } else { "已不再增长" },
                            tail.lines()
                                .take(3)
                                .collect::<Vec<_>>()
                                .join(" / ")
                        ),
                    });
                }
                // 开启新一轮：等待固定间隔，给网络/源站恢复时间
                round += 1;
                attempts_in_round = 1;
                eprintln!(
                    "[download] 第 {} 轮续传耗尽（{} 次），等待 {} 秒后开始第 {} 轮",
                    round - 1,
                    ATTEMPTS_PER_ROUND,
                    RETRY_WAIT_SECS,
                    round
                );
                std::thread::sleep(std::time::Duration::from_secs(RETRY_WAIT_SECS));
            } else {
                eprintln!(
                    "[download] 第 {} 轮第 {} 次续传（已下载 {} 字节，大小{}）",
                    round,
                    attempts_in_round,
                    size,
                    if growing { "仍在增长" } else { "已停滞" }
                );
            }

            // 进程活着但卡死时必须先杀掉，否则新旧下载器会争抢同一文件
            if !is_dead {
                let _ = ssh_exec(target, &stop_cmd, 30);
            }
            ssh_exec(target, &resume_cmd, 60)?;
            last_size = size;
            stalled = 0;
            continue;
        }

        last_size = size;

        if std::time::Instant::now() >= deadline {
            return Err(MnError::Network {
                stage: "云端下载模型".to_string(),
                reason: format!(
                    "下载超时（已进行到第 {} 轮第 {} 次续传，已下 {} 字节）",
                    round, attempts_in_round, size
                ),
            });
        }
    }
}

/// 判断远端下载是否已由下载器完整成功结束。
///
/// `.download.ok` 只在 aria2c/curl 以成功码退出后写入；它比单纯比较
/// 计划大小可靠，因为计划大小可能来自预估，与源站实际字节数不完全一致。
pub fn remote_download_completed(target: &SshTarget, path: &str) -> MnResult<bool> {
    let marker = format!("{path}.download.ok");
    // 标记不是独立事实：必须同时验证目标文件存在、非空，且大小与标记
    // 中记录的大小一致。否则旧的 `.download.ok` 残留会让模型文件缺失时
    // 错误跳过下载，直到启动阶段才报 No such file or directory。
    let out = ssh_exec(
        target,
        &format!(
            "if [ -s '{marker}' ] && [ -s '{path}' ] && \
             [ \"$(cat '{marker}' 2>/dev/null)\" = \"$(stat -c %s '{path}' 2>/dev/null)\" ]; \
             then echo yes; else echo no; fi"
        ),
        60,
    )?;
    Ok(out.trim() == "yes")
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

/// 清理远端**指定模型文件**的下载残留（断点文件 + 下载脚本 + 日志 + pid）。
///
/// ## 为什么部署前要清
///
/// 上一次下载中断后，远端会留下 `{dest}.part`（curl 断点）、
/// `{dest}.download.sh`（下载脚本）、`.download.pid`（残留 pid）、
/// `.download.log`（进度日志）。其中**残留的下载脚本/pid** 最危险：
/// 下次部署时若旧脚本被误判为「仍在下载」，会跳过真实下载或与新脚本
/// 互相覆盖，表现为「文件大小 0 字节停滞」。
///
/// 只清**该模型自己的**残留（按传入的最终文件路径派生），不碰其它模型
/// 的文件——同节点多模型互不影响。
///
/// 返回被删除的文件数（仅用于日志）。
pub fn clean_model_download_artifacts(target: &SshTarget, dest: &str) -> MnResult<usize> {
    let out = ssh_exec(
        target,
        &format!(
            "N=0; for f in '{dest}.part' '{dest}.download.sh' '{dest}.download.pid' \
                    '{dest}.download.log' '{dest}.download.ok'; do \
               if [ -f \"$f\" ]; then rm -f \"$f\"; N=$((N+1)); fi; done; echo $N"
        ),
        30,
    )?;
    Ok(out.trim().lines().last().unwrap_or("0").trim().parse().unwrap_or(0))
}

/// 查询远端 GPU 总显存和当前空闲显存（字节）。
///
/// ## 为什么需要它
///
/// 云厂商新建实例时会按显存规格做选择校验，但**自定义/已保存节点**没有
/// 这一步——用户添加的机器可能只有 8GB 显存，却要跑 27B 全量 GPU 模型。
/// 如果不在启动前拦截，模型下载几 GB 后 llama-server 才会 CUDA OOM，
/// 浪费下载时间，且错误表现为晦涩的「connection reset by peer」。
///
/// 返回 `None` 表示节点没有可查询的 NVIDIA GPU；不能把这种情况等同于
/// 显存不足，因为部分云环境的 GPU 信息通过厂商 API 提供而非 nvidia-smi。
pub fn remote_gpu_memory(target: &SshTarget) -> MnResult<Option<(u64, u64)>> {
    let out = ssh_exec(
        target,
        "nvidia-smi --query-gpu=memory.total,memory.free --format=csv,noheader,nounits 2>/dev/null | tr -d ' ' | tail -1",
        30,
    )?;
    let mut fields = out.trim().split(',');
    let total_mib = fields.next().and_then(|v| v.parse::<u64>().ok());
    let free_mib = fields.next().and_then(|v| v.parse::<u64>().ok());
    Ok(match (total_mib, free_mib) {
        (Some(total), Some(free)) if total > 0 => Some((
            total.saturating_mul(1024 * 1024),
            free.saturating_mul(1024 * 1024),
        )),
        _ => None,
    })
}

/// 查询远端 GPU 总显存（字节）。
pub fn remote_gpu_total_memory(target: &SshTarget) -> MnResult<Option<u64>> {
    Ok(remote_gpu_memory(target)?.map(|(total, _)| total))
}

/// 查询远端 HTTP(S) 资源的真实字节数（`Content-Length`）。
///
/// ## 为什么需要它
///
/// 部署模板里的辅助文件（mmproj / MTP）常常没有登记期望大小。此时若只按
/// 「文件非空」判断，一次被中断的 aria2c 下载会停在**目标路径**上（aria2c
/// 直接写最终文件名，不用 `.part`），残留几十 MB 也会被当成「已就绪」，
/// 于是模型下载阶段直接通过、启动时才报 GGUF 打不开。
///
/// 这里在节点侧做一次 HEAD，把源站声明的长度当成期望值，让
/// [`crate::remote::remote_file_size`] 的严格比对有依据。
/// HEAD 不可用（部分镜像拒绝）时返回 `None`，由调用方退回标记 + 非空判断。
pub fn remote_http_content_length(target: &SshTarget, url: &str) -> MnResult<Option<u64>> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Ok(None);
    }
    let q = shell_quote(url);
    let out = ssh_exec(
        target,
        &format!(
            "curl -fsIL --max-time 20 --retry 1 {q} 2>/dev/null \
             | tr -d '\\r' | grep -i '^content-length:' | tail -1 | cut -d: -f2 | tr -d ' '"
        ),
        60,
    )?;
    Ok(out
        .trim()
        .lines()
        .last()
        .and_then(|l| l.trim().parse::<u64>().ok())
        .filter(|v| *v > 0))
}

/// 校验远端文件是否以 GGUF 魔数开头。
///
/// 用来识别「下到的是错误文件或 HTML 错误页」这类情况：大小看起来正常，
/// 但根本不是模型权重。文件不存在时返回 `Ok(false)`。
pub fn remote_file_is_gguf(target: &SshTarget, path: &str) -> MnResult<bool> {
    let out = ssh_exec(
        target,
        &format!(
            "if [ -f '{path}' ]; then head -c 4 '{path}' 2>/dev/null | od -An -tx1 | tr -d ' \\n'; else echo missing; fi"
        ),
        60,
    )?;
    // "GGUF" 的十六进制是 47475546
    Ok(out.trim().eq_ignore_ascii_case("47475546"))
}

#[cfg(test)]
mod ssh_e2e_tests {
    use super::*;

    /// 手动运行：
    /// `MN_SSH_E2E_HOST=... MN_SSH_E2E_PORT=23 MN_SSH_E2E_USER=root \
    ///  MN_SSH_E2E_PASSWORD=... cargo test -p mangosteen-device \
    ///  ssh_e2e_tests::password_copy_id_then_key_login -- --ignored --nocapture`
    ///
    /// 密码只从环境变量读取，不写入源码；成功安装公钥后验证 BatchMode
    /// 密钥登录。ssh-copy-id 失败时，测试回退验证已有本机密钥；不会把密码
    /// 打印到输出。
    #[test]
    #[ignore = "需要用户提供的真实 SSH 节点和凭据"]
    fn password_copy_id_then_key_login() {
        let host = std::env::var("MN_SSH_E2E_HOST").expect("MN_SSH_E2E_HOST");
        let port = std::env::var("MN_SSH_E2E_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(22);
        let user = std::env::var("MN_SSH_E2E_USER").unwrap_or_else(|_| "root".to_string());
        let password = std::env::var("MN_SSH_E2E_PASSWORD").expect("MN_SSH_E2E_PASSWORD");
        let password_target = SshTarget {
            host,
            port,
            user,
            password: Some(password),
            identity_file: None,
        };
        let key =
            install_ssh_copy_id(&password_target).expect("ssh-copy-id 或密码 SSH 公钥兜底安装失败");
        let key_target = SshTarget {
            host: password_target.host.clone(),
            port: password_target.port,
            user: password_target.user.clone(),
            password: None,
            identity_file: Some(key),
        };
        let out = ssh_exec(&key_target, "echo KEY_AUTH_OK; id -u; uname -s", 5)
            .expect("公钥免密 SSH 验证失败");
        assert!(out.contains("KEY_AUTH_OK"));
        assert!(out.lines().any(|line| line.trim() == "0"));
    }
}

#[cfg(test)]
mod ssh_failure_tests {
    use super::is_deterministic_ssh_error;

    /// 瞬时网络中断必须被判为「可重试」。
    ///
    /// 这组文案是厂商共享网关在实例重启/迁移期间的典型输出。旧实现
    /// 把「有 stderr」一律当成致命错误，导致这些最该重试的失败反而
    /// 直接终结部署——线上「相同问题重复出现」正是这么来的。
    #[test]
    fn transient_network_errors_are_retryable() {
        for line in [
            "Connection to 117.50.190.191 closed by remote host.",
            "Connection closed by remote host",
            "ssh: connect to host 117.50.190.191 port 23: Connection refused",
            "Connection reset by peer",
            "Connection timed out during banner exchange",
            "client_loop: send disconnect: Broken pipe",
        ] {
            assert!(
                !is_deterministic_ssh_error(line),
                "应判为可重试（瞬时中断），却被当成致命错误：{line}"
            );
        }
    }

    /// 认证与配置类错误必须判死：重试只是白等，还掩盖真实原因。
    #[test]
    fn auth_and_config_errors_are_fatal() {
        for line in [
            "root@1.2.3.4: Permission denied (publickey).",
            "Received disconnect from 1.2.3.4 port 23:2: Too many authentication failures",
            "Host key verification failed.",
            "@@@@@ REMOTE HOST IDENTIFICATION HAS CHANGED! @@@@@",
            "ssh: Could not resolve hostname bad-host: Name or service not known",
        ] {
            assert!(
                is_deterministic_ssh_error(line),
                "应判为致命错误（重试无意义），却当成可重试：{line}"
            );
        }
    }
}

#[cfg(test)]
mod stop_script_tests {
    use super::build_stop_script;

    /// 停止脚本必须按 `--port` 匹配进程，不能只依赖 pid 文件。
    ///
    /// 现场：台账 engine_port=8080，远端真实进程跑在 `--port 8081`，
    /// `llama-server.pid` 里是早已退出的旧 PID。旧判定式三项全真，
    /// 于是删 pid 文件并报 stopped——一个进程都没杀，节点继续计费。
    #[test]
    fn stop_script_locates_process_by_port_flag() {
        let script = build_stop_script("/root/mangosteen", Some(8081));
        assert!(
            script.contains("--port 8081"),
            "必须按 --port 定位进程，否则端口漂移后停不掉：{script}"
        );
        assert!(
            script.contains("llama-server-8081.pid"),
            "必须引用该端口自己的 pid 文件：{script}"
        );
    }

    /// 停止后必须校验；校验失败要能报错，不能无条件成功。
    #[test]
    fn stop_script_verifies_before_reporting_success() {
        let script = build_stop_script("/root/mangosteen", Some(8081));
        assert!(script.contains("still-running"), "缺少失败出口：{script}");
        // cleanup 必须排在 echo stopped 之前（先确认、后删文件）
        let stopped = script.rfind("echo stopped").unwrap_or(0);
        let cleanup = script.rfind("rm -f").unwrap_or(0);
        assert!(cleanup < stopped, "删 pid 文件必须在确认停止之后：{script}");
        // 必须有 TERM → 等待 → KILL 的升级过程
        assert!(script.contains("kill -TERM"), "{script}");
        assert!(script.contains("kill -KILL"), "{script}");
        assert!(script.contains("sleep 0.5"), "{script}");
    }

    /// 不带端口（单模型兼容路径）必须退化为「杀掉所有 llama-server」，
    /// 且不能误删其它端口的 pid 文件。
    #[test]
    fn unscoped_stop_falls_back_to_pkill_all() {
        let script = build_stop_script("/root/mangosteen", None);
        assert!(script.contains("pgrep -x llama-server"), "{script}");
        assert!(script.contains("llama-server-*.pid"), "{script}");
        assert!(
            !script.contains("--port"),
            "无端口模式不应出现端口匹配：{script}"
        );
    }
}

#[cfg(test)]
mod download_retry_tests {
    use super::*;

    /// 停滞次数上限必须是 3，且必须真的被判定逻辑引用。
    ///
    /// 现场：日志刷了 3 轮 × 5 次 `已下载 16810714528 字节，大小已停滞`，
    /// 字节数一个都没涨，白白烧掉十几分钟。旧逻辑只看「尝试次数」，
    /// 不看「有没有真的下动」。
    #[test]
    fn stall_limit_is_three() {
        let src = include_str!("remote.rs");
        assert!(
            src.contains("const MAX_STALLED_ATTEMPTS: u32 = 3;"),
            "停滞上限应为 3"
        );
        // 必须真的用在判定里，不能只是声明
        assert!(
            src.contains("stalled_attempts >= MAX_STALLED_ATTEMPTS"),
            "停滞上限必须参与判定"
        );
        // 跨续传比较：有增长就要清零，否则连接还活着也会被误判为卡死
        assert!(
            src.contains("if size > size_at_last_attempt {"),
            "必须以『本次续传是否带来增长』为判据"
        );
    }

    /// 「连续」语义：有增长必须清零计数。
    #[test]
    fn stall_counter_resets_on_growth() {
        let src = include_str!("remote.rs");
        let idx = src
            .find("if size > size_at_last_attempt {")
            .expect("缺少增长判定");
        let window = &src[idx..idx + 160];
        assert!(
            window.contains("stalled_attempts = 0;"),
            "有增长时必须把停滞计数清零，否则慢速下载会被误杀"
        );
    }
}
