//! 云端算力操作：候选打分、规格列举、最便宜节点与裸实例管理。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;

/// 档位强弱：同一规格同时命中多档时保留更强的一个。
fn tier_rank(tier: Option<&str>) -> u8 {
    match tier {
        Some("performance") => 3,
        Some("balanced") => 2,
        Some("minimal") => 1,
        _ => 0,
    }
}

impl Orchestrator {
    /// 三档候选：最低成本 / 综合最优 / 性能最强。
    ///
    /// 供部署确认弹窗让用户显式选择 GPU。三档都基于同一组硬约束
    /// （显存/内存/磁盘/价格/库存），只是排序目标不同：
    ///
    /// | 档位 | 排序依据 |
    /// |------|---------|
    /// | `minimal` | 价格升序（满足可加载前提的最便宜节点）|
    /// | `balanced` | 现有四维打分（价格/延迟/库存/硬件）|
    /// | `performance` | 显存优先，其次价格（同显存选便宜的）|
    ///
    /// 同一规格若同时命中多档只保留一次，档位记为最强的那个
    /// （performance > balanced > minimal），避免列表里出现重复项。
    pub fn cloud_tiers(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<mangosteen_core::policy::ScoredCandidate>> {
        let cloud = self.cloud();
        let o = cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;

        // 一次查询复用三份排序：候选列举要打厂商接口，重复查询既慢又可能被限流
        let (all, latencies) = o.collect_candidates()?;
        let ranked = o.rank(&all, &latencies, constraints);

        // minimal：约束内最便宜
        let mut cheapest = ranked.clone();
        cheapest.sort_by(|a, b| {
            a.price_per_hour
                .total_cmp(&b.price_per_hour)
                // 同价时显存大的优先——同样的钱能跑更大的模型
                .then(b.vram_bytes.cmp(&a.vram_bytes))
        });

        // performance：显存优先，其次价格
        let mut strongest = ranked.clone();
        strongest.sort_by(|a, b| {
            b.vram_bytes
                .cmp(&a.vram_bytes)
                .then(a.price_per_hour.total_cmp(&b.price_per_hour))
        });

        let mut out: Vec<mangosteen_core::policy::ScoredCandidate> = Vec::new();
        let mut push = |c: Option<&mangosteen_core::policy::ScoredCandidate>, tier: &str| {
            let Some(c) = c else { return };
            if let Some(existing) = out.iter_mut().find(|x| x.spec_id == c.spec_id) {
                // 已存在：保留更强档位（performance > balanced > minimal）
                if tier_rank(Some(tier)) > tier_rank(existing.tier.as_deref()) {
                    existing.tier = Some(tier.to_string());
                }
                return;
            }
            let mut c = c.clone();
            c.tier = Some(tier.to_string());
            out.push(c);
        };
        push(cheapest.first(), "minimal");
        push(ranked.first(), "balanced");
        push(strongest.first(), "performance");
        Ok(out)
    }

    /// 三档候选 + 规格细节（库存/CPU/内存），供部署确认弹窗直接渲染。
    ///
    /// 与 [`Self::cloud_tiers`] 共用同一次厂商查询：`cloud_tiers` 只返回
    /// 打分结果，而这个方法把原始 `AvailableSpec` 里的资源信息合并进来，
    /// 避免 UI 为了展示「几核几 G」再查一次接口。
    pub fn cloud_tier_briefs(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<crate::deployment::CloudCandidateBrief>> {
        let cloud = self.cloud();
        let o = cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        // 这里必须返回本次厂商查询的完整排序结果，而不是 cloud_tiers
        // 的三档摘要；部署 UI 需要展示所有实时可用/空闲规格。
        let (all, latencies) = o.collect_candidates()?;
        let ranked = o.rank(&all, &latencies, constraints);

        Ok(ranked
            .into_iter()
            .map(|c| {
                let spec = all
                    .iter()
                    .find(|(p, s)| s.spec_id == c.spec_id && *p == c.provider)
                    .map(|(_, s)| s);
                crate::deployment::CloudCandidateBrief {
                    provider: c.provider.clone(),
                    spec_id: c.spec_id.clone(),
                    gpu_model: c.gpu_model.clone(),
                    region: c.region.clone(),
                    price_per_hour: crate::deployment::sane_hourly_price(c.price_per_hour),
                    vram_bytes: c.vram_bytes,
                    latency_ms: c.latency_ms,
                    score_total: c.score_total,
                    tier: "available".to_string(),
                    stock: spec.and_then(|s| s.stock),
                    cpu_cores: spec.map(|s| s.spec.cpu_cores).unwrap_or(0),
                    memory_bytes: spec.map(|s| s.spec.memory_bytes).unwrap_or(0),
                }
            })
            .collect())
    }
    /// 云端**正在运行**的实例 + 其上占用的模型清单（预览 / 节点选择用）。
    ///
    /// ## 为什么不把失败当错误
    ///
    /// 复用是可选项：某个服务商查询失败只意味着「看不到它名下的实例」，
    /// 新购创建链路不受影响。预览是 UI 每次打开弹窗都会调的接口，
    /// 一次厂商抖动就把整个预览打挂，等于把可选项变成了阻断项。
    ///
    /// ## 只返回 Running 实例
    ///
    /// 节点选择里只出现两类节点：可新购的空闲规格，和**已经在运行**的
    /// 已有实例（用户「正在使用」的机器，可直接同机挂载新模型）。
    /// 已停机（Stopped/Created）实例**不进入选择列表**——但部署时后端
    /// 会按 GPU 型号在后台自动匹配并开机复用它们（见 provider 的
    /// `find_reusable_instance`），用户无需关心，也不会误选一台
    /// 需要等待开机的机器。
    ///
    /// `Destroyed` 同样不返回：终态机器与磁盘都没有了，展示只会误导。
    pub fn cloud_instance_briefs(&self) -> Vec<crate::deployment::CloudInstanceBrief> {
        use mangosteen_core::cloud::InstanceState;

        let cloud = self.cloud();
        let Some(o) = cloud.as_ref() else {
            return Vec::new();
        };
        // 台账活跃记录 → (provider, instance_id) → 占用模型清单。
        // UI 据此展示「这台机器上正在跑哪些模型」，提示复用是同机多模型。
        let occupied = |provider: &str, instance_id: &str| -> Vec<String> {
            self.state
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.status.is_active())
                .filter(|r| match &r.plan.origin {
                    ComputeOrigin::Cloud {
                        provider: p,
                        instance_id: i,
                    } => p == provider && i == instance_id,
                })
                .map(|r| {
                    mangosteen_core::model::ModelEntry::derive_id(
                        &r.plan.model.name,
                        r.plan.model.quant,
                    )
                })
                .collect()
        };

        let mut out = Vec::new();
        for (provider, result) in o.list_instances() {
            let Ok(instances) = result else {
                continue;
            };
            for inst in instances {
                // 节点选择只展示「正在使用」的实例；停机实例由后端在
                // 部署时按 GPU 型号后台自动开机复用，不出现在列表里。
                if !matches!(inst.state, InstanceState::Running) {
                    continue;
                }
                out.push(crate::deployment::CloudInstanceBrief {
                    provider: provider.clone(),
                    instance_id: inst.id.clone(),
                    gpu_model: inst.spec.gpu.model.clone(),
                    gpu_count: inst.spec.gpu.count,
                    vram_bytes: inst.spec.gpu.vram_bytes,
                    state: inst.state.as_str().to_string(),
                    ssh_target: inst.network.ssh_target(),
                    price_per_hour: inst.price.as_ref().map(|p| p.per_hour_cny),
                    occupied_models: occupied(&provider, &inst.id),
                    display_name: inst.display_name.clone(),
                });
            }
        }
        out
    }

    /// 设置页「云端节点」管理的全量实例清单（**含已关机**）。
    ///
    /// 与 [`Self::cloud_instance_briefs`]（部署页复用选择，只列运行中）不同：
    /// 管理页需要看到**所有**实例才能关机/开机/删除。`Destroyed` 仍然排除
    /// （磁盘已没了，展示只会误导）。
    pub fn cloud_instance_admin_briefs(&self) -> Vec<crate::deployment::CloudInstanceBrief> {
        use mangosteen_core::cloud::InstanceState;

        let cloud = self.cloud();
        let Some(o) = cloud.as_ref() else {
            return Vec::new();
        };
        let occupied = |provider: &str, instance_id: &str| -> Vec<String> {
            self.state
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.status.is_active())
                .filter(|r| match &r.plan.origin {
                    ComputeOrigin::Cloud {
                        provider: p,
                        instance_id: i,
                    } => p == provider && i == instance_id,
                })
                .map(|r| {
                    mangosteen_core::model::ModelEntry::derive_id(
                        &r.plan.model.name,
                        r.plan.model.quant,
                    )
                })
                .collect()
        };

        let mut out = Vec::new();
        for (provider, result) in o.list_instances() {
            let Ok(instances) = result else {
                continue;
            };
            for inst in instances {
                // 管理页展示全部非销毁实例（运行中 + 已关机）
                if matches!(inst.state, InstanceState::Destroyed) {
                    continue;
                }
                out.push(crate::deployment::CloudInstanceBrief {
                    provider: provider.clone(),
                    instance_id: inst.id.clone(),
                    gpu_model: inst.spec.gpu.model.clone(),
                    gpu_count: inst.spec.gpu.count,
                    vram_bytes: inst.spec.gpu.vram_bytes,
                    state: inst.state.as_str().to_string(),
                    ssh_target: inst.network.ssh_target(),
                    price_per_hour: inst.price.as_ref().map(|p| p.per_hour_cny),
                    occupied_models: occupied(&provider, &inst.id),
                    display_name: inst.display_name.clone(),
                });
            }
        }
        out
    }

