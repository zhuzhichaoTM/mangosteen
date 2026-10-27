//! 模型文件下载：`.part` 临时文件 + 断点续传 + SHA-256 校验 + 原子改名。
//!
//! 设计参照 Ollama 的 blob 拉取流程，四个要点：
//!
//! | 机制 | 目的 |
//! |------|------|
//! | 先落 `.part` | 中断不会留下「看着能用实则残缺」的 GGUF |
//! | `--continue-at -` | 网络抖动后重跑从断点继续，不从头下 |
//! | 流式 SHA-256 | 边下边算，不必下完再读一遍 4.5 GiB |
//! | 校验通过才 rename | 同一文件系统内 rename 原子，不会出现半个模型 |
//!
//! 依赖系统的 `curl`：跨平台一致（macOS/Linux/Windows 10+ 均自带），
//! 且天然支持 TLS、重定向、续传，比自带 HTTP 客户端更省事。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use mangosteen_core::source::{ModelRef, ModelSource};
use mangosteen_core::{gguf, MnError, MnResult};

/// 下载结果。
#[derive(Debug, Clone)]
pub struct Downloaded {
    /// 最终文件路径（`.gguf`）
    pub path: PathBuf,
    /// 字节数
    pub bytes: u64,
    /// SHA-256（来自源的官方元数据，或本次实算）
    pub sha256: Option<String>,
    /// 是否复用了断点续传
    pub resumed: bool,
}

/// 进度回调的包装。
///
/// 单独包一层是为了给 `Arc<dyn Fn>` 实现 `Debug` 与 `Clone`：
/// 裸的 trait object 无法 derive `Debug`，而 `DownloadOptions` 需要它。
#[derive(Clone)]
pub struct ProgressSink(pub std::sync::Arc<dyn Fn(u64, Option<u64>) + Send + Sync>);

impl std::fmt::Debug for ProgressSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProgressSink(<callback>)")
    }
}

impl ProgressSink {
    /// 触发回调。
    pub fn call(&self, done: u64, total: Option<u64>) {
        (self.0)(done, total);
    }
}

/// 下载配置。
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// 落盘目录（模型库）
    pub dest_dir: PathBuf,
    /// 单个文件体积上限
    pub max_bytes: u64,
    /// 单次网络操作超时（秒）。`0` 表示不限（大文件慢链路必需）
    pub timeout_secs: u64,
    /// 失败自动重试次数
    pub retries: u32,
    /// curl 可执行文件
    pub curl_bin: String,
    /// 期望的 SHA-256（来自源元数据）。提供则强制校验。
    pub expected_sha256: Option<String>,
    /// 期望大小（来自源元数据）。用于显示进度与提前发现异常。
    pub expected_bytes: Option<u64>,
    /// 进度回调：`(已下载字节, 总字节)`。
    ///
    /// 总字节在源未提供大小时为 `None`——此时调用方只能显示「已下载多少」，
    /// 不能编一个假的百分比。
    ///
    /// 回调在轮询线程里执行，因此必须是 `Send + Sync`。
    /// 不在这里返回 `FnMut` 是因为要跨线程共享。
    pub on_progress: Option<ProgressSink>,
    /// 取消检查：返回 `true` 时中止下载并杀掉 curl。
    ///
    /// ## 为什么需要它
    ///
    /// UI 的「停止」要求真的停下来并回滚。只把任务标成「已取消」而让
    /// curl 继续跑，会有两个后果：① 用户以为停了，磁盘还在涨；
    /// ② 几分钟后文件下完、登记进台账，又冒出个「幽灵模型」。
    ///
    /// ## 实现方式
    ///
    /// 轮询线程每 250ms 调一次；为真时主循环 `kill()` curl 子进程。
    /// 用轮询而不是信号：跨平台（Windows 没有 SIGTERM 的等价语义），
    /// 且 curl 被 kill 后 `.part` 保留，续传能力不受影响。
    pub should_cancel: Option<CancelCheck>,
}

/// 取消检查回调。
///
/// 单独包一层是为了与 [`ProgressSink`] 一样能给 `Arc<dyn Fn>` 实现
/// `Debug`/`Clone`——`DownloadOptions` 需要 `Debug`。
#[derive(Clone)]
pub struct CancelCheck(pub std::sync::Arc<dyn Fn() -> bool + Send + Sync>);

impl CancelCheck {
    /// 是否已请求取消。
    pub fn is_cancelled(&self) -> bool {
        (self.0)()
    }
}

impl std::fmt::Debug for CancelCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancelCheck")
    }
}

impl DownloadOptions {
    /// 构造。
    pub fn new(dest_dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        Self {
            dest_dir: dest_dir.into(),
            max_bytes,
            // 大文件可能跑很久，默认不设总时限；连接级超时由 curl 自己的
            // --connect-timeout 兜住
            timeout_secs: 0,
            retries: 3,
            curl_bin: "curl".to_string(),
            expected_sha256: None,
            expected_bytes: None,
            on_progress: None,
            should_cancel: None,
        }
    }
}

/// 临时文件后缀。
///
/// 以 `.part` 结尾有两个作用：一眼能看出「没下完」，且不会被
/// `models add` 当作可登记的 GGUF（它要求 `.gguf` 结尾）。
const PART_SUFFIX: &str = "part";

/// 远端模型元数据探测结果。
#[derive(Debug, Clone)]
pub struct RemoteMeta {
    /// 完整文件字节数（来自 `Content-Range` 或调用方给的已知大小）
    pub file_size_bytes: u64,
    /// 解析出的 GGUF 信息（架构、层数、KV 头数…）
    pub info: gguf::GgufInfo,
}

/// 预取头部的起始大小。
///
/// 实测：Qwen2.5-0.5B 的元数据段（含 15 万 token 的字符串表）约 6 MB，
/// 8 MB 够用；更大的模型词表可达几十 MB，由 `probe_remote_metadata`
/// 在解析失败时翻倍重试，不必一开始就要几十 MB。
const HEADER_FIRST_BYTES: u64 = 8 * 1024 * 1024;

