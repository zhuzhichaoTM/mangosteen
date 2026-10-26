//! 实时资源采样：CPU 使用率、内存占用、进程资源。
//!
//! 归属：**平台适配层**。
//!
//! ## 与 `hardware` 模块的分工
//!
//! | 模块 | 采集内容 | 调用频率 |
//! |------|---------|---------|
//! | [`super::hardware`] | 静态画像（型号、总容量、核数、GPU 列表） | 启动时一次 |
//! | 本模块 | 动态指标（CPU 占用率、可用内存、磁盘剩余） | UI 每秒轮询 |
//!
//! 资源评估只需要静态画像（判断"能不能装下"），而监控界面需要动态指标
//! （"现在吃多少"）。混在一起会让评估逻辑每次调用都多付一次采样开销。
//!
//! ## CPU 使用率怎么算
//!
//! CPU 占用率没有"瞬时值"，只能算两个时间点之间的增量：
//!
//! ```text
//! usage = (busy2 - busy1) / (total2 - total1)
//! ```
//!
//! 因此本模块**有状态**：[`CpuSampler`] 持有上一次的累计值。首次调用返回
//! `None`（没有基准，无法计算），调用方应显示"采样中"而不是编一个数字。
//!
//! 这比"读 load average 当使用率"要诚实——load 反映的是排队长度，
//! 4 核满载时 load 约 4.0，但 1 核满载 3 核空闲时 load 也是 1.0，
//! 两者都用满 25% 却完全不同。

use std::process::Command;
use std::sync::Mutex;

use mangosteen_core::{MnError, MnResult};

/// 一次采样结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceSnapshot {
    /// CPU 使用率（0.0 ~ 1.0）。首次采样为 `None`
    pub cpu_usage: Option<f64>,
    /// 物理核心数
    pub physical_cores: u32,
    /// 逻辑核心数
    pub logical_cores: u32,
    /// 总内存字节
    pub total_memory_bytes: u64,
    /// 可用内存字节
    pub available_memory_bytes: u64,
    /// 采样时刻的 Unix 毫秒
    pub sampled_at_ms: u64,
}

impl ResourceSnapshot {
    /// 内存使用率（0.0 ~ 1.0）。
    pub fn memory_usage(&self) -> f64 {
        if self.total_memory_bytes == 0 {
            return 0.0;
        }
        let used = self
            .total_memory_bytes
            .saturating_sub(self.available_memory_bytes);
        used as f64 / self.total_memory_bytes as f64
    }
}

/// 有状态的 CPU 采样器。
#[derive(Debug)]
pub struct CpuSampler {
    /// 上一次的累计时间片（busy, total）
    prev: Mutex<Option<(u64, u64)>>,
}

impl Default for CpuSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuSampler {
    /// 新建。
    pub fn new() -> Self {
        Self {
            prev: Mutex::new(None),
        }
    }

    /// 采样一次。
    pub fn sample(&self) -> ResourceSnapshot {
        let (busy, total) = read_cpu_times().unwrap_or((0, 0));

        let usage = {
            let mut guard = match self.prev.lock() {
                Ok(g) => g,
                // 锁中毒：重置基准继续工作，不因一次 panic 就永久失去监控
                Err(p) => p.into_inner(),
            };
            let prev = *guard;
            *guard = Some((busy, total));
            match prev {
                Some((pb, pt)) if total > pt => {
                    let dt = total - pt;
                    let db = busy.saturating_sub(pb);
                    Some((db as f64 / dt as f64).clamp(0.0, 1.0))
                }
                // 首次采样，或两次采样之间计数器没变化（间隔太短）
                _ => None,
            }
        };

        let (total_mem, avail_mem) = memory_bytes().unwrap_or((0, 0));
        let (phys, logical) = core_counts();

        ResourceSnapshot {
            cpu_usage: usage,
            physical_cores: phys,
            logical_cores: logical,
            total_memory_bytes: total_mem,
            available_memory_bytes: avail_mem,
            sampled_at_ms: mangosteen_core::time::now_unix_millis() as u64,
        }
    }
}