    /// 云端选点：查询规格 → 打分排序 → 返回排序结果（**不创建资源**）。
    ///
    /// 供 `mangosteen cloud pick` 使用，让用户在花钱前看清决策依据。
    pub fn pick_cloud(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<mangosteen_core::policy::ScoredCandidate>> {
        let cloud = self.cloud();
        let o = cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        let (candidates, latencies) = o.collect_candidates()?;
        Ok(o.rank(&candidates, &latencies, constraints))
    }
    /// 查询云端可创建规格（跨服务商）。
    ///
    /// 返回 `(服务商名, 规格)` 列表。
    pub fn list_cloud_specs(
        &self,
    ) -> MnResult<Vec<(String, mangosteen_core::cloud::AvailableSpec)>> {
        let cloud = self.cloud();
        let o = cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.collect_candidates().map(|(c, _)| c)
    }
    /// 当前可用的**最低价**节点（按价格升序，**不创建任何资源**）。
    ///
    /// 与 [`Self::pick_cloud`] 的区别：后者走打分策略（默认 balanced，
    /// 综合价格/延迟/库存/硬件），「综合最优」未必最便宜。用户明确要
    /// 「最便宜」时用这个——它不受 `scheduler.strategy` 影响。
    ///
    /// 返回列表按价格升序，第一项即最便宜。**「可用」不等于库存字段为真**：
    /// 优云的库存接口是模拟调度（对能开的机器也返回 false），所以真正
    /// 「能不能开出来」要由 [`Self::create_cheapest_cloud`] 按顺序真开才知道。
    pub fn cheapest_cloud(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<(String, mangosteen_core::cloud::AvailableSpec)>> {
        let cloud = self.cloud();
        let o = cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.cheapest(constraints)
    }
    /// 创建一台「当前可用的最低价」实例，返回 `(服务商名, 实例)`。
    ///
    /// ## 这是**裸创建**，不是部署
    ///
    /// 不建 SSH 隧道、不上传模型、不启动引擎。适合「先拿到一台机器」的场景
    /// （手工调试、跑别人的镜像）。要跑推理请走 [`Self::deploy`]——它会
    /// 完整编排，并且写部署台账。
    ///
    /// ## 裸实例不在部署台账里
    ///
    /// 因此**不会被 `stop`/`abort` 自动回收**，也不会出现在「模型实例」列表。
    /// 用 [`Self::destroy_cloud_instance`] 销毁。调用方（CLI）必须在输出里
    /// 明确告知用户这一点，否则就是「开了台机器没人记得」的持续计费。
    pub fn create_cheapest_cloud(
        &self,
        constraints: &SelectionConstraints,
        name: Option<&str>,
    ) -> MnResult<(String, mangosteen_core::cloud::Instance)> {
        let cloud = self.cloud();
        let o = cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.create_cheapest(constraints, name)
    }
    /// 销毁任意云端实例（按 `服务商 + 实例 ID`，**不要求**它在台账里）。
    ///
    /// 存在的意义就是清理 [`Self::create_cheapest_cloud`] 这类裸实例：
    /// 没有台账记录，`stop(部署 ID)` 找不到它，用户就只能去控制台手工删——
    /// 那正是「忘关实例持续扣费」最常见的成因。
    ///
    /// 返回清理过程中的错误列表（空 = 全部成功）。
    pub fn destroy_cloud_instance(
        &self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<Vec<String>> {
        let mut cloud = self.cloud();
        let o = cloud
            .as_mut()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        Ok(o.destroy_instance(provider, instance_id))
    }

    /// 关机实例（设置页「云端节点」管理动作）。
    ///
    /// 与部署卸载（unload）不同：这里不做「其它模型还在跑」的保护判断——
    /// 管理页的关机是用户**显式**的电源操作，确认弹窗已提示后果。
    /// 若实例上仍有 llama-server 在跑，厂商关机后进程随之消失，
    /// 台账里的部署会在下一轮探测中转为失败——这是预期行为。
    pub fn cloud_power_off(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let mut cloud = self.cloud();
        let o = cloud
            .as_mut()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        o.power_off(provider, instance_id)
    }

    /// 开机实例并等待进入运行状态（设置页「云端节点」管理动作）。
    pub fn cloud_power_on(&self, provider: &str, instance_id: &str) -> MnResult<()> {
        let mut cloud = self.cloud();
        let o = cloud
            .as_mut()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        // 管理动作没有部署上下文（无 CloudPlan）：走 provider 的管理入口。
        o.power_on_managed(provider, instance_id)
    }
}

/// 云端实例 SSH 登录信息（厂商 API 实时查询结果）。
///
/// 台账里的 `node_ssh_port/node_ssh_user` 是**部署时刻的快照**：实例
/// 停机再开机后，厂商可能重新映射 SSH 端口（22/23/高位端口都可能），
/// 快照就过时了。「登陆方式」必须以厂商当前返回为准，本地拼装只做兜底。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InstanceSshInfo {
    pub deployment_id: String,
    pub instance_id: String,
    pub provider: String,
    /// 实例当前状态（厂商口径，如 `running`/`stopped`）
    pub state: String,
    /// 登录指令：厂商原生指令优先，否则按 `user@host -p port` 组装
    pub ssh_command: String,
    pub host: Option<String>,
    pub port: u16,
    pub user: String,
    /// 数据来源：`provider`（厂商原生）/ `assembled`（本地拼装）
    pub source: String,
}

/// 由归一化实例组装登录信息（纯函数，便于单测）。
fn ssh_info_from_instance(
    deployment_id: &str,
    provider: &str,
    user_override: Option<&str>,
    inst: &mangosteen_core::cloud::Instance,
) -> InstanceSshInfo {
    let net = &inst.network;
    let host = net.ssh_host.clone();
    let port = net.ssh_port.unwrap_or(22);
    let user = user_override
        .filter(|u| !u.trim().is_empty())
        .unwrap_or("root")
        .to_string();
    let (command, source) = match net
        .ssh_login_command
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(c) => (c.to_string(), "provider"),
        None => (
            format!("ssh -p {port} {user}@{}", host.clone().unwrap_or_default()),
            "assembled",
        ),
    };
    InstanceSshInfo {
        deployment_id: deployment_id.to_string(),
        instance_id: inst.id.clone(),
        provider: provider.to_string(),
        state: inst.state.as_str().to_string(),
        ssh_command: command,
        host,
        port,
        user,
        source: source.to_string(),
    }
}

impl Orchestrator {
    /// 只从缓存读 SSH 登录信息，**绝不打厂商 API**。
    ///
    /// ## 为什么需要这个「纯缓存」入口
    ///
    /// 网关是 `Arc<Mutex<Gateway>>` 单锁模型：任何在持锁期间做网络 I/O
    /// 的处理器都会把**所有**请求串起来。UI 的「登陆方式」是轮询项，
    /// 缓存每 60 秒过期一次，那一刻 `driver.describe()` 的延迟（厂商
    /// 接口慢、或网络抖动重试数十秒）会被放大成整站卡顿——下载/启动
    /// 过程中表现为「消息中断、连接失败」。
    ///
    /// 因此把「读」与「刷新」拆开：HTTP 处理器持锁期间只用这个方法，
    /// 未命中就返回 `None` 让调用方去**锁外**刷新。
    pub fn cached_ssh_info(&self, deployment_id: &str) -> Option<InstanceSshInfo> {
        let key = self.ssh_info_cache_key(deployment_id)?;
        let cache = self.ssh_info_cache.lock().ok()?;
        let (at, info) = cache.get(&key)?;
        if at.elapsed() < std::time::Duration::from_secs(60) {
            Some(info.clone())
        } else {
            None
        }
    }

