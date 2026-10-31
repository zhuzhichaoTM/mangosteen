//! 云端算力编排器：多云实例的统一生命周期管理。
//!
//! 归属：**算力执行层 · 云端算力链路**。
//!
//! ## 职责
//!
//! 把「在云端跑起一个 llama.cpp 推理服务」这件事，从「选节点、建实例、等启动、
//! 建隧道、等引擎就绪」这一串步骤收敛成一个方法调用：
//!
//! ```text
//! provision(约束) →
//!   1. 遍历各驱动 list_available_specs   收集候选
//!   2. 探测各服务商延迟
//!   3. NodeSelector 打分排序 → 最优候选
//!   4. create → 轮询至 Created → start → 轮询至 Running
//!   5. 建 SSH 隧道到实例内的引擎端口
//!   6. 返回 (Instance, Tunnel, local_url)
//! ```
//!
//! ## 失败处理原则
//!
//! 任何一步失败都要**回滚已创建的云资源**（销毁实例、关隧道），
//! 否则用户会被持续计费。这是本模块最重要的不变量：
//! `provision` 返回 `Err` 时，云端不应残留任何由本次调用创建的资源。
//! 若回滚本身也失败，必须把「残留实例 ID」写进错误消息，让用户能手工清理。

use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use mangosteen_core::cloud::{AvailableSpec, DriverRegistry, Instance, InstanceState};
use mangosteen_core::net::TunnelSpec;
use mangosteen_core::plan::{CloudPlan, EngineLaunchConfig};
use mangosteen_core::policy::{NodeSelector, ScoredCandidate, SelectionConstraints};
use mangosteen_core::{MnError, MnResult};

use super::net::TunnelManager;

/// 候选快照：`(候选规格, 各服务商探测到的延迟毫秒)`。
///
/// 单独成别名而非内联元组：`collect_candidates` 与 `rank` 之间传递的是
/// **同一次采集**的结果，两处写法分叉过一次（一边 `Vec` 一边 `HashMap`
/// 顺序颠倒），别名把形状钉死。
pub type CandidateSnapshot = (
    Vec<(String, AvailableSpec)>,
    std::collections::HashMap<String, u32>,
);

/// `pick` 的三元返回：`(最优, 全部排序结果, 原始候选)`。
pub type PickResult = (
    ScoredCandidate,
    Vec<ScoredCandidate>,
    Vec<(String, AvailableSpec)>,
);

/// 编排结果。
#[derive(Debug)]
pub struct Provisioned {
    /// 已运行的云端实例
    pub instance: Instance,
    /// 是否已建立 SSH 隧道（隧道本体由 [`TunnelManager`] 持有，
    /// 通过 [`CloudOrchestrator::tunnels`] 访问与关闭）
    pub tunnel_established: bool,
    /// 上层可直接使用的 OpenAI 兼容 base URL
    pub base_url: String,
    /// 实际使用的本地映射端口（`None` 表示服务商直接提供了公网入口）
    pub local_port: Option<u16>,
    /// 远端连接信息（设备配置阶段拿到，启动阶段复用）。
    pub ssh: mangosteen_device::remote::SshTarget,
    /// 远端工作目录
    pub workdir: String,
}

impl Provisioned {
    /// 实例 ID。
    pub fn instance_id(&self) -> &str {
        &self.instance.id
    }

    /// 服务商。
    pub fn provider(&self) -> &str {
        &self.instance.provider
    }
}

/// 待尝试创建的候选（只保留创建所需的三项）。
///
/// 刻意不用 `ScoredCandidate` 也不用 `AvailableSpec`：前者带着四维分数
/// （创建时毫无用处），后者带着一整套规格详情（创建只需要 `spec_id`）。
/// 只保留「谁家、哪个规格、多少钱」能让「按价格顺序重试」的意图一眼看清。
#[derive(Debug, Clone)]
struct CreateCandidate {
    provider: String,
    spec_id: String,
    price: f64,
}

impl CreateCandidate {
    /// 日志标签：`provider · spec_id（¥x.xxx/h）`。
    ///
    /// 带上价格是刻意的——排障时最常问的就是「它为什么先试这个贵的」，
    /// 价格直接印在日志里，一眼就能看出顺序对不对。
    fn label(&self) -> String {
        format!(
            "{} · {}（¥{:.3}/h）",
            self.provider, self.spec_id, self.price
        )
    }
}

/// 编排配置。
#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    /// 实例创建超时
    pub provision_timeout: Duration,
    /// 实例启动超时（从 start 到 Running）
    pub start_timeout: Duration,
    /// 隧道建立超时
    pub tunnel_timeout: Duration,
    /// 轮询间隔
    pub poll_interval: Duration,
    /// 实例 root 登录密码（`None` 表示靠密钥/agent 认证）。
    ///
    /// 优云智算这类厂商不支持创建时注入 SSH key，只能由我们主动设密码。
    /// 设完之后隧道用它认证，ssh 通过 `SSH_ASKPASS` 取——见
    /// [`TunnelSpec::password`]。
    pub ssh_password: Option<String>,
    /// 云端工作目录（模型、二进制、日志都放这儿）
    pub remote_workdir: String,
    /// llama.cpp 预编译包下载地址（远端没有 llama-server 时用）
    pub llama_server_url: String,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            provision_timeout: Duration::from_secs(600),
            start_timeout: Duration::from_secs(300),
            tunnel_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_secs(2),
            ssh_password: None,
            remote_workdir: "/root/mangosteen".to_string(),
            llama_server_url: String::new(),
        }
    }
}

/// 云端编排器。
pub struct CloudOrchestrator {
    registry: DriverRegistry,
    selector: NodeSelector,
    tunnels: TunnelManager,
    config: OrchestratorConfig,
}

impl CloudOrchestrator {
    /// 构造。
    pub fn new(
        registry: DriverRegistry,
        selector: NodeSelector,
        tunnels: TunnelManager,
        config: OrchestratorConfig,
    ) -> Self {
        Self {
            registry,
            selector,
            tunnels,
            config,
        }
    }

    /// 驱动注册表引用。
    pub fn registry(&self) -> &DriverRegistry {
        &self.registry
    }

    /// 隧道管理器引用。
    pub fn tunnels(&self) -> &TunnelManager {
        &self.tunnels
    }

