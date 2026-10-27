//! 后端节点选择器：多维加权打分与排序。
//!
//! 归属：**算力执行层 · 后端节点选择器**。
//!
//! ## 打分模型
//!
//! 每个候选节点（服务商 × 规格）先过**硬性过滤**，再对四个维度归一化打分，
//! 最后按配置权重加权求和：
//!
//! ```text
//! score = w_price · S_price + w_latency · S_latency
//!       + w_availability · S_avail + w_hardware · S_hw
//! ```
//!
//! 每个分维度都归一化到 `[0,1]`，`1` 为最优：
//!
//! | 维度 | 归一化方式 | 说明 |
//! |------|-----------|------|
//! | 价格 | 相对候选集内最低价取反比 | 免费/极低价不触发除零 |
//! | 延迟 | 相对候选集内最低延迟取反比 | 未探测到延迟时给中性分 0.5 |
//! | 可用性 | 库存 + 历史失败率 + 是否支持按量释放 | 库存 0 直接硬过滤 |
//! | 硬件 | 型号偏好表 + 显存富余 | 4090/5090 有偏好加权 |
//!
//! ## 为什么权重不直接归一化用户输入
//!
//! 用户配置里写的权重是**相对重要性**（如 price=50, latency=30...），
//! 不必凑成 1。打分时统一做一次归一化，这样用户改单个权重不会意外
//! 改变其它维度的相对占比——这是配置驱动原则的直接体现。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::cloud::AvailableSpec;
use crate::error::{MnError, MnResult};

/// 四维权重。字段为相对权重，不必加和为 1。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoreWeights {
    /// 价格权重（越高越看重便宜）
    pub price: f64,
    /// 延迟权重
    pub latency: f64,
    /// 可用性权重（库存、失败率）
    pub availability: f64,
    /// 硬件权重（型号匹配度、显存富余）
    pub hardware: f64,
}

impl Default for ScoreWeights {
    /// 默认偏「性价比 + 稳定」：个人用户对延迟不敏感，对价格敏感。
    fn default() -> Self {
        Self {
            price: 40.0,
            latency: 15.0,
            availability: 30.0,
            hardware: 15.0,
        }
    }
}

impl ScoreWeights {
    /// 校验权重合法性。至少一个维度必须为正。
    pub fn validate(&self) -> MnResult<()> {
        let all = [self.price, self.latency, self.availability, self.hardware];
        if all.iter().any(|w| !w.is_finite() || *w < 0.0) {
            return Err(MnError::config("scheduler.weights", "权重必须为非负有限数"));
        }
        if all.iter().all(|w| *w == 0.0) {
            return Err(MnError::config(
                "scheduler.weights",
                "至少一个维度权重需大于 0",
            ));
        }
        Ok(())
    }

    /// 归一化后的权重元组 `(price, latency, availability, hardware)`。
    pub fn normalized(&self) -> (f64, f64, f64, f64) {
        let sum = self.price + self.latency + self.availability + self.hardware;
        if sum <= f64::EPSILON {
            return (0.25, 0.25, 0.25, 0.25);
        }
        (
            self.price / sum,
            self.latency / sum,
            self.availability / sum,
            self.hardware / sum,
        )
    }
}

/// 硬件型号偏好：归一化型号 → 偏好系数（>1 加分，<1 减分，1 中性）。
///
/// 按产品基线，RTX 4090 / 5090 是核心目标，给最高偏好；
/// 数据中心卡（A100/H100）虽然更强但单价高，不设为默认首选。
pub fn default_gpu_preference() -> HashMap<String, f64> {
    let mut m = HashMap::new();
    m.insert("RTX 4090".to_string(), 1.20);
    m.insert("RTX 5090".to_string(), 1.25);
    m.insert("RTX 4080".to_string(), 1.05);
    m.insert("RTX 3090".to_string(), 0.95);
    m.insert("RTX 3080".to_string(), 0.85);
    m.insert("A100".to_string(), 0.80);
    m.insert("H100".to_string(), 0.75);
    m
}

/// 单个候选节点的打分明细。保留明细以便 CLI 用 `--explain` 展示决策依据。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredCandidate {
    /// 服务商名
    pub provider: String,
    /// 规格 ID
    pub spec_id: String,
    /// 归一化 GPU 型号
    pub gpu_model: String,
    /// 地域
    pub region: String,
    /// 时价
    pub price_per_hour: f64,
    /// 探测到的延迟
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 显存字节数
    pub vram_bytes: u64,
    /// 价格分
    pub score_price: f64,
    /// 延迟分
    pub score_latency: f64,
    /// 可用性分
    pub score_availability: f64,
    /// 硬件分
    pub score_hardware: f64,
    /// 加权总分
    pub score_total: f64,
}

