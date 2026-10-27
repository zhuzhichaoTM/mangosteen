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
        self.child.terminate(Duration::from_secs(3))
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
            log_dir,
        })
    }

    /// 隧道 key。
    fn key(spec: &TunnelSpec, remote_port: u16) -> String {
        format!("{}:{}:{}", spec.ssh_host, spec.ssh_port, remote_port)
    }

    /// 建立隧道。
    ///
    /// 步骤：
    /// 1. 分配本地空闲端口；
    /// 2. 修正 `forwards` 里的本地端口（调用方给的可能是占位值）；
    /// 3. 拉起 `ssh -N -L ...`；
    /// 4. 等待本地端口连通（最多 `connect_timeout`）；
    /// 5. 失败则清理进程与端口分配。
    pub fn establish(&mut self, spec: &TunnelSpec, connect_timeout: Duration) -> MnResult<&Tunnel> {
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
                return Ok(&self.tunnels[pos].1);
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
        let mut askpass: Option<PathBuf> = None;
        if let Some(pw) = effective.password.as_deref() {
            let script = std::env::temp_dir()
                .join(format!("mn-askpass-{}-{local_port}.sh", std::process::id()));
            // 单引号转义：密码里可能有 $ 或反引号，直接塞进 shell 会被解释
            let escaped = pw.replace('\'', r#"'\''"#);
            std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{escaped}'\n")).map_err(
                |e| {
                    self.allocator.release(local_port);
                    MnError::io(format!("写入 askpass 脚本 {}", script.display()), &e)
                },
            )?;
            // 权限收敛统一走平台层（Windows 无 POSIX 权限位，那里是无操作）
            let _ = mangosteen_platform::fs::set_private_permissions(&script);
            args.push("-o".to_string());
            args.push("PreferredAuthentications=password".to_string());
            args.push("-o".to_string());
            args.push("PubkeyAuthentication=no".to_string());
            askpass = Some(script);
        }

        let mut opts = SpawnOptions::new(PathBuf::from(&self.ssh_bin), args, "ssh-tunnel");
        // 必须捕获 stderr：ssh 失败的唯一线索就在里面。
        // 不捕获时 `spawn` 把 stderr 丢进 /dev/null，错误信息只会写
        // 「stderr：（空）」——而真实原因可能是密码错、host key 变更、
        // 端口不通。排障时这行字等于什么都没说。
        if self.log_dir.is_none() {
            opts.capture_output = true;
        }
        if let Some(script) = &askpass {
            opts = opts
                .with_env("SSH_ASKPASS", script.display().to_string())
                .with_env("SSH_ASKPASS_REQUIRE", "force")
                .with_env("DISPLAY", ":0");
        }
        if let Some(dir) = &self.log_dir {
            let log = dir.join(format!("tunnel-{local_port}.log"));
            opts = opts.with_log_file(&log)?;
        }

        let mut child = opts.spawn().map_err(|e| {
            // 启动失败要归还端口
            self.allocator.release(local_port);
            if let Some(s) = &askpass {
                let _ = std::fs::remove_file(s);
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

        // 等待本地端口连通
        let deadline = Instant::now() + connect_timeout;
        loop {
            if let Some(code) = child.exit_code() {
                let tail = child.stderr_tail(8);
                self.allocator.release(local_port);
                if let Some(p) = &askpass {
                    let _ = std::fs::remove_file(p);
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
            if std::net::TcpStream::connect_timeout(
                &([127, 0, 0, 1], local_port).into(),
                Duration::from_millis(300),
            )
            .is_ok()
            {
                break;
            }
            if Instant::now() >= deadline {
                let tail = child.stderr_tail(8);
                let _ = child.terminate(Duration::from_millis(800));
                self.allocator.release(local_port);
                if let Some(p) = &askpass {
                    let _ = std::fs::remove_file(p);
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

        // 隧道已通，认证必然已完成，现在才安全删除密码脚本
        if let Some(p) = &askpass {
            let _ = std::fs::remove_file(p);
        }

        self.tunnels.push((
            Self::key(spec, remote_port),
            Tunnel {
                child,
                local_port,
                remote_port,
                established_at: mangosteen_core::time::now_unix(),
                target: format!("{}@{}:{}", spec.ssh_user, spec.ssh_host, spec.ssh_port),
            },
        ));
        Ok(&self.tunnels.last().unwrap().1)
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
            return Ok(true);
        }
        Ok(false)
    }

    /// 关闭全部隧道。进程退出前必须调用，否则残留 ssh 进程。
    pub fn close_all(&mut self) -> MnResult<usize> {
        let mut n = 0;
        for (_, mut t) in std::mem::take(&mut self.tunnels) {
            let _ = t.close();
            self.allocator.release(t.local_port);
            n += 1;
        }
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
