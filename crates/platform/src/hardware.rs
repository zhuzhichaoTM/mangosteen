//! 硬件画像采集：CPU / 内存 / GPU。
//!
//! 归属：**平台适配层**。资源评估引擎的数据来源。
//!
//! ## 各平台采集策略
//!
//! | 平台 | GPU 显存 | 内存 | CPU |
//! |------|---------|------|-----|
//! | macOS | `system_profiler SPDisplaysDataType -json`（Apple Silicon 读 `unified_memory`） | `sysctl hw.memsize` | `sysctl hw.physicalcpu hw.logicalcpu` |
//! | Linux | `nvidia-smi --query-gpu=... --format=csv`，AMD 读 `rocm-smi` | `/proc/meminfo` `MemTotal` | 解析 `/proc/cpuinfo`（去重 physical id + core id） |
//! | Windows | `nvidia-smi` 同上；无则 WMI 查询 | `GlobalMemoryStatusEx` | `GetSystemInfo` |
//!
//! ## 降级策略（重要）
//!
//! 采集失败**绝不**导致进程退出。每一项失败都退回保守默认值并记录到
//! [`HardwareProbe::warnings`]，让用户在 `mangosteen doctor` 里看到真实原因。
//! 这与「部署失败自动降级重试」是同一设计哲学：宁可慢一点也别跑不起来。

use std::process::Command;

use mangosteen_core::hardware::{Accelerator, AcceleratorVendor, HardwareProfile, HostProfile};
use serde::{Deserialize, Serialize};

/// 采集结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardwareProbe {
    /// 硬件画像
    pub profile: HardwareProfile,
    /// 降级/异常提示
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl HardwareProbe {
    /// 采集失败时的保守兜底画像：假设 8GB 内存、4 核、无 GPU。
    pub fn fallback<S: Into<String>>(reason: S) -> Self {
        let reason = reason.into();
        Self {
            profile: HardwareProfile {
                os: super::os_name().to_string(),
                arch: super::arch_name().to_string(),
                host: HostProfile {
                    total_memory_bytes: 8 << 30,
                    available_memory_bytes: 4 << 30,
                    physical_cores: 4,
                    logical_cores: 4,
                    cpu_model: "unknown".to_string(),
                },
                accelerators: Vec::new(),
            },
            warnings: vec![format!(
                "硬件采集失败，已回退保守默认值（8GiB 内存 / 4 核 / 无加速卡）：{reason}"
            )],
        }
    }
}

/// 统一采集入口。
pub fn probe() -> HardwareProbe {
    let mut warnings = Vec::new();

    // ---- CPU ----
    let (cpu_model, physical, logical) = cpu_info(&mut warnings);

    // ---- 内存 ----
    let (total_mem, avail_mem) = memory_info(&mut warnings);

    // ---- GPU ----
    let mut accelerators = Vec::new();
    let mut gpu_warn = Vec::new();
    accelerators.extend(nvidia_gpus(&mut gpu_warn));
    #[cfg(target_os = "macos")]
    {
        if let Some(apple) = apple_silicon_gpu(total_mem, &mut gpu_warn) {
            accelerators.push(apple);
        } else {
            accelerators.extend(macos_discrete_gpus(&mut gpu_warn));
        }
    }
    #[cfg(target_os = "linux")]
    {
        accelerators.extend(amd_gpus(&mut gpu_warn));
    }
    warnings.extend(gpu_warn);

    let profile = HardwareProfile {
        os: super::os_name().to_string(),
        arch: super::arch_name().to_string(),
        host: HostProfile {
            total_memory_bytes: total_mem,
            available_memory_bytes: avail_mem,
            physical_cores: physical,
            logical_cores: logical,
            cpu_model,
        },
        accelerators,
    };
    HardwareProbe { profile, warnings }
}

// ---------------------------------------------------------------- CPU

