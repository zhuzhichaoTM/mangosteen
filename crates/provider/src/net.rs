//! 云端网络工具集：SSH 隧道管理。
//!
//! 归属：**算力执行层 · 云端网络工具集**。
//!
//! ## 职责
//!
//! 把云端实例的推理端口**映射到本机 127.0.0.1**，使上层拿到的地址形态
//! 与本地部署完全一致。隧道的建立、健康检查、重建都在这里。
//!
//! ## 为什么用 SSH 隧道而不是公网直连
//!
//! 1. **安全**：推理服务不暴露公网，只经加密隧道访问；
//! 2. **一致**：本地与云端在网关看来都是 `127.0.0.1:<port>`；
//! 3. **通用**：所有支持 SSH 的服务商都能用，无需厂商提供「自定义服务」功能。
//!
//! ## 依赖
//!
//! 使用系统 `ssh` 客户端（三平台自带 OpenSSH），不引入 SSH 库。
//! 密钥认证优先，禁止交互式密码（会导致无人值守场景挂死）：
//! 参数里固定 `BatchMode=yes`。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use mangosteen_core::net::{PortAllocator, TunnelSpec};
use mangosteen_core::{MnError, MnResult};
use mangosteen_platform::process::{self, ManagedChild, SpawnOptions};

/// 隧道运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelStatus {
    /// 未建立
    Down,
    /// 已建立且端口可连通
    Up {
        /// 本地映射端口
        local_port: u16,
    },
    /// 已建立但端口不通（远端服务未起或转发失败）
    Degraded {
        /// 本地映射端口
        local_port: u16,
        /// 原因
        reason: String,
    },
}

impl TunnelStatus {
    /// 是否可用。
    pub fn is_up(&self) -> bool {
        matches!(self, TunnelStatus::Up { .. })
    }

    /// 本地端口。
    pub fn local_port(&self) -> Option<u16> {
        match self {
            TunnelStatus::Up { local_port } | TunnelStatus::Degraded { local_port, .. } => {
                Some(*local_port)
            }
            TunnelStatus::Down => None,
        }
    }

    /// 人类可读描述。
    pub fn describe(&self) -> String {
        match self {
            TunnelStatus::Down => "隧道未建立".to_string(),
            TunnelStatus::Up { local_port } => {
                format!("隧道已建立 → 127.0.0.1:{local_port}")
            }
            TunnelStatus::Degraded { local_port, reason } => {
                format!("隧道异常（127.0.0.1:{local_port}）：{reason}")
            }
        }
    }
}

/// 一条活跃的隧道。
#[derive(Debug)]
pub struct Tunnel {
    child: ManagedChild,
    /// 本地映射端口
    pub local_port: u16,
    /// 远端端口
    pub remote_port: u16,
    /// 建立时间
    pub established_at: i64,
    /// SSH 目标（供日志）
    pub target: String,
    /// 密码认证用的 askpass 脚本 + 密码 sidecar。只在隧道真正关闭时才删除：
    /// ssh 是「延迟」调用 askpass 取密码的，脚本存活期必须覆盖
    /// 整个认证窗口，否则 ssh 拿不到密码退出 255 且 stderr 为空。
    askpass: Option<mangosteen_platform::ssh::Askpass>,
}

impl Tunnel {
    /// 检测是否仍然健康。
    ///
    /// 判据：SSH 进程存活 **且** 本地端口可连通。
    /// 只查进程不够——SSH 可能活着但转发已断（云侧网络抖动）。
    pub fn check(&mut self) -> TunnelStatus {
        if !self.child.is_running() {
            return TunnelStatus::Down;
        }
        // 本地端口能建立 TCP 连接即认为转发链路可用
        match std::net::TcpStream::connect_timeout(
            &([127, 0, 0, 1], self.local_port).into(),
            Duration::from_millis(700),
        ) {
            Ok(_) => TunnelStatus::Up {
                local_port: self.local_port,
            },
            Err(e) => TunnelStatus::Degraded {
                local_port: self.local_port,
                reason: format!("本地端口 {0} 不可连通：{e}", self.local_port),
            },
        }
    }

    /// 关闭隧道。
    pub fn close(&mut self) -> MnResult<()> {
        let r = self.child.terminate(Duration::from_secs(3));
        // 认证此刻必然已经结束（无论成败），可安全删除密码脚本与 sidecar。
        if let Some(ap) = self.askpass.take() {
            ap.cleanup();
        }
        r
    }

    /// PID。
    pub fn pid(&self) -> u32 {
        self.child.pid()
    }
}

/// 隧道管理器。
///
/// 维护「逻辑目标 → 隧道」的映射，负责端口分配、建立、健康检查与重建。
pub struct TunnelManager {
    /// 端口分配器
    allocator: PortAllocator,
    /// ssh 可执行文件
    ssh_bin: String,
    /// 活跃隧道：key 为 `host:port:remote_port`
    tunnels: Vec<(String, Tunnel)>,
    /// 每个远端端口对应的隧道规格，供 [`Self::heal`] 原地重建。
    ///
    /// 必须记录规格而不能从 `Tunnel` 反推：重建需要 `identity_file`、
    /// `allow_unready_remote` 等建立参数，`Tunnel` 只保存了连接结果。
    specs: std::collections::HashMap<u16, TunnelSpec>,
    /// 日志目录
    log_dir: Option<PathBuf>,
}

