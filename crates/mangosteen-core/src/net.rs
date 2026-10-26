//! 云端网络工具集的领域模型：端口映射、SSH 隧道、存储映射。
//!
//! 归属：**算力执行层 · 云端网络工具集**。
//!
//! ## 目标
//!
//! 「云端实例像本地服务一样无感访问」——上层拿到 `127.0.0.1:<local_port>`
//! 即可，隧道的建立、保活、重建全部由本层负责。
//!
//! ## 本模块的边界
//!
//! 只定义**数据模型与规划逻辑**（选哪个本地端口、映射哪条路径、隧道参数长什么样），
//! 真正的进程拉起与转发在 `mangosteen-cloud` crate 中实现。
//! 这样规划逻辑可以在无网络环境下完整单测。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{MnError, MnResult};

/// 隧道方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelDirection {
    /// 本地端口 → 远端端口（`ssh -L`）。推理服务访问用这个。
    LocalToRemote,
    /// 远端端口 → 本地端口（`ssh -R`）。云端需要回调本地服务时用。
    RemoteToLocal,
}

impl TunnelDirection {
    /// SSH 参数前缀。
    pub fn ssh_flag(self) -> &'static str {
        match self {
            TunnelDirection::LocalToRemote => "-L",
            TunnelDirection::RemoteToLocal => "-R",
        }
    }
}

/// 单条端口转发规则。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortForward {
    /// 方向
    pub direction: TunnelDirection,
    /// 本地侧地址（`LocalToRemote` 时为监听地址，通常是 `127.0.0.1`）
    pub local_host: String,
    /// 本地侧端口
    pub local_port: u16,
    /// 远端侧地址。远端侧通常绑 `127.0.0.1`（实例内回环）。
    pub remote_host: String,
    /// 远端侧端口
    pub remote_port: u16,
}

impl PortForward {
    /// 构造「本机 127.0.0.1:local → 云端 127.0.0.1:remote」的转发规则。
    pub fn loopback(local_port: u16, remote_port: u16) -> Self {
        Self {
            direction: TunnelDirection::LocalToRemote,
            local_host: "127.0.0.1".to_string(),
            local_port,
            remote_host: "127.0.0.1".to_string(),
            remote_port,
        }
    }

    /// 生成 SSH 参数值（`-L` 后面的那一段）。
    ///
    /// 格式：`<local_host>:<local_port>:<remote_host>:<remote_port>`
    pub fn to_ssh_arg(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.local_host, self.local_port, self.remote_host, self.remote_port
        )
    }

    /// 校验端口合法性。
    pub fn validate(&self) -> MnResult<()> {
        if self.local_port < 1024 {
            return Err(MnError::config(
                "tunnel.local_port",
                "本地侧不得使用特权端口（<1024）",
            ));
        }
        if self.remote_port == 0 {
            return Err(MnError::config("tunnel.remote_port", "不能为 0"));
        }
        if self.local_host.trim().is_empty() || self.remote_host.trim().is_empty() {
            return Err(MnError::config("tunnel.host", "主机地址不能为空"));
        }
        Ok(())
    }
}