/// 返回 `(型号, 物理核, 逻辑核)`。
fn cpu_info(warnings: &mut Vec<String>) -> (String, u32, u32) {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);

    #[cfg(target_os = "macos")]
    {
        let physical = sysctl_u64("hw.physicalcpu")
            .unwrap_or(logical as u64)
            .max(1) as u32;
        let model = sysctl_str("machdep.cpu.brand_string")
            .or_else(|| sysctl_str("hw.model"))
            .unwrap_or_else(|| {
                warnings.push("无法读取 CPU 型号（sysctl 不可用）".into());
                "Apple Silicon".to_string()
            });
        (model, physical, logical)
    }

    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let model = info
            .lines()
            .find_map(|l| {
                l.strip_prefix("model name")
                    .and_then(|r| r.split_once(':'))
                    .map(|(_, v)| v.trim().to_string())
            })
            .or_else(|| {
                info.lines().find_map(|l| {
                    l.strip_prefix("Model")
                        .and_then(|r| r.split_once(':'))
                        .map(|(_, v)| v.trim().to_string())
                })
            })
            .unwrap_or_else(|| {
                warnings.push("无法从 /proc/cpuinfo 读取 CPU 型号".into());
                "unknown".to_string()
            });
        // 物理核 = 去重后的 (physical id, core id) 对数
        let physical = count_linux_physical_cores(&info).unwrap_or(logical);
        (model, physical, logical)
    }

    #[cfg(target_os = "windows")]
    {
        let model = env_or(
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_Processor).Name",
                ])
                .output(),
            "CPU 型号",
            warnings,
        )
        .unwrap_or_else(|| "unknown".to_string());
        let physical = env_or(
            Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "(Get-CimInstance Win32_Processor | Measure-Object -Property NumberOfCores -Sum).Sum",
                ])
                .output(),
            "物理核数",
            warnings,
        )
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(logical);
        (model, physical.max(1), logical)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        warnings.push("未知平台，CPU 信息使用默认值".into());
        ("unknown".to_string(), logical, logical)
    }
}

/// 解析 Linux `/proc/cpuinfo` 中的物理核数量。
#[cfg(target_os = "linux")]
fn count_linux_physical_cores(info: &str) -> Option<u32> {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    let mut cur_phys: Option<String> = None;
    let mut cur_core: Option<String> = None;
    for line in info.lines() {
        let line = line.trim();
        if line.is_empty() {
            if let (Some(p), Some(c)) = (cur_phys.take(), cur_core.take()) {
                set.insert((p, c));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim().to_string());
            match k {
                "physical id" => cur_phys = Some(v),
                "core id" => cur_core = Some(v),
                _ => {}
            }
        }
    }
    if let (Some(p), Some(c)) = (cur_phys, cur_core) {
        set.insert((p, c));
    }
    if set.is_empty() {
        None
    } else {
        Some(set.len() as u32)
    }
}

// ---------------------------------------------------------------- 内存