    /// 缓存键：`deployment_id:provider:instance_id`。
    ///
    /// 必须含 deployment_id：同节点多模型共享同一实例 ID，只用
    /// provider:instance 会让第二个部署拿到第一个的缓存。
    fn ssh_info_cache_key(&self, deployment_id: &str) -> Option<String> {
        let rec = self.state.get(deployment_id).ok()??;
        let cloud = rec.plan.cloud.as_ref()?;
        let ComputeOrigin::Cloud { instance_id, .. } = &rec.plan.origin;
        if instance_id.trim().is_empty() {
            return None;
        }
        Some(format!("{}:{}:{}", deployment_id, cloud.provider, instance_id))
    }

    /// 刷新 SSH 登录信息（**会调厂商 API**）。
    ///
    /// 调用方必须保证自己**没有持有网关全局锁**——这正是卡顿的根源。
    /// HTTP 层在锁外调用本方法，再把结果交给 [`Self::cached_ssh_info`] 复用。
    pub fn refresh_ssh_info(&self, deployment_id: &str) -> MnResult<Option<InstanceSshInfo>> {
        let Some(rec) = self.state.get(deployment_id)? else {
            return Ok(None);
        };
        let Some(cloud) = rec.plan.cloud.as_ref() else {
            return Ok(None);
        };
        let instance_id = match &rec.plan.origin {
            ComputeOrigin::Cloud { instance_id, .. } => instance_id.clone(),
        };
        if instance_id.trim().is_empty() {
            return Ok(None);
        }
        let Some(key) = self.ssh_info_cache_key(deployment_id) else {
            return Ok(None);
        };

        let cloud_guard = self.cloud();
        let o = cloud_guard
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        let driver = o.registry().get(&cloud.provider)?;
        let inst = driver.describe(&instance_id)?;
        let info = ssh_info_from_instance(
            deployment_id,
            &cloud.provider,
            cloud.node_ssh_user.as_deref(),
            &inst,
        );
        if let Ok(mut cache) = self.ssh_info_cache.lock() {
            cache.insert(key, (std::time::Instant::now(), info.clone()));
        }
        Ok(Some(info))
    }