/// 预取头部的上限。
///
/// 到这一步说明元数据异常大（或文件损坏）。继续翻倍只会让用户干等，
/// 不如如实报「元数据过大，改用按体积估算」。64 MB 在 2~5 秒内可取完。
const HEADER_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// 预取远端的**GGUF 头部**并解析元数据，用于「下载前评估资源」。
///
/// ## 为什么需要它
///
/// 部署流程把「模型评估」放在「模型下载」之前：用户点完部署，
/// 应当先看到「这台机器跑不跑得动、要占多少内存」，再决定要不要
/// 花时间下载几 GB。但资源评估依赖 GGUF 元数据里的真实架构参数
/// （`block_count` / `head_count_kv` / `embedding_length`），这些
/// 只有文件里有。
///
/// 解法：用 `Range: bytes=0-N` 只取文件头部。**大小自适应**——
/// 先用 [`HEADER_FIRST_BYTES`]，若解析到「读取提前结束」说明元数据段
/// 比这更大，翻倍重试直到 [`HEADER_MAX_BYTES`]。
///
/// ## 失败不是致命错误
///
/// 源不支持 Range、网络抖动、元数据超过上限——这些都只应导致
/// **降级为按文件大小估算**，不该让整个部署流程中断。
/// 因此返回 `Err` 由调用方决定怎么降级，而不是在这里 panic。
pub fn probe_remote_metadata(
    source: &ModelSource,
    model: &ModelRef,
    opts: &DownloadOptions,
    header_bytes: u64,
) -> MnResult<RemoteMeta> {
    let url = source.download_url(model)?;
    // 调用方给的 header_bytes 作为起始值；传 0 则用默认。
    let start = if header_bytes == 0 {
        HEADER_FIRST_BYTES
    } else {
        header_bytes
    };

    let mut want = start;
    loop {
        let fetched = fetch_header_bytes(&url, opts, want)?;
        let total = fetched
            .total_bytes
            .or(opts.expected_bytes)
            .unwrap_or(fetched.bytes.len() as u64);

        match gguf::parse_stream(std::io::Cursor::new(&fetched.bytes), total) {
            Ok(info) => {
                return Ok(RemoteMeta {
                    file_size_bytes: total,
                    info,
                })
            }
            Err(e) => {
                // 只有「元数据段没取全」才值得加倍重试。
                // 其它错误（magic 不对、不是模型文件）翻倍也是白费。
                let truncated = is_truncated_error(&e);
                if truncated && want < HEADER_MAX_BYTES {
                    want = (want * 2).min(HEADER_MAX_BYTES);
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// 判断解析失败是否由「头部不够长」引起。
///
/// GGUF 元数据段是顺序读的：头部截断时底层读到流尾，报
/// 「读取提前结束 / failed to fill whole buffer」。只有这种情况
/// 加大预取量才有意义。
fn is_truncated_error(e: &MnError) -> bool {
    let s = e.to_string();
    s.contains("读取提前结束")
        || s.contains("failed to fill whole buffer")
        || s.contains("unexpected end of file")
}

/// 一次 Range 请求的结果。
struct HeaderFetch {
    /// 实际拿到的字节
    bytes: Vec<u8>,
    /// 服务端声明的完整文件大小（`Content-Range: bytes 0-4095/12345678`）
    total_bytes: Option<u64>,
}

/// 用 curl 取 URL 的前 `n` 字节。
///
/// ## 头与体必须分开走
///
/// 最初把 `--dump-header -` 与 `--output -` 都指向 stdout，再按
/// 「最后一个空行」切分——**这是错的**：响应体是二进制 GGUF，
/// 里面完全可能出现 `\r\n\r\n` 字节序列，按它切会把元数据截断，
/// 表现为 `读取提前结束（failed to fill whole buffer）`。
///
/// 所以头走临时文件（`--dump-header <file>`），体走 stdout。
///
/// ## 体为什么不能也落文件
///
/// 若源忽略 Range 直接返回整文件，`--output <file>` 会老老实实
/// 把整个模型（可能 4 GiB）下完才返回——把「2 秒的预取」变成
/// 「几十分钟的下载」。所以体留在 stdout，只读 `n` 字节就主动
/// 关管道、杀进程。
fn fetch_header_bytes(url: &str, opts: &DownloadOptions, n: u64) -> MnResult<HeaderFetch> {
    if n == 0 {
        return Err(MnError::bad_request("预取字节数不能为 0"));
    }

    let dir = std::env::temp_dir();
    let head_path = dir.join(format!(
        "mn-hdr-{}-{}.txt",
        std::process::id(),
        mangosteen_core::time::now_unix_millis()
    ));

    let mut cmd = Command::new(&opts.curl_bin);
    cmd.arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--connect-timeout")
        .arg("15")
        .arg("--max-time")
        // 头部预取在**交互路径**上：用户等着看评估结果，
        // 不能因为源站卡住就一直转圈。
        .arg("30")
        .arg("--header")
        .arg(format!("Range: bytes=0-{}", n - 1))
        .arg("--dump-header")
        .arg(&head_path)
        .arg("--output")
        .arg("-") // 体走 stdout，由我们限长读取
        .arg(url);

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&head_path);
            return Err(MnError::Network {
                stage: "预取模型头部".to_string(),
                reason: format!("无法启动 {}：{e}", opts.curl_bin),
            });
        }
    };

    // 只读 n 字节（+少量余量）后关闭管道。若服务端忽略 Range 返回整文件，
    // 这里提前结束读取，curl 收到 SIGPIPE 自行退出——不会真下完整个模型。
    let mut buf = Vec::with_capacity(n as usize + 8192);
    if let Some(stdout) = child.stdout.take() {
        use std::io::Read;
        let mut limited = stdout.take(n + 8192);
        let _ = limited.read_to_end(&mut buf);
    }
    let _ = child.kill();
    let status = child.wait().ok();
    let head_text = std::fs::read_to_string(&head_path).unwrap_or_default();
    let _ = std::fs::remove_file(&head_path);

    let code = status.and_then(|s| s.code());
    // curl 因 SIGPIPE 退出时码是负数（被信号杀掉），这属于**预期**：
    // 我们主动不读完。只有真正失败（非 0 且非被杀）才算错。
    if !buf.is_empty() {
        // 已经有数据，即使 curl 被我们杀掉也能用
        let total_bytes =
            parse_content_range_total(&head_text).or_else(|| parse_content_length(&head_text));
        return Ok(HeaderFetch {
            bytes: buf,
            total_bytes,
        });
    }

    Err(MnError::Network {
        stage: "预取模型头部".to_string(),
        reason: format!(
            "{url} 未返回数据（curl 退出码 {code:?}）。源可能不支持 Range，\
             或网络不可达"
        ),
    })
}

/// 从响应头里取 `Content-Range: bytes 0-4095/12345678` 的总长度。
///
/// 只看**最后一个** `content-range`：经过代理/重定向时前面的响应头
/// 属于跳转，不是最终文件。
fn parse_content_range_total(head: &str) -> Option<u64> {
    head.lines().rev().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        let rest = lower.strip_prefix("content-range:")?;
        let after_slash = rest.split('/').nth(1)?;
        after_slash.trim().parse().ok()
    })
}

