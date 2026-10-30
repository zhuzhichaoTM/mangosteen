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
        let o = self
            .cloud
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
        let tiers = self.cloud_tiers(constraints)?;
        let o = self
            .cloud
            .as_ref()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        let (all, _) = o.collect_candidates()?;

        Ok(tiers
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
                    price_per_hour: c.price_per_hour,
                    vram_bytes: c.vram_bytes,
                    latency_ms: c.latency_ms,
                    score_total: c.score_total,
                    tier: c.tier.clone().unwrap_or_else(|| "balanced".to_string()),
                    stock: spec.and_then(|s| s.stock),
                    cpu_cores: spec.map(|s| s.spec.cpu_cores).unwrap_or(0),
                    memory_bytes: spec.map(|s| s.spec.memory_bytes).unwrap_or(0),
                }
            })
            .collect())
    }
    /// 云端选点：查询规格 → 打分排序 → 返回排序结果（**不创建资源**）。
    ///
    /// 供 `mangosteen cloud pick` 使用，让用户在花钱前看清决策依据。
    pub fn pick_cloud(
        &self,
        constraints: &SelectionConstraints,
    ) -> MnResult<Vec<mangosteen_core::policy::ScoredCandidate>> {
        let o = self
            .cloud
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
        let o = self
            .cloud
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
        let o = self
            .cloud
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
        let o = self
            .cloud
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
        &mut self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<Vec<String>> {
        let o = self
            .cloud
            .as_mut()
            .ok_or_else(|| MnError::NoViableCandidate {
                reason: "未配置任何可用的云服务商".to_string(),
            })?;
        Ok(o.teardown(provider, instance_id))
    }
}