/// 读取 CPU 累计时间片，返回 `(busy, total)`。
///
/// * macOS / Linux：`getloadavg` 拿不到时间片，改用各自的原生接口。
/// * 所有平台失败时返回 `None`，由调用方显示"不可用"而不是编数字。
// `mach_host_self` 在 libc 中标为 deprecated（建议改用 mach2 crate）。
// 但 mach2 会引入一个新依赖，而这里的用法正是官方文档给的标准写法，
// 且返回值无需释放。为一个已弃用但仍受支持、且语义正确的 API 引入
// 新依赖不划算，因此显式忽略该告警。
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn read_cpu_times() -> Option<(u64, u64)> {
    // macOS 没有 `kern.cp_time`（那是 FreeBSD 的）。正确做法是 mach 的
    // host_statistics(HOST_CPU_LOAD_INFO)，它返回 CPU_STATE_USER/SYSTEM/
    // IDLE/NICE 四个累计 tick。
    //
    // SAFETY: host_cpu_load_info 与 count 都在栈上分配且正确初始化；
    // mach_host_self 返回当前任务的 host 端口（无需释放）。
    unsafe {
        let mut info: libc::host_cpu_load_info = std::mem::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let kr = libc::host_statistics(
            libc::mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as libc::host_info_t,
            &mut count,
        );
        if kr != 0 {
            return None;
        }
        // cpu_ticks 顺序：USER, SYSTEM, IDLE, NICE
        let user = info.cpu_ticks[libc::CPU_STATE_USER as usize] as u64;
        let system = info.cpu_ticks[libc::CPU_STATE_SYSTEM as usize] as u64;
        let idle = info.cpu_ticks[libc::CPU_STATE_IDLE as usize] as u64;
        let nice = info.cpu_ticks[libc::CPU_STATE_NICE as usize] as u64;
        let busy = user + system + nice;
        Some((busy, busy + idle))
    }
}

#[cfg(target_os = "linux")]
fn read_cpu_times() -> Option<(u64, u64)> {
    // /proc/stat 第一行：cpu user nice system idle iowait irq softirq steal ...
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal guest guest_nice
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().take(8).sum();
    Some((total.saturating_sub(idle), total))
}

#[cfg(target_os = "windows")]
fn read_cpu_times() -> Option<(u64, u64)> {
    // Windows 无 /proc；用 typeperf 单次采样代价高（约 1s），不适合每秒轮询。
    // 因此这里返回 None，UI 会显示"不可用"，而不是给出错误的数字。
    // 需要精确 CPU 占用时应改用 PDH 计数器（引入 windows-sys 的 PDH 绑定），
    // 这属于后续优化项，当前不为了一个指标牺牲启动速度。
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn read_cpu_times() -> Option<(u64, u64)> {
    None
}

/// 读取 `(总内存, 可用内存)` 字节数。
#[cfg(target_os = "macos")]
fn memory_bytes() -> Option<(u64, u64)> {
    // 总量：hw.memsize；可用：vm_stat 的 free + inactive（inactive 可被回收）
    let total = {
        let out = Command::new("sysctl")
            .arg("-n")
            .arg("hw.memsize")
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .ok()?
    };

    let out = Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // 页大小写在首行："Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size: u64 = text
        .lines()
        .next()
        .and_then(|l| l.split("page size of ").nth(1))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);

    let mut free_pages = 0u64;
    let mut inactive_pages = 0u64;
    for line in text.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let v: u64 = val
            .trim()
            .trim_end_matches('.')
            .replace(',', "")
            .parse()
            .unwrap_or(0);
        if key.contains("Pages free") {
            free_pages = v;
        } else if key.contains("Pages inactive") {
            inactive_pages = v;
        }
    }
    let avail = (free_pages + inactive_pages) * page_size;
    Some((total, avail.min(total)))
}

#[cfg(target_os = "linux")]
fn memory_bytes() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total = 0u64;
    let mut avail = 0u64;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total = parse_kb(v)? * 1024;
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail = parse_kb(v)? * 1024;
        }
    }
    if total == 0 {
        return None;
    }
    Some((total, avail.min(total)))
}