/// 从响应头里取 `Content-Length`。
fn parse_content_length(head: &str) -> Option<u64> {
    // 只看最后一组头（多段重定向时前面的 Content-Length 属于跳转页）
    for line in head.lines().rev() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// 下载一个模型文件。
///
/// 流程：`<name>.gguf.part` → 续传 → 校验 → rename → `<name>.gguf`
pub fn download_model(
    source: &ModelSource,
    model: &ModelRef,
    opts: &DownloadOptions,
    show_progress: bool,
) -> MnResult<Downloaded> {
    let url = source.download_url(model)?;

    std::fs::create_dir_all(&opts.dest_dir)
        .map_err(|e| MnError::io(format!("创建模型目录 {}", opts.dest_dir.display()), &e))?;

    let file_name = model.local_file_name();
    let final_path = opts.dest_dir.join(&file_name);
    let part_path = opts.dest_dir.join(format!("{file_name}.{PART_SUFFIX}"));

    // 已存在且大小吻合 → 不重复下载（幂等：再跑一次 pull 不会重新下 4.5GB）
    if let Some(expected) = opts.expected_bytes {
        if final_path.is_file() {
            if let Ok(meta) = std::fs::metadata(&final_path) {
                if meta.len() == expected {
                    // 已有完整文件：也要回调一次，否则 UI 会停在「等待中」
                    if let Some(cb) = &opts.on_progress {
                        cb.call(meta.len(), Some(expected));
                    }
                    let sha = opts.expected_sha256.clone();
                    return Ok(Downloaded {
                        path: final_path,
                        bytes: meta.len(),
                        sha256: sha,
                        resumed: false,
                    });
                }
            }
        }
    }

    let resume_from = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
    let expected_total = opts
        .expected_bytes
        .or_else(|| resume_from.checked_add(0).filter(|v| *v > 0));

    // 体积上限：已知大小就先拒，避免下了几个 GB 才发现越界
    if let Some(total) = opts.expected_bytes {
        if total > opts.max_bytes {
            return Err(MnError::bad_request(format!(
                "模型 {total} 字节超过配置上限 storage.max_model_bytes={}。\
                 确认要下载请调高该配置",
                opts.max_bytes
            )));
        }
    }

    let mut last_err: Option<MnError> = None;
    for attempt in 0..=opts.retries {
        match run_curl(&url, &part_path, opts, expected_total, show_progress) {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(e) => {
                // 用户主动取消：立即返回，绝不重试。
                // 重试会把用户刚停掉的下载又拉起来——「点了停止反而更忙」。
                if e.is_cancelled() {
                    return Err(e);
                }
                // 404 / 403 这类确定性失败不重试
                if is_permanent(&e) {
                    cleanup_failed(&part_path);
                    return Err(e);
                }
                last_err = Some(e);
                if attempt < opts.retries {
                    std::thread::sleep(Duration::from_millis(500u64 * (attempt as u64 + 1)));
                }
            }
        }
    }
    if let Some(e) = last_err {
        // 保留 .part 以便续传，不删除
        return Err(e);
    }

    // ---- 校验 ----
    let size = std::fs::metadata(&part_path)
        .map_err(|e| MnError::io(format!("读取 {}", part_path.display()), &e))?
        .len();

    if size > opts.max_bytes {
        cleanup_failed(&part_path);
        return Err(MnError::bad_request(format!(
            "下载文件 {size} 字节超过配置上限 storage.max_model_bytes={}",
            opts.max_bytes
        )));
    }
    if let Some(expected) = opts.expected_bytes {
        if size != expected {
            // 大小不符说明传输不完整，保留 .part 让下次续传
            return Err(MnError::Network {
                stage: "下载模型".to_string(),
                reason: format!(
                    "文件大小不符：期望 {expected} 字节，实际 {size} 字节。\
                     已保留下载进度（{}），重新执行可续传",
                    part_path.display()
                ),
            });
        }
    }

    let actual_sha = if let Some(expected) = &opts.expected_sha256 {
        let got = gguf::sha256_file(&part_path)?;
        if !got.eq_ignore_ascii_case(expected) {
            // 哈希不符 = 文件内容坏了，续传也没用，必须丢弃重下
            cleanup_failed(&part_path);
            return Err(MnError::HashMismatch {
                path: part_path,
                expected: expected.clone(),
                actual: got,
            });
        }
        Some(got)
    } else {
        None
    };

    // ---- 原子改名 ----
    // rename 在同一文件系统内是原子的：要么看到完整的 .gguf，
    // 要么什么都没有，不会出现「半个文件被当成模型」的状态。
    std::fs::rename(&part_path, &final_path).map_err(|e| {
        MnError::io(
            format!("重命名 {} → {}", part_path.display(), final_path.display()),
            &e,
        )
    })?;

    Ok(Downloaded {
        path: final_path,
        bytes: size,
        sha256: actual_sha.or(opts.expected_sha256.clone()),
        resumed: resume_from > 0,
    })
}

/// curl 等待结果。
struct WaitOutcome {
    /// 退出状态。被取消时为 `None`（`kill` 后的状态没有意义）
    status: Option<std::process::ExitStatus>,
    /// stderr 文本
    stderr: String,
    /// 是否被用户取消
    cancelled: bool,
}

/// 等 curl 结束，期间响应取消请求。
///
/// ## 为什么不能直接用 `wait_with_output()`
///
/// 那是阻塞调用：用户点「停止」后只能等 curl 自己下完（可能几十分钟），
/// 期间磁盘还在涨。必须轮询 `try_wait`，一旦收到取消就 `kill()`。
///
/// ## stderr 为什么要单独读
///
/// `try_wait` 不会读管道，若 curl 往 stderr 写满缓冲区就会卡死。
/// curl 开了 `--silent --show-error`，正常只输出错误信息（很少），
/// 但为稳妥仍起一个线程把 stderr 读干净。
fn wait_or_cancel(
    child: &mut std::process::Child,
    cancel: Option<&CancelCheck>,
) -> MnResult<WaitOutcome> {
    // 起线程读 stderr，避免管道缓冲写满导致 curl 阻塞
    let stderr_pipe = child.stderr.take();
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let stderr_reader = stderr_pipe.map(|mut pipe| {
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut s = String::new();
            let _ = pipe.read_to_string(&mut s);
            if let Ok(mut g) = buf.lock() {
                *g = s;
            }
        })
    });

    let mut cancelled = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {}
            Err(e) => {
                return Err(MnError::Network {
                    stage: "等待 curl 完成".to_string(),
                    reason: e.to_string(),
                })
            }
        }
        if let Some(c) = cancel {
            if c.is_cancelled() {
                let _ = child.kill();
                // kill 后必须 wait 回收，否则留下僵尸进程
                let _ = child.wait();
                cancelled = true;
                break None;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    if let Some(h) = stderr_reader {
        let _ = h.join();
    }
    let stderr = stderr_buf.lock().map(|g| g.clone()).unwrap_or_default();

    Ok(WaitOutcome {
        status,
        stderr,
        cancelled,
    })
}

/// 判断错误是否为「重试也没用」的确定性失败。
fn is_permanent(e: &MnError) -> bool {
    match e {
        MnError::Network { reason, .. } => {
            // curl 退出码 22 = HTTP >= 400；错误文本里带状态码
            reason.contains("HTTP 404")
                || reason.contains("HTTP 403")
                || reason.contains("HTTP 401")
                || reason.contains("HTTP 400")
        }
        _ => false,
    }
}

/// 清理失败的下载文件。
fn cleanup_failed(part: &Path) {
    let _ = std::fs::remove_file(part);
}

/// 调用 curl 把 `url` 下到 `part`（自动续传）。
fn run_curl(
    url: &str,
    part: &Path,
    opts: &DownloadOptions,
    expected_total: Option<u64>,
    show_progress: bool,
) -> MnResult<()> {
    let mut cmd = Command::new(&opts.curl_bin);
    cmd.arg("--fail") // HTTP >= 400 直接失败，不把错误页写进文件
        .arg("--location") // 跟随重定向（ModelScope 会跳到 CDN）
        .arg("--silent")
        .arg("--show-error")
        .arg("--continue-at")
        .arg("-") // 断点续传
        .arg("--connect-timeout")
        .arg("15")
        .arg("--output")
        .arg(part)
        .arg(url);

    if opts.timeout_secs > 0 {
        cmd.arg("--max-time").arg(opts.timeout_secs.to_string());
    }

    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| MnError::Network {
        stage: "调用 curl 下载".to_string(),
        reason: format!(
            "无法启动 {}：{e}。请确认系统已安装 curl（macOS/Linux 自带；\
             Windows 10 1803+ 自带 curl.exe）",
            opts.curl_bin
        ),
    })?;

    // 单一进度源：轮询 `.part` 文件大小。
    //
    // 为什么不用 curl 自带的进度条：它写 stderr，且与这里轮询出的进度会
    // 争抢终端同一行，输出必然错乱。所以 curl 一侧 `--silent` 关掉，
    // 由这里统一驱动「终端画行」与「回调通知」两条输出。
    //
    // 注意：即使 `show_progress == false`（比如从 Web UI 触发下载），
    // 只要注册了回调就必须起轮询线程——否则界面看不到任何进度。
    let want_progress = show_progress || opts.on_progress.is_some();
    let progress_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let progress_handle = if want_progress {
        let part = part.to_path_buf();
        let stop = progress_stop.clone();
        let sink = opts.on_progress.clone();
        Some(std::thread::spawn(move || {
            let mut last_line = String::new();
            // 每 250ms 采一次；同一秒内多次回调对 UI 没意义，但无害
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

                if let Some(cb) = &sink {
                    cb.call(done, expected_total);
                }
                if !show_progress {
                    continue;
                }

                let line = match expected_total {
                    Some(t) if t > 0 => format!(
                        "  ↓ {:>5.1}%  {} / {}",
                        (done as f64 / t as f64 * 100.0).min(100.0),
                        human_bytes(done),
                        human_bytes(t)
                    ),
                    _ => format!("  ↓ {}", human_bytes(done)),
                };
                // 只在内容变化时重画，避免终端反复刷同一行
                if line != last_line {
                    eprint!("\r{line}   ");
                    last_line = line;
                }
                let _ = std::io::stderr().flush();
            }
            if show_progress && !last_line.is_empty() {
                eprintln!();
            }
        }))
    } else {
        None
    };

    let output = wait_or_cancel(&mut child, opts.should_cancel.as_ref())?;

    progress_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = progress_handle {
        let _ = h.join();
    }

    if let Some(err) = output.cancelled.then(|| MnError::Cancelled {
        stage: "模型下载".to_string(),
    }) {
        // 用户主动中止：不是失败，但必须如实上报，让上层走回滚而不是
        // 当成「网络错误」去重试（重试会把用户刚取消的下载又拉起来）。
        return Err(err);
    }

    let status = match output.status {
        Some(st) => st,
        None => {
            return Err(MnError::Cancelled {
                stage: "模型下载".to_string(),
            })
        }
    };

    if !status.success() {
        let err_text = output.stderr.trim().to_string();
        let code = status.code().unwrap_or(-1);
        // curl 的 22 号退出码 = HTTP 错误；把常见状态码翻成可操作提示
        let detail = if code == 22 {
            if err_text.contains("404") {
                format!(
                    "HTTP 404 —— 文件不存在。请核对仓库名与文件名（大小写敏感）。\
                     原始信息：{err_text}"
                )
            } else if err_text.contains("403") {
                format!("HTTP 403 —— 无权访问。该模型可能需要登录或已下架。原始信息：{err_text}")
            } else {
                format!("HTTP 错误（curl 退出码 22）：{err_text}")
            }
        } else if code == 33 {
            format!("服务器不支持断点续传（curl 退出码 33）。删除 .part 文件后重试：{err_text}")
        } else if code == 6 || code == 7 {
            format!("无法连接主机（curl 退出码 {code}）：{err_text}")
        } else {
            format!("curl 退出码 {code}：{err_text}")
        };
        return Err(MnError::Network {
            stage: "下载模型".to_string(),
            reason: detail,
        });
    }

    // 有些 CDN 在续传完成时仍返回 200 而非 206，curl 会覆盖整个文件，
    // 这不影响正确性（最终大小/哈希校验会兜住）。但若服务器忽略了 Range
    // 又只写了部分内容，大小校验会发现。
    Ok(())
}

