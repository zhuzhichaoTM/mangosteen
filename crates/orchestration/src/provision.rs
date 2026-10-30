//! 云端/远端节点部署：设备配置阶段（建资源）与启动模型阶段（拉引擎）。
//!
//! 归属：第三层 · 编排调度核心（部署编排子模块）。
//!
//! ## 为什么分两段
//!
//! 产品要求「节点选好后先完成设备初始化配置，再启动推理引擎」。两段各自
//! 有独立的失败语义：
//!
//! | 阶段 | 做什么 | 失败时 |
//! |------|--------|--------|
//! | 设备配置 | 建实例（或连已有设备）→ 准备二进制 → 远端下载模型 → 建隧道 | 云厂商实例销毁；自建设备只停进程 |
//! | 启动模型 | SSH 拉起 `llama-server` → 经隧道探 `/health` | 同上，且台账标 `Failed` |
//!
//! 拆开之后，用户能在界面上看到「设备已就绪（IP/端口/用户）」再看到
//! 「推理进程已就绪」，而不是一个漫长的黑盒等待。

use super::*;
use mangosteen_device::remote::SshTarget;

use crate::deployment::DeviceTarget;

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

    /// 解析部署目标节点：已保存设备优先，其次云厂商创建。
    ///
    /// 规则（与 UI 的确认弹窗一致）：
    ///
    /// 1. `req.device_id` 有值 → 用这台设备，**绝不创建实例**；
    /// 2. 用户显式选了云规格（`cloud_spec`）→ 走云厂商；
    /// 3. 设备列表非空 → 默认第一台（用户「添加了机器就希望用它」）；
    /// 4. 否则走云厂商。
    pub(crate) fn resolve_device(&self, req: &DeploymentRequest) -> Option<DeviceTarget> {
        let find = |id: &str| -> Option<DeviceTarget> {
            self.config
                .remote_devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| DeviceTarget::from_config(d, &self.config.cloud.remote_workdir))
        };
        if let Some(id) = req.device_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            return find(id);
        }
        if req.cloud_spec.is_some() {
            return None;
        }
        self.config
            .remote_devices
            .first()
            .map(|d| DeviceTarget::from_config(d, &self.config.cloud.remote_workdir))
    }

    /// 执行云端/远端部署的**设备配置阶段**。
    ///
    /// `why_cloud` 说明「为什么走云端」，用于无可用服务商时给出准确原因。
    pub(crate) fn prepare_cloud(
        &mut self,
        evaluated: &Evaluated,
        cloud_plan: mangosteen_core::plan::CloudPlan,
        trail: Vec<String>,
        why_cloud: &str,
        device: Option<DeviceTarget>,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedDeployment> {
        use crate::progress::{DeployStage, StageOutcome};

        // 云端必须在远端把权重拿到，否则会建出一台「实例就绪但没有推理服务」
        // 的机器——用户看到部署成功、对话却 502，而且还在按小时计费。
        if cloud_plan.download_url.is_none() && cloud_plan.remote_model.is_none() {
            return Err(MnError::bad_request(format!(
                "无法在远端获取模型 {}：没有可用的下载地址。\
                 云端部署要求模型来自可下载的源（ModelScope / HuggingFace）；\
                 若是本地自有文件，请把它打进镜像。",
                evaluated.spec.name
            )));
        }

        let required = self.assessor.cloud_vram_requirement(
            &evaluated.spec,
            evaluated.assessment.context_len,
            evaluated.assessment.kv_cache,
            evaluated.assessment.batch_size,
        );

        let id = self.new_deployment_id(evaluated);
        let mut plan = self.scheduler.assemble_plan(
            id.clone(),
            evaluated,
            &Decision::Cloud {
                plan: Box::new(cloud_plan.clone()),
                reason: trail.last().cloned().unwrap_or_default(),
            },
            trail,
        )?;

        // 目标节点信息先落台账（此时还没连上，先记账再动手）
        if let (Some(c), Some(d)) = (plan.cloud.as_mut(), device.as_ref()) {
            c.device_id = Some(d.device.id.clone());
            c.node_ip = Some(d.device.host.clone());
            c.node_ssh_port = Some(d.device.port);
            c.node_ssh_user = Some(d.device.user.clone());
            c.provider = "ssh".to_string();
        }
        self.state
            .upsert(DeploymentRecord::from_plan(plan.clone()))?;
        self.state
            .set_status(&id, DeploymentStatus::Provisioning, None)?;

        sink(
            DeployStage::Selecting,
            StageOutcome::Done,
            match &device {
                Some(d) => format!(
                    "已选择设备：{}@{}:{} · 工作目录 {}",
                    d.device.user, d.device.host, d.device.port, d.workdir
                ),
                None => format!("已选择云端节点（显存 ≥ {}）", mangosteen_platform::fs::human_bytes(required)),
            },
        );
        sink(
            DeployStage::Configuring,
            StageOutcome::Running,
            "正在完成设备配置：连接节点、准备推理引擎、下载模型、建立隧道".to_string(),
        );

        // 远端下载进度 → 阶段说明（回调来自轮询线程，只做内存写入）
        let total = cloud_plan.download_bytes;
        let progress = |done: u64, expected: Option<u64>| {
            let total_text = expected
                .or(total)
                .map(mangosteen_platform::fs::human_bytes)
                .unwrap_or_else(|| "未知".to_string());
            sink(
                DeployStage::Downloading,
                StageOutcome::Running,
                format!(
                    "云端下载模型中：{} / {}",
                    mangosteen_platform::fs::human_bytes(done),
                    total_text
                ),
            );
        };

        let engine_port = cloud_plan.engine_port;
        let prepared = match device {
            Some(d) => self.prepare_device_node(&id, plan, d, &progress),
            None => self.prepare_vendor_node(&id, plan, &progress, why_cloud),
        };

        match prepared {
            Ok(p) => {
                sink(
                    DeployStage::Configuring,
                    StageOutcome::Done,
                    format!(
                        "设备已就绪：{}@{}:{} · 隧道 127.0.0.1:{} → 远端 {}",
                        p.ssh.user, p.ssh.host, p.ssh.port, p.plan.cloud.as_ref().map(|c| c.local_port).unwrap_or(0), engine_port
                    ),
                );
                Ok(PreparedDeployment::Cloud(Box::new(p)))
            }
            Err(e) => {
                self.state
                    .set_status(&id, DeploymentStatus::Failed, Some(e.to_string()))?;
                Err(e)
            }
        }
    }

    /// 自建设备：连 SSH → 准备远端资源 → 建隧道。**不创建任何云资源**。
    fn prepare_device_node(
        &mut self,
        id: &str,
        mut plan: DeploymentPlan,
        device: DeviceTarget,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> MnResult<PreparedCloud> {
        // 连通性先验：密码错/端口不通要在这里就报出来，而不是等到下载时
        mangosteen_device::remote::ssh_exec(&device.ssh, "true", 30)?;

        let cloud_plan = plan
            .cloud
            .clone()
            .ok_or_else(|| MnError::bad_request("部署方案缺少云端参数".to_string()))?;

        mangosteen_provider::orchestrator::prepare_node(
            &device.ssh,
            &device.workdir,
            &cloud_plan,
            &self.config.cloud.llama_server_url,
            Some(progress),
        )?;

        let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
            &device.ssh,
            cloud_plan.engine_port,
            true,
        );
        let local_port = self
            .device_tunnels
            .establish(&spec, Duration::from_secs(30))?
            .local_port;

        if let Some(c) = plan.cloud.as_mut() {
            c.local_port = local_port;
        }
        self.state
            .upsert(DeploymentRecord::from_plan(plan.clone()))?;

        Ok(PreparedCloud {
            id: id.to_string(),
            plan,
            ssh: device.ssh,
            workdir: device.workdir,
        })
    }

    /// 云厂商：按候选创建实例 → 准备远端资源 → 建隧道。
    fn prepare_vendor_node(
        &mut self,
        id: &str,
        mut plan: DeploymentPlan,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
        why_cloud: &str,
    ) -> MnResult<PreparedCloud> {
        if self.cloud.is_none() {
            return Err(MnError::NoViableCandidate {
                reason: format!(
                    "{why_cloud}；但既没有已保存的设备，也没有可用的云服务商。\
                     请在设置页「设备列表」添加一台机器，或检查 config.providers 的启用状态与凭证"
                ),
            });
        }

        let cloud_plan = plan
            .cloud
            .clone()
            .ok_or_else(|| MnError::bad_request("部署方案缺少云端参数".to_string()))?;
        let constraints = SelectionConstraints {
            min_vram_bytes: cloud_plan.vram_bytes_per_gpu,
            min_memory_bytes: cloud_plan.memory_bytes.max(8 << 30),
            min_disk_bytes: cloud_plan.disk_bytes.max(50 << 30),
            max_price_per_hour: cloud_plan.max_price_per_hour,
            allowed_providers: Vec::new(),
            allowed_regions: Vec::new(),
            require_stock: self.config.scheduler.require_stock,
        };

        let orchestrator = self.cloud.as_mut().expect("checked above");
        let p = orchestrator.provision_resources(&constraints, &cloud_plan, Some(progress))?;
        let instance = p
            .instance
            .ok_or_else(|| MnError::bad_request("云端编排未返回实例信息".to_string()))?;

        if let Some(c) = plan.cloud.as_mut() {
            Scheduler::apply_selection(
                c,
                &instance.provider,
                &instance.id,
                &instance.spec.gpu.model,
                instance.spec.gpu.vram_bytes,
            );
            c.local_port = p.local_port;
            c.node_ip = instance.network.ssh_host.clone();
            c.node_ssh_port = instance.network.ssh_port;
            c.node_ssh_user = Some("root".to_string());
        }
        plan.origin = ComputeOrigin::Cloud {
            provider: instance.provider.clone(),
            instance_id: instance.id.clone(),
        };
        self.state
            .upsert(DeploymentRecord::from_plan(plan.clone()))?;

        Ok(PreparedCloud {
            id: id.to_string(),
            plan,
            ssh: p.ssh,
            workdir: p.workdir,
        })
    }

    /// **启动模型阶段**：拉起引擎并等待 `/health`。
    pub fn commit_cloud(
        &mut self,
        prepared: PreparedCloud,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        let result = self.commit_cloud_inner(prepared, sink);
        if let Err(e) = &result {
            sink(
                crate::progress::DeployStage::Failed,
                crate::progress::StageOutcome::Failed,
                format!("{e}"),
            );
        }
        result
    }

    fn commit_cloud_inner(
        &mut self,
        prepared: PreparedCloud,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        let PreparedCloud {
            id,
            plan,
            ssh,
            workdir,
        } = prepared;
        let cloud_plan = plan
            .cloud
            .clone()
            .ok_or_else(|| MnError::bad_request("部署方案缺少云端参数".to_string()))?;
        let launch_config = self.cloud_launch_config(&cloud_plan)?;
        let local_port = cloud_plan.local_port;

        sink(
            DeployStage::Launching,
            StageOutcome::Running,
            format!(
                "正在启动推理引擎：{}@{}:{} · {}",
                ssh.user,
                ssh.host,
                ssh.port,
                cloud_plan.engine.engine.display_name()
            ),
        );

        let started = mangosteen_provider::orchestrator::start_node_engine(
            &ssh,
            &workdir,
            &launch_config,
        );
        if let Err(e) = started {
            self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
            return Err(e);
        }

        // 经隧道确认端到端可用。少了这一步，台账会标 Ready 而对话直接 502。
        let health = format!("http://127.0.0.1:{local_port}/health");
        let timeout = Duration::from_secs(self.config.scheduler.launch_timeout_secs);
        if let Err(e) = mangosteen_device::remote::wait_http_ready(
            &health,
            timeout,
            Duration::from_secs(2),
        ) {
            self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
            return Err(e);
        }

        self.state.set_status(&id, DeploymentStatus::Ready, None)?;
        sink(
            DeployStage::Launching,
            StageOutcome::Done,
            format!(
                "推理进程已就绪（/health 通过）· 节点 {}@{} · 隧道端口 {local_port}",
                ssh.user, ssh.host
            ),
        );
        self.state
            .get(&id)?
            .map(|r| r.plan)
            .ok_or_else(|| MnError::bad_request("台账记录丢失"))
    }

    /// 启动失败的回滚：云厂商实例必须销毁（持续计费），自建设备只停进程。
    fn rollback_failed_cloud(
        &mut self,
        id: &str,
        ssh: &SshTarget,
        workdir: &str,
        origin: &ComputeOrigin,
        err: &MnError,
    ) {
        // 先停远端进程：它可能已经起来了但加载失败
        let _ = mangosteen_device::remote::stop_llama_server(ssh, workdir);
        let _ = self.device_tunnels.close_all();

        let ComputeOrigin::Cloud { provider, instance_id } = origin;
        // 只有云厂商创建的实例才销毁；用户自己的机器不能关机
        let vendor_managed = self
            .state
            .get(id)
            .ok()
            .flatten()
            .and_then(|r| r.plan.cloud)
            .map(|c| c.device_id.is_none())
            .unwrap_or(false);
        if vendor_managed {
            if let Some(o) = self.cloud.as_mut() {
                let errors = o.teardown(provider, instance_id);
                if !errors.is_empty() {
                    let _ = self.state.set_status(
                        id,
                        DeploymentStatus::Failed,
                        Some(format!(
                            "{err}；且实例 {instance_id} 可能仍在计费：{}",
                            errors.join("；")
                        )),
                    );
                    return;
                }
            }
        }
        let _ = self
            .state
            .set_status(id, DeploymentStatus::Failed, Some(err.to_string()));
    }
}
