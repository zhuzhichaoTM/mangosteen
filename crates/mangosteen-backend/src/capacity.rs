//! 跨区域 GPU 库存矩阵扫描。
//!
//! 归属：**算力执行层 · 云端后端管理**（`CheckCompShareResourceCapacity` 的批量封装）。
//!
//! ## 为什么需要单独一个模块
//!
//! 厂商的库存接口只回答「**某个可用区 + 某个卡型 + 某个镜像 + 某个计费模式**
//! 下哪些 (GPU卡数, CPU核数, 内存) 组合还有货」。要回答「**全平台现在哪里能开到
//! 4090**」这种问题，就得把若干维度组合起来逐个问：
//!
//! ```text
//! 可用区(5) × 卡型(11) × 计费模式(1~5)  = 最多 55+ 次请求
//! ```
//!
//! 如果直接串行全量扫，既慢又容易触发厂商限流。本模块的设计目标是：
//!
//! 1. **按需裁剪**：先用机型表（1 次）算出「该可用区到底有哪些卡型」，
//!    再只对这些卡型发库存请求，避免盲扫；
//! 2. **跳过无意义组合**：容器可用区不支持系统镜像时直接跳过（必然会失败）；
//! 3. **单点失败不中断**：某个可用区/卡型失败只记录原因，其余继续；
//! 4. **结果可聚合**：最终按卡型汇总「哪些可用区有货、最低有货配置是什么」。
//!
//! ## 与驱动内 `ensure_capacity` 的分工
//!
//! * `ensure_capacity`：创建前对**一个**组合做确认，失败即中止（安全闸门）；
//! * 本模块：为「选点」收集**全部**可用信息，允许部分失败（决策辅助）。
//!
//! 两者调用同一个底层接口，不允许各自拼参数。

use std::collections::BTreeMap;

use mangosteen_core::MnResult;

use crate::client::{CapacityRequest, CompshareClient};
use crate::model::{CapacitySpec, ZoneInfo};
use crate::transport::Transport;

/// 单个 (可用区, 卡型) 的库存查询结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneGpuCapacity {
    /// 地域
    pub region: String,
    /// 可用区
    pub zone: String,
    /// 可用区显示名
    pub zone_name: String,
    /// 卡型代号（如 `4090`）
    pub gpu_type: String,
    /// 该可用区给出的所有规格组合及其可用性
    pub specs: Vec<CapacitySpec>,
    /// 查询失败原因（`None` 表示查询成功）
    pub error: Option<String>,
}

impl ZoneGpuCapacity {
    /// 是否有任意组合可用。
    pub fn has_any(&self) -> bool {
        self.specs.iter().any(|s| s.resource_enough)
    }

    /// 可用组合（有货的那些）。
    pub fn available_specs(&self) -> Vec<&CapacitySpec> {
        self.specs.iter().filter(|s| s.resource_enough).collect()
    }

    /// 有货组合里最小的那个（最少卡数、最少核数、最小内存）。
    ///
    /// 「够用就好」场景下，最小可用配置通常就是最便宜的选择。
    pub fn smallest_available(&self) -> Option<&CapacitySpec> {
        self.specs
            .iter()
            .filter(|s| s.resource_enough)
            .min_by_key(|s| (s.gpu, s.cpu, s.mem_gb))
    }

    /// 有货组合里最大的那个（用于判断能否跑大模型）。
    pub fn largest_available(&self) -> Option<&CapacitySpec> {
        self.specs
            .iter()
            .filter(|s| s.resource_enough)
            .max_by_key(|s| (s.gpu, s.cpu, s.mem_gb))
    }
}

/// 跨区域的库存扫描结果。
#[derive(Debug, Clone, Default)]
pub struct CapacityMatrix {
    /// 逐 (可用区, 卡型) 的原始结果（保持扫描顺序）
    pub entries: Vec<ZoneGpuCapacity>,
    /// 扫描中跳过的组合及原因（便于向用户解释「为什么少查了」）
    pub skipped: Vec<String>,
}

impl CapacityMatrix {
    /// 全部有货的条目。
    pub fn in_stock(&self) -> Vec<&ZoneGpuCapacity> {
        self.entries.iter().filter(|e| e.has_any()).collect()
    }

    /// 有货条目数。
    pub fn in_stock_count(&self) -> usize {
        self.in_stock().len()
    }

    /// 查询失败条目数。
    pub fn failed_count(&self) -> usize {
        self.entries.iter().filter(|e| e.error.is_some()).count()
    }

    /// 按卡型汇总：卡型 → 有货的可用区列表。
    pub fn by_gpu_type(&self) -> BTreeMap<String, Vec<&ZoneGpuCapacity>> {
        let mut m: BTreeMap<String, Vec<&ZoneGpuCapacity>> = BTreeMap::new();
        for e in &self.entries {
            if e.has_any() {
                m.entry(e.gpu_type.clone()).or_default().push(e);
            }
        }
        m
    }

    /// 按地域汇总：地域 → 有货的条目。
    pub fn by_region(&self) -> BTreeMap<String, Vec<&ZoneGpuCapacity>> {
        let mut m: BTreeMap<String, Vec<&ZoneGpuCapacity>> = BTreeMap::new();
        for e in &self.entries {
            if e.has_any() {
                m.entry(e.region.clone()).or_default().push(e);
            }
        }
        m
    }

    /// 某卡型在全平台是否至少有一个可用区有货。
    ///
    /// 忽略首尾空白，便于直接传入用户输入（如配置里的 `gpu_types = " 4090 "`）。
    pub fn is_anywhere_in_stock(&self, gpu_type: &str) -> bool {
        let want = gpu_type.trim();
        self.entries
            .iter()
            .any(|e| e.gpu_type.eq_ignore_ascii_case(want) && e.has_any())
    }

    /// 缺货的卡型列表（查过但全平台无货）。用于告诉用户「哪些卡确实没货」。
    ///
    /// 与「查询失败」区分开：失败是**不知道**，缺货是**知道了但没有**。
    /// 两者对用户的含义完全不同，混在一起会误导（失败时可能实际有货）。
    pub fn out_of_stock_types(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in &self.entries {
            // 只要该卡型有一条成功返回，就以「该次结果」为准
            if e.error.is_none() && !e.has_any() && !out.iter().any(|x| x == &e.gpu_type) {
                out.push(e.gpu_type.clone());
            }
        }
        // 若同一卡型在某区有货，则它不属于「全平台缺货」
        out.retain(|t| !self.is_anywhere_in_stock(t));
        out
    }

    /// 推荐一个「最省事」的开机组合（卡型 + 可用区 + 规格）。
    ///
    /// 排序偏好：先比卡型偏好（4090/5090 这类消费卡更适合本项目定位），
    /// 再比最小可用配置的规模（越小越便宜）。
    pub fn recommend(&self) -> Option<&ZoneGpuCapacity> {
        self.in_stock().into_iter().min_by_key(|e| {
            let pref = gpu_preference_rank(&e.gpu_type);
            let spec = e.smallest_available().map(|s| (s.gpu, s.cpu, s.mem_gb));
            (pref, spec)
        })
    }

    /// 全部「有货」的 (可用区, 卡型, 规格) 三元组，用于批量查价。
    ///
    /// 只返回有货组合——给缺货配置查价没有意义，纯属浪费一次接口调用。
    /// 顺序稳定（先按矩阵顺序，再按规格从小到大），便于结果可复现。
    pub fn quotable_specs(&self) -> Vec<QuotableSpec> {
        let mut out = Vec::new();
        for e in &self.entries {
            if e.error.is_some() {
                continue;
            }
            let mut specs: Vec<&CapacitySpec> = e.available_specs();
            specs.sort_by_key(|s| (s.gpu, s.cpu, s.mem_gb));
            for s in specs {
                out.push(QuotableSpec {
                    region: e.region.clone(),
                    zone: e.zone.clone(),
                    zone_name: e.zone_name.clone(),
                    gpu_type: e.gpu_type.clone(),
                    gpu: s.gpu,
                    cpu: s.cpu,
                    mem_gb: s.mem_gb,
                });
            }
        }
        out
    }
}