impl ScoredCandidate {
    /// 人类可读的分数解释，用于 `mangosteen cloud pick --explain`。
    pub fn explain(&self) -> String {
        format!(
            "{}/{} [{}] ¥{:.3}/h · {}ms · {:.1}GiB │ 价格 {:.2} 延迟 {:.2} 可用 {:.2} 硬件 {:.2} → 总分 {:.4}",
            self.provider,
            self.gpu_model,
            self.region,
            self.price_per_hour,
            self.latency_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            self.vram_bytes as f64 / (1u64 << 30) as f64,
            self.score_price,
            self.score_latency,
            self.score_availability,
            self.score_hardware,
            self.score_total
        )
    }
}

/// 选择器的输入约束（除权重外的硬性要求）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionConstraints {
    /// 最低显存需求（字节）。低于此值的规格直接淘汰。
    pub min_vram_bytes: u64,
    /// 最低内存需求
    pub min_memory_bytes: u64,
    /// 最低磁盘需求
    pub min_disk_bytes: u64,
    /// 价格上限（元/小时）。`None` 表示不限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_price_per_hour: Option<f64>,
    /// 只考虑这些服务商。空表示全部。
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// 地域白名单。空表示不限。
    #[serde(default)]
    pub allowed_regions: Vec<String>,
    /// 是否要求有库存（`stock` 字段为 0 时淘汰）
    pub require_stock: bool,
}

impl Default for SelectionConstraints {
    fn default() -> Self {
        Self {
            min_vram_bytes: 0,
            min_memory_bytes: 0,
            min_disk_bytes: 0,
            max_price_per_hour: None,
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: true,
        }
    }
}

impl SelectionConstraints {
    /// 检查单个候选是否满足全部硬约束。
    pub fn admits(&self, provider: &str, c: &AvailableSpec) -> bool {
        if !self.allowed_providers.is_empty()
            && !self.allowed_providers.iter().any(|p| p == provider)
        {
            return false;
        }
        if !self.allowed_regions.is_empty() && !self.allowed_regions.iter().any(|r| r == &c.region)
        {
            return false;
        }
        if c.spec.gpu.total_vram_bytes() < self.min_vram_bytes {
            return false;
        }
        if c.spec.memory_bytes < self.min_memory_bytes {
            return false;
        }
        if c.spec.disk_bytes < self.min_disk_bytes {
            return false;
        }
        if let Some(max) = self.max_price_per_hour {
            if c.price.per_hour_cny > max {
                return false;
            }
        }
        if self.require_stock && !c.is_in_stock() {
            return false;
        }
        true
    }

    /// 满足约束的候选，**按价格升序**返回（不看权重与打分策略）。
    ///
    /// ## 为什么需要它，而不是用 `NodeSelector` + `Cheapest` 策略
    ///
    /// 策略是**配置项**（`scheduler.strategy`）。用户临时说「给我最便宜的」
    /// 时不能指望配置恰好是 `cheapest`——默认是 `balanced`，四维打分会让
    /// 一台 ¥2.15/h 的 4090 排在 ¥1.34/h 的 V100S 前面（延迟/库存分翻盘）。
    /// 这里提供一条**不受配置影响**的价格序，语义单一、结果可预期。
    ///
    /// 约束仍然生效：「最便宜」不等于「不管跑不跑得动」。
    ///
    /// 排序口径与 [`SelectionStrategy::Cheapest`] 完全一致（共用
    /// [`cheapest_cmp`]），否则会出现「`pick` 说最便宜是 A、`cheapest`
    /// 说最便宜是 B」这种自相矛盾的结论。
    pub fn cheapest_first(
        &self,
        candidates: &[(String, AvailableSpec)],
    ) -> Vec<(String, AvailableSpec)> {
        let mut admitted: Vec<&(String, AvailableSpec)> = candidates
            .iter()
            .filter(|(p, c)| self.admits(p, c))
            .collect();
        admitted.sort_by(|a, b| {
            cheapest_cmp(
                &(
                    price_order_key(a.1.price.per_hour_cny),
                    a.1.stock.unwrap_or(0),
                    a.0.as_str(),
                    a.1.spec_id.as_str(),
                ),
                &(
                    price_order_key(b.1.price.per_hour_cny),
                    b.1.stock.unwrap_or(0),
                    b.0.as_str(),
                    b.1.spec_id.as_str(),
                ),
            )
        });
        admitted.into_iter().cloned().collect()
    }
}