fn memory_info(warnings: &mut Vec<String>) -> (u64, u64) {
    #[cfg(target_os = "macos")]
    {
        let total = sysctl_u64("hw.memsize").unwrap_or_else(|| {
            warnings.push("无法读取 hw.memsize，内存按 8GiB 估算".into());
            8 << 30
        });
        // macOS 无简单可靠的空闲内存接口；用 vm_stat 的 free+inactive 近似
        let avail = macos_available_memory().unwrap_or(total / 2);
        (total, avail.min(total))
    }

    #[cfg(target_os = "linux")]
    {
        let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let total = parse_meminfo_kb(&info, "MemTotal")
            .map(|kb| kb * 1024)
            .unwrap_or_else(|| {
                warnings.push("无法从 /proc/meminfo 读取 MemTotal".into());
                8 << 30
            });
        let avail = parse_meminfo_kb(&info, "MemAvailable")
            .map(|kb| kb * 1024)
            .unwrap_or(total / 2);
        (total, avail.min(total))
    }

    #[cfg(target_os = "windows")]
    {
        match windows_memory() {
            Some((t, a)) => (t, a),
            None => {
                warnings.push("无法读取 Windows 内存信息，按 8GiB 估算".into());
                (8 << 30, 4 << 30)
            }
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        warnings.push("未知平台，内存使用默认值".into());
        (8 << 30, 4 << 30)
    }
}

/// 解析 `/proc/meminfo` 中某个键的 kB 值。
#[cfg(target_os = "linux")]
fn parse_meminfo_kb(info: &str, key: &str) -> Option<u64> {
    info.lines().find_map(|l| {
        let rest = l.strip_prefix(key)?;
        let rest = rest.strip_prefix(':')?;
        rest.split_whitespace().next()?.parse::<u64>().ok()
    })
}

#[cfg(target_os = "macos")]
fn macos_available_memory() -> Option<u64> {
    let out = Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let page_size = text
        .lines()
        .find(|l| l.contains("page size of"))
        .and_then(|l| {
            l.split("page size of")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(4096);
    let get = |label: &str| -> u64 {
        text.lines()
            .find(|l| l.trim_start().starts_with(label))
            .and_then(|l| l.split(':').nth(1))
            .map(|v| v.trim().trim_end_matches('.').replace('.', ""))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    // free + inactive + speculative 约等于可用
    let pages = get("Pages free") + get("Pages inactive") + get("Pages speculative");
    if pages == 0 {
        None
    } else {
        Some(pages * page_size)
    }
}

#[cfg(target_os = "windows")]
fn windows_memory() -> Option<(u64, u64)> {
    // 用 PowerShell 读，避免引入额外 winapi 调用签名维护成本
    let script = r#"
$os = Get-CimInstance Win32_OperatingSystem
$total = [uint64]$os.TotalVisibleMemorySize * 1024
$free  = [uint64]$os.FreePhysicalMemory * 1024
"$total,$free"
"#;
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.trim().split(',').map(|s| s.trim()).collect();
    if parts.len() != 2 {
        return None;
    }
    let total = parts[0].parse::<u64>().ok()?;
    let free = parts[1].parse::<u64>().ok()?;
    Some((total, free))
}

// ---------------------------------------------------------------- GPU

/// NVIDIA GPU 采集（三平台通用，依赖 `nvidia-smi`）。
///
/// 查询字段刻意最小化：显存、名称、算力、带宽。
fn nvidia_gpus(warnings: &mut Vec<String>) -> Vec<Accelerator> {
    let out = match Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,compute_cap,memory.bus_width",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(), // 未安装 nvidia-smi：正常情况，不告警
    };
    if !out.status.success() {
        warnings.push("nvidia-smi 执行失败，未能获取 NVIDIA 显存信息".into());
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut gpus = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() < 2 {
            continue;
        }
        let name = parts[0].to_string();
        // nvidia-smi 的 memory.total 单位是 MiB
        let vram_mib = parts[1].parse::<u64>().ok().unwrap_or(0);
        let compute_capability = parts
            .get(2)
            .filter(|s| !s.is_empty() && **s != "[N/A]")
            .map(|s| format!("sm_{}", s.replace('.', "")));
        gpus.push(Accelerator {
            name,
            vendor: AcceleratorVendor::Nvidia,
            vram_bytes: vram_mib * (1 << 20),
            unified_memory_bytes: 0,
            compute_capability,
            fp16_tflops: None,
            memory_bandwidth_gbps: None,
        });
    }
    gpus
}

#[cfg(target_os = "macos")]
fn apple_silicon_gpu(total_mem: u64, warnings: &mut Vec<String>) -> Option<Accelerator> {
    if !cfg!(target_arch = "aarch64") {
        return None;
    }
    // Apple Silicon：统一内存，显存不独立。型号从 sysctl 的系统标识推出。
    let brand = sysctl_str("machdep.cpu.brand_string")
        .or_else(|| sysctl_str("hw.model"))
        .unwrap_or_else(|| {
            warnings.push("无法读取 Apple Silicon 型号".into());
            "Apple Silicon".to_string()
        });
    // 内存带宽：不同代差别很大，只对已知系列给出保守值
    let bandwidth = if brand.contains("M1") {
        Some(68.0)
    } else if brand.contains("M2") {
        Some(100.0)
    } else if brand.contains("M3") {
        Some(150.0)
    } else if brand.contains("M4") {
        Some(120.0)
    } else if brand.contains("Ultra") {
        Some(800.0)
    } else if brand.contains("Max") {
        Some(400.0)
    } else {
        None
    };
    Some(Accelerator {
        name: brand.clone(),
        vendor: AcceleratorVendor::Apple,
        vram_bytes: 0,
        unified_memory_bytes: total_mem,
        compute_capability: Some("metal".to_string()),
        fp16_tflops: None,
        memory_bandwidth_gbps: bandwidth,
    })
}

/// macOS 独显（Intel Mac）。
#[cfg(target_os = "macos")]
fn macos_discrete_gpus(warnings: &mut Vec<String>) -> Vec<Accelerator> {
    let out = match Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => {
            warnings.push("system_profiler 不可用，无法探测 macOS 独显".into());
            return Vec::new();
        }
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // 不引入 JSON 依赖：手动扫描 "model" / "vram" 字段
    let mut gpus = Vec::new();
    let mut cur_name: Option<String> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = json_field(t, "sppci_model") {
            cur_name = Some(v);
        } else if let Some(v) = json_field(t, "spdisplays_vram_shared") {
            if let (Some(name), Some(gib)) = (cur_name.take(), parse_vram(&v)) {
                gpus.push(Accelerator {
                    name: name.clone(),
                    vendor: vendor_of(&name),
                    vram_bytes: 0,
                    unified_memory_bytes: gib,
                    compute_capability: Some("metal".into()),
                    fp16_tflops: None,
                    memory_bandwidth_gbps: None,
                });
            }
        } else if let Some(v) = json_field(t, "spdisplays_vram") {
            if let (Some(name), Some(bytes)) = (cur_name.take(), parse_vram(&v)) {
                gpus.push(Accelerator {
                    name: name.clone(),
                    vendor: vendor_of(&name),
                    vram_bytes: bytes,
                    unified_memory_bytes: 0,
                    compute_capability: Some("metal".into()),
                    fp16_tflops: None,
                    memory_bandwidth_gbps: None,
                });
            }
        }
    }
    gpus
}

/// 从 `"key" : "value"` 形式的 JSON 行中取值。
#[cfg(target_os = "macos")]
fn json_field(line: &str, key: &str) -> Option<String> {
    if !line.contains(key) {
        return None;
    }
    let after = line.split(':').nth(1)?.trim();
    let v = after.trim_matches(|c| c == '"' || c == ',').trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// 解析 `"24 GB"` / `"8192 MB"` / `"8 GB"` 这类显存描述。
#[cfg(target_os = "macos")]
fn parse_vram(s: &str) -> Option<u64> {
    let s = s.trim();
    let num: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let val: f64 = num.parse().ok()?;
    let upper = s.to_ascii_uppercase();
    let multiplier: f64 = if upper.contains("GB") {
        (1u64 << 30) as f64
    } else if upper.contains("MB") {
        (1u64 << 20) as f64
    } else if upper.contains("TB") {
        (1u64 << 40) as f64
    } else {
        return None;
    };
    Some((val * multiplier) as u64)
}

#[cfg(target_os = "macos")]
fn vendor_of(name: &str) -> AcceleratorVendor {
    let n = name.to_lowercase();
    if n.contains("amd") || n.contains("radeon") {
        AcceleratorVendor::Amd
    } else if n.contains("intel") {
        AcceleratorVendor::Intel
    } else {
        AcceleratorVendor::Nvidia
    }
}

/// AMD GPU（Linux，`rocm-smi`）。
#[cfg(target_os = "linux")]
fn amd_gpus(warnings: &mut Vec<String>) -> Vec<Accelerator> {
    let out = match Command::new("rocm-smi")
        .args(["--showproductname", "--showmeminfo", "vram", "--csv"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    // 输出格式跨版本不稳定，只做尽力而为的解析
    let mut gpus = Vec::new();
    for line in text.lines().skip(1) {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() < 2 {
            continue;
        }
        let name = parts
            .iter()
            .find(|p| p.to_lowercase().contains("radeon") || p.to_lowercase().contains("instinct"))
            .map(|s| s.to_string());
        let vram = parts
            .iter()
            .find_map(|p| p.parse::<u64>().ok().filter(|v| *v > 1 << 20));
        if let (Some(name), Some(bytes)) = (name, vram) {
            gpus.push(Accelerator {
                name,
                vendor: AcceleratorVendor::Amd,
                vram_bytes: bytes,
                unified_memory_bytes: 0,
                compute_capability: Some("rocm".into()),
                fp16_tflops: None,
                memory_bandwidth_gbps: None,
            });
        }
    }
    if gpus.is_empty() {
        warnings.push("检测到 rocm-smi 但未解析出 AMD 显存信息".into());
    }
    gpus
}

// ---------------------------------------------------------------- 工具

#[cfg(target_os = "macos")]
fn sysctl_u64(key: &str) -> Option<u64> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(target_os = "macos")]
fn sysctl_str(key: &str) -> Option<String> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 从命令输出中取首行；失败时记录告警。
#[allow(dead_code)]
fn env_or(
    result: std::io::Result<std::process::Output>,
    what: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    match result {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                warnings.push(format!("未能读取{what}（输出为空）"));
                None
            } else {
                Some(s)
            }
        }
        Ok(_) => {
            warnings.push(format!("读取{what}的命令返回非零退出码"));
            None
        }
        Err(e) => {
            warnings.push(format!("读取{what}失败：{e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_never_panics_and_is_plausible() {
        let p = probe();
        assert_eq!(p.profile.os, super::super::os_name());
        assert!(p.profile.host.total_memory_bytes > 0);
        assert!(p.profile.host.physical_cores >= 1);
        assert!(p.profile.host.logical_cores >= p.profile.host.physical_cores);
        assert!(
            p.profile.host.available_memory_bytes <= p.profile.host.total_memory_bytes,
            "available 不应大于 total"
        );
    }

    #[test]
    fn fallback_profile_is_conservative() {
        let p = HardwareProbe::fallback("test");
        assert_eq!(p.profile.host.total_memory_bytes, 8 << 30);
        assert_eq!(p.profile.host.physical_cores, 4);
        assert!(p.profile.accelerators.is_empty());
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("test"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_meminfo_handles_real_format() {
        let sample = "MemTotal:       32716512 kB\n\
                      MemFree:         1234567 kB\n\
                      MemAvailable:   20971520 kB\n\
                      Buffers:          123456 kB\n";
        assert_eq!(parse_meminfo_kb(sample, "MemTotal"), Some(32_716_512));
        assert_eq!(parse_meminfo_kb(sample, "MemAvailable"), Some(20_971_520));
        assert_eq!(parse_meminfo_kb(sample, "Nonexistent"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn count_linux_physical_cores_dedups_hyperthreads() {
        // 2 物理核 × 2 超线程 = 4 逻辑核
        let sample = "\
processor\t: 0
physical id\t: 0
core id\t\t: 0

processor\t: 1
physical id\t: 0
core id\t\t: 1

processor\t: 2
physical id\t: 0
core id\t\t: 0

processor\t: 3
physical id\t: 0
core id\t\t: 1

";
        assert_eq!(count_linux_physical_cores(sample), Some(2));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn count_linux_physical_cores_returns_none_when_absent() {
        assert_eq!(count_linux_physical_cores("processor\t: 0\n"), None);
        assert_eq!(count_linux_physical_cores(""), None);
    }

    #[test]
    fn nvidia_probe_does_not_panic_without_tool() {
        // 本机大概率没有 nvidia-smi；无论有无都不应 panic
        let mut w = Vec::new();
        let gpus = nvidia_gpus(&mut w);
        for g in &gpus {
            assert_eq!(g.vendor, AcceleratorVendor::Nvidia);
            assert!(g.vram_bytes > 0);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn vram_string_parsing() {
        assert_eq!(parse_vram("24 GB"), Some(24 << 30));
        assert_eq!(parse_vram("8192 MB"), Some(8192 << 20));
        assert_eq!(parse_vram("1 TB"), Some(1 << 40));
        assert_eq!(parse_vram("unknown"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn json_field_extraction() {
        assert_eq!(
            json_field(
                r#"  "sppci_model" : "AMD Radeon Pro 5500M","#,
                "sppci_model"
            ),
            Some("AMD Radeon Pro 5500M".to_string())
        );
        assert_eq!(json_field(r#""foo" : "bar","#, "sppci_model"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn vendor_classification() {
        assert_eq!(vendor_of("AMD Radeon Pro"), AcceleratorVendor::Amd);
        assert_eq!(vendor_of("Intel UHD 630"), AcceleratorVendor::Intel);
        assert_eq!(vendor_of("GeForce GTX 780M"), AcceleratorVendor::Nvidia);
    }
}
