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
    /// 实例 root 登录密码（仅用于隧道认证，不用于重置密码）。
    ///
    /// 云端实例密码由厂商 API 返回，本字段仅用于厂商不返回密码时的兜底。
    /// 不会调用 `reset_root_password` API。
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
    /// 云端隧道表。
    ///
    /// 内部锁：部署长操作（创建实例、等开机、下载权重）绝不能在整段时间
    /// 里独占上层网关锁，否则 `/api/status` 等管理接口全部排队阻塞、UI
    /// 一直显示「连接中」。把它下沉为细粒度锁后，编排器方法一律 `&self`。
    tunnels: std::sync::Mutex<TunnelManager>,
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
            tunnels: std::sync::Mutex::new(tunnels),
            config,
        }
    }

    /// 驱动注册表引用。
    pub fn registry(&self) -> &DriverRegistry {
        &self.registry
    }

    /// 用新的驱动注册表替换当前注册表（设置页热更新厂商密钥后调用）。
    ///
    /// 驱动无状态（每次调用独立发 curl），替换不影响活跃部署；
    /// 隧道管理器与节点选择器保持原样。
    pub fn replace_registry(&mut self, registry: DriverRegistry) {
        self.registry = registry;
    }

    /// 取隧道表锁（细粒度内部锁，见字段注释）。
    pub fn tunnels(&self) -> std::sync::MutexGuard<'_, TunnelManager> {
        // 隧道表只是一张「远端端口 → 本地转发」映射，锁中毒后继续用
        // 比让整条部署链路崩掉代价小。
        self.tunnels.lock().unwrap_or_else(|e| e.into_inner())
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
        &self,
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
        &self,
        constraints: &SelectionConstraints,
        plan: &CloudPlan,
        launch: &EngineLaunchConfig,
        on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    ) -> MnResult<Provisioned> {
        // GPU 型号已由上层在节点选择阶段确定。先查询云端实例列表，严格按
        // 型号匹配可复用实例；只有没有匹配实例时才进入规格排序和创建流程。
        // 这是“优先复用、再创建”的唯一入口，避免重复创建相同 GPU 节点。
        let requested_gpu_owned = plan
            .requested_gpu_model
            .as_deref()
            .or_else(|| (!plan.gpu_model.trim().is_empty()).then_some(plan.gpu_model.as_str()))
            .map(|s| s.trim().to_string());
        if let Some(requested_gpu) = requested_gpu_owned.as_deref().filter(|s| !s.is_empty()) {
            if let Some((provider, mut instance)) = self.find_reusable_instance(requested_gpu)? {
                eprintln!(
                    "[cloud] ✓ 复用 GPU 型号匹配实例 {} · {}（状态 {}）",
                    provider, instance.id, instance.state
                );
                let driver = self.registry.get(&provider)?;
                if !instance.state.is_serving() {
                    self.wait_until_running(driver, &mut instance, Some(plan.engine_port))?;
                }
                return finish_provision(
                    driver,
                    instance,
                    plan,
                    launch,
                    &mut self.tunnels(),
                    &self.config,
                    on_progress,
                );
            }
            eprintln!("[cloud] 未找到 GPU 型号 `{requested_gpu}` 的可复用实例，进入创建流程");
        }

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
            &mut self.tunnels(),
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
                    // 回滚：关闭实例但不删除，保留实例和磁盘供后续复用。
                    let cleanup = stop_with_retry(driver, &instance.id);
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

        // describe/start 的响应不含密码字段（厂商不回传），直接覆盖会把
        // 创建时设置的登录密码冲掉——后续「用密码装公钥」就没凭证了。
        // 这里记住原密码，刷新后补回（仅当新值没带密码时）。
        let login_password = instance.network.ssh_password.clone();

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
                    if instance.network.ssh_password.is_none() {
                        instance.network.ssh_password = login_password.clone();
                    }
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
            if instance.network.ssh_password.is_none() {
                instance.network.ssh_password = login_password.clone();
            }
        }
    }

    /// 关闭部署：关隧道 + 关闭实例（**绝不删除实例与磁盘**）。
    ///
    /// 返回关闭过程中遇到的错误列表（**不中断**后续清理步骤）。
    ///
    /// ## 为什么不删除
    ///
    /// 产品要求「任何环节失败、卸载都只关闭，不删除实例」：机器上的权重、
    /// 引擎和运行环境是重新部署时最贵的部分（几 GB 下载 + 依赖安装）。
    /// 删除等于把下次部署的成本全部重付一遍。关机已停止 GPU 计费，
    /// 实例与磁盘保留供后续复用。
    pub fn teardown(&self, provider: &str, instance_id: &str) -> Vec<String> {
        let mut errors = Vec::new();

        // 1. 关隧道（失败只记录）
        if let Err(e) = self.tunnels().close_all() {
            errors.push(format!("关闭隧道失败：{e}"));
        }

        // 2. 关闭实例（关键步骤：关机保留，不删除）
        match self.registry.get(provider) {
            Ok(driver) => {
                // 已经是终态（已不存在）就不再操作；其余状态一律尝试关机。
                match driver.describe(instance_id) {
                    Ok(inst) if inst.state == InstanceState::Destroyed => {}
                    Ok(_) => {
                        if let Err(e) = stop_with_retry(driver, instance_id) {
                            errors.push(format!("关闭实例 {instance_id} 失败：{e}"));
                        }
                    }
                    // 查不到实例：可能是记录残留，视为已不存在，不报错也不删除。
                    Err(_) => {}
                }
            }
            Err(e) => errors.push(format!("找不到服务商驱动 {provider}：{e}")),
        }

        errors
    }

    /// **真正销毁**实例（区别于 [`Self::teardown`] 的「关机保留」）。
    ///
    /// `cloud destroy --instance` 的语义就是销毁：实例消失、计费彻底停止。
    /// 此前的实现错拿了 [`Self::teardown`]——只关机不删除，CLI 却输出
    /// 「实例已销毁」，用户以为资源释放了，实际实例一直挂在厂商那里。
    ///
    /// 销毁前的关机由驱动内部保证（Terminate 仅对 Stopped 有效）；厂商
    /// 处于异步任务中（8903）时按 [`stop_with_retry`] 的节奏重试。
    pub fn destroy_instance(&self, provider: &str, instance_id: &str) -> Vec<String> {
        let mut errors = Vec::new();

        // 隧道关了实例就不可达，先清理本地侧资源（失败只记录）
        if let Err(e) = self.tunnels().close_all() {
            errors.push(format!("关闭隧道失败：{e}"));
        }

        match self.registry.get(provider) {
            Ok(driver) => {
                match driver.describe(instance_id) {
                    // 已是终态：幂等成功
                    Ok(inst) if inst.state == InstanceState::Destroyed => {}
                    Ok(_) => {
                        const ATTEMPTS: usize = 6;
                        let mut last: Option<MnError> = None;
                        for i in 0..ATTEMPTS {
                            match driver.destroy(instance_id) {
                                Ok(()) => {
                                    last = None;
                                    break;
                                }
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
                        if let Some(e) = last {
                            errors.push(format!(
                                "销毁实例 {instance_id} 失败（可能仍在计费）：{e}"
                            ));
                        }
                    }
                    // 查不到实例：可能已删除，视为幂等成功
                    Err(_) => {}
                }
            }
            Err(e) => errors.push(format!("找不到服务商驱动 {provider}：{e}")),
        }

        errors
    }

    /// 构造某个实例的 SSH 目标（供「启动/停止服务」用）。
    ///
    /// ## 必须与供给流程用同一套认证策略
    ///
    /// 供给阶段走 [`ensure_key_target`]：密码只用于**第一次**把本机公钥装进
    /// 远端 `authorized_keys`，之后所有连接一律密钥 + `BatchMode=yes`。
    ///
    /// 这个函数曾经自己拼一个「只有密码」的目标（`identity_file: None`），
    /// 于是与供给阶段脱节：实例一旦被回收重建——厂商换机、用户手动重装、
    /// 或被别的实例覆盖了 `authorized_keys`——密码认证就会失效，报
    /// `Permission denied (publickey,password)`。此时启动/停止服务全部失败，
    /// 而远端推理进程可能仍在跑、仍在计费。
    ///
    /// 现在统一复用 [`ensure_key_target`]：它会先尝试安装公钥（幂等，装过
    /// 就直接命中），失败才降级回密码。这样「新建实例」与「复用实例」走的是
    /// 同一条认证路径，不会出现只有一条路径能用的情况。
    pub fn service_target(
        &self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<mangosteen_device::remote::SshTarget> {
        let driver = self.registry.get(provider)?;
        let inst = driver.describe(instance_id)?;
        ensure_key_target(&inst, &self.config)
    }

    /// **停止服务**：停掉远端 `llama-server`，保留实例。
    ///
    /// 与 [`Self::teardown`] 的区别见 `remote::stop_llama_server` 的说明——
    /// 这里**不销毁实例**，所以仍在计费。上层必须把这一点告诉用户。
    pub fn stop_service(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let target = self.service_target(provider, instance_id)?;
        mangosteen_device::remote::stop_llama_server(&target, &self.config.remote_workdir)
    }

    /// 停止指定部署端口对应的 llama-server，不影响同节点其它模型。
    pub fn stop_service_on_port(
        &self,
        provider: &str,
        instance_id: &str,
        engine_port: u16,
    ) -> MnResult<()> {
        let target = self.service_target(provider, instance_id)?;
        // 修复1：外层不再叠加 3 次重试。停止脚本幂等，ssh_exec 内部已有
        // 5 次连接层重试；旧实现 3×5 最坏要数分钟，期间持有云锁，把
        // preview/deploy 等所有云端操作全部堵死（实测「卡在模型评估」）。
        // 失败后上层 unload 会按「进程是否真的还在」判定，不依赖这里的重试。
        mangosteen_device::remote::stop_llama_server_on_port(
            &target,
            &self.config.remote_workdir,
            Some(engine_port),
        )
    }

    /// 检查指定端口的推理服务是否在运行。
    pub fn is_service_running(
        &self,
        provider: &str,
        instance_id: &str,
        engine_port: u16,
    ) -> MnResult<bool> {
        let target = self.service_target(provider, instance_id)?;
        mangosteen_device::remote::llama_server_running_on_port(
            &target,
            &self.config.remote_workdir,
            engine_port,
        )
    }

    /// **启动服务**：在已有实例上重新拉起 `llama-server`.
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

    /// 隧道管理器（可变访问，用于关闭全部隧道）。
    pub fn tunnels_mut(&self) -> std::sync::MutexGuard<'_, TunnelManager> {
        self.tunnels()
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
        &self,
        provider: &str,
        instance_id: &str,
        plan: &CloudPlan,
    ) -> MnResult<String> {
        let driver = self.registry.get(provider)?;
        let instance = driver.describe(instance_id)?;
        let spec = self.tunnel_spec_for(&instance, plan, Some(plan.engine_port))?;
        // 优先用 heal：隧道已存在且健康时直接复用，断了则原地重建。
        // 直接 establish 也能复用，但会绕过「按远端端口定位」的清理逻辑，
        // 残留的 ssh 进程会占着本地端口不放。
        let mut local_port = match self.tunnels().heal(plan.engine_port, self.config.tunnel_timeout) {
            Ok(status) if status.is_up() => status.local_port(),
            _ => None,
        };
        if local_port.is_none() {
            // 没有记录（首次启动服务）或重建失败：走完整建立流程
            local_port = Some(
                self.tunnels()
                    .establish(&spec, self.config.tunnel_timeout)?
                    .local_port,
            );
        }
        let local_port = local_port.ok_or_else(|| MnError::Network {
            stage: "建立 ssh 隧道".to_string(),
            reason: "隧道建立后仍未获得本地端口".to_string(),
        })?;
        Ok(format!("http://127.0.0.1:{local_port}/v1"))
    }

    /// 由实例信息与方案构造隧道参数（供给与「启动服务」共用）。
    fn tunnel_spec_for(
        &self,
        instance: &Instance,
        plan: &CloudPlan,
        local_port: Option<u16>,
    ) -> MnResult<TunnelSpec> {
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
                local_port.unwrap_or(0),
                plan.engine_port,
            )],
            control_master: true,
            allow_unready_remote: false,
            password: instance.network.ssh_password.clone(),
        })
    }

    /// 关机实例（停止 GPU 计费，保留磁盘与数据）。
    ///
    /// 与 [`Self::teardown`] 的区别：关机只是把机器停了，实例还在、数据还在，
    /// 按量付费下不再收计算费用（磁盘可能仍计少量）。这是「停止」的正确语义——
    /// 用户想暂停一下，不是想删机器。
    pub fn power_off(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let driver = self.registry.get(provider)?;
        let target = self.service_target(provider, instance_id)?;
        // 仅验证当前实例的指定引擎端口，不把同节点其它模型误判为当前模型。
        // 关机前由上层按端口停止当前模型，再由这里做最终节点级 llama 检查。
        if mangosteen_device::remote::llama_server_running(&target, &self.config.remote_workdir)? {
            return Err(MnError::bad_request(format!(
                "实例 {instance_id} 仍有 llama-server 进程运行，已阻止关机；请先停止推理进程后重试"
            )));
        }
        driver.stop(instance_id)?;
        // 关机后立即验证实例状态（防止厂商 API 延迟导致状态还是 running）
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let inst = driver.describe(instance_id)?;
            if inst.state == InstanceState::Stopped {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(MnError::Network {
                    stage: "关机（停止实例）".to_string(),
                    reason: format!("实例 {instance_id} 关机后状态仍为 running：{}", inst.state),
                });
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// 开机实例并等待 Running（**管理动作**：无部署上下文，不绑定引擎端口）。
    ///
    /// 供编排层「云端节点」管理页调用——那里只有 (provider, instance_id)，
    /// 没有 CloudPlan。与 [`Self::power_on`] 的区别只是缺少 plan 参数。
    pub fn power_on_managed(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let driver = self.registry.get(provider)?;
        let mut instance = driver.describe(instance_id)?;
        if instance.state == InstanceState::Running {
            return Ok(());
        }
        if instance.state == InstanceState::Stopped || instance.state == InstanceState::Created {
            instance = driver.start(instance_id)?;
        }
        self.wait_until_running(driver, &mut instance, None)
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

    /// 按 `(provider, instance_id)` 精确查找用户指定复用的实例。
    ///
    /// `provider` 为空时跨全部驱动查找兜底（实例 ID 跨厂商不保证唯一，
    /// UI 必须成对传回；这里的遍历只为兼容旧调用方）。
    /// 实例不存在（已销毁）时返回明确错误——不能静默改走创建流程，
    /// 否则用户以为在复用旧机器，实际又开了一台新的计费实例。
    fn find_instance_by_id(
        &self,
        provider: Option<&str>,
        instance_id: &str,
    ) -> MnResult<(String, Instance)> {
        if let Some(name) = provider.map(str::trim).filter(|s| !s.is_empty()) {
            let d = self.registry.get(name)?;
            return d.describe(instance_id).map(|inst| (name.to_string(), inst)).map_err(|e| {
                MnError::bad_request(format!(
                    "要复用的云实例 {instance_id} 不可用（{name}）：{e}。\
                     实例可能已被销毁，请重新选择 GPU 节点"
                ))
            });
        }
        for (name, result) in self.list_instances() {
            let Ok(list) = result else { continue };
            if let Some(inst) = list.into_iter().find(|i| i.id == instance_id) {
                return Ok((name, inst));
            }
        }
        Err(MnError::bad_request(format!(
            "未找到要复用的云实例 {instance_id}：实例可能已被销毁，请重新选择 GPU 节点"
        )))
    }

    /// 查询可复用的同 GPU 型号实例（跨服务商）。
    ///
    /// 只接受能够继续进入 Running 的实例状态：Created、Stopped、Starting
    /// 和 Running。Failed/Destroyed/Stopping/Creating 不作为复用候选，避免
    /// 把处于不可恢复或过渡状态的实例误交给部署流程。
    ///
    /// 查询某个服务商失败不会阻断其它服务商；如果所有查询都失败，返回
    /// `None`，调用方仍可按原有策略创建新实例。
    fn find_reusable_instance(&self, requested_gpu: &str) -> MnResult<Option<(String, Instance)>> {
        let wanted = mangosteen_core::policy::normalize_gpu_model(requested_gpu);
        if wanted.is_empty() || wanted == "unknown" {
            return Ok(None);
        }
        for (provider, result) in self.list_instances() {
            let Ok(instances) = result else {
                continue;
            };
            if let Some(instance) = instances.into_iter().find(|instance| {
                let model = mangosteen_core::policy::normalize_gpu_model(&instance.spec.gpu.model);
                model == wanted
                    && matches!(
                        instance.state,
                        InstanceState::Created
                            | InstanceState::Stopped
                            | InstanceState::Starting
                            | InstanceState::Running
                    )
            }) {
                return Ok(Some((provider, instance)));
            }
        }
        Ok(None)
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
    // ## 密码优先级：实例自带 > 配置统一密码
    //
    // 厂商为**每台实例**生成独立密码（创建时指定或厂商随机），
    // `DescribeCompShareInstance` 会通过 `Password` 字段回传。配置里的
    // `ssh_password` 只是「用户手动填的统一密码」，对厂商实例通常早已
    // 失效（换机、重建、或从未对上）。
    //
    // 旧实现把配置密码放在前面，于是只要用户填过一次，就会永久覆盖
    // 厂商真实密码——每台新实例都拿旧密码认证，`Permission denied`
    // 必然发生，且现象是「密码明明对却登不上」，极难定位。
    let password = instance
        .network
        .ssh_password
        .clone()
        .or_else(|| config.ssh_password.clone());

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
        // 没给密码但可能已经装过公钥（同机重复部署、用户手动 ssh-copy-id）。
        // 这种情况不该直接拒绝——先试用本机默认密钥。
        if let Some(key) = mangosteen_device::remote::default_identity_key() {
            let key_target = mangosteen_device::remote::SshTarget {
                host: target.host.clone(),
                port: target.port,
                user: target.user.clone(),
                password: None,
                identity_file: Some(key),
            };
            if mangosteen_device::remote::ssh_exec(&key_target, "true", 10).is_ok() {
                return Ok(key_target);
            }
        }
        return Err(MnError::Network {
            stage: "节点配置环境 / SSH 免密登录".to_string(),
            reason: "自定义节点未提供登录密码，且本机密钥无法直接登录".to_string(),
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

    // 密码认证降级（方案1）：公钥安装连续失败时，若用户提供了密码，
    // 退回密码 askpass 认证而不是直接中止部署。askpass 的平台差异
    // （macOS/Linux `/bin/sh` 脚本、Windows `.cmd` 批处理）全部由
    // `mangosteen_platform::ssh::write_askpass` 收敛，三平台行为一致。
    //
    // 为什么这是「能用但脆」的降级路径：ssh 延迟调用 askpass，临时脚本
    // 必须活到认证时刻（见 `Tunnel` 的 askpass 生命周期注释）；密钥才是
    // 主路径。但降级远好于报错——用户至少还能把模型跑起来。
    if target.password.is_some() {
        eprintln!(
            "[cloud] ⚠ 节点 {}:{} 公钥安装失败，降级为密码认证（后续命令走 askpass）：{last_error}",
            target.host, target.port
        );
        return Ok(target.clone());
    }

    Err(MnError::Network {
        stage: "节点配置环境 / SSH 免密登录".to_string(),
        reason: format!("连续 3 次未能确认自定义节点 SSH 公钥登录成功：{last_error}"),
    })
}


/// 为云端实例建立**密钥认证**的 SSH 通道。
///
/// ## 为什么必须走密钥，而不是密码 askpass
///
/// 密码认证只能通过 askpass 协议传给 ssh：写一个临时脚本、靠环境变量
/// `SSH_ASKPASS` 让 ssh 去 exec 它。这条路在真实环境里非常脆：
///
/// * ssh 是**延迟**调用 askpass 的（TCP 握手 + 密钥交换之后），脚本必须
///   活到那一刻。清理逻辑稍早一步，ssh 就报
///   `exec(...): No such file or directory` → `Permission denied` → 退出 255，
///   而 stderr 往往还是空的，完全看不出原因；
/// * Windows 上没有 `/bin/sh`，同一套脚本根本跑不起来；
/// * 每次建隧道都要铺一遍临时文件，并发/端口复用时还会互相踩。
///
/// 所以正确做法是：**密码只用于「第一次把本机公钥装进远端 authorized_keys」**，
/// 之后所有连接（探测、下载、启动、隧道）一律用密钥 + `BatchMode=yes`。
/// 密钥认证没有交互、没有临时文件、没有时序依赖，天然可靠。
///
/// 密钥安装失败时才退回密码——那是「能用但脆」的降级路径，不是主路径。
fn ensure_key_target(
    instance: &Instance,
    config: &OrchestratorConfig,
) -> MnResult<mangosteen_device::remote::SshTarget> {
    let base = vendor_password_target(instance, config)?;
    // 已经在装过公钥的机器上重复部署时，这一步会直接命中探测分支，无额外开销。
    match mangosteen_device::remote::install_ssh_copy_id(&base) {
        Ok(key) => {
            eprintln!(
                "[cloud] ✓ 实例 {} 已切换到 SSH 密钥认证（{}）",
                instance.id,
                key.display()
            );
            Ok(mangosteen_device::remote::SshTarget {
                host: base.host,
                port: base.port,
                user: base.user,
                password: None,
                identity_file: Some(key),
            })
        }
        Err(e) => {
            // 降级而非中止：用户至少还能把模型跑起来。但必须留下明显痕迹，
            // 否则下次又会在隧道阶段看到那个「退出码 255、stderr 空」的谜题。
            eprintln!(
                "[cloud] ⚠ 实例 {} 安装 SSH 公钥失败，暂时退回密码认证：{e}",
                instance.id
            );
            Ok(base)
        }
    }
}

/// 由远端连接信息构造隧道参数（云实例与自建设备共用）。
pub fn tunnel_spec_for(
    ssh: &mangosteen_device::remote::SshTarget,
    remote_port: u16,
    allow_unready_remote: bool,
    local_port: Option<u16>,
) -> TunnelSpec {
    TunnelSpec {
        ssh_host: ssh.host.clone(),
        ssh_port: ssh.port,
        ssh_user: ssh.user.clone(),
        identity_file: ssh.identity_file.clone(),
        forwards: vec![mangosteen_core::net::PortForward::loopback(local_port.unwrap_or(0), remote_port)],
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
    /// ## 参数 `local_port`
    ///
    /// 若非 0，则使用指定的本地端口建立隧道（由编排层从调度器分配，
    /// 避免网关重启后端口从错误起始值如 8080 开始分配）；
    /// 若为 0，则由隧道管理器自动分配。
    pub fn provision_resources(
        &self,
        constraints: &SelectionConstraints,
        plan: &CloudPlan,
        _on_progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
        local_port: u16,
    ) -> MnResult<ProvisionedResources> {
        // 先查询云端现有实例。这里是当前部署编排实际使用的入口（区别于
        // 旧的 provision_with_progress），因此复用逻辑必须在这里执行；否则
        // UI 选择 P40 后仍会直接走 pick/create，重复创建 uhost 实例。
        //
        // 用户显式指定了复用实例（节点选择弹窗里挑的「已有节点」）时，
        // 严格按 (provider, id) 复用，优先级高于 GPU 型号自动匹配：
        // 用户明确挑了机器，就不该再开一台新的计费实例。停机实例先开机，
        // 运行中实例直接挂载——同机多模型由编排层分配互不冲突的引擎端口。
        if let Some(want_id) = plan
            .reuse_instance_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let (provider, mut instance) =
                self.find_instance_by_id(plan.reuse_provider.as_deref(), want_id)?;
            eprintln!(
                "[cloud] ✓ 复用用户指定实例 {provider} · {}（状态 {}）",
                instance.id, instance.state
            );
            let driver = self.registry.get(&provider)?;
            if !instance.state.is_serving() {
                self.wait_until_running(driver, &mut instance, Some(plan.engine_port))?;
            }
            // 先把本机公钥装进节点，隧道才走密钥认证（密码 askpass 极易失手）
            let ssh = ensure_key_target(&instance, &self.config)?;
            let local_port = if local_port != 0 { local_port } else { 0 };
            let spec = tunnel_spec_for(&ssh, plan.engine_port, true, Some(local_port));
            let local_port = self
                .tunnels()
                .establish(&spec, self.config.tunnel_timeout)?
                .local_port;
            return Ok(ProvisionedResources {
                instance: Some(instance),
                ssh,
                workdir: self.config.remote_workdir.trim_end_matches('/').to_string(),
                local_port,
            });
        }
        let requested_gpu_owned = plan
            .requested_gpu_model
            .as_deref()
            .or_else(|| (!plan.gpu_model.trim().is_empty()).then_some(plan.gpu_model.as_str()))
            .map(|s| s.trim().to_string());
        if let Some(requested_gpu) = requested_gpu_owned.as_deref().filter(|s| !s.is_empty()) {
            if let Some((provider, mut instance)) = self.find_reusable_instance(requested_gpu)? {
                eprintln!(
                    "[cloud] ✓ provision_resources 复用 GPU 型号匹配实例 {} · {}（状态 {}）",
                    provider, instance.id, instance.state
                );
                let driver = self.registry.get(&provider)?;
                if !instance.state.is_serving() {
                    self.wait_until_running(driver, &mut instance, Some(plan.engine_port))?;
                }
                let ssh = ensure_key_target(&instance, &self.config)?;
                let local_port = if local_port != 0 { local_port } else { 0 };
                let spec = tunnel_spec_for(&ssh, plan.engine_port, true, Some(local_port));
                let local_port = self
                    .tunnels()
                    .establish(&spec, self.config.tunnel_timeout)?
                    .local_port;
                return Ok(ProvisionedResources {
                    instance: Some(instance),
                    ssh,
                    workdir: self.config.remote_workdir.trim_end_matches('/').to_string(),
                    local_port,
                });
            }
            eprintln!(
                "[cloud] provision_resources 未找到 GPU 型号 `{requested_gpu}` 的匹配实例，进入创建流程"
            );
        }

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
            // 确认实例 Running 后立刻安装公钥：后续下载、启动、隧道全部走密钥，
            // 不再依赖密码 askpass（它对时序极敏感，是本模块最脆的一环）。
            let ssh = ensure_key_target(&instance, &self.config)?;
            let local_port = if local_port != 0 { local_port } else { 0 };
            let spec = tunnel_spec_for(&ssh, plan.engine_port, true, Some(local_port));
            let local_port = self
                .tunnels()
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
                let cleanup = stop_with_retry(driver, &instance.id);
                Err(augment_with_cleanup(e, &instance, cleanup))
            }
        }
    }

    pub fn configure_ssh_access(
        &self,
        instance: &Instance,
    ) -> MnResult<mangosteen_device::remote::SshTarget> {
        vendor_password_target(instance, &self.config)
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
        &self,
        ssh: &mangosteen_device::remote::SshTarget,
        remote_port: u16,
    ) -> MnResult<u16> {
        let spec = tunnel_spec_for(ssh, remote_port, true, Some(remote_port));
        Ok(self
            .tunnels()
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
        &self,
        ssh: &mangosteen_device::remote::SshTarget,
        remote_port: u16,
    ) -> MnResult<u16> {
        // 本地端口不能跟随 remote_port（多模型同节点时必然撞车）：
        // 传 0 让 TunnelManager 在分配区间内挑一个空闲端口。
        // 业务上 local_port 已由调度器写入台账（plan.cloud.local_port），
        // 调用方（commit_cloud）随后会把真实值回写台账，这里只是建立通道。
        let spec = tunnel_spec_for(ssh, remote_port, false, Some(0));
        // 严格模式前主动关闭同一目标的旧隧道。旧隧道可能是配置阶段
        // 用 allow_unready_remote 建立的惰性转发：SSH 进程还活着，但
        // 远端端口并未真正转发，继续复用会表现为本地 connection refused。
        let _ = self.tunnels().close(&spec);
        Ok(self
            .tunnels()
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
    let target = match vendor_password_target(&instance, config) {
        Ok(t) => t,
        Err(e) => {
            let cleanup = stop_with_retry(driver, &instance.id);
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
            let cleanup = stop_with_retry(driver, &instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    }

    // 本地端口不能跟随 engine_port（多模型同节点时必然撞车）：
    // 传 0 让 TunnelManager 在分配区间内挑一个空闲端口。
    let spec = tunnel_spec_for(&target, plan.engine_port, false, Some(0));
    let local_port = match tunnels.establish(&spec, config.tunnel_timeout) {
        Ok(t) => t.local_port,
        Err(e) => {
            let cleanup = stop_with_retry(driver, &instance.id);
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
            let cleanup = stop_with_retry(driver, &instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
    } else if let Some(model) = plan.remote_model.as_deref() {
        if let Err(e) = mangosteen_device::remote::wait_http_ready(
            &format!("{base}/api/version"),
            config.start_timeout,
            config.poll_interval,
        ) {
            let _ = tunnels.close(&spec);
            let cleanup = stop_with_retry(driver, &instance.id);
            return Err(augment_with_cleanup(e, &instance, cleanup));
        }
        if let Err(e) =
            mangosteen_device::remote::ensure_ollama_model(&base, model, config.start_timeout)
        {
            let _ = tunnels.close(&spec);
            let cleanup = stop_with_retry(driver, &instance.id);
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

/// 确认远端 SSH 通道当下真的能执行命令（轻量、带短重试）。
///
/// ## 为什么值得单独做一次
///
/// 「隧道建成」与「远端能执行命令」是两件不同的事：
///
/// * 隧道是 `ssh -L` 的**本地端口转发**，它只证明 ssh 进程活着；
///   `allow_unready_remote` 模式下甚至不要求远端有任何服务在监听。
/// * 节点配置、模型下载走的 `ssh_exec` 每次都是**新建连接**执行命令，
///   与那条隧道没有依赖关系。
///
/// 于是存在一个时间窗：实例刚报 `Running`（厂商状态机意义上的），
/// sshd 却还没接受登录，或厂商的端口转发尚未就绪。此时隧道能建、
/// 但第一个真正的叠加操作必然失败。这个窗口通常只有几秒，所以
/// 用短重试跨过去，而不是一撞就判死。
///
/// 重试用尽后仍然失败，就把「连不上」这个事实**明确**报出来——
/// 不要让调用方把它误读成下载失败或解压失败。
fn confirm_remote_reachable(target: &mangosteen_device::remote::SshTarget) -> MnResult<()> {
    // 15 次 × 6s ≈ 90s 窗口：实测压测中，实例从厂商报 Running 到 sshd
    // 真正接受登录（含密码同步）最长超过 36s——12 次 × 3s 的窗口依然
    // 全部撞在 `Permission denied` 上。冷启动慢不是失败，窗口跨过去即可。
    const ATTEMPTS: u32 = 15;
    const RETRY_WAIT: Duration = Duration::from_secs(6);
    let mut last_reason = String::new();
    for attempt in 1..=ATTEMPTS {
        match mangosteen_device::remote::ssh_exec(target, "true", 15) {
            Ok(_) => {
                if attempt > 1 {
                    eprintln!("[node-config] SSH 通道在重试 {attempt} 次后可用了");
                }
                return Ok(());
            }
            Err(e) => {
                last_reason = e.to_string();
                if attempt < ATTEMPTS {
                    std::thread::sleep(RETRY_WAIT);
                }
            }
        }
    }
    Err(MnError::Network {
        stage: "节点配置环境 / 确认远端连接".to_string(),
        reason: format!(
            "远端 {user}@{host}:{port} 暂时无法执行命令（已重试 {ATTEMPTS} 次，每次间隔 {}s）：\
             {last_reason}。常见原因：实例刚开机 sshd 尚未就绪、厂商端口转发未生效、\
             安全组拦截、实例已被关机或销毁",
            RETRY_WAIT.as_secs(),
            user = target.user,
            host = target.host,
            port = target.port,
        ),
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
    // 动真格之前先确认这条 SSH 通道真的还能执行命令。
    //
    // 节点配置的每一步都是重活：装依赖最长 600s、llama 包约 600MB、
    // 模型动辄十几 GB。如果连接在建隧道之后、到这里之前已经失效
    // （实例刚开机 sshd 还没起、厂商端口转发抖动、宿主机重启），
    // 直接扎进长任务只会得到一个与真实原因无关的报错。
    // 一次 `true` 只花几百毫秒，却能把故障定位到「连不上」，
    // 而不是等到十分钟后误报成下载或解压失败。
    confirm_remote_reachable(target)?;
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
    // 不在节点配置阶段执行 `llama-server --version`：部分 CUDA 发行包
    // 初始化动态库/设备上下文会耗时几十秒，甚至触发一次不必要的 CUDA
    // 探测，导致模型下载迟迟不开始。这里只确认文件可执行；真正的
    // 可执行性由启动阶段 probe + 健康检查验证。
    mangosteen_device::remote::ssh_exec(target, &format!("test -x '{}'", llama_path), 30)?;
    eprintln!("[node-config] llama-server 文件已确认，进入模型下载：{llama_path}");
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
    // 期望大小只是**参考值**，不是完成判据（与本地下载路径同一结论，
    // 见 device/remote.rs download_model_remote 的注释）。原因：
    // 计划里的字节数来自白名单默认文件的元数据（pull.rs 受理时从源站
    // 索引取），而实际下载哪个文件由**配置模板**决定——两者不是同一个
    // 文件时大小必然对不上（如 default 的 Q4_K_M ≈ 15.3 GiB 与
    // Uncensored 的 Q4_K_P ≈ 16.7 GiB）。把它当强校验的后果实测踩到：
    // 文件已下载完好，却因「大小不符」被判未完成 → 重新下载整个文件 →
    // 再次判失败，部署报「远端文件大小校验失败」。
    //
    // 完整性改由三道真实证据保证：下载器 0 退出写的 `.download.ok` 标记、
    // 文件非空、GGUF 魔数正确。期望大小只在无标记时作快速通道。
    let planned = plan.download_bytes.filter(|v| *v > 0);
    let want = match planned {
        Some(v) => Some(v),
        None => match plan.download_url.as_deref() {
            Some(url) => mangosteen_device::remote::remote_http_content_length(target, url)?,
            None => None,
        },
    };
    let remote_size = mangosteen_device::remote::remote_file_size(target, remote_model)?;
    // `.download.ok` 由下载器在所有分片成功退出后写入，是「传输真的跑完」的
    // 权威证据；大小相等只是无标记时的快速通道（如手工放置的文件）。
    let marked_complete =
        mangosteen_device::remote::remote_download_completed(target, remote_model)?;
    let ready = remote_size > 0
        && (marked_complete || Some(remote_size) == want)
        && mangosteen_device::remote::remote_file_is_gguf(target, remote_model)?;
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
    let final_size = mangosteen_device::remote::remote_file_size(target, remote_model)?;
    if final_size == 0 {
        return Err(MnError::Network {
            stage: "云端下载模型".to_string(),
            reason: format!("下载结束但远端文件为空：{remote_model}"),
        });
    }
    // 不再做「实际大小 == 期望大小」的强校验：期望字节可能来自另一个
    // 模板文件的元数据（见上注释），严格比对会把完好的下载误判为失败。
    // 完整性由下载器退出码（`.download.ok` 只在 0 退出时写入）与下面的
    // GGUF 魔数检查共同保证。
    // 大小对了也不代表内容对：错误页/错误仓库的文件同样可能有大小。
    // GGUF 魔数是最后一道廉价检查，只在前 4 字节上花一次 SSH。
    if !mangosteen_device::remote::remote_file_is_gguf(target, remote_model)? {
        return Err(MnError::Network {
            stage: "云端下载模型".to_string(),
            reason: format!(
                "远端文件不是有效的 GGUF（魔数不符）：{remote_model}，请检查下载地址是否指向正确的模型文件"
            ),
        });
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

/// 关闭实例，遇到异步任务中的临时错误时重试；不删除实例和磁盘。
fn stop_with_retry(
    driver: &dyn mangosteen_core::cloud::ProviderDriver,
    instance_id: &str,
) -> MnResult<()> {
    const ATTEMPTS: usize = 6;
    let mut last: Option<MnError> = None;
    for i in 0..ATTEMPTS {
        match driver.stop(instance_id) {
            Ok(_) => return Ok(()),
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
        stage: "关闭云端实例".to_string(),
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


/// 在错误消息里附加「资源清理结果」。
///
/// 关键信息：若清理失败，必须让用户看到残留的实例 ID 与手动清理命令，
/// 否则会静默产生费用。
fn augment_with_cleanup(base: MnError, instance: &Instance, cleanup: MnResult<()>) -> MnError {
    let suffix = match cleanup {
        Ok(()) => format!(
            "\n已自动关闭实例 {}，保留实例与磁盘，停止计费，可在后续部署中复用。",
            instance.id
        ),
        Err(e) => format!(
            "\n⚠ 自动关闭实例 {} 失败：{e}\n\
             实例已保留但可能仍在计费，请在云端控制台关闭它。",
            instance.id
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
        /// stop 是否失败
        fail_stop: bool,
        /// destroy 是否失败（仅保留给显式 teardown 测试）
        fail_destroy: bool,
        /// 是否返回 SSH 信息
        with_ssh: bool,
        /// 规格列表
        specs: Vec<AvailableSpec>,
        /// describe 是否失败（模拟实例已销毁/查不到）
        fail_describe: bool,
        /// list 返回的既有实例（模拟厂商名下已有的机器）
        listed: Vec<Instance>,
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
                fail_stop: false,
                fail_destroy: false,
                with_ssh: true,
                specs: vec![spec_entry(name, 24, 2.0)],
                fail_describe: false,
                listed: Vec::new(),
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
                    ssh_login_command: None,
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
            if self.fail_stop {
                return Err(MnError::Provider {
                    provider: self.name.into(),
                    code: "stop_failed".into(),
                    message: "关闭失败".into(),
                });
            }
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
            if self.fail_describe {
                return Err(MnError::Provider {
                    provider: self.name.into(),
                    code: "not_found".into(),
                    message: format!("实例 {id} 不存在"),
                });
            }
            let mut s = self.state_script.lock().unwrap();
            let state = if s.is_empty() {
                InstanceState::Running
            } else {
                s.remove(0)
            };
            Ok(self.make_instance(id, state))
        }
        fn list(&self) -> MnResult<Vec<Instance>> {
            Ok(self.listed.clone())
        }
    }

    fn cfg() -> OrchestratorConfig {
        OrchestratorConfig {
            remote_workdir: "/root/mangosteen".to_string(),
            llama_server_url: String::new(),
            provision_timeout: Duration::from_millis(200),
            start_timeout: Duration::from_millis(300),
            tunnel_timeout: Duration::from_millis(200),
            poll_interval: Duration::from_millis(10),
            ssh_password: None,
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
            requested_gpu_model: None,
            node_ssh_port: None,
            node_ssh_user: None,
            device_id: None,
            reuse_instance_id: None,
            reuse_provider: None,
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
            mmproj_path: None,
            draft_model_path: None,
            auxiliary_downloads: Vec::new(),
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
        // 失败信息里必须说明回滚结果
        assert!(
            msg.contains("已自动关闭") || msg.contains("可能仍在计费"),
            "got {msg}"
        );

        let c = calls.lock().unwrap().clone();
        assert!(c.iter().any(|s| s.starts_with("create")), "{c:?}");
        assert!(c.iter().any(|s| s.starts_with("start")), "{c:?}");
        // 回滚只关闭实例（stop），绝不删除实例与磁盘。
        assert!(
            c.iter().any(|s| s.starts_with("stop")),
            "回滚必须关闭已创建实例，实际调用：{c:?}"
        );
        assert!(
            !c.iter().any(|s| s.starts_with("destroy")),
            "回滚不得删除实例，实际调用：{c:?}"
        );
    }

    /// 显式复用已停机实例：必须先开机，且**绝不** create 新实例。
    ///
    /// 隧道/密码步骤在测试环境无法走通，但实例查找与开机都发生在它们
    /// 之前，用调用记录断言即可覆盖复用语义。
    #[test]
    fn provision_resources_reuses_stopped_instance_without_create() {
        let d = FakeDriver::new(
            "alpha",
            vec![InstanceState::Stopped, InstanceState::Starting, InstanceState::Running],
        );
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);
        let mut p = plan();
        p.reuse_instance_id = Some("reuse-1".into());
        p.reuse_provider = Some("alpha".into());
        let _ = o.provision_resources(&SelectionConstraints::default(), &p, None);
        let c = calls.lock().unwrap().clone();
        assert!(c.iter().any(|s| s == "describe:reuse-1"), "{c:?}");
        assert!(
            c.iter().any(|s| s == "start:reuse-1"),
            "停机实例必须先开机再挂载：{c:?}"
        );
        assert!(
            !c.iter().any(|s| s.starts_with("create")),
            "显式复用绝不新建实例：{c:?}"
        );
    }

    /// 运行中的实例直接挂载：不得重启（start），也不得新建。
    #[test]
    fn provision_resources_reuse_keeps_running_instance_without_restart() {
        let d = FakeDriver::new("alpha", vec![InstanceState::Running]);
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);
        let mut p = plan();
        p.reuse_instance_id = Some("reuse-1".into());
        p.reuse_provider = Some("alpha".into());
        let _ = o.provision_resources(&SelectionConstraints::default(), &p, None);
        let c = calls.lock().unwrap().clone();
        assert!(c.iter().any(|s| s == "describe:reuse-1"), "{c:?}");
        assert!(
            !c.iter().any(|s| s.starts_with("start")),
            "运行中实例不得重启：{c:?}"
        );
        assert!(!c.iter().any(|s| s.starts_with("create")), "{c:?}");
    }

    /// 用户显式挑选的实例优先于 GPU 型号自动匹配：即使型号匹配能命中
    /// 另一台机器，也必须按用户挑选的实例走。
    #[test]
    fn provision_resources_explicit_reuse_beats_gpu_auto_match() {
        let mut d = FakeDriver::new("alpha", vec![InstanceState::Running]);
        d.listed = vec![d.make_instance("shared-1", InstanceState::Running)];
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);
        let mut p = plan();
        p.requested_gpu_model = Some("RTX 4090".into()); // 自动匹配本可命中 shared-1
        p.reuse_instance_id = Some("chosen-1".into());
        p.reuse_provider = Some("alpha".into());
        let _ = o.provision_resources(&SelectionConstraints::default(), &p, None);
        let c = calls.lock().unwrap().clone();
        assert!(
            c.iter().any(|s| s == "describe:chosen-1"),
            "显式复用必须按用户挑选的实例走：{c:?}"
        );
        assert!(!c.iter().any(|s| s.starts_with("create")), "{c:?}");
    }

    /// 指定实例不存在（已销毁）时明确报错，**不得**静默改走创建流程——
    /// 否则用户以为在复用旧机器，实际又开了一台新的计费实例。
    #[test]
    fn provision_resources_missing_reuse_instance_fails_without_create() {
        let mut d = FakeDriver::new("alpha", vec![]);
        d.fail_describe = true;
        let calls = d.calls.clone();
        let mut o = orchestrator(vec![Box::new(d)]);
        let mut p = plan();
        p.reuse_instance_id = Some("ghost-1".into());
        p.reuse_provider = Some("alpha".into());
        let e = o
            .provision_resources(&SelectionConstraints::default(), &p, None)
            .unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("实例可能已被销毁") || msg.contains("未找到要复用的云实例"),
            "{msg}"
        );
        let c = calls.lock().unwrap().clone();
        assert!(
            !c.iter().any(|s| s.starts_with("create")),
            "实例不存在时也不得静默新建：{c:?}"
        );
    }

    /// 未指明服务商时按实例 ID 跨厂商兜底查找；找不到时报错。
    #[test]
    fn find_instance_by_id_falls_back_to_cross_provider_list() {
        let mut d = FakeDriver::new("alpha", vec![]);
        d.listed = vec![d.make_instance("x-1", InstanceState::Running)];
        let o = orchestrator(vec![Box::new(d)]);
        let (provider, inst) = o.find_instance_by_id(None, "x-1").unwrap();
        assert_eq!(provider, "alpha");
        assert_eq!(inst.id, "x-1");
        assert!(o.find_instance_by_id(None, "missing").is_err());
    }

    #[test]
    fn provision_reports_uncleaned_resource_on_rollback_failure() {
        let mut d = FakeDriver::new("alpha", vec![InstanceState::Created]);
        d.fail_start = true;
        d.fail_stop = true;
        let mut o = orchestrator(vec![Box::new(d)]);

        let e = o
            .provision(
                &SelectionConstraints::default(),
                &plan(),
                &EngineLaunchConfig::default(),
            )
            .unwrap_err();
        let msg = e.to_string();
        assert!(
            msg.contains("关闭实例") || msg.contains("可能仍在计费"),
            "必须提示关闭失败：{msg}"
        );
        assert!(!msg.contains("cloud destroy"), "不应再给出删除命令：{msg}");
        assert!(msg.contains("i-1"), "必须给出保留实例 ID：{msg}");
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
        // 拿不到 SSH 也必须关闭实例，但不能删除实例。
        let c = calls.lock().unwrap().clone();
        assert!(
            c.iter().any(|s| s.starts_with("stop")),
            "无 SSH 信息时也要关闭实例：{c:?}"
        );
        assert!(!c.iter().any(|s| s.starts_with("destroy")), "{c:?}");
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
        assert!(c.iter().any(|s| s.starts_with("stop")), "{c:?}");
        assert!(!c.iter().any(|s| s.starts_with("destroy")), "{c:?}");
    }

    #[test]
    fn teardown_collects_errors_without_short_circuit() {
        let mut d = FakeDriver::new("alpha", vec![]);
        d.fail_stop = true;
        let mut o = orchestrator(vec![Box::new(d)]);
        let errs = o.teardown("alpha", "i-1");
        assert!(!errs.is_empty());
        assert!(errs.iter().any(|e| e.contains("关闭实例")), "{errs:?}");
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
        assert!(c.iter().any(|s| s.starts_with("stop")), "{c:?}");
        assert!(!c.iter().any(|s| s.starts_with("destroy")), "{c:?}");
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