/// 一个「有货且可查价」的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotableSpec {
    /// 地域
    pub region: String,
    /// 可用区
    pub zone: String,
    /// 可用区显示名
    pub zone_name: String,
    /// 卡型代号
    pub gpu_type: String,
    /// 卡数
    pub gpu: u32,
    /// CPU 核数
    pub cpu: u32,
    /// 内存（GB）
    pub mem_gb: u64,
}

impl QuotableSpec {
    /// 构成规格 ID（与 `crate::mapping::spec_id` 同格式）。
    pub fn spec_id(&self) -> String {
        crate::mapping::spec_id(&self.gpu_type, self.gpu, self.cpu, self.mem_gb)
    }

    /// 用 `Memory` 参数（MB）表示的取值——价格接口要求 MB。
    pub fn memory_mb(&self) -> u64 {
        self.mem_gb.saturating_mul(1024)
    }
}

/// 一条真实报价。
#[derive(Debug, Clone, PartialEq)]
pub struct PricedOption {
    /// 配置
    pub spec: QuotableSpec,
    /// 每小时价格（元），已按计费模式折算
    pub per_hour_cny: f64,
    /// 厂商原始金额
    pub raw_amount_cny: f64,
    /// 计费模式
    pub charge_type: String,
    /// 分项明细文字（便于核对价格构成）
    pub breakdown: String,
}

/// 按小时价格从低到高排序的报价集合。
#[derive(Debug, Clone, Default)]
pub struct PriceBoard {
    /// 成功取到价格的选项（按 per_hour 升序）
    pub options: Vec<PricedOption>,
    /// 查价失败的配置及原因
    pub failed: Vec<(QuotableSpec, String)>,
}