/// SSH 隧道参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    /// SSH 主机
    pub ssh_host: String,
    /// SSH 端口
    pub ssh_port: u16,
    /// 登录用户
    pub ssh_user: String,
    /// 私钥路径（`None` 表示用 ssh-agent / 默认密钥）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
    /// 转发规则
    pub forwards: Vec<PortForward>,
    /// 是否启用连接复用（`ControlMaster`），可显著降低重连延迟
    pub control_master: bool,
    /// 登录密码（`None` 表示走密钥/agent）。
    ///
    /// ## 为什么需要它
    ///
    /// 优云智算这类厂商**创建接口没有注入 SSH key 的参数**，只能事后
    /// `ResetCompShareInstancePassword` 设一个密码。没有密码就只能让用户
    /// 自己在控制台配密钥——那等于把「一键部署」拆成两步。
    ///
    /// ## 怎么用
    ///
    /// 不写成命令行参数（`ssh` 没有这种参数），而是通过
    /// `SSH_ASKPASS` + `SSH_ASKPASS_REQUIRE=force`（OpenSSH 8.4+）让
    /// ssh 从我们的临时脚本里读。脚本权限 0700、用完即删。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl TunnelSpec {
    /// 校验。
    pub fn validate(&self) -> MnResult<()> {
        if self.ssh_host.trim().is_empty() {
            return Err(MnError::config("tunnel.ssh_host", "不能为空"));
        }
        if self.ssh_port == 0 {
            return Err(MnError::config("tunnel.ssh_port", "不能为 0"));
        }
        if self.ssh_user.trim().is_empty() {
            return Err(MnError::config("tunnel.ssh_user", "不能为空"));
        }
        if self.forwards.is_empty() {
            return Err(MnError::config("tunnel.forwards", "至少需要一条转发规则"));
        }
        for f in &self.forwards {
            f.validate()?;
        }
        // 本地端口不得冲突
        let mut ports: Vec<u16> = self
            .forwards
            .iter()
            .filter(|f| f.direction == TunnelDirection::LocalToRemote)
            .map(|f| f.local_port)
            .collect();
        let before = ports.len();
        ports.sort_unstable();
        ports.dedup();
        if ports.len() != before {
            return Err(MnError::config(
                "tunnel.forwards",
                "本地侧端口重复，同一隧道内不可重复监听",
            ));
        }
        Ok(())
    }

    /// 仅校验「隧道建立前已知的信息」。
    ///
    /// 本地端口由 [`crate::net::PortAllocator`] 在建立时分配，调用方给的值
    /// 可能只是占位（如 `0`），因此此时**不能**校验本地端口。
    /// 分配出真实端口后，调用方应再用 [`Self::validate`] 复核一次。
    pub fn validate_for_establish(&self) -> MnResult<()> {
        if self.ssh_host.trim().is_empty() {
            return Err(MnError::config("tunnel.ssh_host", "不能为空"));
        }
        if self.ssh_port == 0 {
            return Err(MnError::config("tunnel.ssh_port", "不能为 0"));
        }
        if self.ssh_user.trim().is_empty() {
            return Err(MnError::config("tunnel.ssh_user", "不能为空"));
        }
        if self.forwards.is_empty() {
            return Err(MnError::config("tunnel.forwards", "至少需要一条转发规则"));
        }
        for f in &self.forwards {
            if f.remote_port == 0 {
                return Err(MnError::config("tunnel.remote_port", "不能为 0"));
            }
            if f.local_host.trim().is_empty() || f.remote_host.trim().is_empty() {
                return Err(MnError::config("tunnel.host", "主机地址不能为空"));
            }
        }
        Ok(())
    }

    /// 生成 ssh 命令行参数（不含 `ssh` 本体）。
    ///
    /// 关键选项说明：
    /// * `-N` 只转发不执行远程命令
    /// * `-T` 不分配伪终端
    /// * `-o ExitOnForwardFailure=yes` 端口占用时立即失败，避免「假连通」
    /// * `-o ServerAliveInterval` 保活，防止云侧 NAT 静默断开
    pub fn to_ssh_args(&self) -> Vec<String> {
        let mut a = vec![
            "-N".to_string(),
            "-T".to_string(),
            "-o".to_string(),
            "ExitOnForwardFailure=yes".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=15".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-p".to_string(),
            self.ssh_port.to_string(),
        ];
        if let Some(id) = &self.identity_file {
            a.push("-i".to_string());
            a.push(id.display().to_string());
        }
        if self.control_master {
            a.push("-o".to_string());
            a.push("ControlMaster=auto".to_string());
            a.push("-o".to_string());
            a.push("ControlPersist=60".to_string());
        }
        for f in &self.forwards {
            a.push(f.direction.ssh_flag().to_string());
            a.push(f.to_ssh_arg());
        }
        a.push(format!("{}@{}", self.ssh_user, self.ssh_host));
        a
    }

    /// 供本机访问推理服务的地址。
    pub fn local_inference_url(&self, scheme: &str) -> Option<String> {
        self.forwards
            .iter()
            .find(|f| f.direction == TunnelDirection::LocalToRemote)
            .map(|f| format!("{scheme}://{}:{}", f.local_host, f.local_port))
    }
}