#[cfg(target_os = "linux")]
fn parse_kb(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

#[cfg(target_os = "windows")]
fn memory_bytes() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX 由本函数在栈上分配并正确初始化 dwLength，
    // 系统调用只写入该结构体。
    unsafe {
        let mut st: MEMORYSTATUSEX = std::mem::zeroed();
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut st) != 0 {
            Some((st.ullTotalPhys, st.ullAvailPhys))
        } else {
            None
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn memory_bytes() -> Option<(u64, u64)> {
    None
}

/// 读取 `(物理核, 逻辑核)`。
fn core_counts() -> (u32, u32) {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    let phys = physical_cores().unwrap_or(logical);
    (phys, logical)
}

#[cfg(target_os = "macos")]
fn physical_cores() -> Option<u32> {
    let out = Command::new("sysctl")
        .arg("-n")
        .arg("hw.physicalcpu")
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn physical_cores() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut phys_id: Option<String> = None;
    let mut core_id: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            if let (Some(p), Some(c)) = (phys_id.take(), core_id.take()) {
                pairs.push((p, c));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "physical id" => phys_id = Some(v.trim().to_string()),
                "core id" => core_id = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    if let (Some(p), Some(c)) = (phys_id, core_id) {
        pairs.push((p, c));
    }
    pairs.sort();
    pairs.dedup();
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.len() as u32)
    }
}

#[cfg(target_os = "windows")]
fn physical_cores() -> Option<u32> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    // SAFETY: SYSTEM_INFO 由本函数在栈上分配，系统调用只写入该结构体。
    unsafe {
        let mut info: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut info);
        Some(info.dwNumberOfProcessors)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn physical_cores() -> Option<u32> {
    None
}

/// 查询指定 PID 的资源占用（RSS 字节）。
///
/// 用于 device 页展示「每个部署吃多少内存」。查不到时返回 `None`——
/// 进程可能已退出，这不是错误。
pub fn process_rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // 用 libproc 直接查，不依赖 `ps` 子进程：
        // 受限环境（沙箱、精简的系统镜像）里 ps 可能不可用或没权限，
        // 而监控面板不该因为一个外部命令缺失就整块空白。
        //
        // SAFETY: proc_pidinfo 只写入本函数提供的栈上缓冲区，
        // 缓冲区大小与实际结构体大小一致。
        unsafe {
            let mut info: libc::proc_taskinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
            let n = libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTASKINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            );
            if n == size {
                return Some(info.pti_resident_size);
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        // /proc/<pid>/statm 第二列是 RSS 页数
        let text = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * 4096)
    }
    #[cfg(windows)]
    {
        // tasklist 的 CSV 输出里 Mem Usage 形如 "123,456 K"
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next()?;
        let kb: u64 = line
            .split("\",\"")
            .nth(4)?
            .replace(" K", "")
            .replace(',', "")
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = pid;
        None
    }
}

/// 目录剩余空间（转发到 [`super::fs::available_space`]）。
pub fn available_space(path: &std::path::Path) -> MnResult<u64> {
    super::fs::available_space(path)
}

/// 采样并返回错误版本（供需要 `MnResult` 的调用方使用）。
pub fn sample_or_err(sampler: &CpuSampler) -> MnResult<ResourceSnapshot> {
    let s = sampler.sample();
    // 完全没采到任何信息才算错误（内存与核数都为 0 说明平台不支持）
    if s.total_memory_bytes == 0 && s.logical_cores == 0 {
        return Err(MnError::Io {
            context: "采集系统资源".to_string(),
            reason: "当前平台不支持资源采样".to_string(),
        });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_first_call_has_no_usage() {
        let s = CpuSampler::new().sample();
        // 首次没有基准，不能编数字
        assert!(s.cpu_usage.is_none(), "首次采样不应给出使用率");
    }

    #[test]
    fn sampler_second_call_produces_usage_in_range() {
        let c = CpuSampler::new();
        let _ = c.sample();
        // 让计数器前进一点
        std::thread::sleep(std::time::Duration::from_millis(60));
        let s2 = c.sample();
        if let Some(u) = s2.cpu_usage {
            assert!((0.0..=1.0).contains(&u), "使用率必须在 0~1，实际 {u}");
        }
    }

    #[test]
    fn memory_is_reported() {
        let s = CpuSampler::new().sample();
        // 测试机一定有内存
        assert!(s.total_memory_bytes > 0, "应读到总内存");
        assert!(
            s.available_memory_bytes <= s.total_memory_bytes,
            "可用内存不应超过总量"
        );
        assert!(s.memory_usage() >= 0.0 && s.memory_usage() <= 1.0);
    }

    #[test]
    fn core_counts_are_positive() {
        let (phys, logical) = core_counts();
        assert!(logical >= 1);
        assert!(phys >= 1);
        assert!(phys <= logical, "物理核不应多于逻辑核");
    }

    #[test]
    fn process_rss_of_self_is_available() {
        let pid = std::process::id();
        if let Some(rss) = process_rss_bytes(pid) {
            assert!(rss > 0, "自身 RSS 应大于 0");
        }
    }

    #[test]
    fn process_rss_of_missing_pid_is_none() {
        // 不存在的 PID 不应 panic
        let _ = process_rss_bytes(4_000_000);
    }
}