impl PriceBoard {
    /// 最便宜的选项。
    ///
    /// **自己算最小值而不是取 `first()`**：`PriceBoard` 是公开结构，
    /// 调用方可能手工构造或插入（测试、以及将来从缓存反序列化），
    /// 不能假设 `options` 已排好序。取 `first()` 会让「最便宜」
    /// 悄悄变成「第一个」——这类错误在真实计费场景下代价很高。
    pub fn cheapest(&self) -> Option<&PricedOption> {
        self.options.iter().min_by(|a, b| {
            a.per_hour_cny
                .partial_cmp(&b.per_hour_cny)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// 只要「按量可随时释放」的最便宜选项。
    ///
    /// 月付/天付不能在几分钟内释放，与「跑完就关」的使用方式冲突，
    /// 因此需要区分出来。
    pub fn cheapest_releasable(&self) -> Option<&PricedOption> {
        self.options
            .iter()
            .filter(|o| crate::mapping::is_spot_releasable(&o.charge_type))
            .min_by(|a, b| {
                a.per_hour_cny
                    .partial_cmp(&b.per_hour_cny)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// 单位显存成本（元 / GiB·小时），用于跨卡型比性价比。
    ///
    /// 大模型部署的瓶颈是显存，因此「每 GiB 显存每小时多少钱」
    /// 比裸价格更能反映真实性价比。
    pub fn sorted_by_vram_efficiency(
        &self,
        vram_gb_of: impl Fn(&str) -> u64,
    ) -> Vec<&PricedOption> {
        let mut v: Vec<&PricedOption> = self.options.iter().collect();
        v.sort_by(|a, b| {
            let ea = a.per_hour_cny / (vram_gb_of(&a.spec.gpu_type).max(1) as f64);
            let eb = b.per_hour_cny / (vram_gb_of(&b.spec.gpu_type).max(1) as f64);
            ea.partial_cmp(&eb).unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }
}

/// 为一批配置批量查价，返回按价格升序的榜单。
///
/// ## 为什么必须真实查价而不是估算
///
/// 同一卡型在不同规格/可用区的价格不同，不同卡型之间更没有简单可比性
/// （A100 与 4090 的单价可能差 5 倍以上）。只有逐个调
/// `GetCompShareInstancePrice` 才能给出可信的「最便宜」。
///
/// ## 失败处理
///
/// 单个配置查价失败不中断整体（记录到 `failed`）。若**全部**失败，
/// 仍返回空榜单而不是报错——由调用方决定是否把失败原因呈现给用户。
pub fn price_quotes<T: Transport>(
    client: &CompshareClient<T>,
    specs: &[QuotableSpec],
    charge_type: &str,
    disk_type: &str,
    disk_size_gb: u32,
) -> PriceBoard {
    use crate::client::PriceRequest;

    let mut board = PriceBoard::default();
    let transport = client.transport();

    for spec in specs {
        // 价格接口同样需要 Region/Zone 定位，逐配置派生作用域客户端
        let scoped = match client.with_scope(transport, &spec.region, &spec.zone) {
            Ok(c) => c,
            Err(e) => {
                board.failed.push((spec.clone(), e.to_string()));
                continue;
            }
        };
        let req = PriceRequest {
            gpu_type: spec.gpu_type.clone(),
            gpu: spec.gpu,
            cpu: spec.cpu,
            memory_mb: spec.memory_mb(),
            charge_type: Some(charge_type.to_string()),
            disk_type: disk_type.to_string(),
            disk_size_gb,
        };
        match scoped.price(&req) {
            Ok(resp) => {
                // 优先取与请求计费模式一致的那条明细
                let detail = resp
                    .price_details
                    .iter()
                    .find(|d| d.charge_type.eq_ignore_ascii_case(charge_type))
                    .or_else(|| resp.price_details.first());
                let Some(d) = detail else {
                    board
                        .failed
                        .push((spec.clone(), "响应中没有价格明细".to_string()));
                    continue;
                };
                let raw = d.total();
                match crate::mapping::hourly_price(&d.charge_type, raw) {
                    Some(per_hour) if per_hour > 0.0 => {
                        board.options.push(PricedOption {
                            spec: spec.clone(),
                            per_hour_cny: per_hour,
                            raw_amount_cny: raw,
                            charge_type: d.charge_type.clone(),
                            breakdown: format!(
                                "实例 {:?} + 数据盘 {:?} + 系统盘 {:?} + 镜像 {:?}",
                                d.instance, d.disks, d.system_disks, d.image
                            ),
                        });
                    }
                    _ => board.failed.push((
                        spec.clone(),
                        format!("价格无效（模式 {}，金额 {raw}）", d.charge_type),
                    )),
                }
            }
            Err(e) => board.failed.push((spec.clone(), e.to_string())),
        }
    }

    // 按小时价格升序；同价按规格规模升序，保证结果可复现
    board.options.sort_by(|a, b| {
        a.per_hour_cny
            .partial_cmp(&b.per_hour_cny)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then((a.spec.gpu, a.spec.cpu, a.spec.mem_gb).cmp(&(
                b.spec.gpu,
                b.spec.cpu,
                b.spec.mem_gb,
            )))
            .then(a.spec.gpu_type.cmp(&b.spec.gpu_type))
            .then(a.spec.zone.cmp(&b.spec.zone))
    });
    board
}

/// 「找最便宜并启动」的结果。
#[derive(Debug, Clone)]
pub struct Launched {
    /// 最终启动成功的实例（含 IP / SSH 端口）
    pub instance: mangosteen_core::cloud::Instance,
    /// 实际开出来的价格（元/小时）
    pub per_hour_cny: f64,
    /// 实际使用的配置
    pub spec: QuotableSpec,
    /// 尝试过的候选（按价格从低到高），便于解释「为什么是这台」
    pub tried: Vec<String>,
    /// 因「下单前复核库存发现已售罄」而跳过的候选
    pub sold_out: Vec<String>,
}

/// 单个候选在下单前的库存复核结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecheckResult {
    /// 仍有货，可以下单
    Available,
    /// 已售罄，跳过
    SoldOut,
    /// 无法判断（接口失败），仍然尝试（宁可多试一次也不要漏掉）
    Unknown,
}

/// 下单前复核某个候选是否仍有库存。
///
/// ## 为什么必须复核
///
/// 实测发现：库存快照与下单之间有明显延迟。同一可用区在相隔约 20 秒的
/// 两次扫描中，5090 从「有货」变成「售罄」。若不复核，按价格榜单顺序
/// 直接下单，很可能第一台就撞上已售罄，白跑一次创建往返。
///
/// 复核代价只有 1 次接口调用，远小于一次失败创建的成本。
///
/// 接口失败时返回 [`PrecheckResult::Unknown`] 而不是 `SoldOut`：
/// 查不到不等于没有，贸然跳过可能错过唯一可用的机器。
pub fn precheck_stock<T: Transport>(
    client: &CompshareClient<T>,
    spec: &QuotableSpec,
    opts: &ScanOptions,
    image_id: &str,
) -> PrecheckResult {
    let transport = client.transport();
    let Ok(scoped) = client.with_scope(transport, &spec.region, &spec.zone) else {
        return PrecheckResult::Unknown;
    };
    let req = CapacityRequest {
        gpu_type: spec.gpu_type.clone(),
        machine_type: opts.machine_type.clone(),
        cpu_platform: opts.cpu_platform.clone(),
        image_id: image_id.to_string(),
        charge_type: opts.charge_type.clone(),
        disk_type: opts.disk_type.clone(),
        disk_size_gb: opts.disk_size_gb,
    };
    match scoped.check_capacity(&req) {
        Ok(resp) => {
            if resp.is_enough(spec.gpu, spec.cpu, spec.mem_gb) {
                PrecheckResult::Available
            } else {
                PrecheckResult::SoldOut
            }
        }
        Err(_) => PrecheckResult::Unknown,
    }
}

/// 在给定范围内找最便宜的可用 GPU 并**启动**它。
///
/// ## 流程
///
/// ```text
/// 1. 扫描库存（按 scope：单可用区 / 全区）
/// 2. 对「有货」的配置逐个查真实价格，按元/小时升序排
/// 3. 从最便宜开始：下单前先复核库存 → 有货才创建 → 等 Running
/// 4. 某台失败就换下一台（最多 max_attempts 次）
/// ```
///
/// ## 为什么「从最便宜开始逐个试」而不是「选一个就下单」
///
/// 库存接口是**模拟调度**，只说明此刻能调度到，不锁定资源。
/// 从查到价格到真正下单之间，最便宜的机器可能已被别人抢走。
/// 所以必须准备好「失败了换下一台」，否则用户会遇到
/// 「查得到却开不出来」的挫败。
///
/// ## 费用安全
///
/// 每台创建后再等 Running；等待失败会由驱动内 `destroy` 回滚
/// （见 [`crate::driver::CompshareDriver::provision_ready`]）。
/// 本函数**不会**留下计费中的孤儿实例。
pub fn provision_cheapest<T: Transport>(
    client: &CompshareClient<T>,
    scope: &ScanScope,
    image_id: &str,
    scan_opts: &ScanOptions,
    max_attempts: usize,
) -> MnResult<Launched> {
    use crate::driver::CompshareDriver;
    use mangosteen_core::MnError;

    let matrix = scan(client, scope, image_id, scan_opts)?;
    let quotable = matrix.quotable_specs();
    if quotable.is_empty() {
        return Err(MnError::NoViableCandidate {
            reason: format!(
                "扫描范围内没有任何有货的 GPU 规格（已检查 {} 个组合，跳过 {} 项）。\
                 可尝试换计费模式（当前 {}）或放宽卡型过滤。",
                matrix.entries.len(),
                matrix.skipped.len(),
                scan_opts.charge_type
            ),
        });
    }

    let board = price_quotes(
        client,
        &quotable,
        &scan_opts.charge_type,
        &scan_opts.disk_type,
        scan_opts.disk_size_gb,
    );
    if board.options.is_empty() {
        let detail = board
            .failed
            .iter()
            .take(3)
            .map(|(s, e)| format!("{}：{e}", s.spec_id()))
            .collect::<Vec<_>>()
            .join("；");
        return Err(MnError::Provider {
            provider: crate::PROVIDER_NAME.to_string(),
            code: "price_unavailable".to_string(),
            message: format!("所有有货配置都无法取得价格。{detail}"),
        });
    }

    let mut tried = Vec::new();
    let mut sold_out = Vec::new();
    let mut last_err: Option<MnError> = None;
    // 已尝试（含因售罄跳过）的候选数上限。售罄跳过也计入，
    // 避免库存剧烈变动时无限循环。
    let mut attempts = 0usize;
    let budget = max_attempts.max(1) * 3;

    for opt in &board.options {
        if attempts >= budget {
            break;
        }
        attempts += 1;
        let spec = &opt.spec;

        // ---- 下单前复核库存（关键：库存快照会过期）----
        match precheck_stock(client, spec, scan_opts, image_id) {
            PrecheckResult::SoldOut => {
                sold_out.push(format!(
                    "{} @ {}/{}",
                    spec.spec_id(),
                    spec.region,
                    spec.zone
                ));
                continue;
            }
            PrecheckResult::Available | PrecheckResult::Unknown => {}
        }

        tried.push(format!(
            "{} @ {}/{} ¥{:.3}/h",
            spec.spec_id(),
            spec.region,
            spec.zone,
            opt.per_hour_cny
        ));

        // 为该 (可用区, 卡型) 绑定的驱动：不同可用区要用不同的 Region/Zone
        let transport = client.transport();
        let scoped = client.with_scope(transport, &spec.region, &spec.zone)?;
        let cfg = crate::config::CompshareConfig {
            region: spec.region.clone(),
            zone: spec.zone.clone(),
            image_id: image_id.to_string(),
            charge_type: scan_opts.charge_type.clone(),
            machine_type: scan_opts.machine_type.clone(),
            cpu_platform: scan_opts.cpu_platform.clone(),
            disk_type: scan_opts.disk_type.clone(),
            disk_size_gb: scan_opts.disk_size_gb,
            ..Default::default()
        };
        let driver = CompshareDriver::new(
            crate::PROVIDER_NAME,
            cfg,
            scoped.credential().clone(),
            transport,
        )?;

        match driver.provision_ready(&spec.spec_id(), "cheapest") {
            Ok(inst) => {
                return Ok(Launched {
                    instance: inst,
                    per_hour_cny: opt.per_hour_cny,
                    spec: spec.clone(),
                    tried,
                    sold_out,
                });
            }
            Err(e) => {
                // 单台失败继续试下一台；不中断
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| MnError::NoViableCandidate {
        reason: if sold_out.is_empty() {
            format!("尝试 {} 个候选均失败", tried.len())
        } else {
            format!(
                "{} 个候选在下单前复核时已售罄，其余 {} 个创建失败。\
                 库存变动频繁，建议重试。",
                sold_out.len(),
                tried.len()
            )
        },
    }))
}

/// 卡型偏好序（越小越优先）。仅用于「推荐」排序，不是硬性规则。
fn gpu_preference_rank(gpu_type: &str) -> u8 {
    match gpu_type.trim().to_ascii_uppercase().as_str() {
        "4090" => 0,
        "5090" => 1,
        "3090" => 2,
        "4090_48G" => 3,
        "3080TI" => 4,
        "2080TI" | "2080" => 5,
        "A100" | "A800" | "H20" => 6,
        _ => 7,
    }
}

/// 扫描配置。
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// 计费模式。
    ///
    /// ## 实测：计费模式会显著改变库存结果
    ///
    /// 同一时刻、同一镜像下扫描全平台：
    ///
    /// | ChargeType | 有货卡型 |
    /// |------------|----------|
    /// | `Postpay` / `Dynamic` / `Month` / `Day` | 5 种（5090 / A100 / A800 / H20 / V100S）|
    /// | `Spot` | **0 种**（抢占式库存池独立，当时全空）|
    ///
    /// 所以「查不到库存」时**先换计费模式重试**，不要急着判定平台无货。
    pub charge_type: String,
    /// 机型
    pub machine_type: String,
    /// CPU 平台
    pub cpu_platform: String,
    /// 系统盘类型
    pub disk_type: String,
    /// 系统盘大小（GB）
    pub disk_size_gb: u32,
    /// 卡型白名单（空表示不限制）
    pub gpu_allowlist: Vec<String>,
    /// 是否跳过容器可用区（`IsPod=true`）。
    ///
    /// 本项目部署依赖 SSH，容器区用不了，默认跳过。
    pub skip_container_zones: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            // 库存接口文档列出的枚举，Postpay 与项目的按量计费一致
            charge_type: "Postpay".to_string(),
            machine_type: "G".to_string(),
            cpu_platform: "Auto".to_string(),
            disk_type: "CLOUD_SSD".to_string(),
            disk_size_gb: 100,
            gpu_allowlist: Vec::new(),
            skip_container_zones: true,
        }
    }
}

impl ScanOptions {
    /// 该卡型是否在允许范围内。
    pub fn allows(&self, gpu_type: &str) -> bool {
        self.gpu_allowlist.is_empty()
            || self
                .gpu_allowlist
                .iter()
                .any(|g| g.eq_ignore_ascii_case(gpu_type.trim()))
    }
}

/// 扫描全部可用区的 GPU 库存。
///
/// 调用量 = 1（可用区）+ N（各可用区机型表）+ Σ(各可用区卡型数)。
/// 典型值：5 + 5 + 19 ≈ 29 次，属于可接受的量级。
///
/// `image_id` 必须已通过 `DescribeCompShareImages` 确认可用——
/// 库存接口会用它模拟调度，镜像非法会让所有组合都返回「不足」。
pub fn scan_all<T: Transport>(
    client: &CompshareClient<T>,
    image_id: &str,
    opts: &ScanOptions,
) -> MnResult<CapacityMatrix> {
    let zones_resp = client.zones_filtered(Some(""))?;
    scan_zones(client, &zones_resp.zones, image_id, opts)
}

/// 扫描范围：单可用区 或 全区。
///
/// ## 为什么要显式区分
///
/// 两种模式在**调用量**与**结果含义**上完全不同：
///
/// | 模式 | 调用量 | 结果含义 |
/// |------|--------|----------|
/// | 单可用区 | 1 + 1 + 该区卡型数 | 「这个区能不能开」 |
/// | 全区 | 1 + 各区机型 + Σ各区卡型数 | 「整个平台哪里能开、哪里最便宜」 |
///
/// 用户常常已经明确要用某个区（如已在控制台看过），此时全区扫描纯属浪费
/// 十几次请求；反过来，想找最便宜时又必须扫全区。因此由调用方显式声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanScope {
    /// 只扫一个可用区
    Zone {
        /// 地域，如 `cn-wlcb`
        region: String,
        /// 可用区，如 `cn-wlcb-01`
        zone: String,
    },
    /// 扫全部可用区（自动跳过容器区与不支持系统镜像的区）
    AllZones,
}

impl ScanScope {
    /// 构造单可用区范围。
    pub fn zone(region: impl Into<String>, zone: impl Into<String>) -> Self {
        Self::Zone {
            region: region.into(),
            zone: zone.into(),
        }
    }

    /// 是否全区。
    pub fn is_all(&self) -> bool {
        matches!(self, ScanScope::AllZones)
    }
}

/// 按范围扫描库存（统一入口）。
///
/// 这是对外的推荐入口：一次调用同时覆盖「单可用区」与「全区」两种诉求，
/// 调用方不必自己决定用哪个底层函数。
pub fn scan<T: Transport>(
    client: &CompshareClient<T>,
    scope: &ScanScope,
    image_id: &str,
    opts: &ScanOptions,
) -> MnResult<CapacityMatrix> {
    match scope {
        ScanScope::AllZones => scan_all(client, image_id, opts),
        ScanScope::Zone { region, zone } => {
            // 只取目标可用区，避免把别区的机型也算进来
            let resp = client.zones_filtered(Some(region))?;
            let hint: Vec<String> = resp
                .zones
                .iter()
                .filter(|z| z.in_region(region))
                .map(|z| format!("{}（{}）", z.zone, z.describe))
                .collect();
            let selected: Vec<ZoneInfo> =
                resp.zones.into_iter().filter(|z| z.zone == *zone).collect();
            if selected.is_empty() {
                return Err(mangosteen_core::MnError::bad_request(format!(
                    "地域 {region} 下不存在可用区 `{zone}`。该地域可用：{}",
                    if hint.is_empty() {
                        "（该地域无可用区）".to_string()
                    } else {
                        hint.join("、")
                    }
                )));
            }
            scan_zones(client, &selected, image_id, opts)
        }
    }
}

/// 在给定可用区列表上扫描（供已有可用区数据时复用，避免重复查询）。
pub fn scan_zones<T: Transport>(
    client: &CompshareClient<T>,
    zones: &[ZoneInfo],
    image_id: &str,
    opts: &ScanOptions,
) -> MnResult<CapacityMatrix> {
    let mut matrix = CapacityMatrix::default();
    let transport = client.transport();

    for zone in zones {
        // 容器可用区跳过：无独立虚机、不能 SSH，本项目用不了
        if opts.skip_container_zones && !zone.is_vm() {
            matrix.skipped.push(format!(
                "{}/{}（{}）为容器可用区，跳过",
                zone.region, zone.zone, zone.describe
            ));
            continue;
        }
        // 该可用区不支持系统镜像 → 库存查询必然全为「不足」，直接跳过
        if !zone.supports_image_type("System") {
            matrix.skipped.push(format!(
                "{}/{}（{}）不支持系统镜像，跳过",
                zone.region, zone.zone, zone.describe
            ));
            continue;
        }

        // 关键：库存接口的 Region/Zone 是**必填**参数。
        // 必须用绑定到该可用区的客户端发起查询，不能复用未绑定区域的客户端，
        // 否则请求体里不会带 Region/Zone，厂商会直接判参数非法。
        let scoped = client.with_scope(transport, &zone.region, &zone.zone)?;

        // 查该可用区的可用机型，避免对不存在的卡型盲扫
        let types = match scoped.available_types() {
            Ok(r) => r.types,
            Err(e) => {
                matrix.skipped.push(format!(
                    "{}/{}（{}）机型查询失败：{e}",
                    zone.region, zone.zone, zone.describe
                ));
                continue;
            }
        };

        for item in &types {
            if !opts.allows(&item.name) {
                continue;
            }
            let req = CapacityRequest {
                gpu_type: item.name.clone(),
                machine_type: opts.machine_type.clone(),
                cpu_platform: opts.cpu_platform.clone(),
                image_id: image_id.to_string(),
                charge_type: opts.charge_type.clone(),
                disk_type: opts.disk_type.clone(),
                disk_size_gb: opts.disk_size_gb,
            };
            let entry = match scoped.check_capacity(&req) {
                Ok(resp) => ZoneGpuCapacity {
                    region: zone.region.clone(),
                    zone: zone.zone.clone(),
                    zone_name: zone.describe.clone(),
                    gpu_type: item.name.clone(),
                    specs: resp.specs,
                    error: None,
                },
                Err(e) => ZoneGpuCapacity {
                    region: zone.region.clone(),
                    zone: zone.zone.clone(),
                    zone_name: zone.describe.clone(),
                    gpu_type: item.name.clone(),
                    specs: Vec::new(),
                    // 单点失败不中断整个扫描
                    error: Some(e.to_string()),
                },
            };
            matrix.entries.push(entry);
        }
    }

    Ok(matrix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CapacityResponse;
    use crate::transport::HttpResponse;
    use crate::Credential;
    use std::sync::Mutex;

    fn cred() -> Credential {
        Credential::new("pk", "sk").unwrap()
    }

    // ---------------------------------------------------------- 纯逻辑

    fn spec(gpu: u32, cpu: u32, mem: u64, ok: bool) -> CapacitySpec {
        CapacitySpec {
            gpu,
            cpu,
            mem_gb: mem,
            resource_enough: ok,
        }
    }

    fn entry(
        gpu_type: &str,
        region: &str,
        zone: &str,
        specs: Vec<CapacitySpec>,
    ) -> ZoneGpuCapacity {
        ZoneGpuCapacity {
            region: region.into(),
            zone: zone.into(),
            zone_name: zone.into(),
            gpu_type: gpu_type.into(),
            specs,
            error: None,
        }
    }

    #[test]
    fn has_any_and_extremes() {
        let e = entry(
            "4090",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![
                spec(1, 16, 32, true),
                spec(1, 16, 64, true),
                spec(2, 32, 128, false),
            ],
        );
        assert!(e.has_any());
        assert_eq!(e.available_specs().len(), 2);
        assert_eq!(e.smallest_available().map(|s| s.mem_gb), Some(32));
        assert_eq!(e.largest_available().map(|s| s.mem_gb), Some(64));

        // 全部不足
        let out = entry("4090", "cn", "z", vec![spec(1, 16, 64, false)]);
        assert!(!out.has_any());
        assert!(out.smallest_available().is_none());
    }

    #[test]
    fn matrix_aggregates_by_type_and_region() {
        let mut m = CapacityMatrix::default();
        m.entries.push(entry(
            "4090",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![spec(1, 16, 64, true)],
        ));
        m.entries.push(entry(
            "4090",
            "cn-sh2",
            "cn-sh2-02",
            vec![spec(1, 16, 64, true)],
        ));
        m.entries.push(entry(
            "5090",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![spec(1, 16, 96, false)],
        ));
        m.entries.push(ZoneGpuCapacity {
            region: "cn-bj2".into(),
            zone: "cn-bj2-03".into(),
            zone_name: "华北一C".into(),
            gpu_type: "A100".into(),
            specs: vec![],
            error: Some("boom".into()),
        });

        assert_eq!(m.in_stock_count(), 2);
        assert_eq!(m.failed_count(), 1);

        let by_type = m.by_gpu_type();
        assert_eq!(by_type.get("4090").map(|v| v.len()), Some(2));
        // 5090 全部缺货，不应出现
        assert!(by_type.get("5090").is_none());
        assert!(by_type.get("A100").is_none(), "失败条目不算有货");

        let by_region = m.by_region();
        assert_eq!(by_region.get("cn-wlcb").map(|v| v.len()), Some(1));
        assert_eq!(by_region.get("cn-sh2").map(|v| v.len()), Some(1));

        assert!(m.is_anywhere_in_stock("4090"));
        assert!(!m.is_anywhere_in_stock("5090"));
        assert!(m.is_anywhere_in_stock(" 4090 "));
    }

    #[test]
    fn out_of_stock_types_excludes_failures_and_stocked() {
        let mut m = CapacityMatrix::default();
        // 4090 查询成功但全平台无货 → 属于「缺货」
        m.entries.push(entry(
            "4090",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![spec(1, 16, 64, false)],
        ));
        m.entries.push(entry(
            "4090",
            "cn-sh2",
            "cn-sh2-02",
            vec![spec(1, 16, 64, false)],
        ));
        // 5090 有货 → 不属于缺货
        m.entries.push(entry(
            "5090",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![spec(1, 16, 96, true)],
        ));
        // A100 查询失败 → 不属于缺货（是「不知道」）
        m.entries.push(ZoneGpuCapacity {
            region: "cn-x".into(),
            zone: "cn-x-01".into(),
            zone_name: "X".into(),
            gpu_type: "A100".into(),
            specs: vec![],
            error: Some("boom".into()),
        });

        let out = m.out_of_stock_types();
        assert_eq!(out, vec!["4090"]);
        // 失败不能被误报成缺货
        assert!(!out.contains(&"A100".to_string()));
        assert!(!out.contains(&"5090".to_string()));
    }

    #[test]
    fn out_of_stock_drops_type_stocked_elsewhere() {
        // 同一卡型：一个区缺货、另一个区有货 → 不算全平台缺货
        let mut m = CapacityMatrix::default();
        m.entries.push(entry(
            "4090",
            "cn-a",
            "cn-a-01",
            vec![spec(1, 16, 64, false)],
        ));
        m.entries.push(entry(
            "4090",
            "cn-b",
            "cn-b-01",
            vec![spec(1, 16, 64, true)],
        ));
        assert!(m.out_of_stock_types().is_empty());
        assert!(m.is_anywhere_in_stock("4090"));
    }

    #[test]
    fn recommend_prefers_consumer_cards() {
        let mut m = CapacityMatrix::default();
        // A100 有货但偏好靠后
        m.entries.push(entry(
            "A100",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![spec(1, 16, 64, true)],
        ));
        // 4090 有货且优先
        m.entries.push(entry(
            "4090",
            "cn-sh2",
            "cn-sh2-02",
            vec![spec(1, 16, 64, true)],
        ));
        let best = m.recommend().unwrap();
        assert_eq!(best.gpu_type, "4090");
        assert_eq!(best.region, "cn-sh2");
    }

    #[test]
    fn recommend_none_when_nothing_in_stock() {
        let mut m = CapacityMatrix::default();
        m.entries
            .push(entry("4090", "cn", "z", vec![spec(1, 16, 64, false)]));
        assert!(m.recommend().is_none());
    }

    #[test]
    fn gpu_allowlist_matching() {
        let mut o = ScanOptions::default();
        assert!(o.allows("4090"), "空白名单不限制");
        o.gpu_allowlist = vec!["4090".into(), "5090".into()];
        assert!(o.allows("4090"));
        assert!(o.allows(" 5090 "));
        assert!(!o.allows("A100"));
    }

    // ---------------------------------------------------------- 扫描行为

    /// 按 Action + GpuType 路由的假厂商。
    ///
    /// 可用区库存接口对同一可用区的不同卡型返回不同结果，因此路由键
    /// 必须是 (Action, GpuType)，而不是只有 Action。
    struct ZoneVendor {
        zones: String,
        /// (zone, gpu_type) -> Specs JSON 片段
        capacity: Mutex<BTreeMap<(String, String), String>>,
        /// zone -> AvailableInstanceTypes JSON 片段
        types: Mutex<BTreeMap<String, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl ZoneVendor {
        fn new(zones: &str) -> Self {
            Self {
                zones: zones.to_string(),
                capacity: Mutex::new(BTreeMap::new()),
                types: Mutex::new(BTreeMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_type(self, zone: &str, types_json: &str) -> Self {
            self.types
                .lock()
                .unwrap()
                .insert(zone.to_string(), types_json.to_string());
            self
        }

        fn with_capacity(self, zone: &str, gpu: &str, specs_json: &str) -> Self {
            self.capacity
                .lock()
                .unwrap()
                .insert((zone.to_string(), gpu.to_string()), specs_json.to_string());
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().map(|v| v.clone()).unwrap_or_default()
        }

        fn capacity_call_count(&self) -> usize {
            self.calls()
                .iter()
                .filter(|c| c.contains("Action=CheckCompShareResourceCapacity"))
                .count()
        }
    }

    impl Transport for ZoneVendor {
        fn execute(&self, req: &crate::transport::HttpRequest) -> MnResult<HttpResponse> {
            let body = req.body.clone().unwrap_or_default();
            self.calls.lock().unwrap().push(body.clone());
            let get = |k: &str| {
                body.split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                    .unwrap_or("")
                    .replace("%2F", "/")
                    .replace("%3A", ":")
            };
            let action = get("Action");
            let zone = get("Zone");
            let gpu = get("GpuType");

            let resp = match action.as_str() {
                "DescribeCompShareSupportZone" => self.zones.clone(),
                "DescribeAvailableCompShareInstanceTypes" => {
                    let t = self.types.lock().unwrap();
                    format!(
                        r#"{{"RetCode":0,"AvailableInstanceTypes":{}}}"#,
                        t.get(&zone).map(|s| s.as_str()).unwrap_or("[]")
                    )
                }
                "CheckCompShareResourceCapacity" => {
                    let c = self.capacity.lock().unwrap();
                    match c.get(&(zone.clone(), gpu.clone())) {
                        Some(specs) => {
                            format!(r#"{{"RetCode":0,"Specs":{specs}}}"#)
                        }
                        // 未注册的 (zone, gpu) 视为接口报错，用于测试容错
                        None => r#"{"RetCode":8433,"Message":"service error"}"#.to_string(),
                    }
                }
                _ => r#"{"RetCode":1,"Message":"unrouted"}"#.to_string(),
            };
            Ok(HttpResponse::new(200, resp))
        }
    }

    const ZONES_TWO_VM: &str = r#"{"RetCode":0,"ZoneInfo":[
        {"Region":"cn-wlcb","Zone":"cn-wlcb-01","Describe":"华北二A","IsPod":false,"UnsupportedImageTypes":[]},
        {"Region":"cn-sh2","Zone":"cn-sh2-02","Describe":"上海二B","IsPod":false,"UnsupportedImageTypes":[]},
        {"Region":"cn-bj2","Zone":"cn-bj2-03","Describe":"华北一C","IsPod":true,"UnsupportedImageTypes":["System"]}
    ]}"#;

    fn types_4090() -> &'static str {
        r#"[{"Name":"4090","Status":"Normal","GraphicsMemory":{"Value":24,"Rate":3}}]"#
    }

    fn client(vendor: ZoneVendor) -> CompshareClient<ZoneVendor> {
        CompshareClient::new(crate::DEFAULT_ENDPOINT, cred(), vendor, "", "").unwrap()
    }

    #[test]
    fn scan_skips_container_zones_and_probes_vm_zones() {
        let vendor = ZoneVendor::new(ZONES_TWO_VM)
            .with_type("cn-wlcb-01", types_4090())
            .with_type("cn-sh2-02", types_4090())
            .with_capacity(
                "cn-wlcb-01",
                "4090",
                r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]"#,
            )
            .with_capacity(
                "cn-sh2-02",
                "4090",
                r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":false}]"#,
            );
        let c = client(vendor);
        let m = scan_all(&c, "img-1", &ScanOptions::default()).unwrap();

        // 容器可用区被跳过并记录原因
        assert!(m
            .skipped
            .iter()
            .any(|s| s.contains("cn-bj2-03") && s.contains("容器")));
        // 两个虚机可用区各一条
        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.in_stock_count(), 1);
        let best = m.recommend().unwrap();
        assert_eq!(best.zone, "cn-wlcb-01");
    }

    #[test]
    fn scan_tolerates_single_point_failure() {
        // cn-sh2-02 的库存查询未注册 → 该点报错，但不影响 cn-wlcb-01
        let vendor = ZoneVendor::new(ZONES_TWO_VM)
            .with_type("cn-wlcb-01", types_4090())
            .with_type("cn-sh2-02", types_4090())
            .with_capacity(
                "cn-wlcb-01",
                "4090",
                r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]"#,
            );
        let c = client(vendor);
        let m = scan_all(&c, "img-1", &ScanOptions::default()).unwrap();

        assert_eq!(m.entries.len(), 2);
        assert_eq!(m.failed_count(), 1);
        assert!(m.is_anywhere_in_stock("4090"));
        assert_eq!(m.recommend().unwrap().zone, "cn-wlcb-01");
    }

    #[test]
    fn scan_queries_only_allowlisted_gpu_types() {
        let two_types = r#"[
            {"Name":"4090","Status":"Normal","GraphicsMemory":{"Value":24,"Rate":3}},
            {"Name":"A100","Status":"Normal","GraphicsMemory":{"Value":80,"Rate":5}}
        ]"#;
        let vendor = ZoneVendor::new(ZONES_TWO_VM)
            .with_type("cn-wlcb-01", two_types)
            .with_type("cn-sh2-02", two_types)
            .with_capacity(
                "cn-wlcb-01",
                "4090",
                r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]"#,
            );
        let c = client(vendor);
        let opts = ScanOptions {
            gpu_allowlist: vec!["4090".into()],
            ..Default::default()
        };
        let m = scan_all(&c, "img-1", &opts).unwrap();
        // A100 被白名单排除，不应发起库存查询
        assert!(m.entries.iter().all(|e| e.gpu_type == "4090"));
        assert_eq!(m.entries.len(), 2);
    }

    #[test]
    fn scan_sends_expected_params() {
        let vendor = ZoneVendor::new(ZONES_TWO_VM)
            .with_type("cn-wlcb-01", types_4090())
            .with_type("cn-sh2-02", types_4090())
            .with_capacity(
                "cn-wlcb-01",
                "4090",
                r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]"#,
            )
            .with_capacity("cn-sh2-02", "4090", r#"[]"#);
        let c = client(vendor);
        let opts = ScanOptions {
            charge_type: "Spot".into(),
            disk_size_gb: 60,
            ..Default::default()
        };
        scan_all(&c, "img-9", &opts).unwrap();

        let calls = c.transport().calls();
        let cap: Vec<&String> = calls
            .iter()
            .filter(|c| c.contains("Action=CheckCompShareResourceCapacity"))
            .collect();
        assert!(!cap.is_empty());
        let body = cap[0];
        // 必填参数齐全
        assert!(body.contains("GpuType=4090"), "{body}");
        assert!(body.contains("MachineType=G"), "{body}");
        assert!(body.contains("MinimalCpuPlatform=Auto"), "{body}");
        assert!(body.contains("CompShareImageId=img-9"), "{body}");
        assert!(body.contains("ChargeType=Spot"), "{body}");
        // 磁盘扁平化
        assert!(body.contains("Disks.0.IsBoot=true"), "{body}");
        assert!(body.contains("Disks.0.Size=60"), "{body}");
        // Region/Zone 是必填参数，必须真的出现在请求里
        assert!(body.contains("Region="), "库存查询必须带 Region：{body}");
        assert!(body.contains("Zone="), "库存查询必须带 Zone：{body}");
        assert!(
            body.contains("Zone=cn-wlcb-01") || body.contains("Zone=cn-sh2-02"),
            "{body}"
        );
        // 每个可用区的请求应带自己的 Zone，不能张冠李戴
        for call in cap {
            let zone_ok = (call.contains("Zone=cn-wlcb-01") && call.contains("Region=cn-wlcb"))
                || (call.contains("Zone=cn-sh2-02") && call.contains("Region=cn-sh2"));
            assert!(zone_ok, "Region 与 Zone 必须成对且匹配：{call}");
        }
    }

    #[test]
    fn scan_reports_zone_without_system_image_support() {
        // 虚机但列了不支持 System（异常配置）也应跳过
        let zones = r#"{"RetCode":0,"ZoneInfo":[
            {"Region":"cn-x","Zone":"cn-x-01","Describe":"X","IsPod":false,
             "UnsupportedImageTypes":["System"]}
        ]}"#;
        let c = client(ZoneVendor::new(zones));
        let m = scan_all(&c, "img-1", &ScanOptions::default()).unwrap();
        assert!(m.entries.is_empty());
        assert!(m.skipped.iter().any(|s| s.contains("不支持系统镜像")));
    }