    /// 列出「需要刷新 SSH 登录信息」的部署 ID。
    ///
    /// 供后台刷新线程使用：持有网关锁期间只做这一次廉价读取，
    /// 随后在锁外逐个 [`Self::refresh_ssh_info`]。
    ///
    /// 只挑云端、实例 ID 非空、且缓存已过期的记录——缓存仍新鲜的不必重复
    /// 打厂商接口。读台账失败时返回空表（宁可这一轮不刷新，也不阻塞调用方）。
    pub fn deployments_needing_ssh_info(&self) -> Vec<String> {
        let Ok(records) = self.state.list() else {
            return Vec::new();
        };
        records
            .into_iter()
            .filter(|r| r.plan.cloud.is_some())
            .filter(|r| {
                matches!(
                    &r.plan.origin,
                    ComputeOrigin::Cloud { instance_id, .. } if !instance_id.trim().is_empty()
                )
            })
            .map(|r| r.plan.id.clone())
            .filter(|id| self.cached_ssh_info(id).is_none())
            .collect()
    }

    /// 实时查询部署实例的 SSH 登录信息（缓存未命中时直连厂商 API）。
    ///
    /// **不要在持有网关全局锁时调用**——它会阻塞整个 HTTP 层。
    /// HTTP 层请用 [`Self::cached_ssh_info`] + 锁外 [`Self::refresh_ssh_info`]。
    /// 保留本方法是为了 CLI 等无锁场景与向后兼容。
    pub fn instance_ssh_info(&self, deployment_id: &str) -> MnResult<Option<InstanceSshInfo>> {
        if let Some(info) = self.cached_ssh_info(deployment_id) {
            return Ok(Some(info));
        }
        self.refresh_ssh_info(deployment_id)
    }
}

#[cfg(test)]
mod ssh_info_tests {
    use super::*;