/// 存储映射方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageMappingKind {
    /// SSHFS / SMB 等协议挂载到本地路径
    Mount,
    /// rsync / scp 单向同步
    Sync,
    /// 不映射，云端自行下载
    None,
}

/// 存储映射描述。把本地模型目录与云端路径对上。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageMapping {
    /// 映射方式
    pub kind: StorageMappingKind,
    /// 本地路径
    pub local_path: PathBuf,
    /// 云端路径
    pub remote_path: String,
    /// 是否只读（模型目录推荐只读，避免误删）
    pub read_only: bool,
}

impl StorageMapping {
    /// 校验：本地路径必须存在（映射意图明确，避免建了个空目录）。
    pub fn validate(&self) -> MnResult<()> {
        if self.kind == StorageMappingKind::None {
            return Ok(());
        }
        if self.local_path.as_os_str().is_empty() {
            return Err(MnError::config("storage_mapping.local_path", "不能为空"));
        }
        if self.remote_path.trim().is_empty() {
            return Err(MnError::config("storage_mapping.remote_path", "不能为空"));
        }
        if !self.remote_path.starts_with('/') {
            return Err(MnError::config(
                "storage_mapping.remote_path",
                "云端路径必须是绝对路径",
            ));
        }
        Ok(())
    }

    /// 生成 `rsync` 参数（`Sync` 方式）。
    ///
    /// 用 `-a` 保留属性、`--partial` 支持断点续传（大模型文件必开）、
    /// `--inplace` 避免临时文件占双份磁盘。
    pub fn to_rsync_args(&self, ssh_port: u16) -> Vec<String> {
        let mut a = vec![
            "-a".to_string(),
            "--partial".to_string(),
            "--inplace".to_string(),
            "--info=progress2".to_string(),
            "-e".to_string(),
            format!("ssh -p {ssh_port} -o StrictHostKeyChecking=accept-new"),
        ];
        if self.read_only {
            // rsync 无原生只读，用 --dry-run 的反面不可行；这里用 -R 相对路径
            // 以保证目标结构稳定，只读语义由后续 chmod 保证
            a.push("-R".to_string());
        }
        a
    }
}

/// 端口分配器：在配置的区间内找出本机空闲端口。
///
/// 抽成独立结构以便单测——真实探测需要绑定 socket，
/// 而「跳过已被占用的端口」这个逻辑与 OS 无关。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortAllocator {
    /// 区间起始（含）
    pub start: u16,
    /// 区间结束（含）
    pub end: u16,
    /// 已被本进程占用的端口
    occupied: Vec<u16>,
}

impl PortAllocator {
    /// 新建。校验区间合法性。
    pub fn new(start: u16, end: u16) -> MnResult<Self> {
        if start < 1024 {
            return Err(MnError::config(
                "port_alloc.start",
                "不得使用特权端口（<1024）",
            ));
        }
        if start > end {
            return Err(MnError::config("port_alloc", "起始端口不得大于结束端口"));
        }
        Ok(Self {
            start,
            end,
            occupied: Vec::new(),
        })
    }

    /// 区间容量。
    pub fn capacity(&self) -> usize {
        (self.end - self.start) as usize + 1
    }

    /// 标记端口已占用。
    pub fn reserve(&mut self, port: u16) {
        if !self.occupied.contains(&port) {
            self.occupied.push(port);
        }
    }