/// 字节数转人类可读。
pub fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 速度转人类可读。
pub fn human_speed(bps: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bps;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}/s", UNITS[i])
}

/// 探测远端仓库的文件清单（含官方 SHA-256 与大小）。
///
/// 用 curl 取 JSON 后手工解析：不引入 JSON 之外的依赖，
/// 且响应体很小（几 KB），内存解析完全够用。
pub fn fetch_remote_files(
    source: &ModelSource,
    repo: &str,
    opts: &DownloadOptions,
) -> MnResult<Vec<RemoteFile>> {
    let Some(tpl) = &source.metadata_url_template else {
        return Ok(Vec::new());
    };
    let url = tpl
        .replace("{base}", source.base_url.trim_end_matches('/'))
        .replace("{repo}", repo);

    let mut cmd = Command::new(&opts.curl_bin);
    cmd.arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--max-time")
        .arg("30")
        .arg(&url);

    let out = cmd.output().map_err(|e| MnError::Network {
        stage: "查询模型仓库".to_string(),
        reason: format!("无法启动 curl：{e}"),
    })?;
    if !out.status.success() {
        return Err(MnError::Network {
            stage: "查询模型仓库".to_string(),
            reason: format!(
                "{} 返回失败（退出码 {:?}）：{}",
                url,
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }

    let body = String::from_utf8_lossy(&out.stdout).to_string();
    parse_file_list(source, &body)
}

/// 远端仓库条目（关键字搜索的一条结果）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    /// 仓库全名，如 `Qwen/Qwen2.5-0.5B-Instruct-GGUF`
    pub repo: String,
    /// 下载量（源提供时）
    pub downloads: Option<u64>,
    /// 点赞数（源提供时）
    pub likes: Option<u64>,
    /// 标签（最多保留若干条，用于判断是否 GGUF / 什么架构）
    pub tags: Vec<String>,
    /// 是否已确认包含 GGUF 文件
    pub has_gguf: bool,
}

/// 按关键字搜索模型仓库。
///
/// 与 [`fetch_remote_files`] 的分工：
///
/// | 函数 | 输入 | 输出 | 用户意图 |
/// |------|------|------|---------|
/// | `search_repos` | 关键字（`qwen`） | 仓库列表 | 「有哪些模型可以下」 |
/// | `fetch_remote_files` | 仓库全名 | 文件列表 | 「这个仓库里哪个文件能下」 |
///
/// 源不支持搜索（`search_url_template` 为 `None`）时返回
/// `bad_request` 并**说明该换哪个源**——而不是让用户看到一句
/// 「查询失败」后无路可走。
pub fn search_repos(
    source: &ModelSource,
    query: &str,
    limit: usize,
    opts: &DownloadOptions,
) -> MnResult<Vec<RemoteRepo>> {
    let Some(tpl) = &source.search_url_template else {
        let supported: Vec<&str> = ["hf-mirror", "hf"]
            .iter()
            .copied()
            .filter(|n| *n != source.name)
            .collect();
        return Err(MnError::bad_request(format!(
            "`{}` 不支持按关键字搜索仓库（该源未提供公开检索接口）。\n\
             可选做法：\n\
             · 切换到支持搜索的源：{}\n\
             · 或直接填写完整仓库路径，如 `Qwen/Qwen2.5-0.5B-Instruct-GGUF`",
            source.name,
            supported.join(" / ")
        )));
    };

    // ---- 分页拉取，不过滤格式 ----
    //
    // 早先这里只保留 `has_gguf` 的仓库，但那样会让用 vLLM / SGLang /
    // TensorRT 的用户搜不到需要的 safetensors 仓库。现在**不再按格式过滤**：
    // 结果里带 `has_gguf` 标记，前端据此显示「含 GGUF」徽标，
    // 由用户自己判断要哪个。
    //
    // 仍然翻页累积到 limit，因为上游搜索是模糊匹配、单页命中率不高。
    // 上限保护：最多翻 5 页（每页 100），避免极端情况下无限请求。
    const PAGE: usize = 100;
    const MAX_PAGES: usize = 5;

    let mut out: Vec<RemoteRepo> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for page in 0..MAX_PAGES {
        let offset = page * PAGE;
        let url = tpl
            .replace("{base}", source.base_url.trim_end_matches('/'))
            .replace("{query}", &url_encode(query))
            .replace("{limit}", &PAGE.to_string())
            // 老模板可能没有 `{offset}`：此时所有页都请求同一份数据，
            // 由下面的 seen 去重保证不会重复累加。
            .replace("{offset}", &offset.to_string());

        let mut cmd = Command::new(&opts.curl_bin);
        cmd.arg("--fail")
            .arg("--location")
            .arg("--silent")
            .arg("--show-error")
            .arg("--max-time")
            .arg("30")
            .arg(&url);

        let out_cmd = cmd.output().map_err(|e| MnError::Network {
            stage: "搜索模型仓库".to_string(),
            reason: format!("无法启动 curl：{e}"),
        })?;
        if !out_cmd.status.success() {
            // 第一页就失败 → 如实报错；后续页失败 → 用已拿到的结果，
            // 不因为翻页出问题就让用户什么也搜不到。
            if page == 0 {
                return Err(MnError::Network {
                    stage: "搜索模型仓库".to_string(),
                    reason: format!(
                        "{} 返回失败（退出码 {:?}）：{}",
                        url,
                        out_cmd.status.code(),
                        String::from_utf8_lossy(&out_cmd.stderr).trim()
                    ),
                });
            }
            break;
        }

        let body = String::from_utf8_lossy(&out_cmd.stdout);
        let page_items = parse_repo_list(&body)?;
        let got = page_items.len();
        if got == 0 {
            break; // 没有更多结果
        }

        for r in page_items {
            if !seen.insert(r.repo.clone()) {
                continue; // 跨页重复
            }
            out.push(r);
            if out.len() >= limit {
                break;
            }
        }
        if out.len() >= limit || got < PAGE {
            break; // 够了，或上游已到底
        }
    }

    Ok(out)
}

/// 解析仓库搜索结果。
///
/// HuggingFace 及其镜像返回的是数组，每项含 `id` / `downloads` / `likes` / `tags`。
/// 这里只取这几个字段，其他字段随厂商改版也不影响。
/// 供测试直接调用解析逻辑（`parse_repo_list` 本身不对外）。
#[doc(hidden)]
pub fn parse_repo_list_for_test(body: &str) -> MnResult<Vec<RemoteRepo>> {
    parse_repo_list(body)
}

fn parse_repo_list(body: &str) -> MnResult<Vec<RemoteRepo>> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| MnError::Network {
        stage: "解析搜索结果".to_string(),
        reason: format!(
            "响应不是合法 JSON：{e}。响应前 200 字节：{}",
            truncate(body, 200)
        ),
    })?;

    let items = v.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for it in items {
        let Some(repo) = it.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        let tags: Vec<String> = it
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .take(12)
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let has_gguf = tags.iter().any(|t| t.eq_ignore_ascii_case("gguf"))
            || repo.to_ascii_lowercase().contains("gguf");
        out.push(RemoteRepo {
            repo: repo.to_string(),
            downloads: it.get("downloads").and_then(|x| x.as_u64()),
            likes: it.get("likes").and_then(|x| x.as_u64()),
            tags,
            has_gguf,
        });
    }
    Ok(out)
}