    fn instance(cmd: Option<&str>, host: Option<&str>, port: Option<u16>) -> mangosteen_core::cloud::Instance {
        mangosteen_core::cloud::Instance {
            id: "i-1".into(),
            provider: "compshare".into(),
            state: mangosteen_core::cloud::InstanceState::Running,
            spec: mangosteen_core::cloud::InstanceSpec {
                gpu: mangosteen_core::cloud::GpuSpec {
                    model: "P40".into(),
                    count: 1,
                    vram_bytes: 24 << 30,
                },
                cpu_cores: 16,
                memory_bytes: 60 << 30,
                disk_bytes: 50 << 30,
                image: "img".into(),
            },
            network: mangosteen_core::cloud::NetworkEndpoint {
                ssh_host: host.map(|s| s.to_string()),
                ssh_port: port,
                inference_host: None,
                inference_port: None,
                public_url: None,
                ssh_password: None,
                ssh_login_command: cmd.map(|s| s.to_string()),
            },
            created_at: 0,
            price: None,
            display_name: None,
        }
    }

    #[test]
    fn provider_command_wins_over_assembly() {
        let info = ssh_info_from_instance(
            "dep-1",
            "compshare",
            Some("root"),
            &instance(Some("ssh root@1.2.3.4 -p 60022"), Some("1.2.3.4"), Some(23)),
        );
        assert_eq!(info.ssh_command, "ssh root@1.2.3.4 -p 60022");
        assert_eq!(info.source, "provider");
        assert_eq!(info.state, "running");
    }

    #[test]
    fn falls_back_to_assembly_without_provider_command() {
        let info = ssh_info_from_instance(
            "dep-1",
            "compshare",
            None,
            &instance(None, Some("1.2.3.4"), Some(60022)),
        );
        assert_eq!(info.ssh_command, "ssh -p 60022 root@1.2.3.4");
        assert_eq!(info.source, "assembled");
        assert_eq!(info.user, "root");
    }

    #[test]
    fn blank_provider_command_is_ignored() {
        let info = ssh_info_from_instance(
            "dep-1",
            "compshare",
            Some("ubuntu"),
            &instance(Some("   "), Some("1.2.3.4"), Some(22)),
        );
        assert_eq!(info.ssh_command, "ssh -p 22 ubuntu@1.2.3.4");
        assert_eq!(info.user, "ubuntu");
    }
}