    /// 收集全部候选规格（跨服务商），并探测延迟。
    ///
    /// 单个服务商失败不影响其它服务商的结果（容错要求）。
    /// 全部失败时返回错误消息汇总，便于排障。
    pub fn collect_candidates(&self) -> MnResult<CandidateSnapshot> {
        let mut candidates = Vec::new();
        let mut latencies = std::collections::HashMap::new();
        let mut failures = Vec::new();

        for (name, driver) in self.registry.iter() {
            // 延迟探测（可选能力）
            if let Some(ms) = driver.probe_latency_ms() {
                latencies.insert(name.to_string(), ms);
            }
            match driver.list_available_specs() {
                Ok(specs) => {
                    for s in specs {
                        candidates.push((name.to_string(), s));
                    }
                }
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }

        if candidates.is_empty() {
            return Err(if failures.is_empty() {
                MnError::NoViableCandidate {
                    reason: "所有已启用的服务商都没有返回可用规格（检查密钥权限或账户余额）"
                        .to_string(),
                }
            } else {
                MnError::NoViableCandidate {
                    reason: format!("所有服务商查询均失败：{}", failures.join("；")),
                }
            });
        }
        Ok((candidates, latencies))
    }

    /// 列出所有可用规格（跨服务商），不执行排序或创建。
    pub fn collect_all_available_specs(&self) -> MnResult<Vec<(String, AvailableSpec)>> {
        self.collect_candidates().map(|(candidates, _)| candidates)
    }

    /// 按打分排序（不创建资源）。供 CLI 的 `cloud pick` 使用。
    pub fn rank(
        &self,
        candidates: &[(String, AvailableSpec)],
        latencies: &std::collections::HashMap<String, u32>,
        constraints: &SelectionConstraints,
    ) -> Vec<ScoredCandidate> {
        self.selector.rank(candidates, latencies, constraints)
    }

    /// 选出最优候选（不创建资源）。
    ///
    /// 供 `mangosteen cloud pick --explain` 使用，便于用户在花钱前看清决策依据。
    pub fn pick(&self, constraints: &SelectionConstraints) -> MnResult<PickResult> {
        let (candidates, latencies) = self.collect_candidates()?;
        let ranked = self.selector.rank(&candidates, &latencies, constraints);
        let best = ranked
            .first()
            .cloned()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: format!(
                    "{} 个候选规格均不满足约束（最低显存 {:.1} GiB）",
                    candidates.len(),
                    constraints.min_vram_bytes as f64 / (1u64 << 30) as f64
                ),
            })?;
        Ok((best, ranked, candidates))
    }

    /// 只看价格：满足约束的候选按**价格升序**返回（不创建资源）。
    ///
    /// ## 与 [`Self::pick`] 的区别
    ///
    /// `pick` 走 `NodeSelector`，受配置里的 `scheduler.strategy` 与四维权重
    /// 影响——默认 `balanced` 时「综合最优」未必最便宜（延迟/库存分能翻盘，
    /// 实测出现过 ¥2.15/h 的 4090 排在 ¥1.34/h 的 V100S 前面）。
    /// 用户明确说「要最便宜」时不能拿它凑合，所以单开一条纯价格序。
    ///
    /// 约束（显存/内存/磁盘/价格上限/库存）仍然生效。
    pub fn cheapest(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<(String, AvailableSpec)>> {
        let (candidates, _) = self.collect_candidates()?;
        let ordered = constraints.cheapest_first(&candidates);
        if ordered.is_empty() {
            return Err(MnError::NoViableCandidate {
                reason: format!(
                    "{} 个候选规格均不满足约束（最低显存 {:.1} GiB{}）",
                    candidates.len(),
                    constraints.min_vram_bytes as f64 / (1u64 << 30) as f64,
                    constraints
                        .max_price_per_hour
                        .map(|p| format!("，价格上限 ¥{p:.2}/h"))
                        .unwrap_or_default()
                ),
            });
        }
        Ok(ordered)
    }

    /// 创建一台「当前可用的最低价」实例，直到真的有一台跑起来。
    ///
    /// 返回 `(服务商名, 实例)`。
    ///
    /// ## 「可用」由创建结果定义，不由库存字段定义
    ///
    /// 优云的库存接口是模拟调度（对能开出来的机器也返回 false），AutoDL 的
    /// 库存只覆盖部分机型。所以这里**不拿 `stock` 当准入条件**，而是按价格
    /// 顺序逐个真开：谁先开出来，谁就是「当前可用的最低价」。这比「先过滤
    /// 库存再开」更准——后者会在一批明明能开的机器上直接报「无可用节点」。
    ///
    /// ## 不建立 SSH 隧道、不上传模型
    ///
    /// 这是「先拿到一台机器」的裸创建。要跑推理请走
    /// [`Self::provision`]（它会建隧道、传权重、起引擎）。
    pub fn create_cheapest(
        &self,
        constraints: &SelectionConstraints,
        name: Option<&str>,
    ) -> MnResult<(String, Instance)> {
        let ordered = self.cheapest(constraints)?;
        let cands: Vec<CreateCandidate> = ordered
            .iter()
            .map(|(p, s)| CreateCandidate {
                provider: p.clone(),
                spec_id: s.spec_id.clone(),
                price: s.price.per_hour_cny,
            })
            .collect();
        self.create_first_running(&cands, name, None)
    }

    /// 完整供给流程。
    ///
    /// ## 为什么要逐个候选重试
    ///
    /// 「选最便宜节点」不等于「只试最便宜那个」。厂商的库存查询是模拟
    /// 调度，与真实创建结果不一致（实测优云智算：查询说全部无货，
    /// 创建 P40 报 226604 无库存，而 V100S 能开出来）。如果只试排第一的
    /// 候选，用户会在「明明有机器」的情况下收到「无库存」而卡住。
    ///
    /// 因此按价格顺序逐个尝试，**只在「该规格无库存」这类可重试错误上
    /// 换下一个**；鉴权、参数、余额类错误立刻中止——继续试只会把同一个
    /// 错误重复 N 遍，白白拉长等待。
    pub fn provision(
        &mut self,
        constraints: &SelectionConstraints,
        plan: &CloudPlan,
        launch: &EngineLaunchConfig,
    ) -> MnResult<Provisioned> {
        self.provision_with_progress(constraints, plan, launch, None)
    }

    /// 与 [`Self::provision`] 相同，但支持上报**远端下载**进度。
    ///
    /// 回调参数：`(已下载字节, 期望总字节)`。轮询线程里调用，所以要求
    /// `Send + Sync`；只做内存写入，不做 IO。
    pub fn provision_with_progress(
        &mut self,
        constraints: &SelectionConstraints,
        plan: &CloudPlan,
        launch: &EngineLaunchConfig,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> MnResult<Provisioned> {
        let (_best, ranked, _all) = self.pick(constraints)?;
        let mut ordered: Vec<CreateCandidate> = ranked
            .iter()
            .map(|c| CreateCandidate {
                provider: c.provider.clone(),
                spec_id: c.spec_id.clone(),
                price: c.price_per_hour,
            })
            .collect();

        prefer_requested_spec(&mut ordered, plan.requested_spec_id.as_deref());

        let (provider, instance) = self.create_first_running(
            &ordered,
            Some(&format!("mangosteen-{}", plan.engine_port)),
            Some(plan.engine_port),
        )?;

        let driver = self.registry.get(&provider)?;
        finish_provision(
            driver,
            instance,
            plan,
            launch,
            &mut self.tunnels,
            &self.config,
            on_progress,
        )
    }

    /// 按给定顺序逐个尝试创建，直到有一台进入 `Running`。
    ///
    /// 返回 `(服务商名, 实例)`。全部失败时返回汇总错误，带上「试过哪些节点、
    /// 各自最后错误」——只回一个最后错误无法区分「全都没货」和「被限流」。
    ///
    /// ## 不变量
    ///
    /// 本函数返回 `Err` 时**不留下任何计费中的实例**：创建成功但启动失败的
    /// 那台会先被销毁（带重试），销毁成功后才换下一个候选；销毁失败则
    /// 立刻返回错误并带上残留实例 ID，绝不继续建下一台（会同时留两台计费）。
    fn create_first_running(
        &self,
        ordered: &[CreateCandidate],
        name: Option<&str>,
        engine_port: Option<u16>,
    ) -> MnResult<(String, Instance)> {
        let mut last_err: Option<MnError> = None;
        let mut tried: Vec<String> = Vec::new();

        for cand in ordered {
            let driver = self.registry.get(&cand.provider)?;
            let label = cand.label();
            tried.push(label.clone());

            let mut instance = match driver.create(&cand.spec_id, name) {
                Ok(i) => {
                    eprintln!("[cloud] ✓ 已创建 {label} → {}", i.id);
                    i
                }
                Err(e) => {
                    eprintln!("[cloud] ✗ {label}：{e}");
                    if is_retryable_create_error(&e) {
                        // 该规格没货 → 换下一个（这就是「最便宜优先」的落地方式）
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            };

            match self.wait_until_running(driver, &mut instance, engine_port) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("[cloud] ✗ {} 启动失败：{e}", instance.id);
                    // 回滚：绝不留下计费中的实例
                    let cleanup = destroy_with_retry(driver, &instance.id);
                    // 实例自己起不来（Failed / 启动超时）是**该节点**的问题，
                    // 换个节点可能就好了。只有在清理干净之后才换——清理没成功
                    // 就继续建下一台，会同时留两台机器计费。
                    if cleanup.is_ok() && is_retryable_instance_failure(&e) {
                        last_err = Some(e);
                        continue;
                    }
                    return Err(augment_with_cleanup(e, &instance, cleanup));
                }
            }

            return Ok((cand.provider.clone(), instance));
        }

        // 全部候选都没开出来
        Err(MnError::NoViableCandidate {
            reason: format!(
                "按价格顺序尝试了 {} 个候选，均无法创建实例（{}）。\
                 最后错误：{}",
                tried.len(),
                tried.join("、"),
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "未知".to_string())
            ),
        })
    }

    /// 轮询实例直到 Running。必要时调用 start。
    ///
    /// `engine_port` 只用于超时错误里的提示（`None` 表示这次创建没有绑定
    /// 推理端口，例如「只开一台机器」的裸创建）。
    fn wait_until_running(
        &self,
        driver: &dyn mangosteen_core::cloud::ProviderDriver,
        instance: &mut Instance,
        engine_port: Option<u16>,
    ) -> MnResult<()> {
        let deadline = Instant::now() + self.config.start_timeout;
        let mut started = false;

        loop {
            match instance.state {
                InstanceState::Running => return Ok(()),
                InstanceState::Failed => {
                    return Err(MnError::Provider {
                        provider: instance.provider.clone(),
                        code: "instance_failed".to_string(),
                        message: format!("实例 {} 进入 Failed 状态", instance.id),
                    })
                }
                InstanceState::Destroyed => {
                    return Err(MnError::Provider {
                        provider: instance.provider.clone(),
                        code: "instance_destroyed".to_string(),
                        message: format!("实例 {} 已被销毁", instance.id),
                    })
                }
                InstanceState::Created | InstanceState::Stopped if !started => {
                    // 创建后处于关机态 → 主动启动
                    *instance = driver.start(&instance.id)?;
                    started = true;
                }
                _ => {}
            }

            if Instant::now() >= deadline {
                let port_hint = engine_port
                    .map(|p| format!("目标端口 {p}"))
                    .unwrap_or_else(|| "本次创建未绑定推理端口".to_string());
                return Err(MnError::Provider {
                    provider: instance.provider.clone(),
                    code: "start_timeout".to_string(),
                    message: format!(
                        "实例 {} 在 {}s 内未进入 Running（当前 {}）。若为镜像首次启动，\
                         可能需要更长的初始化时间，可调大 scheduler.provision_timeout_secs。\
                         {port_hint}",
                        instance.id,
                        self.config.start_timeout.as_secs(),
                        instance.state,
                    ),
                });
            }
            std::thread::sleep(self.config.poll_interval);
            *instance = driver.describe(&instance.id)?;
        }
    }

    /// 销毁部署：关隧道 + 销毁实例。
    ///
    /// 返回销毁过程中遇到的错误列表（**不中断**后续清理步骤）。
    ///
    /// 为什么用「收集错误而非短路」：关隧道失败不应阻止销毁实例——
    /// 后者才是停止计费的关键动作。
    pub fn teardown(&mut self, provider: &str, instance_id: &str) -> Vec<String> {
        let mut errors = Vec::new();

        // 1. 关隧道（失败只记录）
        if let Err(e) = self.tunnels.close_all() {
            errors.push(format!("关闭隧道失败：{e}"));
        }

        // 2. 销毁实例（关键步骤）
        match self.registry.get(provider) {
            Ok(driver) => {
                // 已销毁时 destroy 可能报错，先查状态避免误报
                match driver.describe(instance_id) {
                    Ok(inst) if inst.state == InstanceState::Destroyed => {}
                    Ok(_) => {
                        if let Err(e) = driver.destroy(instance_id) {
                            errors.push(format!("销毁实例 {instance_id} 失败：{e}"));
                        }
                    }
                    Err(_) => {
                        // 查不到 → 尝试销毁（幂等）
                        if let Err(e) = driver.destroy(instance_id) {
                            errors.push(format!("销毁实例 {instance_id} 失败：{e}"));
                        }
                    }
                }
            }
            Err(e) => errors.push(format!("找不到服务商驱动 {provider}：{e}")),
        }

        errors
    }

    /// 构造某个实例的 SSH 目标（供「启动/停止服务」用）。
    ///
    /// 密码优先级与供给流程一致：厂商下发的密码 > 配置里的统一密码。
    pub fn service_target(
        &self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<mangosteen_device::remote::SshTarget> {
        let driver = self.registry.get(provider)?;
        let inst = driver.describe(instance_id)?;
        let host = inst
            .network
            .ssh_host
            .clone()
            .ok_or_else(|| MnError::Network {
                stage: "读取 SSH 信息".to_string(),
                reason: format!("实例 {instance_id} 未返回 SSH 地址"),
            })?;
        Ok(mangosteen_device::remote::SshTarget {
            host,
            port: inst.network.ssh_port.unwrap_or(22),
            user: "root".to_string(),
            password: inst
                .network
                .ssh_password
                .clone()
                .or_else(|| self.config.ssh_password.clone()),
            identity_file: None,
        })
    }

    /// **停止服务**：停掉远端 `llama-server`，保留实例。
    ///
    /// 与 [`Self::teardown`] 的区别见 `remote::stop_llama_server` 的说明——
    /// 这里**不销毁实例**，所以仍在计费。上层必须把这一点告诉用户。
    pub fn stop_service(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let target = self.service_target(provider, instance_id)?;
        mangosteen_device::remote::stop_llama_server(&target, &self.config.remote_workdir)
    }

    /// **启动服务**：在已有实例上重新拉起 `llama-server`。
    ///
    /// 权重与二进制都已在远端（供给时传过/装过），所以这里只需一条启动命令。
    /// 启动后等 `/health` 由调用方负责——本函数只保证进程活着。
    pub fn start_service(
        &self,
        provider: &str,
        instance_id: &str,
        _plan: &CloudPlan,
        launch: &EngineLaunchConfig,
    ) -> MnResult<()> {
        let target = self.service_target(provider, instance_id)?;
        let workdir = self.config.remote_workdir.trim_end_matches('/').to_string();
        let bin = format!("{workdir}/bin/llama-server");
        // 二进制丢了（用户清理过系统盘 / 换了实例）就不能假装能启动，
        // 直接说清楚要重新部署，而不是抛一个看不懂的 no such file。
        let probe = mangosteen_device::remote::ssh_exec(
            &target,
            &format!("if [ -x {bin} ]; then echo ok; else echo missing; fi"),
            60,
        )?;
        if !probe.contains("ok") {
            return Err(MnError::Network {
                stage: "启动云端推理服务".to_string(),
                reason: format!(
                    "远端找不到 {bin}（可能实例被重置或系统盘被清理）。请重新部署一次。"
                ),
            });
        }
        mangosteen_device::remote::start_llama_server(&target, &bin, &launch.args, &workdir)
    }

    /// 隧道管理器（可变借用，用于关闭全部隧道）。
    pub fn tunnels_mut(&mut self) -> &mut TunnelManager {
        &mut self.tunnels
    }

    /// 实例启动超时。
    pub fn start_timeout(&self) -> Duration {
        self.config.start_timeout
    }

    /// 状态轮询间隔。
    pub fn poll_interval(&self) -> Duration {
        self.config.poll_interval
    }

    /// 为**已存在的**实例重建 SSH 隧道，返回 `[OI]` 兼容 base URL。
    ///
    /// 「停止服务」会把隧道一起关掉（机器都关了，留着只会反复重连），
    /// 所以「启动服务」必须能把它重新建起来。这里复用与供给流程完全相同的
    /// 隧道参数构造，避免两处逻辑漂移。
    pub fn establish_tunnel(
        &mut self,
        provider: &str,
        instance_id: &str,
        plan: &CloudPlan,
    ) -> MnResult<String> {
        let driver = self.registry.get(provider)?;
        let instance = driver.describe(instance_id)?;
        let spec = self.tunnel_spec_for(&instance, plan)?;
        let t = self.tunnels.establish(&spec, self.config.tunnel_timeout)?;
        let local_port = t.local_port;
        Ok(format!("http://127.0.0.1:{local_port}/v1"))
    }

    /// 由实例信息与方案构造隧道参数（供给与「启动服务」共用）。
    fn tunnel_spec_for(&self, instance: &Instance, plan: &CloudPlan) -> MnResult<TunnelSpec> {
        let ssh_host = instance
            .network
            .ssh_host
            .clone()
            .ok_or_else(|| MnError::Network {
                stage: "读取 SSH 信息".to_string(),
                reason: format!("实例 {} 未返回 SSH 地址，无法建立隧道", instance.id),
            })?;
        Ok(TunnelSpec {
            ssh_host,
            ssh_port: instance.network.ssh_port.unwrap_or(22),
            ssh_user: "root".to_string(),
            identity_file: None,
            forwards: vec![mangosteen_core::net::PortForward::loopback(
                0,
                plan.engine_port,
            )],
            control_master: true,
            allow_unready_remote: false,
            password: instance
                .network
                .ssh_password
                .clone()
                .or_else(|| self.config.ssh_password.clone()),
        })
    }

    /// 关机实例（停止 GPU 计费，保留磁盘与数据）。
    ///
    /// 与 [`Self::teardown`] 的区别：关机只是把机器停了，实例还在、数据还在，
    /// 按量付费下不再收计算费用（磁盘可能仍计少量）。这是「停止」的正确语义——
    /// 用户想暂停一下，不是想删机器。
    pub fn power_off(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let driver = self.registry.get(provider)?;
        driver.stop(instance_id)?;
        Ok(())
    }

    /// 开机实例并等到 Running。
    pub fn power_on(&self, provider: &str, instance_id: &str, plan: &CloudPlan) -> MnResult<()> {
        let driver = self.registry.get(provider)?;
        let mut instance = driver.describe(instance_id)?;
        if instance.state == InstanceState::Running {
            return Ok(());
        }
        if instance.state == InstanceState::Stopped || instance.state == InstanceState::Created {
            instance = driver.start(instance_id)?;
        }
        self.wait_until_running(driver, &mut instance, Some(plan.engine_port))
    }

    /// 列出全部云端实例（跨服务商）。
    pub fn list_instances(&self) -> Vec<(String, MnResult<Vec<Instance>>)> {
        self.registry
            .iter()
            .map(|(name, d)| (name.to_string(), d.list()))
            .collect()
    }

    /// 查询指定实例的实时监控（跨服务商）。
    ///
    /// 返回值语义（与 [`ProviderDriver::instance_metrics`] 一致）：
    ///
    /// * `Ok(Some(m))` — 取到指标（`has_any()` 可能为 `false`，表示暂无数据）；
    /// * `Ok(None)` — 服务商未提供监控能力，或实例不在其返回列表里；
    /// * `Err(..)` — 调用失败（鉴权/网络/厂商错误码）。
    ///
    /// `provider` 为空时按名字遍历全部驱动找第一个能回答的——
    /// 台账里通常记了服务商名，但老记录可能缺失，遍历比直接报错更实用。
    pub fn instance_metrics(
        &self,
        provider: Option<&str>,
        instance_id: &str,
    ) -> MnResult<Option<mangosteen_core::cloud::InstanceMetrics>> {
        // 1. 指明了服务商：直接问它
        if let Some(name) = provider.filter(|n| !n.trim().is_empty()) {
            let d = self.registry.get(name)?;
            return d.instance_metrics(instance_id);
        }

        // 2. 未指明：逐个试，取第一个有结果的。
        //    单个驱动失败不影响其它驱动（容错要求）。
        let mut last_err = None;
        for (_, d) in self.registry.iter() {
            match d.instance_metrics(instance_id) {
                Ok(Some(m)) => return Ok(Some(m)),
                Ok(None) => {}
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }
}

/// 从实例信息构造可用的 SSH 目标（必要时先设密码并等 sshd 接受）。
///
/// ## 顺序为什么不能反
///
/// 先设密码再连 ssh：反过来会用一个实例还不知道的密码去认证，ssh 直接
/// `Permission denied`，而错误信息看起来像「网络不通」。厂商已经下发了
/// 密码就不用再设——AutoDL 直接给 `root_password`，再调一次「重置密码」
/// 反而会把密码改成一个我们并不知道的值。
fn vendor_password_target(
    instance: &Instance,
    config: &OrchestratorConfig,
) -> MnResult<mangosteen_device::remote::SshTarget> {
    let host = instance
        .network
        .ssh_host
        .clone()
        .ok_or_else(|| MnError::Network {
            stage: "读取 SSH 信息".to_string(),
            reason: format!(
                "服务商 {} 未返回实例 {} 的 SSH 地址",
                instance.provider, instance.id
            ),
        })?;
    let port = instance.network.ssh_port.unwrap_or(22);
    let password = config
        .ssh_password
        .clone()
        .or_else(|| instance.network.ssh_password.clone());
    if password.is_none() {
        return Err(MnError::Network {
            stage: "读取 SSH 信息".to_string(),
            reason: "节点已运行但未返回可用 SSH 密码".to_string(),
        });
    }
    Ok(mangosteen_device::remote::SshTarget {
        host,
        port,
        user: "root".to_string(),
        password,
        identity_file: None,
    })
}

/// 为用户自定义节点配置 SSH 免密登录。
///
/// 与云厂商节点不同：这里绝不调用密码重置接口，只使用用户保存的
/// 原始密码完成首次 `ssh-copy-id`，随后切换到本机密钥登录。这样既
/// 保留用户节点密码不变，又让节点配置、模型下载、隧道和启动阶段
/// 统一使用免密 SSH。
pub fn configure_existing_ssh_access(
    target: &mangosteen_device::remote::SshTarget,
) -> MnResult<mangosteen_device::remote::SshTarget> {
    if target.password.is_none() {
        return Err(MnError::Network {
            stage: "节点配置环境 / SSH 免密登录".to_string(),
            reason: "自定义节点未提供登录密码，无法安装 SSH 公钥".to_string(),
        });
    }

    let mut last_error = String::new();
    for attempt in 1..=3 {
        match mangosteen_device::remote::install_ssh_copy_id(target) {
            Ok(identity_file) => {
                let key_target = mangosteen_device::remote::SshTarget {
                    host: target.host.clone(),
                    port: target.port,
                    user: target.user.clone(),
                    password: None,
                    identity_file: Some(identity_file),
                };
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match mangosteen_device::remote::ssh_exec(&key_target, "true", 5) {
                        Ok(_) => return Ok(key_target),
                        Err(_e) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(250));
                        }
                        Err(e) => {
                            last_error = format!("第 {attempt} 次密钥确认失败：{e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                last_error = format!("第 {attempt} 次 ssh-copy-id 失败：{e}");
            }
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    Err(MnError::Network {
        stage: "节点配置环境 / SSH 免密登录".to_string(),
        reason: format!("连续 3 次未能确认自定义节点 SSH 公钥登录成功：{last_error}"),
    })
}

fn vendor_ssh_target(
    driver: &dyn mangosteen_core::cloud::ProviderDriver,
    instance: &Instance,
    config: &OrchestratorConfig,
) -> MnResult<mangosteen_device::remote::SshTarget> {
    let host = instance
        .network
        .ssh_host
        .clone()
        .ok_or_else(|| MnError::Network {
            stage: "读取 SSH 信息".to_string(),
            reason: format!(
                "服务商 {} 未返回实例 {} 的 SSH 地址，无法建立隧道",
                instance.provider, instance.id
            ),
        })?;
    let port = instance.network.ssh_port.unwrap_or(22);
    // 本次重置使用的密码必须与 ssh-copy-id/SSH 探测使用的密码完全一致。
    // 配置密码优先；只有未配置重置密码时才使用厂商下发密码。
    let password = config
        .ssh_password
        .clone()
        .or_else(|| instance.network.ssh_password.clone());

    let password_target = mangosteen_device::remote::SshTarget {
        host: host.clone(),
        port,
        user: "root".to_string(),
        password: password.clone(),
        identity_file: None,
    };
    if password.is_none() {
        return Err(MnError::Network {
            stage: "SSH 登录".to_string(),
            reason: "节点未提供可用 SSH 密码，无法继续密码认证".to_string(),
        });
    }
    // “重置密码 → 等待生效 → 清理目标指纹 → ssh-copy-id → 密钥确认”
    // 是节点配置环境的一部分；每次尝试都重新执行，最多 3 次。
    let mut last_error = String::new();
    for attempt in 1..=3 {
        if let Some(pw) = config.ssh_password.as_deref() {
            driver.reset_root_password(&instance.id, pw)?;
            std::thread::sleep(Duration::from_secs(2));
        }
        wait_tcp(&host, port, config.start_timeout, config.poll_interval)?;
        match mangosteen_device::remote::install_ssh_copy_id(&password_target) {
            Ok(key) => {
                let key_target = mangosteen_device::remote::SshTarget {
                    host: host.clone(),
                    port,
                    user: "root".to_string(),
                    password: None,
                    identity_file: Some(key),
                };
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match mangosteen_device::remote::ssh_exec(&key_target, "true", 5) {
                        Ok(_) => return Ok(key_target),
                        Err(_e) if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(250));
                        }
                        Err(e) => {
                            last_error = format!("第 {attempt} 次密钥确认失败：{e}");
                            break;
                        }
                    }
                }
            }
            Err(e) => last_error = format!("第 {attempt} 次 ssh-copy-id 失败：{e}"),
        }
        if attempt < 3 {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    Err(MnError::Network {
        stage: "节点配置环境 / SSH 免密登录".to_string(),
        reason: format!("连续 3 次未能确认 SSH 公钥登录成功：{last_error}"),
    })
}

/// 由远端连接信息构造隧道参数（云实例与自建设备共用）。
pub fn tunnel_spec_for(
    ssh: &mangosteen_device::remote::SshTarget,
    remote_port: u16,
    allow_unready_remote: bool,
) -> TunnelSpec {
    TunnelSpec {
        ssh_host: ssh.host.clone(),
        ssh_port: ssh.port,
        ssh_user: ssh.user.clone(),
        identity_file: ssh.identity_file.clone(),
        forwards: vec![mangosteen_core::net::PortForward::loopback(0, remote_port)],
        control_master: true,
        // 两段式部署：设备配置阶段就建隧道，此时引擎还没起，
        // 不能要求转发目标立刻可达。
        allow_unready_remote,
        password: ssh.password.clone(),
    }
}

/// 设备配置阶段的产物：实例（可能为空）、远端连接信息、隧道端口。
#[derive(Debug, Clone)]
pub struct ProvisionedResources {
    /// 厂商实例。用户自建设备时为 `None`。
    pub instance: Option<Instance>,
    /// 远端连接信息
    pub ssh: mangosteen_device::remote::SshTarget,
    /// 远端工作目录
    pub workdir: String,
    /// 隧道映射到本机的端口
    pub local_port: u16,
}

impl CloudOrchestrator {
    /// **设备配置阶段**：建实例（若需要）→ 准备远端资源 → 建隧道。
    ///
    /// 只做到「机器可用、权重就位、隧道通了」，**不启动引擎**。
    /// 启动留给 [`Self::start_engine`]，对应产品要求的两段式：
    /// 设备配置完成后再启动模型。
    ///
    /// 失败时按现有不变量回滚：厂商实例会被销毁，绝不留计费中的机器。
    pub fn provision_resources(
        &mut self,
        constraints: &SelectionConstraints,
        plan: &CloudPlan,
        _on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> MnResult<ProvisionedResources> {
        let (_best, ranked, _all) = self.pick(constraints)?;
        let mut ordered: Vec<CreateCandidate> = ranked
            .iter()
            .map(|c| CreateCandidate {
                provider: c.provider.clone(),
                spec_id: c.spec_id.clone(),
                price: c.price_per_hour,
            })
            .collect();
        prefer_requested_spec(&mut ordered, plan.requested_spec_id.as_deref());

        let (provider, instance) = self.create_first_running(
            &ordered,
            Some(&format!("mangosteen-{}", plan.engine_port)),
            Some(plan.engine_port),
        )?;
        let driver = self.registry.get(&provider)?;

        let result = (|| -> MnResult<ProvisionedResources> {
            // 这里只确认实例 Running、正式 SSH IP/端口和厂商下发的密码信息。
            // 密码重置、ssh-copy-id 和密钥登录属于后续“节点配置环境”。
            let ssh = vendor_password_target(&instance, &self.config)?;
            // 节点创建完成后只建立基础隧道；模型与引擎准备由编排层按阶段执行。
            let spec = tunnel_spec_for(&ssh, plan.engine_port, true);
            let local_port = self
                .tunnels
                .establish(&spec, self.config.tunnel_timeout)?
                .local_port;
            Ok(ProvisionedResources {
                instance: Some(instance.clone()),
                ssh,
                workdir: self.config.remote_workdir.trim_end_matches('/').to_string(),
                local_port,
            })
        })();

        match result {
            Ok(r) => Ok(r),
            Err(e) => {
                let cleanup = destroy_with_retry(driver, &instance.id);
                Err(augment_with_cleanup(e, &instance, cleanup))
            }
        }
    }

    pub fn configure_ssh_access(
        &self,
        instance: &Instance,
    ) -> MnResult<mangosteen_device::remote::SshTarget> {
        let driver = self.registry.get(&instance.provider)?;
        vendor_ssh_target(driver, instance, &self.config)
    }

    /// **启动模型阶段**：在已准备好的节点上拉起 `llama-server`。
    ///
    /// 只拉起进程，不等 `/health`——探活由调用方经隧道完成，避免两处
    /// 超时逻辑各写一套。
    pub fn start_engine(
        &self,
        ssh: &mangosteen_device::remote::SshTarget,
        workdir: &str,
        launch: &EngineLaunchConfig,
    ) -> MnResult<()> {
        start_node_engine(ssh, workdir, launch)
    }

    /// 为自建节点建隧道（不经过厂商实例）。
    pub fn establish_tunnel_to(
        &mut self,
        ssh: &mangosteen_device::remote::SshTarget,
        remote_port: u16,
    ) -> MnResult<u16> {
        let spec = tunnel_spec_for(ssh, remote_port, true);
        Ok(self
            .tunnels
            .establish(&spec, self.config.tunnel_timeout)?
            .local_port)
    }

    /// 在远端引擎启动后确认隧道仍然可用。
    ///
    /// 节点配置阶段允许远端端口尚未监听，因此当时建立的隧道可能在
    /// SSH 转发通道真正打开前退出。启动模型阶段必须使用严格模式重新
    /// 建立/复用隧道，确保远端端口确实已经转发，而不是继续使用旧的
    /// `local_port` 假设。
    pub fn ensure_tunnel_to(
        &mut self,
        ssh: &mangosteen_device::remote::SshTarget,
        remote_port: u16,
    ) -> MnResult<u16> {
        let spec = tunnel_spec_for(ssh, remote_port, false);
        // 严格模式前主动关闭同一目标的旧隧道。旧隧道可能是配置阶段
        // 用 allow_unready_remote 建立的惰性转发：SSH 进程还活着，但
        // 远端端口并未真正转发，继续复用会表现为本地 connection refused。
        let _ = self.tunnels.close(&spec);
        Ok(self
            .tunnels
            .establish(&spec, self.config.tunnel_timeout)?
            .local_port)
    }
}

/// 实例已 Running 之后的**一站式**收尾（保留给 CLI / 兼容调用）。
///
/// 产品部署链路已改为 [`CloudOrchestrator::provision_resources`] +
/// [`CloudOrchestrator::start_engine`] 两段式；这里只是把两段背靠背串起来，
/// 行为与改造前一致。
fn finish_provision(
    driver: &dyn mangosteen_core::cloud::ProviderDriver,
    instance: Instance,
    plan: &CloudPlan,
    launch: &EngineLaunchConfig,
    tunnels: &mut TunnelManager,
    config: &OrchestratorConfig,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> MnResult<Provisioned> {
    let target = match vendor_ssh_target(driver, &instance, config) {
        Ok(t) => t,
        Err(e) => {
            let cleanup = driver.destroy(&instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    };
    let workdir = config.remote_workdir.trim_end_matches('/').to_string();

    if plan.remote_model.is_none() {
        // 权重准备 + 起 llama-server，全部走 ssh（不依赖隧道）
        if let Err(e) = prepare_node(
            &target,
            &workdir,
            plan,
            &config.llama_server_url,
            on_progress,
        )
        .and_then(|_| start_node_engine(&target, &workdir, launch))
        {
            let cleanup = driver.destroy(&instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    }

    let spec = tunnel_spec_for(&target, plan.engine_port, false);
    let local_port = match tunnels.establish(&spec, config.tunnel_timeout) {
        Ok(t) => t.local_port,
        Err(e) => {
            let cleanup = driver.destroy(&instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    };
    let base = format!("http://127.0.0.1:{local_port}");

    if plan.remote_model.is_none() {
        if let Err(e) = mangosteen_device::remote::wait_http_ready(
            &format!("{base}/health"),
            config.start_timeout,
            config.poll_interval,
        ) {
            let _ = tunnels.close(&spec);
            let cleanup = driver.destroy(&instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    } else if let Some(model) = plan.remote_model.as_deref() {
        if let Err(e) = mangosteen_device::remote::wait_http_ready(
            &format!("{base}/api/version"),
            config.start_timeout,
            config.poll_interval,
        ) {
            let _ = tunnels.close(&spec);
            let cleanup = driver.destroy(&instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
        if let Err(e) =
            mangosteen_device::remote::ensure_ollama_model(&base, model, config.start_timeout)
        {
            let _ = tunnels.close(&spec);
            let cleanup = driver.destroy(&instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    }

    Ok(Provisioned {
        instance,
        tunnel_established: true,
        base_url: format!("{base}/v1"),
        local_port: Some(local_port),
        ssh: target,
        workdir,
    })
}

/// 远端资源准备：确保权重就位、llama-server 二进制存在（**不启动**）。
///
/// ## 权重来源（**没有本地上传这条路**）
///
/// 1. **远端已有且大小吻合** → 跳过（同一台机器反复部署是常态）
/// 2. **远端直连下载**（`plan.download_url`）→ 唯一的获取方式
/// 3. 失败 → 直接报错（由调用方决定是否回滚实例）
///
/// 刻意不做「本地上传兜底」：部署位置决定下载位置，选了云端就在云端下。
/// 节点配置阶段：创建模型目录并准备推理引擎，不下载模型、不启动服务。
pub fn configure_node(
    target: &mangosteen_device::remote::SshTarget,
    workdir: &str,
    llama_server_url: &str,
) -> MnResult<()> {
    // SSH 免密登录确认成功后，先完成依赖安装并逐项验证；依赖不完整时
    // 立即失败，不允许继续下载 llama.cpp。
    let deps = mangosteen_device::remote::ensure_node_download_dependencies(target)?;
    eprintln!("[node-config] 远端下载依赖已就绪：{}", deps.trim());
    let workdir = workdir.trim_end_matches('/').to_string();
    mangosteen_device::remote::ssh_exec(target, &format!("mkdir -p {workdir}/models"), 60)?;
    // 依赖确认成功后才下载/准备 llama-server；函数返回成功路径后，
    // 调用方才会把阶段推进到“模型下载”。
    let llama_path = mangosteen_device::remote::ensure_llama_server_from_archive(
        target,
        &workdir,
        llama_server_url,
    )?;
    mangosteen_device::remote::ssh_exec(
        target,
        &format!(
            "test -x '{}' && '{}' --version >/dev/null 2>&1 || test -x '{}'",
            llama_path, llama_path, llama_path
        ),
        60,
    )?;
    eprintln!("[node-config] llama-server 已确认可执行：{llama_path}");
    Ok(())
}

/// 模型下载阶段：在节点配置完成后下载模型并返回远端路径。
pub fn download_model_node(
    target: &mangosteen_device::remote::SshTarget,
    plan: &CloudPlan,
    hf_token: Option<&str>,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    should_cancel: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> MnResult<String> {
    let remote_model = &plan.model_path;
    let remote_size = mangosteen_device::remote::remote_file_size(target, remote_model)?;
    let want = plan.download_bytes;
    let marked_complete =
        mangosteen_device::remote::remote_download_completed(target, remote_model)?;
    let ready = marked_complete
        || match (remote_size, want) {
            (got, Some(exp)) if exp > 0 => got == exp,
            (got, _) => got > 0,
        };
    if !ready {
        let url = plan
            .download_url
            .as_deref()
            .ok_or_else(|| MnError::Network {
                stage: "下载云端模型".to_string(),
                reason: format!("没有可用的下载地址，无法获取模型 {}", plan.model_path),
            })?;
        mangosteen_device::remote::download_model_remote_with_progress(
            target,
            url,
            remote_model,
            want,
            hf_token,
            on_progress,
            should_cancel,
        )?;
    }
    // 下载函数返回后做最终核验，确认文件真的落到远端再进入下一阶段。
    // 期望字节数只作参考：计划里的 `download_bytes` 可能来自预估，
    // 与源站实际大小差几个字节时不应否定一次已完成的下载。
    let final_size = mangosteen_device::remote::remote_file_size(target, remote_model)?;
    if final_size == 0 {
        return Err(MnError::Network {
            stage: "云端下载模型".to_string(),
            reason: format!("下载结束但远端文件为空：{remote_model}"),
        });
    }
    if let Some(exp) = want.filter(|v| *v > 0) {
        if final_size != exp {
            eprintln!(
                "[download] 远端文件大小 {} 与计划期望 {} 不一致，按远端实际大小继续部署",
                final_size, exp
            );
        }
    }
    Ok(remote_model.clone())
}

/// 兼容旧调用：按“配置 → 下载”顺序执行，不负责启动引擎。
pub fn prepare_node(
    target: &mangosteen_device::remote::SshTarget,
    workdir: &str,
    plan: &CloudPlan,
    llama_server_url: &str,
    on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
) -> MnResult<()> {
    configure_node(target, workdir, llama_server_url)?;
    let _ = download_model_node(target, plan, None, on_progress, None)?;
    Ok(())
}

/// 启动阶段的远端引擎拉起。
pub fn start_node_engine(
    target: &mangosteen_device::remote::SshTarget,
    workdir: &str,
    launch: &EngineLaunchConfig,
) -> MnResult<()> {
    let workdir = workdir.trim_end_matches('/').to_string();
    let bin = format!("{workdir}/bin/llama-server");
    // 二进制可能被用户清理掉：先探测，找不到就给出可操作结论
    let probe = mangosteen_device::remote::ssh_exec(
        target,
        &format!("if [ -x {bin} ]; then echo ok; elif command -v llama-server >/dev/null 2>&1; then command -v llama-server; else echo missing; fi"),
        60,
    )?;
    let probe = probe.trim();
    let bin = if probe == "ok" {
        bin
    } else if probe == "missing" || probe.is_empty() {
        return Err(MnError::Network {
            stage: "启动云端推理服务".to_string(),
            reason: format!("远端找不到 llama-server（{bin}）。请重新执行部署以重新准备引擎。"),
        });
    } else {
        probe.to_string()
    };
    // 启动命令包含动态库路径：官方压缩包中的 llama-server 依赖同目录的
    // libllama-server-impl.so，不能假设远端系统已配置 ld.so.cache。
    // 启动失败时清理残留进程并重试 3 次（初次尝试 + 3 次重试）。
    const MAX_START_RETRIES: u32 = 3;
    let mut last_error = None;
    for attempt in 0..=MAX_START_RETRIES {
        match mangosteen_device::remote::start_llama_server(target, &bin, &launch.args, &workdir) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = Some(e.to_string());
                if attempt == MAX_START_RETRIES {
                    break;
                }
                eprintln!(
                    "[engine-start] 第 {} 次启动失败，{} 秒后重试（最多重试 {} 次）：{}",
                    attempt + 1,
                    2,
                    MAX_START_RETRIES,
                    last_error.as_deref().unwrap_or("未知错误")
                );
                let _ = mangosteen_device::remote::stop_llama_server(target, &workdir);
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
    Err(MnError::Network {
        stage: "启动云端推理服务".to_string(),
        reason: format!(
            "启动推理引擎连续失败（初次尝试 + {} 次重试）：{}",
            MAX_START_RETRIES,
            last_error.unwrap_or_else(|| "未知错误".to_string())
        ),
    })
}

/// 把用户显式选定的规格排到最前。失败时保留原排序，由创建阶段尝试其它候选。
fn prefer_requested_spec(ordered: &mut Vec<CreateCandidate>, requested: Option<&str>) {
    let Some(want) = requested else { return };
    if let Some(hit) = ordered.iter().position(|c| c.spec_id == want) {
        let chosen = ordered.remove(hit);
        ordered.insert(0, chosen);
    }
}

/// 实例建出来但起不来，是否值得换下一个候选。
///
/// 实测踩到：优云 P40 能创建成功，但随后进入 `Failed`（该卡型在目标可用区
/// 实际无可用容量，或镜像与卡型不兼容）。这种失败与「选哪个节点」强相关，
/// 换一台就可能好；而鉴权/余额类错误换节点也一样。
fn is_retryable_instance_failure(e: &MnError) -> bool {
    match e {
        MnError::Provider { code, .. } => {
            matches!(
                code.as_str(),
                "instance_failed" | "instance_destroyed" | "start_timeout"
            )
        }
        _ => false,
    }
}

/// 销毁实例，遇到「任务进行中」类错误时重试。
///
/// ## 为什么必须重试
///
/// 优云在实例刚进入 `Failed`/`Stopped` 时返回 8903
/// `UHost is in the task, please try again later`。这时立刻销毁必然失败，
/// 而**销毁失败就意味着继续计费**。只调一次然后把错误抛给用户，等于让用户
/// 自己盯着时间去控制台点删除——这正是要避免的。
///
/// 厂商的异步任务通常在十几秒内结束，所以退避间隔取 3/5/7/9/11/13 秒。
fn destroy_with_retry(
    driver: &dyn mangosteen_core::cloud::ProviderDriver,
    instance_id: &str,
) -> MnResult<()> {
    const ATTEMPTS: usize = 6;
    let mut last: Option<MnError> = None;
    for i in 0..ATTEMPTS {
        match driver.destroy(instance_id) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let text = e.to_string().to_ascii_lowercase();
                let transient = text.contains("8903")
                    || text.contains("in the task")
                    || text.contains("try again later");
                last = Some(e);
                if !transient || i + 1 == ATTEMPTS {
                    break;
                }
                std::thread::sleep(Duration::from_secs(3 + 2 * i as u64));
            }
        }
    }
    Err(last.unwrap_or_else(|| MnError::Network {
        stage: "销毁云端实例".to_string(),
        reason: "未知失败".to_string(),
    }))
}

/// 创建实例失败是否值得换下一个候选重试。
///
/// ## 判据
///
/// 只看「这个规格/这个可用区暂时没有资源」这一类：
///  * 厂商错误码 226604（规格无库存）
///  * 文案含「无库存」「售罄」「sold out」「no stock」「not enough resource」
///
/// **不**重试的情况：鉴权失败、参数非法、余额不足、配额超限——
/// 这些换节点也一样，继续试只会把同一个错误重复 N 遍并拉长等待。
fn is_retryable_create_error(e: &MnError) -> bool {
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "226604",
        "无库存",
        "售罄",
        "sold out",
        "no stock",
        "not enough resource",
        "insufficient resource",
        "resource not enough",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// 等一个 TCP 端口可连（用于等 sshd 接受新设的密码后开始监听）。
fn wait_tcp(host: &str, port: u16, budget: Duration, interval: Duration) -> MnResult<()> {
    let started = Instant::now();
    // 主机名要解析成 SocketAddr；用 ToSocketAddrs 而不是直接拼字符串，
    // 否则 `host:port` 里的 IPv6 会解析失败。
    let addrs: Vec<std::net::SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| MnError::Network {
            stage: "解析实例地址".to_string(),
            reason: format!("{host}:{port} 无法解析：{e}"),
        })?
        .collect();
    while started.elapsed() < budget {
        if addrs
            .iter()
            .any(|a| std::net::TcpStream::connect_timeout(a, Duration::from_millis(800)).is_ok())
        {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
    Err(MnError::Network {
        stage: "等待 SSH 可用".to_string(),
        reason: format!(
            "{host}:{port} 在 {}s 内无法连接。实例可能仍在初始化，\
             或安全组未放行该端口",
            budget.as_secs()
        ),
    })
}

/// 在错误消息里附加「资源清理结果」。
///
/// 关键信息：若清理失败，必须让用户看到残留的实例 ID 与手动清理命令，
/// 否则会静默产生费用。
fn augment_with_cleanup(base: MnError, instance: &Instance, cleanup: MnResult<()>) -> MnError {
    let suffix = match cleanup {
        Ok(()) => format!("\n已自动销毁临时实例 {}，未产生持续费用。", instance.id),
        Err(e) => format!(
            "\n⚠ 自动销毁实例 {} 失败：{e}\n\
             请立即手工处理，避免持续计费：\n  \
             mangosteen cloud destroy {} --provider {}",
            instance.id, instance.id, instance.provider
        ),
    };
    MnError::Provider {
        provider: instance.provider.clone(),
        code: "provision_rollback".to_string(),
        message: format!("{base}{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_core::cloud::{
        DriverCapabilities, GpuSpec, InstanceSpec, NetworkEndpoint, PriceQuote,
    };
    use mangosteen_core::policy::ScoreWeights;
    use std::sync::{Arc, Mutex};

    /// 可编程的假驱动：记录调用序列，按脚本返回状态。
    struct FakeDriver {
        name: &'static str,
        /// 依次返回的实例状态序列（describe 时消费）
        state_script: Arc<Mutex<Vec<InstanceState>>>,
        calls: Arc<Mutex<Vec<String>>>,
        /// create 是否失败
        fail_create: bool,
        /// create 失败时是否为「可重试」类错误（无库存）。
        ///
        /// 与 `fail_create` 分开：不可重试的错误（鉴权/余额）必须立刻中止，
        /// 可重试的才换下一个候选。两者混在一起就测不出这条区别。
        retryable_create_failure: bool,
        /// start 是否失败
        fail_start: bool,
        /// destroy 是否失败
        fail_destroy: bool,
        /// 是否返回 SSH 信息
        with_ssh: bool,
        /// 规格列表
        specs: Vec<AvailableSpec>,
    }

    impl FakeDriver {
        fn new(name: &'static str, states: Vec<InstanceState>) -> Self {
            Self {
                name,
                state_script: Arc::new(Mutex::new(states)),
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_create: false,
                retryable_create_failure: false,
                fail_start: false,
                fail_destroy: false,
                with_ssh: true,
                specs: vec![spec_entry(name, 24, 2.0)],
            }
        }

        fn record(&self, s: &str) {
            self.calls.lock().unwrap().push(s.to_string());
        }

        fn make_instance(&self, id: &str, state: InstanceState) -> Instance {
            Instance {
                id: id.to_string(),
                provider: self.name.to_string(),
                state,
                spec: InstanceSpec {
                    gpu: GpuSpec {
                        model: "RTX 4090".into(),
                        count: 1,
                        vram_bytes: 24 << 30,
                    },
                    cpu_cores: 16,
                    memory_bytes: 60 << 30,
                    disk_bytes: 50 << 30,
                    image: "img".into(),
                },
                network: NetworkEndpoint {
                    ssh_password: None,
                    ssh_host: if self.with_ssh {
                        Some("10.0.0.1".into())
                    } else {
                        None
                    },
                    ssh_port: Some(22),
                    inference_host: None,
                    inference_port: None,
                    public_url: None,
                },
                created_at: 0,
                price: Some(PriceQuote {
                    per_hour_cny: 2.0,
                    billing_granularity_secs: 0,
                    spot_release_allowed: true,
                    note: None,
                }),
                display_name: None,
            }
        }
    }

    fn spec_entry(provider: &str, vram_gib: u64, price: f64) -> AvailableSpec {
        AvailableSpec {
            spec_id: format!("{provider}-spec"),
            spec: InstanceSpec {
                gpu: GpuSpec {
                    model: "RTX 4090".into(),
                    count: 1,
                    vram_bytes: vram_gib << 30,
                },
                cpu_cores: 16,
                memory_bytes: 60 << 30,
                disk_bytes: 50 << 30,
                image: "img".into(),
            },
            price: PriceQuote {
                per_hour_cny: price,
                billing_granularity_secs: 0,
                spot_release_allowed: true,
                note: None,
            },
            region: "cn".into(),
            stock: Some(5),
            extra: Default::default(),
        }
    }

    impl mangosteen_core::cloud::ProviderDriver for FakeDriver {
        fn name(&self) -> &str {
            self.name
        }
        fn display_name(&self) -> &str {
            self.name
        }
        fn capabilities(&self) -> DriverCapabilities {
            DriverCapabilities::default()
        }
        fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
            self.record("list_specs");
            Ok(self.specs.clone())
        }
        fn create(&self, spec_id: &str, _name: Option<&str>) -> MnResult<Instance> {
            self.record(&format!("create:{spec_id}"));
            if self.fail_create {
                // 默认给「不可重试」的错误（容量不足不在重试白名单里），
                // 需要测「换下一个候选」时才置 `retryable_create_failure`。
                return Err(if self.retryable_create_failure {
                    MnError::Provider {
                        provider: self.name.into(),
                        code: "226604".into(),
                        message: "该规格无库存".into(),
                    }
                } else {
                    MnError::Provider {
                        provider: self.name.into(),
                        code: "no_capacity".into(),
                        message: "容量不足".into(),
                    }
                });
            }
            Ok(self.make_instance("i-1", InstanceState::Creating))
        }
        fn start(&self, id: &str) -> MnResult<Instance> {
            self.record(&format!("start:{id}"));
            if self.fail_start {
                return Err(MnError::Provider {
                    provider: self.name.into(),
                    code: "start_failed".into(),
                    message: "启动失败".into(),
                });
            }
            Ok(self.make_instance(id, InstanceState::Starting))
        }
        fn stop(&self, id: &str) -> MnResult<Instance> {
            self.record(&format!("stop:{id}"));
            Ok(self.make_instance(id, InstanceState::Stopped))
        }
        fn destroy(&self, id: &str) -> MnResult<()> {
            self.record(&format!("destroy:{id}"));
            if self.fail_destroy {
                return Err(MnError::Provider {
                    provider: self.name.into(),
                    code: "destroy_failed".into(),
                    message: "销毁失败".into(),
                });
            }
            Ok(())
        }
        fn describe(&self, id: &str) -> MnResult<Instance> {
            self.record(&format!("describe:{id}"));
            let mut s = self.state_script.lock().unwrap();
            let state = if s.is_empty() {
                InstanceState::Running
            } else {
                s.remove(0)
            };
            Ok(self.make_instance(id, state))
        }
        fn list(&self) -> MnResult<Vec<Instance>> {
            Ok(vec![])
        }
    }

    fn cfg() -> OrchestratorConfig {
        OrchestratorConfig {
            ssh_password: None,
            remote_workdir: "/root/mangosteen".to_string(),
            llama_server_url: String::new(),
            provision_timeout: Duration::from_millis(200),
            start_timeout: Duration::from_millis(300),
            tunnel_timeout: Duration::from_millis(200),
            poll_interval: Duration::from_millis(10),
        }
    }

    fn plan() -> CloudPlan {
        use mangosteen_core::estimate::{AssessRequest, KvCacheType};
        use mangosteen_core::hardware::{
            Accelerator, AcceleratorVendor, HardwareProfile, HostProfile,
        };
        use mangosteen_core::model::{ModelSpec, Quantization};
        let spec = ModelSpec {
            name: "qwen2.5-7b-instruct".into(),
            params_b: 7.615,
            quant: Quantization::Q4KM,
            n_layers: 28,
            n_heads: 28,
            n_kv_heads: 4,
            embedding_dim: 3584,
            vocab_size: 152_064,
            native_context: 32_768,
            file_size_bytes: 4_683_073_024,
        };
        let hw = HardwareProfile {
            os: "linux".into(),
            arch: "x86_64".into(),
            host: HostProfile {
                total_memory_bytes: 64 << 30,
                available_memory_bytes: 48 << 30,
                physical_cores: 8,
                logical_cores: 16,
                cpu_model: "t".into(),
            },
            accelerators: vec![Accelerator {
                name: "RTX 4090".into(),
                vendor: AcceleratorVendor::Nvidia,
                vram_bytes: 24 << 30,
                unified_memory_bytes: 0,
                compute_capability: Some("sm_89".into()),
                fp16_tflops: None,
                memory_bandwidth_gbps: None,
            }],
        };
        let a = mangosteen_core::estimate::assess(
            &spec,
            &hw,
            &AssessRequest::default()
                .with_context(4096)
                .with_kv_cache(KvCacheType::F16),
        );
        let engine =
            mangosteen_core::plan::LocalPlan::from_assessment(&spec, "/root/q.gguf", &a, 8080);
        CloudPlan {
            download_url: None,
            download_bytes: None,
            remote_model: None,
            node_ip: None,
            requested_spec_id: None,
            node_ssh_port: None,
            node_ssh_user: None,
            device_id: None,
            provider: "fake".into(),
            gpu_model: "RTX 4090".into(),
            gpu_count: 1,
            vram_bytes_per_gpu: 24 << 30,
            cpu_cores: 16,
            memory_bytes: 60 << 30,
            disk_bytes: 50 << 30,
            image: "img".into(),
            model_path: "/root/q.gguf".into(),
            engine_port: 8080,
            local_port: 11435,
            engine,
            max_price_per_hour: None,
        }
    }

    fn orchestrator(
        drivers: Vec<Box<dyn mangosteen_core::cloud::ProviderDriver>>,
    ) -> CloudOrchestrator {
        let mut reg = DriverRegistry::new();
        for d in drivers {
            reg.register(d);
        }
        let selector = NodeSelector::with_weights(ScoreWeights::default()).unwrap();
        let tunnels = TunnelManager::new(30500, 30510, "/nonexistent/ssh", None).unwrap();
        CloudOrchestrator::new(reg, selector, tunnels, cfg())
    }

    #[test]
    fn collect_candidates_merges_providers() {
        let mut a = FakeDriver::new("alpha", vec![]);
        a.specs = vec![spec_entry("alpha", 24, 2.0), spec_entry("alpha", 32, 3.0)];
        let b = FakeDriver::new("beta", vec![]);
        let o = orchestrator(vec![Box::new(a), Box::new(b)]);
        let (cands, _lat) = o.collect_candidates().unwrap();
        assert_eq!(cands.len(), 3);
        assert!(cands.iter().any(|(p, _)| p == "alpha"));
        assert!(cands.iter().any(|(p, _)| p == "beta"));
    }

    #[test]
    fn collect_candidates_tolerates_partial_failure() {
        struct Broken;
        impl mangosteen_core::cloud::ProviderDriver for Broken {
            fn name(&self) -> &str {
                "broken"
            }
            fn display_name(&self) -> &str {
                "broken"
            }
            fn capabilities(&self) -> DriverCapabilities {
                DriverCapabilities::default()
            }
            fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
                Err(MnError::Provider {
                    provider: "broken".into(),
                    code: "boom".into(),
                    message: "接口挂了".into(),
                })
            }
            fn create(&self, _: &str, _: Option<&str>) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn start(&self, _: &str) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn stop(&self, _: &str) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn destroy(&self, _: &str) -> MnResult<()> {
                Ok(())
            }
            fn describe(&self, _: &str) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn list(&self) -> MnResult<Vec<Instance>> {
                Ok(vec![])
            }
        }
        let o = orchestrator(vec![
            Box::new(FakeDriver::new("alpha", vec![])),
            Box::new(Broken),
        ]);
        let (cands, _) = o.collect_candidates().unwrap();
        assert_eq!(cands.len(), 1, "坏服务商不应影响好的");
        assert_eq!(cands[0].0, "alpha");
    }

    #[test]
    fn collect_candidates_all_failed_reports_reasons() {
        struct AlwaysBroken(&'static str);
        impl mangosteen_core::cloud::ProviderDriver for AlwaysBroken {
            fn name(&self) -> &str {
                self.0
            }
            fn display_name(&self) -> &str {
                self.0
            }
            fn capabilities(&self) -> DriverCapabilities {
                DriverCapabilities::default()
            }
            fn list_available_specs(&self) -> MnResult<Vec<AvailableSpec>> {
                Err(MnError::Provider {
                    provider: self.0.into(),
                    code: "auth".into(),
                    message: "密钥无效".into(),
                })
            }
            fn create(&self, _: &str, _: Option<&str>) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn start(&self, _: &str) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn stop(&self, _: &str) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn destroy(&self, _: &str) -> MnResult<()> {
                Ok(())
            }
            fn describe(&self, _: &str) -> MnResult<Instance> {
                Err(MnError::bad_request("x"))
            }
            fn list(&self) -> MnResult<Vec<Instance>> {
                Ok(vec![])
            }
        }
        let o = orchestrator(vec![
            Box::new(AlwaysBroken("a")),
            Box::new(AlwaysBroken("b")),
        ]);
        let e = o.collect_candidates().unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("a:"), "got {msg}");
        assert!(msg.contains("b:"), "got {msg}");
        assert!(msg.contains("密钥无效"), "got {msg}");
    }

    #[test]
    fn pick_respects_vram_constraint() {
        let mut a = FakeDriver::new("alpha", vec![]);
        a.specs = vec![spec_entry("alpha", 8, 1.0)]; // 显存不够
        let mut b = FakeDriver::new("beta", vec![]);
        b.specs = vec![spec_entry("beta", 24, 3.0)];
        let o = orchestrator(vec![Box::new(a), Box::new(b)]);
        let cons = SelectionConstraints {
            min_vram_bytes: 20 << 30,
            ..Default::default()
        };
        let (best, ranked, _) = o.pick(&cons).unwrap();
        assert_eq!(best.provider, "beta");
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn pick_prefers_cheaper_when_weights_favor_price() {
        let mut a = FakeDriver::new("alpha", vec![]);
        a.specs = vec![spec_entry("alpha", 24, 1.0)];
        let mut b = FakeDriver::new("beta", vec![]);
        b.specs = vec![spec_entry("beta", 24, 3.0)];
        let mut reg = DriverRegistry::new();
        reg.register(Box::new(a));
        reg.register(Box::new(b));
        let selector = NodeSelector::with_weights(ScoreWeights {
            price: 100.0,
            latency: 0.0,
            availability: 0.0,
            hardware: 0.0,
        })
        .unwrap();
        let tunnels = TunnelManager::new(30500, 30510, "/nonexistent/ssh", None).unwrap();
        let o = CloudOrchestrator::new(reg, selector, tunnels, cfg());
        let (best, _, _) = o.pick(&SelectionConstraints::default()).unwrap();
        assert_eq!(best.provider, "alpha");
    }

    #[test]
    fn provision_rolls_back_when_start_fails() {
        let mut d = FakeDriver::new("alpha", vec![InstanceState::Created]);
        d.fail_start = true;
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);

        let e = o
            .provision(
                &SelectionConstraints::default(),
                &plan(),
                &EngineLaunchConfig::default(),
            )
            .unwrap_err();
        let msg = e.to_string();
        // 失败信息里必须包含回滚结果
        assert!(
            msg.contains("自动销毁") || msg.contains("手工处理"),
            "got {msg}"
        );

        let c = calls.lock().unwrap().clone();
        assert!(c.iter().any(|s| s.starts_with("create")), "{c:?}");
        assert!(c.iter().any(|s| s.starts_with("start")), "{c:?}");
        assert!(
            c.iter().any(|s| s.starts_with("destroy")),
            "必须销毁已创建实例，实际调用：{c:?}"
        );
    }

    #[test]
    fn provision_reports_uncleaned_resource_on_rollback_failure() {
        let mut d = FakeDriver::new("alpha", vec![InstanceState::Created]);
        d.fail_start = true;
        d.fail_destroy = true;
        let mut o = orchestrator(vec![Box::new(d)]);

        let e = o
            .provision(
                &SelectionConstraints::default(),
                &plan(),
                &EngineLaunchConfig::default(),
            )
            .unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("手工处理"), "必须提示人工介入：{msg}");
        assert!(
            msg.contains("mangosteen cloud destroy"),
            "必须给出清理命令：{msg}"
        );
        assert!(msg.contains("i-1"), "必须给出残留实例 ID：{msg}");
    }

    #[test]
    fn provision_rolls_back_when_create_fails() {
        let mut d = FakeDriver::new("alpha", vec![]);
        d.fail_create = true;
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);

        let e = o
            .provision(
                &SelectionConstraints::default(),
                &plan(),
                &EngineLaunchConfig::default(),
            )
            .unwrap_err();
        assert!(e.to_string().contains("容量不足"), "got {e}");
        let c = calls.lock().unwrap().clone();
        // 创建就失败了，没有实例可销毁
        assert!(!c.iter().any(|s| s.starts_with("destroy")), "{c:?}");
    }

    #[test]
    fn provision_fails_when_no_ssh_info() {
        let mut d = FakeDriver::new("alpha", vec![InstanceState::Running]);
        d.with_ssh = false;
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);

        let e = o
            .provision(
                &SelectionConstraints::default(),
                &plan(),
                &EngineLaunchConfig::default(),
            )
            .unwrap_err();
        assert!(e.to_string().contains("SSH"), "got {e}");
        // 拿不到 SSH 也必须销毁实例，避免白花钱
        let c = calls.lock().unwrap().clone();
        assert!(
            c.iter().any(|s| s.starts_with("destroy")),
            "无 SSH 信息时也要销毁实例：{c:?}"
        );
    }

    #[test]
    fn provision_times_out_when_never_running() {
        // describe 一直返回 Starting
        let d = FakeDriver::new("alpha", vec![InstanceState::Starting; 200]);
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);
        let e = o
            .provision(
                &SelectionConstraints::default(),
                &plan(),
                &EngineLaunchConfig::default(),
            )
            .unwrap_err();
        assert!(
            e.to_string().contains("未进入 Running") || e.to_string().contains("回滚"),
            "got {e}"
        );
        let c = calls.lock().unwrap().clone();
        assert!(c.iter().any(|s| s.starts_with("destroy")), "{c:?}");
    }

    #[test]
    fn teardown_collects_errors_without_short_circuit() {
        let mut d = FakeDriver::new("alpha", vec![]);
        d.fail_destroy = true;
        let mut o = orchestrator(vec![Box::new(d)]);
        let errs = o.teardown("alpha", "i-1");
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| e.contains("销毁实例")), "{errs:?}");
    }

    #[test]
    fn teardown_unknown_provider_is_reported() {
        let mut o = orchestrator(vec![]);
        let errs = o.teardown("ghost", "i-1");
        assert!(
            errs.iter().any(|e| e.contains("找不到服务商驱动")),
            "{errs:?}"
        );
    }

    #[test]
    fn teardown_succeeds_for_running_instance() {
        let d = FakeDriver::new("alpha", vec![InstanceState::Running]);
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);
        let errs = o.teardown("alpha", "i-1");
        assert!(errs.is_empty(), "{errs:?}");
        let c = calls.lock().unwrap().clone();
        assert!(c.iter().any(|s| s.starts_with("destroy")), "{c:?}");
    }

    #[test]
    fn list_instances_covers_all_providers() {
        let o = orchestrator(vec![
            Box::new(FakeDriver::new("alpha", vec![])),
            Box::new(FakeDriver::new("beta", vec![])),
        ]);
        let all = o.list_instances();
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|(_, r)| r.is_ok()));
    }
    // ---------------- 最便宜节点：查询与创建 ----------------

    /// `cheapest` 必须给出**纯价格序**，不受默认 balanced 策略影响。
    ///
    /// 这是本功能的语义核心：用户说「要最便宜的」，不能因为延迟/库存分
    /// 把贵的排在前面。
    #[test]
    fn cheapest_orders_by_price_not_by_score() {
        let mut a = FakeDriver::new("alpha", vec![]);
        // 贵的但库存充足、名字靠前
        a.specs = vec![spec_entry("alpha", 24, 2.15)];
        let mut b = FakeDriver::new("beta", vec![]);
        b.specs = vec![spec_entry("beta", 32, 1.34)];
        let o = orchestrator(vec![Box::new(a), Box::new(b)]);

        let ordered = o.cheapest(&SelectionConstraints::default()).unwrap();
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].0, "beta", "便宜的必须排第一");
        assert!((ordered[0].1.price.per_hour_cny - 1.34).abs() < 1e-9);

        // 对照：默认 balanced 策略未必选它
        let cons = SelectionConstraints::default();
        let balanced = o.pick(&cons).unwrap().0;
        // balanced 只看结果不强制断言（依赖权重），但价格序必须独立成立
        assert!(balanced.price_per_hour > 0.0);
    }

    /// 最便宜的规格没货 → 自动换下一个，返回的必须是**真的开出来**的那台。
    #[test]
    fn create_cheapest_falls_through_on_no_stock() {
        let mut cheap = FakeDriver::new("alpha", vec![InstanceState::Running]);
        cheap.specs = vec![spec_entry("alpha", 24, 0.60)];
        cheap.fail_create = true;
        cheap.retryable_create_failure = true;

        let mut pricier = FakeDriver::new("beta", vec![InstanceState::Running]);
        pricier.specs = vec![spec_entry("beta", 32, 1.34)];

        let o = orchestrator(vec![Box::new(cheap), Box::new(pricier)]);
        let (provider, inst) = o
            .create_cheapest(&SelectionConstraints::default(), Some("mn-test"))
            .unwrap();

        assert_eq!(provider, "beta", "最便宜的无货，应换下一个而不是报错");
        assert_eq!(inst.state, InstanceState::Running);
        assert_eq!(inst.id, "i-1");
    }

    /// 鉴权/余额类错误立刻中止，不浪费时间去试别的节点。
    #[test]
    fn create_cheapest_aborts_on_non_retryable_error() {
        let mut broken = FakeDriver::new("alpha", vec![InstanceState::Running]);
        broken.specs = vec![spec_entry("alpha", 24, 0.60)];
        broken.fail_create = true; // 不可重试（容量不足/鉴权）

        let mut fine = FakeDriver::new("beta", vec![InstanceState::Running]);
        fine.specs = vec![spec_entry("beta", 32, 1.34)];

        let o = orchestrator(vec![Box::new(broken), Box::new(fine)]);
        let e = o
            .create_cheapest(&SelectionConstraints::default(), None)
            .expect_err("不可重试错误必须直接失败");
        assert!(e.to_string().contains("alpha"), "{e}");
        // beta 根本不该被尝试
        assert!(
            !e.to_string().contains("beta"),
            "不可重试错误不应继续试下一个节点：{e}"
        );
    }

    /// 创建成功但起不来 → 回滚（销毁），再换下一个。
    ///
    /// 这条守护「失败不留计费实例」这个不变量。
    #[test]
    fn create_cheapest_rolls_back_failed_instance_before_retry() {
        let mut bad = FakeDriver::new("alpha", vec![InstanceState::Failed]);
        bad.specs = vec![spec_entry("alpha", 24, 0.60)];

        let mut good = FakeDriver::new("beta", vec![InstanceState::Running]);
        good.specs = vec![spec_entry("beta", 32, 1.34)];

        let o = orchestrator(vec![Box::new(bad), Box::new(good)]);
        let (provider, inst) = o
            .create_cheapest(&SelectionConstraints::default(), None)
            .unwrap();
        assert_eq!(provider, "beta");
        assert_eq!(inst.state, InstanceState::Running);
    }

    /// 约束仍然生效：min_vram 把便宜的淘汰掉。
    #[test]
    fn create_cheapest_respects_constraints() {
        let mut small = FakeDriver::new("alpha", vec![InstanceState::Running]);
        small.specs = vec![spec_entry("alpha", 24, 0.60)];
        let mut big = FakeDriver::new("beta", vec![InstanceState::Running]);
        big.specs = vec![spec_entry("beta", 48, 1.34)];

        let o = orchestrator(vec![Box::new(small), Box::new(big)]);
        let cons = SelectionConstraints {
            min_vram_bytes: 32 << 30,
            ..SelectionConstraints::default()
        };
        let ordered = o.cheapest(&cons).unwrap();
        assert_eq!(ordered.len(), 1, "24G 的必须被淘汰");
        assert_eq!(ordered[0].0, "beta");
    }

    /// 无任何候选时给出可读错误，而不是空列表让上层困惑。
    #[test]
    fn cheapest_without_candidates_errors() {
        let mut d = FakeDriver::new("alpha", vec![]);
        d.specs = vec![];
        let o = orchestrator(vec![Box::new(d)]);
        // 空 specs 会让 collect_candidates 报「都没有返回可用规格」
        let e = o.cheapest(&SelectionConstraints::default()).unwrap_err();
        assert!(!e.to_string().is_empty());
    }
}