    #[test]
    fn scan_with_no_zones_is_empty_not_error() {
        let c = client(ZoneVendor::new(r#"{"RetCode":0,"ZoneInfo":[]}"#));
        let m = scan_all(&c, "img-1", &ScanOptions::default()).unwrap();
        assert!(m.entries.is_empty());
        assert_eq!(m.in_stock_count(), 0);
        assert!(m.recommend().is_none());
        // 不应发出任何库存请求
        assert_eq!(c.transport().capacity_call_count(), 0);
    }

    #[test]
    fn capacity_response_parsing_is_shared() {
        // 保证扫描用的是与驱动同一套解析（Specs 的 Mem 是 GB）
        let r: CapacityResponse = serde_json::from_str(
            r#"{"RetCode":0,"Specs":[{"Gpu":1,"Cpu":16,"Mem":32,"ResourceEnough":true}]}"#,
        )
        .unwrap();
        assert_eq!(r.specs[0].mem_gb, 32);
        assert!(r.is_enough(1, 16, 32));
    }

    #[test]
    fn quotable_specs_only_include_in_stock_and_are_sorted() {
        let mut m = CapacityMatrix::default();
        m.entries.push(entry(
            "4090",
            "cn-wlcb",
            "cn-wlcb-01",
            vec![
                // 故意乱序，验证会按规模排序
                spec(2, 32, 128, true),
                spec(1, 16, 64, true),
                // 缺货的不应出现
                spec(1, 16, 32, false),
            ],
        ));
        // 失败条目不参与
        m.entries.push(ZoneGpuCapacity {
            region: "cn-x".into(),
            zone: "cn-x-01".into(),
            zone_name: "X".into(),
            gpu_type: "A100".into(),
            specs: vec![],
            error: Some("boom".into()),
        });

        let q = m.quotable_specs();
        assert_eq!(q.len(), 2);
        assert_eq!((q[0].gpu, q[0].cpu, q[0].mem_gb), (1, 16, 64));
        assert_eq!((q[1].gpu, q[1].cpu, q[1].mem_gb), (2, 32, 128));
        assert_eq!(q[0].spec_id(), "4090/1x16c/64g");
        // 价格接口要求 MB
        assert_eq!(q[0].memory_mb(), 65536);
    }

    #[test]
    fn price_board_ordering_and_helpers() {
        let spec_a = QuotableSpec {
            region: "cn".into(),
            zone: "z1".into(),
            zone_name: "Z1".into(),
            gpu_type: "A100".into(),
            gpu: 1,
            cpu: 16,
            mem_gb: 64,
        };
        let spec_b = QuotableSpec {
            gpu_type: "4090".into(),
            ..spec_a.clone()
        };
        let spec_c = QuotableSpec {
            gpu_type: "MonthOnly".into(),
            ..spec_a.clone()
        };
        let board = PriceBoard {
            options: vec![
                PricedOption {
                    spec: spec_b,
                    per_hour_cny: 1.5,
                    raw_amount_cny: 1.5,
                    charge_type: "Postpay".into(),
                    breakdown: String::new(),
                },
                PricedOption {
                    spec: spec_a,
                    per_hour_cny: 9.0,
                    raw_amount_cny: 9.0,
                    charge_type: "Postpay".into(),
                    breakdown: String::new(),
                },
                PricedOption {
                    spec: spec_c,
                    per_hour_cny: 0.5,
                    raw_amount_cny: 360.0,
                    charge_type: "Month".into(),
                    breakdown: String::new(),
                },
            ],
            failed: vec![],
        };

        // 裸价格最低是 MonthOnly（0.5/h），但它按月计费不可随时释放
        assert_eq!(board.cheapest().map(|o| o.per_hour_cny), Some(0.5));
        // 只看「可随时释放」时应跳过月付，选 4090（1.5/h）
        let rel = board.cheapest_releasable().unwrap();
        assert_eq!(rel.spec.gpu_type, "4090");
        assert!((rel.per_hour_cny - 1.5).abs() < 1e-9);

        // 每 GiB 显存成本（闭包按卡型给显存）：
        //   MonthOnly 0.5/24 = 0.0208
        //   4090      1.5/24 = 0.0625
        //   A100      9.0/80 = 0.1125
        // 排序应严格按该比值升序。
        let by_eff = board.sorted_by_vram_efficiency(|t| match t {
            "4090" => 24,
            "A100" => 80,
            _ => 24,
        });
        let order: Vec<&str> = by_eff.iter().map(|o| o.spec.gpu_type.as_str()).collect();
        assert_eq!(order, vec!["MonthOnly", "4090", "A100"]);
        // 裸价格最便宜的是 MonthOnly，但若只看「可随时释放」则是 4090
        assert_eq!(board.cheapest().unwrap().spec.gpu_type, "MonthOnly");
        assert_eq!(board.cheapest_releasable().unwrap().spec.gpu_type, "4090");
    }

    // ---------------------------------------------------------- 真实查价

    /// 按 (Action, Zone, GpuType) 路由的假厂商，用于查价测试。
    struct PriceVendor {
        zone: String,
        /// gpu_type -> PriceDetails JSON 片段
        prices: Mutex<BTreeMap<String, String>>,
        calls: Mutex<Vec<String>>,
    }

    impl PriceVendor {
        fn new(zone: &str) -> Self {
            Self {
                zone: zone.to_string(),
                prices: Mutex::new(BTreeMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_price(self, gpu: &str, details: &str) -> Self {
            self.prices
                .lock()
                .unwrap()
                .insert(gpu.to_string(), details.to_string());
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().map(|v| v.clone()).unwrap_or_default()
        }
    }

    impl Transport for PriceVendor {
        fn execute(&self, req: &crate::transport::HttpRequest) -> MnResult<HttpResponse> {
            let body = req.body.clone().unwrap_or_default();
            self.calls.lock().unwrap().push(body.clone());
            let get = |k: &str| {
                body.split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                    .unwrap_or("")
                    .to_string()
            };
            let action = get("Action");
            let gpu = get("GpuType");
            let zone = get("Zone");
            let resp = match action.as_str() {
                "GetCompShareInstancePrice" => {
                    // Zone 必须与客户端绑定的可用区一致，否则视为参数错
                    if zone != self.zone {
                        r#"{"RetCode":230,"Message":"Params [Zone] not available"}"#.to_string()
                    } else {
                        let p = self.prices.lock().unwrap();
                        match p.get(&gpu) {
                            Some(d) => format!(r#"{{"RetCode":0,"PriceDetails":{d}}}"#),
                            None => r#"{"RetCode":8433,"Message":"service error"}"#.to_string(),
                        }
                    }
                }
                _ => r#"{"RetCode":1,"Message":"unrouted"}"#.to_string(),
            };
            Ok(HttpResponse::new(200, resp))
        }
    }

    fn priced_spec(gpu: &str, mem: u64) -> QuotableSpec {
        QuotableSpec {
            region: "cn-wlcb".into(),
            zone: "cn-wlcb-01".into(),
            zone_name: "华北二A".into(),
            gpu_type: gpu.into(),
            gpu: 1,
            cpu: 16,
            mem_gb: mem,
        }
    }

    #[test]
    fn price_quotes_sorts_cheapest_first() {
        let vendor = PriceVendor::new("cn-wlcb-01")
            .with_price(
                "A100",
                r#"[{"ChargeType":"Postpay","Instance":9.0,"SystemDisks":0.0}]"#,
            )
            .with_price(
                "5090",
                r#"[{"ChargeType":"Postpay","Instance":1.5,"SystemDisks":0.0}]"#,
            );
        let c = CompshareClient::new(crate::DEFAULT_ENDPOINT, cred(), vendor, "", "").unwrap();
        let board = price_quotes(
            &c,
            &[priced_spec("A100", 64), priced_spec("5090", 96)],
            "Postpay",
            "CLOUD_SSD",
            100,
        );
        assert_eq!(board.options.len(), 2);
        assert_eq!(
            board.cheapest().map(|o| o.spec.gpu_type.as_str()),
            Some("5090")
        );
        assert!((board.cheapest().unwrap().per_hour_cny - 1.5).abs() < 1e-9);
    }

    #[test]
    fn price_quotes_converts_monthly() {
        // 月价 720 → 1.0 元/小时
        let vendor = PriceVendor::new("cn-wlcb-01")
            .with_price("4090", r#"[{"ChargeType":"Month","Instance":720.0}]"#);
        let c = CompshareClient::new(crate::DEFAULT_ENDPOINT, cred(), vendor, "", "").unwrap();
        let board = price_quotes(&c, &[priced_spec("4090", 64)], "Month", "CLOUD_SSD", 100);
        let opt = board.cheapest().unwrap();
        assert!(
            (opt.per_hour_cny - 1.0).abs() < 1e-9,
            "got {}",
            opt.per_hour_cny
        );
        assert_eq!(opt.raw_amount_cny, 720.0);
    }

    #[test]
    fn price_quotes_tolerates_partial_failure() {
        // A100 未注册价格 → 失败，但不影响 5090
        let vendor = PriceVendor::new("cn-wlcb-01")
            .with_price("5090", r#"[{"ChargeType":"Postpay","Instance":2.0}]"#);
        let c = CompshareClient::new(crate::DEFAULT_ENDPOINT, cred(), vendor, "", "").unwrap();
        let board = price_quotes(
            &c,
            &[priced_spec("A100", 64), priced_spec("5090", 96)],
            "Postpay",
            "CLOUD_SSD",
            100,
        );
        assert_eq!(board.options.len(), 1);
        assert_eq!(board.failed.len(), 1);
        assert_eq!(board.failed[0].0.gpu_type, "A100");
        assert_eq!(board.cheapest().unwrap().spec.gpu_type, "5090");
    }

    #[test]
    fn price_quotes_sends_region_zone_and_memory_in_mb() {
        let vendor = PriceVendor::new("cn-wlcb-01")
            .with_price("5090", r#"[{"ChargeType":"Postpay","Instance":2.0}]"#);
        let c = CompshareClient::new(crate::DEFAULT_ENDPOINT, cred(), vendor, "", "").unwrap();
        price_quotes(&c, &[priced_spec("5090", 96)], "Postpay", "CLOUD_SSD", 100);
        let calls = c.transport().calls();
        let body = &calls[0];
        assert!(body.contains("Action=GetCompShareInstancePrice"), "{body}");
        // Region/Zone 是定位参数，必须带
        assert!(body.contains("Region=cn-wlcb"), "{body}");
        assert!(body.contains("Zone=cn-wlcb-01"), "{body}");
        // Memory 必须是 MB
        assert!(body.contains("Memory=98304"), "96GB 应为 98304MB：{body}");
        assert!(body.contains("Gpu=1"), "{body}");
        assert!(body.contains("Cpu=16"), "{body}");
        assert!(body.contains("ChargeType=Postpay"), "{body}");
    }

    #[test]
    fn price_quotes_all_failed_returns_empty_board() {
        let vendor = PriceVendor::new("cn-wlcb-01");
        let c = CompshareClient::new(crate::DEFAULT_ENDPOINT, cred(), vendor, "", "").unwrap();
        let board = price_quotes(&c, &[priced_spec("4090", 64)], "Postpay", "CLOUD_SSD", 100);
        assert!(board.options.is_empty());
        assert_eq!(board.failed.len(), 1);
        assert!(board.cheapest().is_none());
    }

    // ---------------------------------------------------------- 下单前复核

    /// 按 (Action, Zone, GpuType) 返回固定库存结果的假厂商。
    struct StockVendor {
        zone: String,
        /// `Specs` 数组 JSON
        specs: String,
        /// 是否让接口报错
        broken: bool,
    }

    impl Transport for StockVendor {
        fn execute(&self, req: &crate::transport::HttpRequest) -> MnResult<HttpResponse> {
            let body = req.body.clone().unwrap_or_default();
            if self.broken {
                return Ok(HttpResponse::new(
                    200,
                    r#"{"RetCode":8433,"Message":"service error"}"#,
                ));
            }
            let get = |k: &str| {
                body.split('&')
                    .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
                    .unwrap_or("")
                    .to_string()
            };
            if get("Zone") != self.zone {
                return Ok(HttpResponse::new(
                    200,
                    r#"{"RetCode":230,"Message":"Params [Zone] not available"}"#,
                ));
            }
            Ok(HttpResponse::new(
                200,
                format!(r#"{{"RetCode":0,"Specs":{}}}"#, self.specs),
            ))
        }
    }

    fn stock_client(specs: &str, broken: bool) -> CompshareClient<StockVendor> {
        CompshareClient::new(
            crate::DEFAULT_ENDPOINT,
            cred(),
            StockVendor {
                zone: "cn-wlcb-01".into(),
                specs: specs.to_string(),
                broken,
            },
            "",
            "",
        )
        .unwrap()
    }

    #[test]
    fn precheck_detects_available() {
        let c = stock_client(
            r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]"#,
            false,
        );
        assert_eq!(
            precheck_stock(&c, &priced_spec("5090", 64), &ScanOptions::default(), "img"),
            PrecheckResult::Available
        );
    }

    #[test]
    fn precheck_detects_sold_out() {
        // 快照说有货，复核时已售罄 —— 这正是需要复核的原因
        let c = stock_client(
            r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":false}]"#,
            false,
        );
        assert_eq!(
            precheck_stock(&c, &priced_spec("5090", 64), &ScanOptions::default(), "img"),
            PrecheckResult::SoldOut
        );
    }

    #[test]
    fn precheck_sold_out_when_combo_missing() {
        // 未出现在结果里的组合视为售罄（保守）
        let c = stock_client(
            r#"[{"Gpu":2,"Cpu":32,"Mem":128,"ResourceEnough":true}]"#,
            false,
        );
        assert_eq!(
            precheck_stock(&c, &priced_spec("5090", 64), &ScanOptions::default(), "img"),
            PrecheckResult::SoldOut
        );
    }

    #[test]
    fn precheck_unknown_on_interface_failure() {
        // 查不到 ≠ 没有。接口失败时应返回 Unknown，让调用方仍去尝试，
        // 否则可能错过唯一可用的机器。
        let c = stock_client("[]", true);
        assert_eq!(
            precheck_stock(&c, &priced_spec("5090", 64), &ScanOptions::default(), "img"),
            PrecheckResult::Unknown
        );
    }

    #[test]
    fn precheck_unknown_on_zone_mismatch() {
        // 客户端 zone 与 spec zone 不一致 → 厂商报参数错 → Unknown
        let c = stock_client(
            r#"[{"Gpu":1,"Cpu":16,"Mem":64,"ResourceEnough":true}]"#,
            false,
        );
        let mut spec = priced_spec("5090", 64);
        spec.zone = "cn-sh2-02".into();
        assert_eq!(
            precheck_stock(&c, &spec, &ScanOptions::default(), "img"),
            PrecheckResult::Unknown
        );
    }
}