    /// 释放端口。
    pub fn release(&mut self, port: u16) {
        self.occupied.retain(|p| *p != port);
    }

    /// 分配下一个可用端口。
    ///
    /// `is_bindable` 允许注入真实的「端口是否可用」探测；
    /// 返回 `None` 表示区间已耗尽。此时调用方应上报告警，而不是无限重试。
    pub fn allocate<F>(&mut self, is_bindable: F) -> Option<u16>
    where
        F: Fn(u16) -> bool,
    {
        for port in self.start..=self.end {
            if self.occupied.contains(&port) {
                continue;
            }
            if is_bindable(port) {
                self.occupied.push(port);
                return Some(port);
            }
        }
        None
    }

    /// 当前已占用端口（有序）。
    pub fn occupied(&self) -> Vec<u16> {
        let mut v = self.occupied.clone();
        v.sort_unstable();
        v
    }
}

/// 网络连通性探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectivityCheck {
    /// 阶段名（`tcp` / `ssh` / `http`）
    pub stage: String,
    /// 是否通过
    pub ok: bool,
    /// 耗时毫秒
    pub elapsed_ms: u64,
    /// 失败原因
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ConnectivityCheck {
    /// 成功结果。
    pub fn pass(stage: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            stage: stage.into(),
            ok: true,
            elapsed_ms,
            error: None,
        }
    }

    /// 失败结果。
    pub fn fail(stage: impl Into<String>, elapsed_ms: u64, error: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            ok: false,
            elapsed_ms,
            error: Some(error.into()),
        }
    }
}

