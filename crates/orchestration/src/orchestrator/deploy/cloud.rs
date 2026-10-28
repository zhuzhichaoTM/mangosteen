//! 云端部署：按部署时记录的引擎生成启动配置并创建实例。
//!
//! 归属：第三层 · 编排调度核心（部署编排子模块）。

use super::*;

impl Orchestrator {
    /// 为云端节点按部署时记录的引擎生成启动配置。
    ///
    /// 编排层作为第三层入口负责调用引擎 SPI；CloudOrchestrator 只执行
    /// 收到的标准配置，不依赖 `mangosteen-device`，也不解释引擎 flags。
    pub(crate) fn cloud_launch_config(
        &self,
        plan: &mangosteen_core::plan::CloudPlan,
    ) -> MnResult<mangosteen_core::plan::EngineLaunchConfig> {
        let Some(plugin) = self.engines_registry.get(plan.engine.engine) else {
            return Err(MnError::bad_request(format!(
                "云端部署记录中的推理引擎 {} 尚未适配",
                plan.engine.engine.display_name()
            )));
        };
        Ok(plugin.launch_config(&plan.engine, "0.0.0.0", plan.engine_port))
    }
    /// 执行云端部署。
    ///
    /// `why_cloud` 说明「为什么必须走云端」，用于在无可用服务商时给出**准确**的
    /// 错误信息——是「本地资源不足」还是「本地部署失败」，用户排查方向完全不同。
    pub(crate) fn deploy_cloud(
        &mut self,
        evaluated: &Evaluated,
        cloud_plan: mangosteen_core::plan::CloudPlan,
        trail: Vec<String>,
        why_cloud: &str,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        if self.cloud.is_none() {
            return Err(MnError::NoViableCandidate {
                reason: format!(
                    "{why_cloud}；但未配置任何可用的云服务商，无法降级。\
                     请检查 config.providers 中的启用状态与凭证（`mangosteen cloud providers`）"
                ),
            });
        }

        // 云端所需显存由同一套估算给出（推理引擎唯一性原则）
        let required = self.assessor.cloud_vram_requirement(
            &evaluated.spec,
            evaluated.assessment.context_len,
            evaluated.assessment.kv_cache,
            evaluated.assessment.batch_size,
        );

        let constraints = SelectionConstraints {
            min_vram_bytes: required,
            min_memory_bytes: evaluated.assessment.requirement.total_bytes().max(8 << 30),
            min_disk_bytes: evaluated
                .spec
                .file_size_bytes
                .saturating_mul(2)
                .max(50 << 30),
            max_price_per_hour: cloud_plan.max_price_per_hour,
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: self.config.scheduler.require_stock,
        };

        // 先把本地状态准备好（这些操作需要 &self / &state，不能与 orchestrator 的
        // 可变借用重叠）
        let id = self.new_deployment_id(evaluated);
        let mut plan = self.scheduler.assemble_plan(
            id.clone(),
            evaluated,
            &Decision::Cloud {
                plan: Box::new(cloud_plan.clone()),
                reason: trail.last().cloned().unwrap_or_default(),
            },
            trail.clone(),
        )?;
        // 云端必须在远端把权重拿到，否则会建出一台「实例就绪但没有推理服务」
        // 的机器——用户看到部署成功、对话却 502，而且还在按小时计费。
        //
        // 云端**只有远端下载**这一条路（不做本地上传）：所以要么有下载地址，
        // 要么走 Ollama 自己按名字拉。两者都没有就明确拒绝，别建出空转的机器。
        if let Some(c) = plan.cloud.as_ref() {
            if c.download_url.is_none() && c.remote_model.is_none() {
                return Err(MnError::bad_request(format!(
                    "无法在云端获取模型 {}：没有可用的下载地址。\
                     云端部署要求模型来自可下载的源（ModelScope / HuggingFace）；\
                     若是本地自有文件，请把它打进镜像或改用本地部署。",
                    evaluated.spec.name
                )));
            }
        }

        self.state
            .upsert(DeploymentRecord::from_plan(plan.clone()))?;
        self.state
            .set_status(&id, DeploymentStatus::Provisioning, None)?;
        sink(
            DeployStage::Configuring,
            StageOutcome::Done,
            format!(
                "云端实例配置：显存 ≥ {} · 本地隧道端口 {}",
                mangosteen_platform::fs::human_bytes(required),
                plan.cloud
                    .as_ref()
                    .map(|c| c.local_port.to_string())
                    .unwrap_or_else(|| "—".to_string())
            ),
        );
        // 云端不需要在本地量化：镜像里已有推理环境
        sink(
            DeployStage::Quantizing,
            StageOutcome::Skipped,
            "云端部署无需本地量化".to_string(),
        );
        sink(
            DeployStage::Launching,
            StageOutcome::Running,
            "正在创建云端实例并建立 SSH 隧道".to_string(),
        );

        let launch_config = self.cloud_launch_config(&cloud_plan)?;
        let orchestrator = self.cloud.as_mut().expect("checked above");
        match orchestrator.provision(&constraints, &cloud_plan, &launch_config) {
            Ok(p) => {
                // 回填实际选中的服务商与实例 ID
                if let Some(c) = plan.cloud.as_mut() {
                    Scheduler::apply_selection(
                        c,
                        &p.instance.provider,
                        &p.instance.id,
                        &p.instance.spec.gpu.model,
                        p.instance.spec.gpu.vram_bytes,
                    );
                    c.local_port = p.local_port.unwrap_or(c.local_port);
                }
                plan.origin = ComputeOrigin::Cloud {
                    provider: p.instance.provider.clone(),
                    instance_id: p.instance.id.clone(),
                };
                self.state
                    .upsert(DeploymentRecord::from_plan(plan.clone()))?;
                self.state.set_status(&id, DeploymentStatus::Ready, None)?;
                sink(
                    DeployStage::Launching,
                    StageOutcome::Done,
                    format!(
                        "云端实例已就绪：{} · {}",
                        p.instance.provider, p.instance.id
                    ),
                );
                Ok(plan)
            }
            Err(e) => {
                self.state
                    .set_status(&id, DeploymentStatus::Failed, Some(e.to_string()))?;
                Err(e)
            }
        }
    }
}