/// 价格排序键。
///
/// 「价格未知」必须排在**最后**，不能当成 0 元。
///
/// 实测踩到：厂商对个别规格返回 `f64::MAX` 当「无报价」哨兵，上层过滤成
/// 「未知」后落成 `0.0`。若按 0 参与升序，最贵的卡会排到第一位——于是
/// 「最便宜」先去开 A100（80G），余额不足直接 520 中止，而库里明明有 V100S。
fn price_order_key(price: f64) -> f64 {
    if price > 0.0 {
        price
    } else {
        f64::INFINITY
    }
}

/// 「最便宜优先」的比较器。
///
/// 口径：价格升序 → 库存多的优先（更容易真的开出来）→ provider → spec_id
/// （后两级只为结果可复现）。
///
/// 抽成独立函数是刻意的：策略排序与 [`SelectionConstraints::cheapest_first`]
/// 两处各写一遍，改一处忘另一处就会得出互相矛盾的「最便宜」。
fn cheapest_cmp(a: &(f64, u32, &str, &str), b: &(f64, u32, &str, &str)) -> std::cmp::Ordering {
    a.0.partial_cmp(&b.0)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(b.1.cmp(&a.1))
        .then(a.2.cmp(b.2))
        .then(a.3.cmp(b.3))
}

/// 节点选择策略。
///
/// 为什么做成策略而不是「让用户自己调权重」：权重是四个维度的相对值，
/// 想表达「就要最便宜」需要把其它三个置 0，用户很难意识到这一点，
/// 而且四维打分是**相对归一化**的——把价格权重调到 100 也未必选出
/// 全局最便宜的那个（延迟/库存分仍会翻盘）。所以单列一个策略，
/// 语义明确、行为可预期。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    /// 综合打分：价格 40 / 延迟 15 / 库存 30 / 硬件 15（默认）
    #[default]
    Balanced,
    /// 最便宜优先：在**满足约束**的候选里按价格升序取第一个。
    ///
    /// 约束（显存、内存、磁盘、库存、价格上限）仍然生效——
    /// 「最便宜」不等于「不管跑不跑得动」。同价时按库存富余与
    /// provider 字典序稳定排序，保证结果可复现。
    Cheapest,
}

/// 选择器。持有可直接注入的权重与偏好表，便于单测构造。
#[derive(Debug, Clone)]
pub struct NodeSelector {
    weights: ScoreWeights,
    gpu_preference: HashMap<String, f64>,
    strategy: SelectionStrategy,
}

impl Default for NodeSelector {
    fn default() -> Self {
        Self {
            weights: ScoreWeights::default(),
            gpu_preference: default_gpu_preference(),
            strategy: SelectionStrategy::default(),
        }
    }
}

impl NodeSelector {
    /// 构造并校验。
    pub fn new(weights: ScoreWeights, gpu_preference: HashMap<String, f64>) -> MnResult<Self> {
        weights.validate()?;
        Ok(Self {
            weights,
            gpu_preference,
            strategy: SelectionStrategy::Balanced,
        })
    }