/// 极简 URL 查询参数编码（不引依赖）。
///
/// 只编码在查询串里有特殊含义的字符，并保留常见安全字符。
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 远端文件条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    /// 文件名（仓库内相对路径）
    pub name: String,
    /// 字节数
    pub size: u64,
    /// 官方 SHA-256（源未提供时为 `None`）
    pub sha256: Option<String>,
    /// 模型格式（由文件名推断，如 `gguf` / `safetensors` / `bin` / `onnx` / `其他`）。
    ///
    /// 只做**标识**，不做过滤：用户可能用 vLLM / SGLang / TensorRT 等
    /// 需要 safetensors 的引擎，也可能想自己拿权重文件。
    /// 把格式告诉他，让他自己判断，比替他决定更合适。
    pub format: String,
}

/// 按文件名推断模型格式。识别不了就返回 `其他`，绝不因此拒绝下载。
pub fn detect_format(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gguf") {
        "gguf".to_string()
    } else if lower.ends_with(".safetensors") {
        "safetensors".to_string()
    } else if lower.ends_with(".onnx") {
        "onnx".to_string()
    } else if lower.ends_with(".pt") || lower.ends_with(".pth") || lower.ends_with(".bin") {
        "pytorch".to_string()
    } else if lower.ends_with(".engine") || lower.ends_with(".plan") {
        "tensorrt".to_string()
    } else if lower.ends_with(".json") {
        "config".to_string()
    } else if lower.ends_with(".ggml") {
        "ggml".to_string()
    } else {
        "其他".to_string()
    }
}