impl TunnelManager {
    /// 新建。
    pub fn new(
        port_start: u16,
        port_end: u16,
        ssh_bin: impl Into<String>,
        log_dir: Option<PathBuf>,
    ) -> MnResult<Self> {
        Ok(Self {
            allocator: PortAllocator::new(port_start, port_end)?,
            ssh_bin: ssh_bin.into(),
            tunnels: Vec::new(),
            specs: std::collections::HashMap::new(),
            log_dir,
        })
    }

    /// 隧道 key。
    fn key(spec: &TunnelSpec, remote_port: u16) -> String {
        format!("{}:{}:{}", spec.ssh_host, spec.ssh_port, remote_port)
    }

    /// 取 ssh 的真实报错文本（管道优先，其次回读日志文件）。
    ///
    /// ssh 的重定向方式决定了线索在哪：
    /// * `capture_output`（未配置日志目录）→ stderr 在管道里，进程退出后可读；
    /// * `log_file`（生产路径，配了 `log_dir`）→ stderr **只**写在文件里，
    ///   管道是空的。
    ///
    /// 旧实现无条件读管道，于是第二种情况永远得到空串，错误信息成为
    /// 「stderr：（空）」。这里先取管道，空则回读日志文件尾部；仍为空
    /// 才如实说「空」。日志文件里只保留最后若干行，避免把整段进度刷屏。
    fn diagnose(child: &mut ManagedChild, log_path: Option<&PathBuf>) -> String {
        let from_pipe = child.stderr_tail(8);
        if !from_pipe.trim().is_empty() {
            return from_pipe;
        }
        let Some(path) = log_path else {
            return String::new();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return String::new();
        };
        // ssh 的进度噪音对排障无益，只留真正说明问题的行。
        let noise = ["Warning: Permanently added", "debug1:", "Transferred: "];
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !noise.iter().any(|n| l.starts_with(n)))
            .collect();
        let start = lines.len().saturating_sub(6);
        lines[start..].join("\n")
    }

    /// 建立隧道（带瞬时故障重试）。
    ///
    /// ## 为什么要重试整个建立过程
    ///
    /// 云厂商的 SSH 入口是共享转发网关。实例重启/迁移、网关滚动更新期间，
    /// 会出现一个**秒级窗口**：TCP 能连上但立刻被远端关闭
    /// （`Connection closed by remote host`），随后转发规则重建中
    /// （`connect to host ... port 23: Connection refused`）。
    ///
    /// ssh 自身的 `ConnectionAttempts` 只覆盖「TCP 连接建立失败」，对
    /// 「连上后立刻被关闭」无效——后者已经是一次成功的 TCP 连接了。
    /// 因此必须在应用层再包一层重试，跨过这段窗口。
    ///
    /// 重试是安全的：每次尝试都会独立分配端口、清理进程与临时文件，
    /// 失败分支不留下任何残留（这是 `establish_once` 的既有约定）。
    pub fn establish(&mut self, spec: &TunnelSpec, connect_timeout: Duration) -> MnResult<&Tunnel> {
        const ATTEMPTS: u32 = 4;
        let mut last_reason = String::new();
        for attempt in 1..=ATTEMPTS {
            match self.establish_once(spec, connect_timeout) {
                Ok(_) => break,
                Err(e) => {
                    let transient = Self::is_transient_connect_failure(&e);
                    last_reason = e.to_string();
                    if !transient || attempt == ATTEMPTS {
                        return Err(e);
                    }
                    // 退避：2s / 4s / 6s。网关重建转发通常几秒内完成，
                    // 指数退避反而会让用户白等，线性递增更贴合实际。
                    let wait = Duration::from_secs(2 * attempt as u64);
                    eprintln!(
                        "[tunnel] 第 {attempt}/{ATTEMPTS} 次建立失败（瞬时网关故障），\
                         {}s 后重试：{last_reason}",
                        wait.as_secs()
                    );
                    std::thread::sleep(wait);
                }
            }
        }
        // 成功路径：按 key 取回刚建好的隧道
        let remote_port = spec
            .forwards
            .iter()
            .find(|f| f.direction == mangosteen_core::net::TunnelDirection::LocalToRemote)
            .map(|f| f.remote_port)
            .unwrap_or(0);
        let key = Self::key(spec, remote_port);
        self.tunnels
            .iter()
            .position(|(k, _)| *k == key)
            .map(|pos| &self.tunnels[pos].1)
            .ok_or_else(|| MnError::Network {
                stage: "建立 ssh 隧道".to_string(),
                reason: format!("隧道建立流程未返回可用连接：{last_reason}"),
            })
    }

    /// 判断这次失败是否是「网关侧瞬时故障」——只有这类才值得重试。
    ///
    /// 认证失败（`Permission denied`）、密钥缺失、主机不可达的重试都毫无
    /// 意义，只会把真实错误拖成超时。因此按 stderr 文本白名单判定：
    /// 命中 `Connection closed by remote host` / `Connection reset` /
    /// `Connection refused` / `Connection timed out` 才重试。
    fn is_transient_connect_failure(e: &MnError) -> bool {
        let text = e.to_string();
        const TRANSIENT: [&str; 5] = [
            "Connection closed by remote host",
            "Connection reset by peer",
            "Connection refused",
            "Connection timed out",
            // 进程在观察窗内提前退出且没给出任何说明：多半是网关刚把
            // 连接拆掉，ssh 还没写出原因就退出了。
            "stderr：（空）",
        ];
        TRANSIENT.iter().any(|p| text.contains(p))
    }

    /// 建立隧道（单次）。
    ///
    /// 步骤：
    /// 1. 分配本地空闲端口；
    /// 2. 修正 `forwards` 里的本地端口（调用方给的可能是占位值）；
    /// 3. 拉起 `ssh -N -L ...`；
    /// 4. 等待本地端口连通（最多 `connect_timeout`）；
    /// 5. 失败则清理进程与端口分配。
    fn establish_once(
        &mut self,
        spec: &TunnelSpec,
        connect_timeout: Duration,
    ) -> MnResult<()> {
        // 注意：调用方给的 local_port 可能只是占位（如 0），
        // 真实端口由本管理器分配。因此这里只校验「除本地端口外」的部分。
        spec.validate_for_establish()?;

        let remote_port = spec
            .forwards
            .iter()
            .find(|f| f.direction == mangosteen_core::net::TunnelDirection::LocalToRemote)
            .map(|f| f.remote_port)
            .ok_or_else(|| MnError::config("tunnel.forwards", "缺少本地→远端转发规则"))?;

        // 复用已有隧道
        if let Some(pos) = self
            .tunnels
            .iter()
            .position(|(k, _)| *k == Self::key(spec, remote_port))
        {
            let status = self.tunnels[pos].1.check();
            if status.is_up() {
                return Ok(());
            }
            // 已断：清理后重建
            let (_, mut dead) = self.tunnels.remove(pos);
            let _ = dead.close();
        }

        // 分配本地端口
        let local_port = self
            .allocator
            .allocate(process::is_port_bindable)
            .ok_or_else(|| MnError::Network {
                stage: "分配本地端口".to_string(),
                reason: "端口区间已耗尽，请检查是否有大量残留隧道".to_string(),
            })?;

        // 用真实本地端口重建转发规则
        let mut effective = spec.clone();
        for f in effective.forwards.iter_mut() {
            if f.direction == mangosteen_core::net::TunnelDirection::LocalToRemote {
                f.local_port = local_port;
            }
        }
        effective.validate()?;

        let mut args = effective.to_ssh_args();
        // 密码认证的准备：ssh 没有「密码」命令行参数，只能走 askpass 协议。
        //
        // 两个必要条件：
        //  1. `SSH_ASKPASS_REQUIRE=force` —— 否则 ssh 在有终端时仍去读 tty
        //     （OpenSSH 8.4+ 才有这个变量，更老的版本会忽略它并回退到 tty，
        //     表现是隧道卡在密码提示）；
        //  2. 禁用 pubkey，只留 password —— 否则 ssh 会先尝试一堆不存在的
        //     密钥，每次失败都多几秒，隧道建立时间从 1 秒变成十几秒。
        //
        // 脚本本身由平台层生成（`mangosteen_platform::ssh`）：macOS/Linux 是
        // `#!/bin/sh`，Windows 是 `.cmd`——上层不能出现 `/bin/sh` 这种 Unix 假设。
        //
        // 文件名 tag 必须每次调用唯一：只按 pid+端口命名会在「调度器逐个评估候选
        // 实例、本地端口被复用」时让不同候选的脚本落到同一路径——一个候选的隧道
        // 清理会误删另一个候选正在用的脚本，ssh exec 时报
        // `ssh_askpass: exec(...): No such file or directory` → 拿不到密码 →
        // `Permission denied` → 退出码 255。这里加毫秒时间戳保证唯一。
        let mut askpass: Option<mangosteen_platform::ssh::Askpass> = None;
        if let Some(pw) = effective.password.as_deref() {
            let tag = format!(
                "{}-{}-{}",
                std::process::id(),
                local_port,
                mangosteen_core::time::now_unix_millis()
            );
            let ap = mangosteen_platform::ssh::write_askpass(&tag, Some(pw)).map_err(|e| {
                self.allocator.release(local_port);
                e
            })?;
            if let Some(ap) = ap {
                args.push("-o".to_string());
                args.push("PreferredAuthentications=password".to_string());
                args.push("-o".to_string());
                args.push("PubkeyAuthentication=no".to_string());
                askpass = Some(ap);
            }
        }

        let mut opts = SpawnOptions::new(PathBuf::from(&self.ssh_bin), args, "ssh-tunnel");
        // 必须捕获 stderr：ssh 失败的唯一线索就在里面。
        // 不捕获时 `spawn` 把 stderr 丢进 /dev/null，错误信息只会写
        // 「stderr：（空）」——而真实原因可能是密码错、host key 变更、
        // 端口不通。排障时这行字等于什么都没说。
        if self.log_dir.is_none() {
            opts.capture_output = true;
        }
        if let Some(ap) = &askpass {
            opts = opts
                .with_env("SSH_ASKPASS", ap.script_arg())
                .with_env("SSH_ASKPASS_REQUIRE", "force")
                .with_env("DISPLAY", ":0");
        }
        if let Some(dir) = &self.log_dir {
            let log = dir.join(format!("tunnel-{local_port}.log"));
            opts = opts.with_log_file(&log)?;
        }
        // 记住本次隧道的日志文件路径。ssh 的 stderr 一旦重定向到文件，
        // `stderr_tail` 就永远读不到内容——这正是「stderr：（空）」的由来：
        // 真正的报错（Permission denied、Connection closed by remote host…）
        // 全在文件里躺着，而用户看到的是一句「（空）」，完全无从下手。
        // 失败分支据此回读文件，把真实原因带进错误信息。
        let log_path = self
            .log_dir
            .as_ref()
            .map(|d| d.join(format!("tunnel-{local_port}.log")));

        let mut child = opts.spawn().map_err(|e| {
            // 启动失败要归还端口，并清掉含密码的临时文件
            self.allocator.release(local_port);
            if let Some(ap) = &askpass {
                ap.cleanup();
            }
            MnError::Network {
                stage: "启动 ssh 隧道".to_string(),
                reason: e.to_string(),
            }
        })?;

        // 注意：**不能**在这里删 askpass 脚本。
        //
        // ssh 是「延迟」调用 askpass 的——它在完成 TCP 连接、协商算法之后
        // 才去执行脚本取密码。spawn 返回时认证通常还没开始，此时删掉脚本，
        // ssh 取不到密码就直接退出 255，而且**stderr 一个字都不写**
        // （实测踩到：错误信息只有「stderr：（空）」，看起来像网络问题，
        // 实际是脚本已被删）。
        //
        // 脚本要留到「隧道建成」或「明确失败」之后再删，见下面各返回路径。

        // 远端端口尚未监听（设备配置阶段先建隧道、后起引擎）时，不能要求
        // 「本地端口可连通」：`ssh -L` 是惰性转发，本地端口在引擎起来前
        // 就已经在监听，连通性检测无法区分「隧道好」与「目标没人听」。
        // 因此只确认 ssh 进程在观察窗内活着；真正可用性由启动阶段探 /health 判定。
        if effective.allow_unready_remote {
            // 观察窗必须覆盖 ssh 的完整认证过程：TCP 握手 + 密钥交换 +
            // askpass 取密码，跨公网常需 1~3 秒。窗口太短会在认证完成前
            // 就判定「隧道就绪」，掩盖真实失败；这里的目的是尽早发现
            // 「密码错 / 实例未就绪」这类立刻退出的失败。
            let settle = Duration::from_secs(3);
            let deadline = Instant::now() + connect_timeout.min(settle);
            loop {
                if let Some(code) = child.exit_code() {
                    let tail = Self::diagnose(&mut child, log_path.as_ref());
                    self.allocator.release(local_port);
                    if let Some(ap) = &askpass {
                        ap.cleanup();
                    }
                    return Err(MnError::Network {
                        stage: "建立 ssh 隧道".to_string(),
                        reason: format!(
                            "ssh 在隧道建立前退出（退出码 {code:?}）。常见原因：密码/密钥错误、\
                             实例未就绪、安全组拦截。stderr：{}",
                            if tail.trim().is_empty() {
                                "（空）"
                            } else {
                                &tail
                            }
                        ),
                    });
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        // 等待本地端口连通
        let deadline = Instant::now() + connect_timeout;
        loop {
            if let Some(code) = child.exit_code() {
                let tail = Self::diagnose(&mut child, log_path.as_ref());
                self.allocator.release(local_port);
                if let Some(ap) = &askpass {
                    ap.cleanup();
                }
                return Err(MnError::Network {
                    stage: "建立 ssh 隧道".to_string(),
                    reason: format!(
                        "ssh 在隧道建立前退出（退出码 {code:?}）。常见原因：密钥未授权、\
                         实例未就绪、安全组拦截。stderr：{}",
                        if tail.trim().is_empty() {
                            "（空）"
                        } else {
                            &tail
                        }
                    ),
                });
            }
            // 目标端口尚未监听时（设备配置阶段），上面已确认 ssh 存活，
            // 本地端口连通与否不具判别力，直接视为隧道就绪。
            if effective.allow_unready_remote {
                break;
            }
            if std::net::TcpStream::connect_timeout(
                &([127, 0, 0, 1], local_port).into(),
                Duration::from_millis(300),
            )
            .is_ok()
            {
                break;
            }
            if Instant::now() >= deadline {
                let tail = Self::diagnose(&mut child, log_path.as_ref());
                let _ = child.terminate(Duration::from_millis(800));
                self.allocator.release(local_port);
                if let Some(ap) = &askpass {
                    ap.cleanup();
                }
                return Err(MnError::Network {
                    stage: "等待隧道就绪".to_string(),
                    reason: format!(
                        "{}s 内本地端口 {local_port} 未连通。可能是远端 127.0.0.1:{remote_port} \
                         没有服务在监听。stderr：{}",
                        connect_timeout.as_secs(),
                        if tail.trim().is_empty() {
                            "（空）"
                        } else {
                            &tail
                        }
                    ),
                });
            }
            std::thread::sleep(Duration::from_millis(120));
        }

        // 隧道已就绪。**不在这里删 askpass**：观察窗（settle）可能短于 ssh 的
        // 「TCP 握手 + 密钥交换 + askpass 取密码」完整认证耗时。此时若提前删
        // 脚本，ssh 会在认证中途读不到密码而退出 255、stderr 为空
        // （正是隧道日志里成片 No such file 的来源）。
        //
        // 把脚本交给 Tunnel 持有，由 close()/进程退出时再删——脚本存活期
        // 覆盖整个认证窗口，ssh 必然能读到密码。
        self.tunnels.push((
            Self::key(spec, remote_port),
            Tunnel {
                child,
                local_port,
                remote_port,
                established_at: mangosteen_core::time::now_unix(),
                target: format!("{}@{}:{}", spec.ssh_user, spec.ssh_host, spec.ssh_port),
                askpass,
            },
        ));
        // 记录「如何重建它」：重建需要 identity_file / allow_unready_remote
        // 等原始参数，只靠 Tunnel 反推不出来。spec 的 local_port 是占位值，
        // 重建时由 establish 重新分配，因此存原始 spec 即可。
        self.specs.insert(remote_port, spec.clone());
        Ok(())
    }

    /// 健康检查并**自动重建**已断隧道。
    ///
    /// 这是「云端实例像本地服务一样无感访问」的关键：网络抖动导致 SSH 断开时，
    /// 上层不必感知，管理器自己重连。
    pub fn ensure_alive(
        &mut self,
        spec: &TunnelSpec,
        reconnect_timeout: Duration,
    ) -> MnResult<TunnelStatus> {
        let remote_port = spec
            .forwards
            .iter()
            .find(|f| f.direction == mangosteen_core::net::TunnelDirection::LocalToRemote)
            .map(|f| f.remote_port)
            .unwrap_or(0);
        let key = Self::key(spec, remote_port);

        if let Some(pos) = self.tunnels.iter().position(|(k, _)| *k == key) {
            let status = self.tunnels[pos].1.check();
            if status.is_up() {
                return Ok(status);
            }
            // 断了 → 重建
            let (_, mut dead) = self.tunnels.remove(pos);
            let port = dead.local_port;
            let _ = dead.close();
            self.allocator.release(port);
        }
        // 先建立（返回不可变引用），再按 key 取可变引用探活
        self.establish(spec, reconnect_timeout)?;
        match self.tunnels.iter_mut().find(|(k, _)| *k == key) {
            Some((_, t)) => Ok(t.check()),
            None => Ok(TunnelStatus::Down),
        }
    }

    /// 关闭指定隧道。
    pub fn close(&mut self, spec: &TunnelSpec) -> MnResult<bool> {
        let remote_port = spec
            .forwards
            .iter()
            .find(|f| f.direction == mangosteen_core::net::TunnelDirection::LocalToRemote)
            .map(|f| f.remote_port)
            .unwrap_or(0);
        let key = Self::key(spec, remote_port);
        if let Some(pos) = self.tunnels.iter().position(|(k, _)| *k == key) {
            let (_, mut t) = self.tunnels.remove(pos);
            let port = t.local_port;
            t.close()?;
            self.allocator.release(port);
            // 记录一并清掉，否则 heal 会对着一条已主动关闭的隧道重建
            self.specs.remove(&remote_port);
            return Ok(true);
        }
        Ok(false)
    }

    /// 按远端端口关闭单条隧道。
    ///
    /// 同节点多模型时必须用这个：`close_all` 会把其它模型的业务转发一起
    /// 切断，而只按 `TunnelSpec` 关闭又要求调用方完整重建 spec。
    /// key 的格式是 `host:port:remote_port`，因此远端端口足以唯一定位
    /// 本进程内属于该模型的那条隧道。
    ///
    /// 返回是否真的关掉了一条（不存在时为 `false`，保持幂等）。
    pub fn close_by_remote_port(&mut self, remote_port: u16) -> MnResult<bool> {
        let suffix = format!(":{remote_port}");
        let pos = self.tunnels.iter().position(|(k, _)| k.ends_with(&suffix));
        let Some(pos) = pos else {
            return Ok(false);
        };
        let (_, mut t) = self.tunnels.remove(pos);
        let port = t.local_port;
        // 关闭失败也要释放本地端口：ssh 已被 terminate 尝试过，
        // 端口预留不释放会让反复卸载逐渐吃光端口区间。
        let closed = t.close();
        self.allocator.release(port);
        self.specs.remove(&remote_port);
        closed.map(|_| true)
    }

    /// 关闭全部隧道。进程退出前必须调用，否则残留 ssh 进程。
    ///
    /// 共享节点卸载时按远端端口关闭单条隧道（`close_by_remote_port`），
    /// 这样能保留其它模型的隧道和业务转发。全部关闭时使用 `close_all`。
    pub fn close_all(&mut self) -> MnResult<usize> {
        let mut n = 0;
        for (_, mut t) in std::mem::take(&mut self.tunnels) {
            let _ = t.close();
            self.allocator.release(t.local_port);
            n += 1;
        }
        self.specs.clear();
        Ok(n)
    }

    /// 当前隧道数。
    pub fn len(&self) -> usize {
        self.tunnels.len()
    }

    /// 是否无隧道。
    pub fn is_empty(&self) -> bool {
        self.tunnels.is_empty()
    }

    /// 已用端口。
    pub fn occupied_ports(&self) -> Vec<u16> {
        self.allocator.occupied()
    }

    /// 隧道快照（本地端口、远端端口、目标）。
    pub fn snapshot(&self) -> Vec<(String, u16, u16)> {
        self.tunnels
            .iter()
            .map(|(k, t)| (k.clone(), t.local_port, t.remote_port))
            .collect()
    }

    /// 某个远端端口对应的本地映射端口。
    ///
    /// 供编排器在建立隧道后取回实际分配到的端口（隧道本体仍由管理器持有）。
    pub fn local_port_for_remote(&self, remote_port: u16) -> Option<u16> {
        self.tunnels
            .iter()
            .find(|(_, t)| t.remote_port == remote_port)
            .map(|(_, t)| t.local_port)
    }

    /// 确保托管中的隧道仍可用；断了就按同一规格**原地重建**。
    ///
    /// ## 为什么需要它
    ///
    /// `ssh -N -L` 是长连接，云端 NAT 超时、网关滚动重启、实例迁移都会
    /// 把它断掉。断掉之后本地端口不再监听，对上层表现为「推理服务突然
    /// 502 / 连接被拒」——而远端 llama-server 其实活得好好的。
    ///
    /// 这里按 `remote_port` 找回当初的隧道规格（由 `remember` 记录），
    /// 健康则不动，不健康则关闭残留 + 重新 establish。重连失败会如实
    /// 返回错误，由调用方决定是重试还是报给用户。
    ///
    /// ## 为什么按远端端口而不是 SSH 目标
    ///
    /// 同节点多模型部署时，同一 `host:port` 上有好几条隧道（各自转发
    /// 不同 engine_port）。按远端端口定位才能精确重建属于这个模型的那条，
    /// 不会误伤其它模型的业务转发。
    pub fn heal(&mut self, remote_port: u16, timeout: Duration) -> MnResult<TunnelStatus> {
        let spec = match self.specs.get(&remote_port).cloned() {
            Some(s) => s,
            None => {
                return Err(MnError::config(
                    "tunnel.spec",
                    &format!("远端端口 {remote_port} 没有对应的隧道规格记录，无法重建"),
                ))
            }
        };
        // 还活着就什么都不做
        if let Some(pos) = self
            .tunnels
            .iter()
            .position(|(_, t)| t.remote_port == remote_port)
        {
            let status = self.tunnels[pos].1.check();
            if status.is_up() {
                return Ok(status);
            }
            // 断了：先彻底清理，避免残留 ssh 进程抢占本地端口
            let (_, mut dead) = self.tunnels.remove(pos);
            let port = dead.local_port;
            let _ = dead.close();
            self.allocator.release(port);
            eprintln!("[tunnel] 远端端口 {remote_port} 的隧道已断开，正在重建");
        }
        self.establish(&spec, timeout)?;
        match self
            .tunnels
            .iter_mut()
            .find(|(_, t)| t.remote_port == remote_port)
        {
            Some((_, t)) => Ok(t.check()),
            None => Ok(TunnelStatus::Down),
        }
    }
}

impl Drop for TunnelManager {
    fn drop(&mut self) {
        // 兜底：进程退出时不让 ssh 变孤儿
        let _ = self.close_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::net::PortForward;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-tunnel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 为本测试进程派生一段独占的端口区间。
    ///
    /// ## 为什么不能写死端口
    ///
    /// 这些用例要真的绑定本地端口转发。写死固定区间在同一台机器上
    /// 并行跑多份测试（`cargo test --workspace` 会同时启动多个测试二进制）时
    /// 会互相抢占，表现为偶发失败——每次失败的用例还不一样，非常难查。
    ///
    /// 按 pid 派生：不同进程段不同；进程内各用例用 `offset` 错开。
    /// 每段 100 个端口，足够容纳全部 offset（0/10/20/30/40/50）。
    fn port_base(offset: u16) -> (u16, u16) {
        let seg = (std::process::id() % 150) as u16;
        let start = 21_000 + seg * 100 + offset;
        (start, start + 9)
    }

    /// 构造一个假的 ssh 脚本：只保持存活，不真连接。
    #[cfg(unix)]
    fn fake_ssh(dir: &std::path::Path, behavior: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("fake-ssh");
        std::fs::write(&p, format!("#!/bin/sh\n{behavior}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// 构造隧道规格。`local_port` 用 0 占位，真实端口由管理器分配。
    fn spec(remote_port: u16) -> TunnelSpec {
        TunnelSpec {
            ssh_host: "10.0.0.1".into(),
            ssh_port: 22,
            ssh_user: "root".into(),
            identity_file: None,
            forwards: vec![PortForward::loopback(0, remote_port)],
            control_master: false,
            allow_unready_remote: false,
            password: None,
        }
    }

    #[test]
    fn manager_construction_validates_ports() {
        let (lo, hi) = port_base(0);
        assert!(TunnelManager::new(lo, hi, "ssh", None).is_ok());
        // 特权端口必须拒绝
        assert!(TunnelManager::new(80, hi, "ssh", None).is_err());
        // 区间反向必须拒绝
        assert!(TunnelManager::new(hi, lo, "ssh", None).is_err());
    }

    #[test]
    fn empty_manager_state() {
        let (lo, _hi) = port_base(0);
        let m = TunnelManager::new(lo, lo + 5, "ssh", None).unwrap();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert!(m.occupied_ports().is_empty());
        assert!(m.snapshot().is_empty());
    }

    #[test]
    fn status_helpers() {
        assert!(TunnelStatus::Up { local_port: 1 }.is_up());
        assert!(!TunnelStatus::Down.is_up());
        assert!(!TunnelStatus::Degraded {
            local_port: 1,
            reason: "x".into()
        }
        .is_up());
        assert_eq!(TunnelStatus::Up { local_port: 9 }.local_port(), Some(9));
        assert_eq!(TunnelStatus::Down.local_port(), None);
        assert!(TunnelStatus::Up { local_port: 11435 }
            .describe()
            .contains("11435"));
        assert!(TunnelStatus::Degraded {
            local_port: 1,
            reason: "boom".into()
        }
        .describe()
        .contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn establish_fails_when_ssh_exits() {
        let d = tmpdir("ssh-fail");
        // 假 ssh 立即报错退出，模拟密钥未授权
        let bin = fake_ssh(&d, "echo 'Permission denied (publickey)' >&2; exit 255");
        let mut m = TunnelManager::new(
            port_base(10).0,
            port_base(10).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let s = spec(8080);
        let e = m.establish(&s, Duration::from_millis(500)).unwrap_err();
        assert!(matches!(e, MnError::Network { .. }));
        let msg = e.to_string();
        assert!(msg.contains("退出"), "got {msg}");
        // 失败后端口必须归还，不能泄漏
        assert!(m.occupied_ports().is_empty(), "失败时端口未归还");
        assert!(m.is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn establish_retries_transient_gateway_failure() {
        // 云厂商 SSH 网关在实例重启期间会把连接「连上后立刻关闭」。
        // 第一次照做，第二次正常转发——必须能自愈，而不是直接判死部署。
        let d = tmpdir("ssh-transient");
        let marker = d.join("first-done");
        let script = format!(
            r#"
port=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-L" ]; then port=$(echo "$a" | cut -d: -f2); fi
  prev="$a"
done
if [ ! -f '{marker}' ]; then
  : > '{marker}'
  echo 'Connection closed by remote host.' >&2
  exit 255
fi
exec python3 -c "import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('127.0.0.1',$port)); s.listen(5); time.sleep(30)"
"#,
            marker = marker.display()
        );
        let bin = fake_ssh(&d, &script);
        let mut m = TunnelManager::new(
            port_base(60).0,
            port_base(60).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let t = m
            .establish(&spec(8080), Duration::from_secs(8))
            .expect("瞬时网关故障应被重试跨过");
        assert!(t.local_port >= port_base(60).0);
        assert_eq!(m.len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn establish_does_not_retry_auth_failures() {
        // 认证失败重试毫无意义，只会把真实错误拖成超时。
        let d = tmpdir("ssh-auth-fail");
        let bin = fake_ssh(&d, "echo 'Permission denied (publickey).' >&2; exit 255");
        let mut m = TunnelManager::new(
            port_base(70).0,
            port_base(70).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let started = Instant::now();
        let e = m
            .establish(&spec(8080), Duration::from_secs(3))
            .unwrap_err();
        assert!(e.to_string().contains("Permission denied"), "got {e}");
        // 不重试 → 耗时远小于一次重试退避（2 秒）
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "认证失败不应重试，实际耗时 {:?}",
            started.elapsed()
        );
        assert!(m.occupied_ports().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn establish_times_out_when_port_never_comes_up() {
        let d = tmpdir("ssh-stall");
        // 假 ssh 活着但不转发任何端口
        let bin = fake_ssh(&d, "sleep 30");
        let mut m = TunnelManager::new(
            port_base(20).0,
            port_base(20).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let s = spec(8080);
        let started = Instant::now();
        let e = m.establish(&s, Duration::from_millis(600)).unwrap_err();
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "应真的等待超时"
        );
        assert!(e.to_string().contains("未连通"), "got {e}");
        assert!(m.occupied_ports().is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn establish_succeeds_with_real_forwarding() {
        // 用真实 ssh 到本地不可行（无 sshd）。改为模拟：假 ssh 直接监听目标端口，
        // 这验证了「端口连通即视为隧道就绪」的判据。
        let d = tmpdir("ssh-ok");
        // 假 ssh 从参数里取 -L 的本地端口并监听它
        let script = r#"
port=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-L" ]; then
    port=$(echo "$a" | cut -d: -f2)
  fi
  prev="$a"
done
if [ -n "$port" ]; then
  exec python3 -c "import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('127.0.0.1',$port)); s.listen(5); time.sleep(30)"
fi
sleep 30
"#;
        let bin = fake_ssh(&d, script);
        let mut m = TunnelManager::new(
            port_base(0).0,
            port_base(0).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let s = spec(8080);
        let t = m
            .establish(&s, Duration::from_secs(8))
            .expect("隧道应建立成功");
        let port = t.local_port;
        assert!(
            port >= port_base(0).0 && port <= port_base(0).1,
            "端口 {port} 应在本进程区间内"
        );
        assert_eq!(m.len(), 1);
        assert_eq!(m.snapshot().len(), 1);
        assert!(m.occupied_ports().contains(&port));

        // 复用：再次 establish 同一目标应返回同一端口，不新建
        let t2 = m.establish(&s, Duration::from_secs(2)).unwrap();
        assert_eq!(t2.local_port, port);
        assert_eq!(m.len(), 1, "不应重复建立隧道");

        // 关闭
        assert!(m.close(&s).unwrap());
        assert!(m.is_empty());
        assert!(!m.close(&s).unwrap());
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn ensure_alive_rebuilds_dead_tunnel() {
        let d = tmpdir("rebuild");
        let script = r#"
port=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-L" ]; then port=$(echo "$a" | cut -d: -f2); fi
  prev="$a"
done
exec python3 -c "import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('127.0.0.1',$port)); s.listen(5); time.sleep(30)"
"#;
        let bin = fake_ssh(&d, script);
        let mut m = TunnelManager::new(
            port_base(30).0,
            port_base(30).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let s = spec(8080);
        let status = m.ensure_alive(&s, Duration::from_secs(8)).unwrap();
        assert!(status.is_up(), "{status:?}");

        // 模拟进程被杀
        m.close_all().unwrap();
        assert!(m.is_empty());

        // 再次 ensure_alive 应重建
        let status2 = m.ensure_alive(&s, Duration::from_secs(8)).unwrap();
        assert!(status2.is_up(), "{status2:?}");
        // 端口应仍在分配区间内。注意：上一个端口可能处于 TIME_WAIT，
        // 分配器会跳到下一个可用端口，这是预期行为而非泄漏。
        let port2 = status2.local_port().unwrap();
        assert!(
            port2 >= port_base(30).0 && port2 <= port_base(30).1,
            "重建后的端口应仍在分配区间内，实际 {port2}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// 隧道被远端断开后，heal 必须能按记录下来的规格原地重建。
    ///
    /// 这是「部署成功但过一段时间对话报错」的根因路径：`ssh -N -L` 是长连接，
    /// 云端 NAT 超时/网关重启会把它断掉，而本地端口随之不再监听。
    #[cfg(unix)]
    #[test]
    fn heal_rebuilds_broken_tunnel_from_recorded_spec() {
        let d = tmpdir("heal");
        let script = r#"
port=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-L" ]; then port=$(echo "$a" | cut -d: -f2); fi
  prev="$a"
done
exec python3 -c "import socket,time
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('127.0.0.1',$port)); s.listen(5); time.sleep(30)"
"#;
        let bin = fake_ssh(&d, script);
        let mut m = TunnelManager::new(
            port_base(80).0,
            port_base(80).1,
            bin.to_string_lossy(),
            Some(d.clone()),
        )
        .unwrap();
        let first = m
            .establish(&spec(8080), Duration::from_secs(8))
            .unwrap()
            .local_port;

        // 健康时 heal 是空操作：端口不变
        let st = m.heal(8080, Duration::from_secs(8)).unwrap();
        assert!(st.is_up(), "{st:?}");
        assert_eq!(st.local_port(), Some(first), "健康隧道不应被重建");

        // 制造「在册但已死」的状态：把底层 ssh 进程杀掉，记录仍保留
        {
            let pos = m
                .tunnels
                .iter()
                .position(|(_, t)| t.remote_port == 8080)
                .expect("应有隧道记录");
            let _ = m.tunnels[pos].1.close();
        }
        let st = m.heal(8080, Duration::from_secs(8)).unwrap();
        assert!(st.is_up(), "断开后 heal 应重建成功，实际 {st:?}");
        assert_eq!(m.len(), 1, "重建后不应残留多余隧道");
        std::fs::remove_dir_all(&d).ok();
    }

    /// 没有记录过的远端端口不能瞎猜规格，必须明确报错。
    #[test]
    fn heal_without_spec_errors_clearly() {
        let mut m = TunnelManager::new(port_base(90).0, port_base(90).1, "ssh", None).unwrap();
        let e = m.heal(9999, Duration::from_millis(200)).unwrap_err();
        assert!(e.to_string().contains("没有对应的隧道规格"), "got {e}");
    }

    #[test]
    fn close_all_is_idempotent() {
        let mut m = TunnelManager::new(port_base(40).0, port_base(40).1, "ssh", None).unwrap();
        assert_eq!(m.close_all().unwrap(), 0);
        assert_eq!(m.close_all().unwrap(), 0);
    }

    #[test]
    fn missing_ssh_binary_reports_clearly() {
        let mut m = TunnelManager::new(
            port_base(50).0,
            port_base(50).1,
            "/nonexistent/ssh-xyz",
            None,
        )
        .unwrap();
        let e = m
            .establish(&spec(8080), Duration::from_millis(300))
            .unwrap_err();
        assert!(matches!(e, MnError::Network { .. }));
        assert!(m.occupied_ports().is_empty());
    }
}