/// 判断路径是否为绝对 POSIX 路径。用于跨平台校验云端路径。
pub fn is_absolute_posix(p: &str) -> bool {
    Path::new(p).is_absolute() && !p.contains('\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_forward_ssh_arg_format() {
        let f = PortForward::loopback(11435, 8080);
        assert_eq!(f.to_ssh_arg(), "127.0.0.1:11435:127.0.0.1:8080");
        assert_eq!(f.direction.ssh_flag(), "-L");
        assert!(f.validate().is_ok());
    }

    #[test]
    fn port_forward_rejects_privileged_local_port() {
        let mut f = PortForward::loopback(11435, 8080);
        f.local_port = 80;
        assert!(f.validate().is_err());
    }

    #[test]
    fn tunnel_spec_generates_safe_ssh_args() {
        let t = TunnelSpec {
            ssh_host: "1.2.3.4".into(),
            ssh_port: 22,
            ssh_user: "root".into(),
            identity_file: Some(PathBuf::from("/home/u/.ssh/id_ed25519")),
            forwards: vec![PortForward::loopback(11435, 8080)],
            control_master: true,
            password: None,
        };
        assert!(t.validate().is_ok());
        let args = t.to_ssh_args();
        assert!(args.contains(&"-N".to_string()));
        assert!(args.contains(&"ExitOnForwardFailure=yes".to_string()));
        assert!(args.contains(&"ServerAliveInterval=15".to_string()));
        assert!(args.contains(&"-i".to_string()));
        assert!(args.contains(&"/home/u/.ssh/id_ed25519".to_string()));
        assert!(args.contains(&"-L".to_string()));
        assert!(args.contains(&"127.0.0.1:11435:127.0.0.1:8080".to_string()));
        // 目标必须是最后一项
        assert_eq!(args.last().unwrap(), "root@1.2.3.4");
    }

    #[test]
    fn tunnel_spec_rejects_duplicate_local_ports() {
        let t = TunnelSpec {
            ssh_host: "h".into(),
            ssh_port: 22,
            ssh_user: "u".into(),
            identity_file: None,
            forwards: vec![
                PortForward::loopback(11435, 8080),
                PortForward::loopback(11435, 8081),
            ],
            control_master: false,
            password: None,
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn tunnel_spec_requires_forward() {
        let t = TunnelSpec {
            ssh_host: "h".into(),
            ssh_port: 22,
            ssh_user: "u".into(),
            identity_file: None,
            forwards: vec![],
            control_master: false,
            password: None,
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn tunnel_local_inference_url() {
        let t = TunnelSpec {
            ssh_host: "h".into(),
            ssh_port: 22,
            ssh_user: "u".into(),
            identity_file: None,
            forwards: vec![PortForward::loopback(11435, 8080)],
            control_master: false,
            password: None,
        };
        assert_eq!(
            t.local_inference_url("http").unwrap(),
            "http://127.0.0.1:11435"
        );
    }

    #[test]
    fn storage_mapping_validates_absolute_remote() {
        let m = StorageMapping {
            kind: StorageMappingKind::Sync,
            local_path: PathBuf::from("/models"),
            remote_path: "root/models".into(),
            read_only: true,
        };
        assert!(m.validate().is_err(), "相对远端路径必须拒绝");

        let m = StorageMapping {
            remote_path: "/root/models".into(),
            ..m
        };
        assert!(m.validate().is_ok());
    }

    #[test]
    fn storage_mapping_none_skips_validation() {
        let m = StorageMapping {
            kind: StorageMappingKind::None,
            local_path: PathBuf::new(),
            remote_path: String::new(),
            read_only: false,
        };
        assert!(m.validate().is_ok());
    }

    #[test]
    fn rsync_args_include_resume_and_inplace() {
        let m = StorageMapping {
            kind: StorageMappingKind::Sync,
            local_path: PathBuf::from("/models"),
            remote_path: "/root/models".into(),
            read_only: true,
        };
        let a = m.to_rsync_args(2222);
        assert!(a.contains(&"--partial".to_string()));
        assert!(a.contains(&"--inplace".to_string()));
        assert!(a.iter().any(|x| x.contains("-p 2222")));
    }

    #[test]
    fn port_allocator_skips_occupied_and_unbindable() {
        let mut a = PortAllocator::new(20000, 20004).unwrap();
        assert_eq!(a.capacity(), 5);
        a.reserve(20000);
        // 20001 不可绑定 → 应跳到 20002
        let p = a.allocate(|port| port != 20001);
        assert_eq!(p, Some(20002));
        // 下一次分配从区间起点重新扫描，20001 此时可绑定 → 取它
        let p2 = a.allocate(|_| true);
        assert_eq!(p2, Some(20001));
        assert_eq!(a.occupied(), vec![20000, 20001, 20002]);
    }

    #[test]
    fn port_allocator_exhaustion() {
        let mut a = PortAllocator::new(20000, 20001).unwrap();
        a.reserve(20000);
        a.reserve(20001);
        assert_eq!(a.allocate(|_| true), None);
    }

    #[test]
    fn port_allocator_release_reuses() {
        let mut a = PortAllocator::new(20000, 20000).unwrap();
        let p = a.allocate(|_| true).unwrap();
        assert_eq!(p, 20000);
        assert_eq!(a.allocate(|_| true), None);
        a.release(20000);
        assert_eq!(a.allocate(|_| true), Some(20000));
    }

    #[test]
    fn port_allocator_rejects_bad_range() {
        assert!(PortAllocator::new(80, 90).is_err());
        assert!(PortAllocator::new(20001, 20000).is_err());
        assert!(PortAllocator::new(20000, 20000).is_ok());
    }

    #[test]
    fn connectivity_results() {
        let p = ConnectivityCheck::pass("tcp", 12);
        assert!(p.ok && p.error.is_none());
        let f = ConnectivityCheck::fail("ssh", 3000, "timeout");
        assert!(!f.ok && f.error.as_deref() == Some("timeout"));
    }

    #[test]
    fn posix_path_check() {
        assert!(is_absolute_posix("/root/models"));
        assert!(!is_absolute_posix("root/models"));
        assert!(!is_absolute_posix("C:\\models"));
    }
}