/// 是否为 GGUF 容器里的**辅助数据**（不是模型权重）。
///
/// 判断依据是社区通行命名：imatrix 由 llama.cpp 的 `--imatrix` 产出，
/// 文件名通常含 `imatrix`；`mmproj` 是多模态投影层，需配合主模型使用；
/// `LoRA`/`adapter` 是微调增量，不能独立推理。
///
/// 这里用文件名而非读文件头，是因为筛选发生在**远端列表阶段**——
/// 此时文件还没下载，读不到元数据。宁可漏掉个别怪名，
/// 也不要让用户先下 13 MB 再看到「格式错误」。
fn is_auxiliary_gguf(lower_name: &str) -> bool {
    const AUX: [&str; 5] = ["imatrix", "mmproj", "lora", "adapter", "vocab"];
    if AUX.iter().any(|k| lower_name.contains(k)) {
        return true;
    }
    // 分片模型（`...-00001-of-00002.gguf`）也不能单独下载使用：
    // 它们必须全部下齐、由引擎按同名前缀拼接。本项目单文件部署，
    // 列出来只会让用户下到半个模型然后报格式错误。
    lower_name.contains("-of-0000") || lower_name.contains("-of-000")
}

/// 从源的元数据响应里解析出 .gguf 文件清单。
///
/// 两个源的响应结构不同，但都只需 `name` / `size` / `sha256` 三个字段，
/// 因此这里做**结构宽松**的遍历：在 JSON 里找带这三个键的对象，
/// 不依赖具体嵌套层级。这样厂商调整外层包装也不会立刻失效
/// （这正是「配置驱动 / 容忍厂商改版」的落地方式）。
fn parse_file_list(source: &ModelSource, body: &str) -> MnResult<Vec<RemoteFile>> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| MnError::Network {
        stage: "解析仓库元数据".to_string(),
        reason: format!(
            "响应不是合法 JSON：{e}。响应前 200 字节：{}",
            truncate(body, 200)
        ),
    })?;

    let mut out = Vec::new();
    collect_files(&v, source, &mut out);
    // 去重（同一文件可能在多个结构里出现）
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// 递归查找形如 `{name, size}` 的对象。
/// 供测试直接调用文件清单解析。
#[doc(hidden)]
pub fn parse_files_for_test(body: &str) -> MnResult<Vec<RemoteFile>> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| MnError::Network {
        stage: "解析文件列表".to_string(),
        reason: format!("不是合法 JSON：{e}"),
    })?;
    let src = ModelSource::hf_mirror();
    let mut out = Vec::new();
    collect_files(&v, &src, &mut out);
    Ok(out)
}