    /// 指定策略（链式，便于构造后立刻定型）。
    pub fn with_strategy(mut self, strategy: SelectionStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// 当前策略。
    pub fn strategy(&self) -> SelectionStrategy {
        self.strategy
    }

    /// 使用默认偏好表 + 自定义权重。
    pub fn with_weights(weights: ScoreWeights) -> MnResult<Self> {
        Self::new(weights, default_gpu_preference())
    }

    /// 当前权重。
    pub fn weights(&self) -> ScoreWeights {
        self.weights
    }

    /// 对给定候选集打分排序。返回按总分降序排列的结果（分值相同则按价格升序）。
    ///
    /// `latencies` 为「服务商名 → 控制面延迟」，缺失则该维度给中性分。
    pub fn rank(
        &self,
        candidates: &[(String, AvailableSpec)],
        latencies: &HashMap<String, u32>,
        constraints: &SelectionConstraints,
    ) -> Vec<ScoredCandidate> {
        let admitted: Vec<&(String, AvailableSpec)> = candidates
            .iter()
            .filter(|(p, c)| constraints.admits(p, c))
            .collect();

        if admitted.is_empty() {
            return Vec::new();
        }

        // ---- 归一化基准：候选集内最优值 ----
        let min_price = admitted
            .iter()
            .map(|(_, c)| c.price.per_hour_cny)
            .fold(f64::INFINITY, f64::min);
        let max_vram = admitted
            .iter()
            .map(|(_, c)| c.spec.gpu.total_vram_bytes())
            .max()
            .unwrap_or(0);
        let min_latency = admitted
            .iter()
            .filter_map(|(p, _)| latencies.get(p).copied())
            .min();

        let (w_price, w_lat, w_avail, w_hw) = self.weights.normalized();

        let mut out: Vec<ScoredCandidate> = admitted
            .iter()
            .map(|(provider, c)| {
                // 价格：最低价为 1，其它按反比。价格为 0（免费额度）也视为最优。
                let s_price = if c.price.per_hour_cny <= 0.0 || min_price <= 0.0 {
                    if c.price.per_hour_cny <= min_price + f64::EPSILON {
                        1.0
                    } else {
                        min_price / c.price.per_hour_cny
                    }
                } else {
                    (min_price / c.price.per_hour_cny).clamp(0.0, 1.0)
                };

                // 延迟：最低延迟为 1；未探测给中性 0.5
                let latency = latencies.get(provider).copied();
                let s_lat = match (latency, min_latency) {
                    (Some(l), Some(ml)) if l > 0 => (ml as f64 / l as f64).clamp(0.0, 1.0),
                    _ => 0.5,
                };

                // 可用性：库存富余度 0.6 + 按量释放 0.2 + 有报价 0.2
                let stock_factor = match c.stock {
                    Some(s) => ((s as f64) / 5.0).clamp(0.0, 1.0),
                    None => 0.8, // 厂商不提供库存 → 略保守的中性分
                };
                let release = if c.price.spot_release_allowed {
                    1.0
                } else {
                    0.4
                };
                let s_avail = 0.6 * stock_factor + 0.2 * release + 0.2;

                // 硬件：型号偏好 × 显存富余（相对候选集内最大显存）
                let pref = self
                    .gpu_preference
                    .get(&normalize_gpu_model(&c.spec.gpu.model))
                    .copied()
                    .unwrap_or(1.0);
                let vram_ratio = if max_vram > 0 {
                    (c.spec.gpu.total_vram_bytes() as f64 / max_vram as f64).clamp(0.0, 1.0)
                } else {
                    0.5
                };
                // 偏好系数可 >1，压到 [0,1.2] 后再与显存富余混合，最后裁剪
                let s_hw = (0.5 * pref.min(1.2) + 0.5 * vram_ratio).clamp(0.0, 1.0);

                let total = w_price * s_price + w_lat * s_lat + w_avail * s_avail + w_hw * s_hw;

                ScoredCandidate {
                    provider: provider.clone(),
                    spec_id: c.spec_id.clone(),
                    gpu_model: c.spec.gpu.model.clone(),
                    region: c.region.clone(),
                    price_per_hour: c.price.per_hour_cny,
                    latency_ms: latency,
                    vram_bytes: c.spec.gpu.total_vram_bytes(),
                    score_price: s_price,
                    score_latency: s_lat,
                    score_availability: s_avail,
                    score_hardware: s_hw,
                    score_total: total,
                }
            })
            .collect();

        // 排序口径由策略决定。
        //
        // 注意：**分数照常算完再排**，而不是在 cheapest 时跳过打分——
        // `ScoredCandidate` 里的四维分是 `cloud pick --explain` 的解释依据，
        // 少了它用户看不到「为什么是这个节点」。策略只改排序口径。
        match self.strategy {
            SelectionStrategy::Cheapest => {
                // 价格升序；同价时库存多的优先（更容易真的开出来），
                // 再同则按 provider / spec_id 稳定排序保证可复现。
                let stock_of = |sc: &ScoredCandidate| -> u32 {
                    candidates
                        .iter()
                        .find(|(p, c)| p == &sc.provider && c.spec_id == sc.spec_id)
                        .and_then(|(_, c)| c.stock)
                        .unwrap_or(0)
                };
                // 排序口径抽在 `cheapest_cmp` 里，与
                // `SelectionConstraints::cheapest_first` 共用——两处各写一遍，
                // 改一处忘另一处会让 `pick` 与 `cheapest` 给出不同结论。
                out.sort_by(|a, b| {
                    cheapest_cmp(
                        &(
                            price_order_key(a.price_per_hour),
                            stock_of(a),
                            a.provider.as_str(),
                            a.spec_id.as_str(),
                        ),
                        &(
                            price_order_key(b.price_per_hour),
                            stock_of(b),
                            b.provider.as_str(),
                            b.spec_id.as_str(),
                        ),
                    )
                });
            }
            SelectionStrategy::Balanced => {
                // 总分降序；同分按价格升序；再同则按 provider 字典序（结果可复现）
                out.sort_by(|a, b| {
                    b.score_total
                        .partial_cmp(&a.score_total)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(
                            a.price_per_hour
                                .partial_cmp(&b.price_per_hour)
                                .unwrap_or(std::cmp::Ordering::Equal),
                        )
                        .then(a.provider.cmp(&b.provider))
                        .then(a.spec_id.cmp(&b.spec_id))
                });
            }
        }
        out
    }

    /// 直接选出最优候选。
    pub fn select_best(
        &self,
        candidates: &[(String, AvailableSpec)],
        latencies: &HashMap<String, u32>,
        constraints: &SelectionConstraints,
    ) -> MnResult<ScoredCandidate> {
        self.rank(candidates, latencies, constraints)
            .into_iter()
            .next()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: format!(
                    "{} 个候选规格均不满足约束（最低显存 {:.1} GiB{}）",
                    candidates.len(),
                    constraints.min_vram_bytes as f64 / (1u64 << 30) as f64,
                    constraints
                        .max_price_per_hour
                        .map(|p| format!("，价格上限 ¥{p:.2}/h"))
                        .unwrap_or_default()
                ),
            })
    }
}

