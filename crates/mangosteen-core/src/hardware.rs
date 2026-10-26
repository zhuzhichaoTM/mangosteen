//! 硬件画像数据结构。
//!
//! 归属：**API 网关层 · 资源评估引擎**（数据结构定义），
//! 实际采集由平台适配层 `mangosteen-platform` 完成。
//!
//! 本模块只定义「结构化快照」，不做任何采集动作，保证平台无关。

use serde::{Deserialize, Serialize};

/// 加速卡（GPU / 统一内存 SoC）厂商分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceleratorVendor {
    /// NVIDIA（消费级 RTX 为基线目标）
    Nvidia,
    /// Apple Silicon（统一内存，MPS/Metal 后端）
    Apple,
    /// AMD（扩展兼容，非核心优化目标）
    Amd,
    /// Intel（扩展兼容）
    Intel,
    /// 无独立加速卡，纯 CPU 推理
    None,
}

/// 单块加速卡信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accelerator {
    /// 展示名，如 `NVIDIA GeForce RTX 4090`
    pub name: String,
    /// 厂商
    pub vendor: AcceleratorVendor,
    /// 专用显存字节数。统一内存架构（Apple Silicon）填 0，改看 `unified_memory_bytes`。
    pub vram_bytes: u64,
    /// 统一内存总字节数。仅 Apple Silicon 等统一内存平台非 0。
    #[serde(default)]
    pub unified_memory_bytes: u64,
    /// 计算能力/架构代号，如 `sm_89`、`m3`。用于选择 llama.cpp 后端。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compute_capability: Option<String>,
    /// 理论 FP16 算力（TFLOPS）。`None` 表示未能获取，评估时退化为按显存带宽判断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fp16_tflops: Option<f32>,
    /// 显存带宽 GB/s，决定解码阶段的实际上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bandwidth_gbps: Option<f32>,
}

impl Accelerator {
    /// 可用于模型权重的「有效显存」。
    ///
    /// * 独立显存：直接取 `vram_bytes`；
    /// * 统一内存（Apple Silicon）：GPU 默认只能使用约 75% 的总统一内存
    ///   （macOS 的 `recommendedMaxWorkingSetSize` 约为总内存的 2/3~3/4，
    ///   这里取保守的 70%），剩余留给系统与 CPU。
    pub fn usable_accelerator_memory(&self) -> u64 {
        match self.vendor {
            AcceleratorVendor::Apple => {
                // 统一内存：保留 30% 给系统
                (self.unified_memory_bytes as f64 * 0.70) as u64
            }
            _ => self.vram_bytes,
        }
    }

    /// 是否为统一内存架构。
    pub fn is_unified_memory(&self) -> bool {
        matches!(self.vendor, AcceleratorVendor::Apple) && self.unified_memory_bytes > 0
    }
}

/// 系统内存与 CPU 画像。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostProfile {
    /// 物理内存总字节数
    pub total_memory_bytes: u64,
    /// 当前可用内存字节数（采集时刻快照）
    pub available_memory_bytes: u64,
    /// 物理核心数
    pub physical_cores: u32,
    /// 逻辑核心数（含超线程）
    pub logical_cores: u32,
    /// CPU 展示名
    pub cpu_model: String,
}

impl HostProfile {
    /// 可安全用于模型推理的内存（为系统与其它进程保留 25%，下限 1 GiB）。
    ///
    /// 依据：llama.cpp 在内存不足时会触发 swap，解码吞吐会塌陷一个数量级，
    /// 因此必须留出足够余量。保留比例按经验取 25%。
    pub fn usable_memory(&self) -> u64 {
        let reserved = (self.total_memory_bytes as f64 * 0.25) as u64;
        let reserved = reserved.max(1 << 30).min(self.total_memory_bytes);
        self.total_memory_bytes.saturating_sub(reserved)
    }

    /// 建议线程数。
    ///
    /// 经验规则：只用**物理**核心数。llama.cpp 的线程池在超线程上会出现
    /// 明显的争用，解码吞吐通常反而下降。
    pub fn recommended_threads(&self) -> u32 {
        self.physical_cores.max(1).min(64)
    }
}

/// 完整硬件快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HardwareProfile {
    /// 操作系统标识：`macos` / `windows` / `linux`
    pub os: String,
    /// CPU 架构：`aarch64` / `x86_64`
    pub arch: String,
    /// 内存与 CPU
    pub host: HostProfile,
    /// 加速卡列表（可多卡；消费级场景通常 1 块）
    pub accelerators: Vec<Accelerator>,
}