fn collect_files(v: &serde_json::Value, source: &ModelSource, out: &mut Vec<RemoteFile>) {
    match v {
        serde_json::Value::Array(items) => {
            for it in items {
                collect_files(it, source, out);
            }
        }
        serde_json::Value::Object(map) => {
            let name = map
                .get("Name")
                .or_else(|| map.get("name"))
                .or_else(|| map.get("rfilename"))
                .and_then(|x| x.as_str());
            let size = map
                .get("Size")
                .or_else(|| map.get("size"))
                .and_then(|x| x.as_u64());

            if let (Some(name), Some(size)) = (name, size) {
                // 不再按扩展名过滤：任何格式都可下载。
                //
                // 理由：用户可能用 vLLM / SGLang / TensorRT（需要 safetensors
                // 或编译后的 engine），也可能只是想拿原始权重。替他决定
                // 「只能下 GGUF」是错的。
                //
                // 但**必须标识格式**，并单独标出「辅助数据」
                // （imatrix / mmproj / LoRA），因为这些文件下载后
                // 确实不能直接部署，提前说清楚比事后报错好。
                let sha = map
                    .get("Sha256")
                    .or_else(|| map.get("sha256"))
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_ascii_lowercase())
                    .filter(|s| s.len() == 64);
                let fmt = detect_format(name);
                let lower = name.to_ascii_lowercase();
                let label = if is_auxiliary_gguf(&lower) {
                    format!("{fmt}（辅助数据）")
                } else {
                    fmt
                };
                out.push(RemoteFile {
                    name: name.to_string(),
                    size,
                    sha256: sha,
                    format: label,
                });
            }
            for (_k, sub) in map {
                collect_files(sub, source, out);
            }
            let _ = source;
        }
        _ => {}
    }
}

#[cfg(test)]
mod aux_filter_tests {
    use super::is_auxiliary_gguf;

    /// 辅助数据必须被筛掉，否则用户会下到不能部署的文件。
    #[test]
    fn filters_non_model_gguf() {
        for name in [
            "imatrix_unsloth.gguf",
            "model-imatrix.gguf",
            "mmproj-model-f16.gguf",
            "lora-adapter.gguf",
            "Qwen3.8-27B-BF16-00001-of-00002.gguf",
            "model-00002-of-00003.gguf",
        ] {
            assert!(
                is_auxiliary_gguf(name),
                "`{name}` 是辅助数据，不应出现在可下载列表里"
            );
        }
    }

    /// 正常模型权重不能被误伤。
    #[test]
    fn keeps_real_model_gguf() {
        for name in [
            "qwen2.5-0.5b-instruct-q4_k_m.gguf",
            "qwen2.5-7b-instruct-fp16.gguf",
            "llama-3.2-3b-q8_0.gguf",
        ] {
            assert!(!is_auxiliary_gguf(name), "`{name}` 是正常模型，不应被过滤");
        }
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

/// 写一个「下载记录」sidecar 文件，便于后续 `models verify` 知道期望值。
///
/// 命名 `<file>.sha256`，内容为纯十六进制。这是 Ollama 的 digest 命名思路
/// 的简化版：不建独立 blob 目录，但保留「期望值」以便离线校验。
pub fn write_sha_sidecar(model_path: &Path, sha: &str) -> MnResult<()> {
    let side = model_path.with_extension(format!(
        "{}.sha256",
        model_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("gguf")
    ));
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&side)
        .map_err(|e| MnError::io(format!("写入 {}", side.display()), &e))?;
    f.write_all(sha.as_bytes())
        .map_err(|e| MnError::io(format!("写入 {}", side.display()), &e))?;
    Ok(())
}

/// 读取 sidecar 里记录的期望 SHA-256。
pub fn read_sha_sidecar(model_path: &Path) -> Option<String> {
    let side = model_path.with_extension(format!(
        "{}.sha256",
        model_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("gguf")
    ));
    std::fs::read_to_string(side)
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.len() == 64)
}

/// 打开文件并确保其父目录存在（供测试与调用方使用）。
fn _ensure_parent(path: &Path) -> MnResult<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| MnError::io("创建目录", &e))?;
    }
    Ok(())
}