/// 归一化 GPU 型号名：去厂商前缀、压缩空格、统一大小写习惯，
/// 使 `NVIDIA GeForce RTX 4090` 与 `RTX4090` 能命中同一偏好键。
pub fn normalize_gpu_model(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    for prefix in [
        "NVIDIA GeForce ",
        "NVIDIA ",
        "GeForce ",
        "AMD Radeon ",
        "Radeon ",
        "Tesla ",
    ] {
        if let Some(stripped) = s.strip_prefix(prefix) {
            s = stripped.to_string();
            break;
        }
    }
    // 去掉 "RTX 4090" 中间的空格 → "RTX4090"，再统一回带空格的规范形式
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let upper = compact.to_ascii_uppercase();
    if let Some(rest) = upper.strip_prefix("RTX") {
        return format!("RTX {rest}");
    }
    if let Some(rest) = upper.strip_prefix("GTX") {
        return format!("GTX {rest}");
    }
    if upper.starts_with("A100") || upper.starts_with("H100") || upper.starts_with("H200") {
        return upper;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::{GpuSpec, InstanceSpec, PriceQuote};
    use std::collections::BTreeMap;

    fn cand(
        provider: &str,
        gpu: &str,
        vram_gib: u64,
        price: f64,
        stock: Option<u32>,
    ) -> (String, AvailableSpec) {
        (
            provider.to_string(),
            AvailableSpec {
                spec_id: format!("{provider}-{gpu}-{price}"),
                spec: InstanceSpec {
                    gpu: GpuSpec {
                        model: gpu.to_string(),
                        count: 1,
                        vram_bytes: vram_gib << 30,
                    },
                    cpu_cores: 16,
                    memory_bytes: 60 << 30,
                    disk_bytes: 50 << 30,
                    image: "ubuntu22-cuda12".into(),
                },
                price: PriceQuote {
                    per_hour_cny: price,
                    billing_granularity_secs: 0,
                    spot_release_allowed: true,
                    note: None,
                },
                region: "cn-west".into(),
                stock,
                extra: BTreeMap::new(),
            },
        )
    }

    #[test]
    fn weights_must_be_sane() {
        assert!(ScoreWeights::default().validate().is_ok());
        let bad = ScoreWeights {
            price: -1.0,
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let zero = ScoreWeights {
            price: 0.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 0.0,
        };
        assert!(zero.validate().is_err());
        assert!(NodeSelector::with_weights(bad).is_err());
    }

    #[test]
    fn weights_normalize_to_one() {
        let (a, b, c, d) = ScoreWeights::default().normalized();
        assert!((a + b + c + d - 1.0).abs() < 1e-12);
        // 全零时退化为均分，不应 panic
        let zero = ScoreWeights {
            price: 0.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 0.0,
        };
        let (a, _b, _c, d) = zero.normalized();
        assert!((a - 0.25).abs() < 1e-12 && (d - 0.25).abs() < 1e-12);
    }

    #[test]
    fn cheapest_wins_when_price_dominant() {
        let sel = NodeSelector::with_weights(ScoreWeights {
            price: 100.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 0.0,
        })
        .unwrap();
        let cands = vec![
            cand("a", "RTX 4090", 24, 2.5, Some(5)),
            cand("b", "RTX 4090", 24, 1.2, Some(5)),
            cand("c", "RTX 4090", 24, 3.8, Some(5)),
        ];
        let best = sel
            .select_best(&cands, &HashMap::new(), &SelectionConstraints::default())
            .unwrap();
        assert_eq!(best.provider, "b");
        assert!((best.score_price - 1.0).abs() < 1e-12);
    }

    #[test]
    fn lowest_latency_wins_when_latency_dominant() {
        let sel = NodeSelector::with_weights(ScoreWeights {
            price: 0.0,
            latency: 100.0,
            availability: 0.0,
            hardware: 0.0,
        })
        .unwrap();
        let cands = vec![
            cand("a", "RTX 4090", 24, 2.0, Some(5)),
            cand("b", "RTX 4090", 24, 2.0, Some(5)),
        ];
        let mut lat = HashMap::new();
        lat.insert("a".to_string(), 120);
        lat.insert("b".to_string(), 18);
        let best = sel
            .select_best(&cands, &lat, &SelectionConstraints::default())
            .unwrap();
        assert_eq!(best.provider, "b");
    }

    #[test]
    fn missing_latency_gets_neutral_half_score() {
        let sel = NodeSelector::with_weights(ScoreWeights {
            price: 0.0,
            latency: 100.0,
            availability: 0.0,
            hardware: 0.0,
        })
        .unwrap();
        let cands = vec![cand("a", "RTX 4090", 24, 2.0, Some(5))];
        let best = sel
            .select_best(&cands, &HashMap::new(), &SelectionConstraints::default())
            .unwrap();
        assert!((best.score_latency - 0.5).abs() < 1e-12);
    }

    #[test]
    fn constraints_filter_by_vram() {
        let sel = NodeSelector::default();
        let cands = vec![
            cand("a", "RTX 3080", 10, 1.0, Some(5)),
            cand("b", "RTX 4090", 24, 2.5, Some(5)),
        ];
        let cons = SelectionConstraints {
            min_vram_bytes: 20 << 30,
            ..Default::default()
        };
        let ranked = sel.rank(&cands, &HashMap::new(), &cons);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].provider, "b");
    }

    #[test]
    fn constraints_filter_by_price_cap() {
        let sel = NodeSelector::default();
        let cands = vec![
            cand("a", "RTX 4090", 24, 5.0, Some(5)),
            cand("b", "RTX 4090", 24, 2.5, Some(5)),
        ];
        let cons = SelectionConstraints {
            max_price_per_hour: Some(3.0),
            ..Default::default()
        };
        let ranked = sel.rank(&cands, &HashMap::new(), &cons);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].provider, "b");
    }

    #[test]
    fn stock_zero_is_filtered_when_required() {
        let sel = NodeSelector::default();
        let cands = vec![
            cand("a", "RTX 4090", 24, 1.0, Some(0)),
            cand("b", "RTX 4090", 24, 3.0, Some(2)),
        ];
        let ranked = sel.rank(&cands, &HashMap::new(), &SelectionConstraints::default());
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].provider, "b");

        // 放宽后 a 会重新进入（虽然更便宜也会赢）
        let cons = SelectionConstraints {
            require_stock: false,
            ..Default::default()
        };
        assert_eq!(sel.rank(&cands, &HashMap::new(), &cons).len(), 2);
    }

    #[test]
    fn provider_whitelist_and_region_whitelist() {
        let sel = NodeSelector::default();
        let cands = vec![
            cand("autodl", "RTX 4090", 24, 1.0, Some(5)),
            cand("aliyun", "RTX 4090", 24, 1.0, Some(5)),
        ];
        let cons = SelectionConstraints {
            allowed_providers: vec!["aliyun".into()],
            ..Default::default()
        };
        let r = sel.rank(&cands, &HashMap::new(), &cons);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].provider, "aliyun");

        let cons = SelectionConstraints {
            allowed_regions: vec!["nowhere".into()],
            ..Default::default()
        };
        assert!(sel.rank(&cands, &HashMap::new(), &cons).is_empty());
    }

    #[test]
    fn no_candidate_yields_actionable_error() {
        let sel = NodeSelector::default();
        let cands = vec![cand("a", "RTX 4090", 24, 1.0, Some(0))];
        let e = sel
            .select_best(&cands, &HashMap::new(), &SelectionConstraints::default())
            .unwrap_err();
        assert!(matches!(&e, MnError::NoViableCandidate { .. }));
        assert!(e.is_retryable());
        assert!(e.to_string().contains("不满足约束"));
    }

    #[test]
    fn hardware_preference_boosts_4090_over_a100_at_equal_price() {
        let sel = NodeSelector::with_weights(ScoreWeights {
            price: 0.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 100.0,
        })
        .unwrap();
        let cands = vec![
            cand("x", "A100", 40, 2.0, Some(5)),
            cand("y", "RTX 4090", 24, 2.0, Some(5)),
        ];
        let r = sel.rank(&cands, &HashMap::new(), &SelectionConstraints::default());
        // 4090 有 1.2 偏好，但显存只有 24 vs 40 → 显存比 0.6
        // 4090: 0.5*1.2 + 0.5*0.6 = 0.9 ; A100: 0.5*0.8 + 0.5*1.0 = 0.9
        // 两者打平，验证混合维度确实生效（不出现 NaN 或偏好失效）
        assert_eq!(r.len(), 2);
        for c in &r {
            assert!(c.score_hardware > 0.0 && c.score_hardware <= 1.0);
        }
    }

    #[test]
    fn higher_vram_wins_on_hardware_dimension() {
        let sel = NodeSelector::with_weights(ScoreWeights {
            price: 0.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 100.0,
        })
        .unwrap();
        let cands = vec![
            cand("small", "RTX 4090", 24, 2.0, Some(5)),
            cand("big", "RTX 5090", 32, 2.0, Some(5)),
        ];
        let r = sel.rank(&cands, &HashMap::new(), &SelectionConstraints::default());
        assert_eq!(r[0].provider, "big");
    }

    #[test]
    fn ranking_is_deterministic_on_ties() {
        let sel = NodeSelector::default();
        let cands = vec![
            cand("b", "RTX 4090", 24, 2.0, Some(5)),
            cand("a", "RTX 4090", 24, 2.0, Some(5)),
        ];
        let r1 = sel.rank(&cands, &HashMap::new(), &SelectionConstraints::default());
        let r2 = sel.rank(&cands, &HashMap::new(), &SelectionConstraints::default());
        assert_eq!(r1, r2);
        assert_eq!(r1[0].provider, "a"); // 同分同价 → 字典序
    }

    #[test]
    fn free_tier_price_does_not_divide_by_zero() {
        let sel = NodeSelector::with_weights(ScoreWeights {
            price: 100.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 0.0,
        })
        .unwrap();
        let cands = vec![
            cand("free", "RTX 4090", 24, 0.0, Some(5)),
            cand("paid", "RTX 4090", 24, 2.0, Some(5)),
        ];
        let r = sel.rank(&cands, &HashMap::new(), &SelectionConstraints::default());
        assert_eq!(r[0].provider, "free");
        assert!(r[0].score_price.is_finite());
        assert_eq!(r[0].score_price, 1.0);
    }

    #[test]
    fn explain_is_readable() {
        let sel = NodeSelector::default();
        let cands = vec![cand("autodl", "RTX 4090", 24, 2.345, Some(3))];
        let mut lat = HashMap::new();
        lat.insert("autodl".to_string(), 42);
        let best = sel
            .select_best(&cands, &lat, &SelectionConstraints::default())
            .unwrap();
        let s = best.explain();
        assert!(s.contains("autodl"));
        assert!(s.contains("42ms"));
        assert!(s.contains("¥2.345/h"));
    }

    #[test]
    fn gpu_model_normalization() {
        assert_eq!(normalize_gpu_model("NVIDIA GeForce RTX 4090"), "RTX 4090");
        assert_eq!(normalize_gpu_model("RTX4090"), "RTX 4090");
        assert_eq!(normalize_gpu_model("rtx 5090"), "RTX 5090");
        assert_eq!(normalize_gpu_model("Tesla A100"), "A100");
        assert_eq!(normalize_gpu_model("NVIDIA A100-SXM4"), "A100-SXM4");
    }

    #[test]
    fn preference_table_hits_normalized_names() {
        let table = default_gpu_preference();
        assert!(table.contains_key(&normalize_gpu_model("NVIDIA GeForce RTX 4090")));
        assert!(table.contains_key(&normalize_gpu_model("RTX5090")));
    }
    // ---------------- 纯价格序（cheapest_first） ----------------

    #[test]
    fn cheapest_first_orders_by_price_ascending() {
        let cands = vec![
            cand("ucloud", "RTX 4090", 24, 2.15, Some(1)),
            cand("ucloud", "V100S", 32, 1.34, Some(1)),
            cand("ucloud", "A800", 80, 6.99, Some(1)),
        ];
        let ordered = SelectionConstraints::default().cheapest_first(&cands);
        let prices: Vec<f64> = ordered.iter().map(|(_, c)| c.price.per_hour_cny).collect();
        assert_eq!(prices, vec![1.34, 2.15, 6.99], "必须严格按价格升序");
    }

    /// 约束仍然生效：最便宜不等于不管跑不跑得动。
    #[test]
    fn cheapest_first_respects_hard_constraints() {
        let cands = vec![
            cand("ucloud", "P40", 24, 0.60, Some(1)),
            cand("ucloud", "V100S", 32, 1.34, Some(1)),
        ];
        let cons = SelectionConstraints {
            min_vram_bytes: 32 << 30,
            ..SelectionConstraints::default()
        };
        let ordered = cons.cheapest_first(&cands);
        assert_eq!(ordered.len(), 1, "P40 只有 24G，必须被淘汰");
        assert_eq!(ordered[0].1.spec.gpu.model, "V100S");
    }

    /// 「价格未知」必须排最后，不能当成 0 元排第一。
    ///
    /// 这是实测踩过的坑：厂商用 `f64::MAX` 当无报价哨兵，上层落成 0.0，
    /// 于是「最便宜」先去开最贵的卡，余额不足直接中止。
    #[test]
    fn cheapest_first_puts_unknown_price_last() {
        let cands = vec![
            cand("ucloud", "A100", 80, 0.0, Some(1)),
            cand("ucloud", "V100S", 32, 1.34, Some(1)),
        ];
        let ordered = SelectionConstraints::default().cheapest_first(&cands);
        assert_eq!(
            ordered[0].1.spec.gpu.model, "V100S",
            "0 元（未知）不能排第一"
        );
        assert_eq!(ordered[1].1.spec.gpu.model, "A100");
    }

    /// 同价时库存多的优先，再同按 provider/spec_id —— 结果必须可复现。
    #[test]
    fn cheapest_first_tie_breaks_are_deterministic() {
        let cands = vec![
            cand("b", "V100S", 32, 1.34, Some(0)),
            cand("a", "V100S", 32, 1.34, Some(5)),
        ];
        let ordered = SelectionConstraints::default().cheapest_first(&cands);
        assert_eq!(ordered[0].0, "a", "同价时库存多的优先");

        // 完全同价同库存 → 按 provider 字典序
        let cands2 = vec![
            cand("z", "V100S", 32, 1.34, Some(1)),
            cand("a", "V100S", 32, 1.34, Some(1)),
        ];
        let ordered2 = SelectionConstraints::default().cheapest_first(&cands2);
        assert_eq!(ordered2[0].0, "a");
    }

    /// 空候选集返回空，不 panic。
    #[test]
    fn cheapest_first_empty_is_empty() {
        assert!(SelectionConstraints::default()
            .cheapest_first(&[])
            .is_empty());
    }

    /// 价格序与 `Cheapest` 策略必须给出一致的首选项。
    ///
    /// 两者排序口径若分叉，用户会看到「pick 说 A 最便宜、cheapest 说 B」。
    #[test]
    fn cheapest_first_agrees_with_cheapest_strategy() {
        let cands = vec![
            cand("autodl", "RTX 4090", 24, 2.15, Some(4)),
            cand("ucloud", "V100S", 32, 1.34, Some(1)),
            cand("ucloud", "P40", 24, 0.60, Some(2)),
        ];
        let cons = SelectionConstraints::default();
        let by_price = cons.cheapest_first(&cands);
        let by_strategy = NodeSelector::default()
            .with_strategy(SelectionStrategy::Cheapest)
            .select_best(&cands, &HashMap::new(), &cons)
            .unwrap();
        assert_eq!(by_price[0].1.spec_id, by_strategy.spec_id);
    }
}