impl HardwareProfile {
    /// 聚合所有加速卡可用显存。
    pub fn total_usable_accelerator_memory(&self) -> u64 {
        self.accelerators
            .iter()
            .map(|a| a.usable_accelerator_memory())
            .sum()
    }

    /// 主加速卡（显存最大者）。本地部署默认单卡策略。
    pub fn primary_accelerator(&self) -> Option<&Accelerator> {
        self.accelerators.iter().max_by_key(|a| a.usable_accelerator_memory())
    }

    /// 是否存在可用加速卡。
    pub fn has_accelerator(&self) -> bool {
        self.total_usable_accelerator_memory() > 0
    }

    /// 平台人类可读描述，用于 CLI 与 TUI 展示。
    pub fn describe(&self) -> String {
        let gpu = match self.primary_accelerator() {
            Some(a) => format!(
                "{} ({:.1} GiB 可用)",
                a.name,
                a.usable_accelerator_memory() as f64 / (1u64 << 30) as f64
            ),
            None => "无加速卡（CPU 推理）".to_string(),
        };
        format!(
            "{} / {} · {} 物理核 · {:.1} GiB 内存 · {}",
            self.os,
            self.arch,
            self.host.physical_cores,
            self.host.total_memory_bytes as f64 / (1u64 << 30) as f64,
            gpu
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtx4090() -> Accelerator {
        Accelerator {
            name: "NVIDIA GeForce RTX 4090".into(),
            vendor: AcceleratorVendor::Nvidia,
            vram_bytes: 24 * (1u64 << 30),
            unified_memory_bytes: 0,
            compute_capability: Some("sm_89".into()),
            fp16_tflops: Some(165.2),
            memory_bandwidth_gbps: Some(1008.0),
        }
    }

    fn m3_max() -> Accelerator {
        Accelerator {
            name: "Apple M3 Max".into(),
            vendor: AcceleratorVendor::Apple,
            vram_bytes: 0,
            unified_memory_bytes: 64 * (1u64 << 30),
            compute_capability: Some("m3".into()),
            fp16_tflops: None,
            memory_bandwidth_gbps: Some(400.0),
        }
    }

    #[test]
    fn discrete_gpu_uses_full_vram() {
        assert_eq!(rtx4090().usable_accelerator_memory(), 24 * (1u64 << 30));
        assert!(!rtx4090().is_unified_memory());
    }

    #[test]
    fn unified_memory_reserves_system_share() {
        let a = m3_max();
        assert!(a.is_unified_memory());
        let usable = a.usable_accelerator_memory();
        // 64GiB × 70% = 44.8GiB
        assert_eq!(usable, (64.0 * 0.70 * (1u64 << 30) as f64) as u64);
        assert!(usable < 64 * (1u64 << 30));
    }

    #[test]
    fn host_memory_reserves_quarter() {
        let h = HostProfile {
            total_memory_bytes: 32 * (1u64 << 30),
            available_memory_bytes: 20 * (1u64 << 30),
            physical_cores: 8,
            logical_cores: 16,
            cpu_model: "Apple M3 Max".into(),
        };
        assert_eq!(h.usable_memory(), 24 * (1u64 << 30));
        // 必须用物理核，不是逻辑核
        assert_eq!(h.recommended_threads(), 8);
    }

    #[test]
    fn tiny_host_still_keeps_1gib_floor() {
        let h = HostProfile {
            total_memory_bytes: 2 * (1u64 << 30),
            available_memory_bytes: 1 * (1u64 << 30),
            physical_cores: 2,
            logical_cores: 4,
            cpu_model: "x".into(),
        };
        // 2GiB - 1GiB(floor) = 1GiB
        assert_eq!(h.usable_memory(), 1 * (1u64 << 30));
    }

    #[test]
    fn profile_picks_largest_accelerator() {
        let p = HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: 64 * (1u64 << 30),
                available_memory_bytes: 60 * (1u64 << 30),
                physical_cores: 16,
                logical_cores: 32,
                cpu_model: "Ryzen 9".into(),
            },
            accelerators: vec![rtx4090(), m3_max()],
        };
        assert_eq!(p.primary_accelerator().unwrap().name, "Apple M3 Max");
        assert!(p.describe().contains("linux"));
        assert!(p.has_accelerator());
    }

    #[test]
    fn cpu_only_profile() {
        let p = HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: 16 * (1u64 << 30),
                available_memory_bytes: 12 * (1u64 << 30),
                physical_cores: 8,
                logical_cores: 16,
                cpu_model: "i7".into(),
            },
            accelerators: vec![],
        };
        assert!(!p.has_accelerator());
        assert!(p.describe().contains("CPU 推理"));
    }
}