#[allow(dead_code)]
fn _touch(p: &Path) -> MnResult<File> {
    File::create(p).map_err(|e| MnError::io("创建文件", &e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::source::ModelSource;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(468 * 1024 * 1024), "468.0 MiB");
    }

    /// `Content-Range: bytes 0-4095/12345678` 里 `/` 后面是总长度。
    #[test]
    fn parse_content_range_total_reads_full_size() {
        let head = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4194303/491400032\r\n";
        assert_eq!(parse_content_range_total(head), Some(491_400_032));
    }

    /// 多组响应头（重定向/代理 CONNECT）时，必须取**最后**一组：
    /// 前面那组属于跳转，尺寸不对。
    #[test]
    fn parse_content_range_total_prefers_last_block() {
        let head = "HTTP/1.1 200 Connection Established\r\n\r\n\
                    HTTP/1.1 302 Found\r\nContent-Range: bytes 0-1/999999999\r\n\r\n\
                    HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4194303/491400032\r\n";
        assert_eq!(parse_content_range_total(head), Some(491_400_032));
    }

    #[test]
    fn parse_content_range_total_handles_unknown_star() {
        // 服务端不知道总长时会写 `*`
        let head = "Content-Range: bytes 0-999/*\r\n";
        assert_eq!(parse_content_range_total(head), None);
    }

    /// 源忽略 Range 返回 200 时，退回用 `Content-Length` 当总大小。
    #[test]
    fn parse_content_length_reads_last_occurrence() {
        let head = "HTTP/1.1 302 Found\r\nContent-Length: 12\r\n\r\n\
                    HTTP/1.1 200 OK\r\nContent-Length: 491400032\r\n\r\n";
        assert_eq!(parse_content_length(head), Some(491_400_032));
    }

    /// 头部预取失败**不能**panic，必须返回 Err 让调用方降级。
    #[test]
    fn probe_fails_cleanly_on_unreachable_host() {
        let src = ModelSource::hf_mirror();
        let model = ModelRef {
            source: "hf-mirror".into(),
            repo: "nonexistent/repo-does-not-exist".into(),
            file: "nope.gguf".into(),
            revision: None,
        };
        let mut opts = DownloadOptions::new(std::env::temp_dir().join("mn-probe-nope"), 1 << 30);
        // 用不存在的 curl 立即失败，避免测试依赖网络
        opts.curl_bin = "/nonexistent/curl-binary".to_string();
        let r = probe_remote_metadata(&src, &model, &opts, 4096);
        assert!(r.is_err(), "curl 不存在必须返回 Err 而不是 panic");
    }

    #[test]
    fn human_speed_scales() {
        assert_eq!(human_speed(512.0), "512.0 B/s");
        assert_eq!(human_speed(2048.0), "2.0 KiB/s");
    }

    #[test]
    fn parse_modelscope_file_list() {
        // 真实响应结构的精简版
        let body = r#"{
            "Code": 200,
            "Data": {
                "Files": [
                    {"Name": "qwen2.5-0.5b-instruct-q4_k_m.gguf", "Size": 491400000,
                     "Sha256": "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db"},
                    {"Name": "README.md", "Size": 1234},
                    {"Name": "config.json", "Size": 700}
                ]
            }
        }"#;
        let files = parse_file_list(&ModelSource::modelscope(), body).unwrap();

        // 语义已变更：不再按扩展名过滤。
        //
        // 原先是「只保留 .gguf」，但这挡住了用 vLLM / SGLang / TensorRT
        // 的用户（他们要 safetensors / 编译产物）。现在**任何格式都列出**，
        // 由 `format` 字段标识类型，用户按自己用的引擎判断。
        assert_eq!(files.len(), 3, "任何格式都应列出：{files:?}");

        let by = |n: &str| files.iter().find(|f| f.name == n).unwrap();
        let gguf = by("qwen2.5-0.5b-instruct-q4_k_m.gguf");
        assert_eq!(gguf.size, 491400000);
        assert!(gguf.sha256.is_some());
        assert_eq!(gguf.format, "gguf", "格式必须被标识出来");

        // 非模型文件也列出，但格式清楚标着
        assert_eq!(by("README.md").format, "其他");
        assert_eq!(by("config.json").format, "config");
    }

    #[test]
    fn parse_hf_file_list() {
        let body = r#"{
            "id": "Qwen/Qwen2.5-0.5B-Instruct-GGUF",
            "siblings": [
                {"rfilename": "a-q4_k_m.gguf", "size": 100},
                {"rfilename": "b-f16.gguf", "size": 200}
            ]
        }"#;
        let files = parse_file_list(&ModelSource::huggingface(), body).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "a-q4_k_m.gguf");
        assert_eq!(files[1].size, 200);
    }

    #[test]
    fn parse_rejects_non_json() {
        let e = parse_file_list(&ModelSource::modelscope(), "<html>404</html>").unwrap_err();
        assert!(e.to_string().contains("JSON"), "got {e}");
    }

    #[test]
    fn parse_ignores_short_sha() {
        let body = r#"[{"Name":"x.gguf","Size":1,"Sha256":"abc"}]"#;
        let files = parse_file_list(&ModelSource::modelscope(), body).unwrap();
        assert_eq!(files[0].sha256, None, "长度不是 64 的 sha 必须忽略");
    }

    #[test]
    fn sidecar_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "mn-dl-test-{}-{}",
            std::process::id(),
            mangosteen_core::time::now_unix_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("m.gguf");
        std::fs::write(&model, b"x").unwrap();

        let sha = "a".repeat(64);
        write_sha_sidecar(&model, &sha).unwrap();
        assert_eq!(read_sha_sidecar(&model), Some(sha));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_file_not_treated_as_final() {
        // .part 后缀不能被 `.gguf` 匹配，否则登记逻辑会把残缺文件当模型
        let name = "m.gguf";
        let part = format!("{name}.{PART_SUFFIX}");
        assert!(!part.ends_with(".gguf"), "临时文件不得以 .gguf 结尾");
    }
}
