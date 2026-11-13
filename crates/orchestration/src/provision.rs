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
use crate::perf::DeployPerfTrace;

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
        // ## 选择优先级（用户明确要求的原则，三者严格互斥）
        //
        // 1. `cloud_spec` 有值 → 用户选了 GPU 规格，**按规格创建新实例**
        //    （停机同型号实例由后端按型号自动开机复用）。此时绝不能落回
        //    自定义节点或运行中实例——历史上正是 device_id 检查在前、
        //    cloud_spec 在后，导致「选了 P40 规格却连到自定义节点」。
        // 2. `reuse_instance_id` 有值 → 复用指定的运行中实例。
        // 3. `device_id` 有值 → 用这台自定义节点，绝不创建实例。
        // 4. 都没有 → 按 target 决定是否兜底到第一台已登记设备。
        let find = |id: &str| -> Option<DeviceTarget> {
            let cfg = self.config();
            cfg.remote_devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| DeviceTarget::from_config(d, &cfg.cloud.remote_workdir))
        };
        // 1. GPU 规格优先：走云厂商创建（复用由 provision_resources 内部处理）
        //
        // `cloud_spec`（完整规格 ID）与 `gpu_model`（仅 GPU 型号）都表示
        // 「用户明确要按 GPU 选点创建」——CLI 的 `--gpu-model 3080Ti` 只设置
        // gpu_model 而不设置 cloud_spec，历史上漏查 gpu_model 导致
        // `deploy --gpu-model X` 被兜底到第一台自定义节点（实测报
        // `Could not resolve hostname 1.1.1.1.1`）。
        if req.cloud_spec.is_some()
            || req
                .gpu_model
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
        {
            return None;
        }
        // 2. 复用指定实例：与设备列表无关（详见下方历史注释）
        //
        // 复用语义已经指明「挂到哪台厂商实例」，与设备列表无关。
        if req
            .reuse_instance_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
        {
            return None;
        }
        // 3. 显式指定自定义节点
        if let Some(id) = req
            .device_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return find(id);
        }
        // ## 明确要云厂商时同样不能兜底到「第一个已保存设备」
        //
        // `target=Cloud` 表示调用方已经决定创建/复用厂商实例（部署页
        // 「部署」按钮的默认路径）。此时若配置里恰好登记过任意一台
        // 自定义节点，旧兜底会把请求劫持成「部署到那台机器」——实测
        // 表现为：新实例已创建成功，部署却去连一台密码为空的旧设备，
        // 报「自定义节点未提供登录密码」，而新实例成为无人认领的
        // 计费孤儿。设备兜底只对「本地优先、跑不动才找已登记机器」
        // 的 Auto/Local 请求生效。
        if req.target == crate::ComputeTarget::Cloud {
            return None;
        }
        let cfg = self.config();
        cfg.remote_devices
            .first()
            .map(|d| DeviceTarget::from_config(d, &cfg.cloud.remote_workdir))
    }

    /// 执行云端/远端部署的**设备配置阶段**。
    ///
    /// `why_cloud` 说明「为什么走云端」，用于无可用服务商时给出准确原因。
    pub(crate) fn prepare_cloud(
        &self,
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
        let mut plan = self.scheduler().assemble_plan(
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
            StageOutcome::Running,
            match &device {
                Some(d) => format!(
                    "进入节点选择：评估资源需求已确定，等待节点连接：{}:{}",
                    d.device.host, d.device.port
                ),
                None => format!(
                    "进入节点选择：评估资源需求已确定，等待创建满足显存 ≥ {} 的 GPU 节点",
                    mangosteen_platform::fs::human_bytes(required)
                ),
            },
        );
        // 远端节点配置和模型下载必须在对应的节点创建阶段完成后才开始。
        // 不在这里提前写 Downloading，避免 UI 在实例创建期间跳过配置阶段。

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

        let prepared = match device {
            Some(d) => self.prepare_device_node(&id, plan, d, &progress, sink),
            None => self.prepare_vendor_node(&id, plan, &progress, why_cloud, sink),
        };

        match prepared {
            Ok(p) => {
                sink(
                    DeployStage::Selecting,
                    StageOutcome::Done,
                    format!("节点创建完成，节点 IP：{}", p.ssh.host),
                );
                sink(
                    DeployStage::Configuring,
                    StageOutcome::Done,
                    format!("节点配置完成：{}@{}:{}", p.ssh.user, p.ssh.host, p.ssh.port),
                );
                Ok(PreparedDeployment::Cloud(Box::new(p)))
            }
            Err(e) => {
                // 失败也**不销毁**实例：节点配置/下载失败往往只是网络或
                // 一次性错误，机器上已装好的环境仍然值钱。停机能立即止住
                // GPU 计费，同时保留现场供用户重试。
                self.park_failed_instance(&id, &e);
                Err(e)
            }
        }
    }

    /// 部署失败时的「停放」：停推理进程 + 关隧道 + 实例关机（不销毁）。
    ///
    /// ## 为什么统一用它取代销毁
    ///
    /// 1. 失败原因多为可恢复（网络抖动、镜像内依赖缺失、参数不兼容），
    ///    把机器留着，用户修完配置点「部署」就能复用同一实例；
    /// 2. 关机已能停止 GPU 计费，不销毁不会造成持续费用；
    /// 3. 真正销毁交给系统按策略回收，或在删除自定义节点时处理。
    ///
    /// 自定义节点是用户自己的机器，**只停进程，不关机**。
    fn park_failed_instance(&self, id: &str, err: &MnError) {
        let Some(rec) = self.state.get(id).ok().flatten() else {
            return;
        };
        let Some(cloud) = rec.plan.cloud.clone() else {
            return;
        };
        let ComputeOrigin::Cloud {
            provider,
            instance_id,
        } = &rec.plan.origin;
        // 「是不是厂商实例」只看 provider + instance_id 是否都有值：
        // `device_id` 只表示用户给这台机器起过名字，与托管归属无关。
        // 详见 `rollback_failed_cloud` 里的同一判据说明。
        let vendor_managed =
            !provider.trim().is_empty() && !instance_id.trim().is_empty();
        // 共享节点保护：用户复用的实例、或同机多模型的自建设备上，
        // 其它模型的推理可能正在运行。此时绝不能 stop_service（会停掉
        // 整机全部 llama-server 并关机），只能保留实例运行状态。
        let shared_node = self.has_other_active_on_node(id, &rec);

        let mut note = err.to_string();
        if vendor_managed
            && !shared_node
            && !provider.trim().is_empty()
            && !instance_id.trim().is_empty()
        {
            if let Some(o) = self.cloud().as_mut() {
                // 先按端口停本部署的进程（可能已起来），再关机。
                // 绝不无差别杀：共享节点保护已在上面判过，但同端口精确
                // 停止是双保险，避免误伤同实例其它模型。
                let _ = o.stop_service(provider, instance_id, cloud.engine_port);
                if let Err(e) = o.power_off(provider, instance_id) {
                    note = format!("{err}；且实例 {instance_id} 关机失败，可能仍在计费：{e}");
                } else {
                    note = format!("{err}；实例 {instance_id} 已停止保存，可再次部署复用");
                }
            }
        } else if vendor_managed && shared_node {
            note = format!("{err}；节点上仍有其它模型的部署在运行，实例保持开机不受影响");
        }
        // 隧道只关本次部署自己的（按 node_ip + engine_port 定位）；
        // 拿不到节点信息时保留隧道不动，绝不 close_all 误伤其它模型。
        if let Some(host) = cloud.node_ip.clone() {
            let ssh = SshTarget {
                host,
                port: cloud.node_ssh_port.unwrap_or(22),
                user: cloud.node_ssh_user.clone().unwrap_or_else(|| "root".into()),
                password: None,
                identity_file: None,
            };
            let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
                &ssh,
                cloud.engine_port,
                false,
                Some(cloud.engine_port),
            );
            if cloud.device_id.is_some() {
                let _ = self.tunnels().close(&spec);
            } else if let Some(o) = self.cloud().as_mut() {
                let _ = o.tunnels_mut().close(&spec);
            }
        }
        let _ = self
            .state
            .set_status(id, DeploymentStatus::Failed, Some(note));
    }

    /// **模型下载阶段**（两条节点路径共用）：主模型 + 全部辅助模型逐个下载并校验。
    ///
    /// ## 为什么必须共用一个实现
    ///
    /// 曾经只有云厂商实例路径遍历 `auxiliary_downloads`，自定义/已保存节点
    /// 路径只下主模型就回报「下载完成」——于是同一节点部署多模态模型时，
    /// mmproj 和 MTP 从来没被下过，UI 上表现为「模型下载」一闪而过。
    /// 阶段是否执行不能取决于节点类型，所以下载清单只允许在这里定义一次。
    ///
    /// ## 每个文件的判定
    ///
    /// 交给 [`mangosteen_provider::orchestrator::download_model_node`]：真实远端
    /// 文件大小严格匹配（缺期望大小时先取源站 `Content-Length`）+ GGUF 魔数。
    /// 只有它返回成功，才认为该文件可用于加载。
    fn download_all_node_models(
        &self,
        ssh: &SshTarget,
        cloud_plan: &mangosteen_core::plan::CloudPlan,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<()> {
        use crate::progress::{DeployStage, StageOutcome};

        let hf_token = self
            .config()
            .cloud
            .hf_token
            .as_ref()
            .and_then(|token| token.resolve(mangosteen_platform::paths::real_env).ok());

        // 清单：主模型在前，辅助模型按模板顺序。索引从 1 开始，供 UI 显示
        // 「第 n/N 个文件」，让多文件模型不再表现为“只下一个就结束”。
        let mut manifest: Vec<(String, Option<String>, Option<u64>, String)> = vec![(
            cloud_plan.model_path.clone(),
            cloud_plan.download_url.clone(),
            cloud_plan.download_bytes,
            "主模型".to_string(),
        )];
        for aux in &cloud_plan.auxiliary_downloads {
            manifest.push((
                aux.path.clone(),
                Some(aux.url.clone()),
                aux.bytes,
                "辅助模型".to_string(),
            ));
        }
        let total = manifest.len();
        for (i, (path, url, bytes, kind)) in manifest.into_iter().enumerate() {
            let name = path.rsplit('/').next().unwrap_or(&path).to_string();
            sink(
                DeployStage::Downloading,
                StageOutcome::Running,
                format!(
                    "[{}/{total}] {kind} {name}：校验远端文件（不存在则下载）",
                    i + 1
                ),
            );
            sink(
                DeployStage::Downloading,
                StageOutcome::Running,
                format!(
                    "[{}/{total}] {kind} {name}：准备下载 {}",
                    i + 1,
                    url.as_deref().unwrap_or("（无地址）")
                ),
            );
            let file_plan = mangosteen_core::plan::CloudPlan {
                model_path: path.clone(),
                download_url: url,
                download_bytes: bytes,
                ..cloud_plan.clone()
            };
            // 字节进度要**同时**写进阶段输出。
            //
            // `progress` 只更新 pull job 的进度数字，不进 `deploy_trail`；
            // 而前端「阶段处理输出」面板读的是 trail。结果是一个 16 GB 的
            // 辅助模型下载几十分钟期间，面板永远停在上一条「校验远端文件」
            // 上不动——用户看到的就是「卡在等待阶段输出」。
            //
            // 这里按 5% 粒度节流上报：既能让用户看到「在下、下了多少」，
            // 又不会因为每秒几十次回调把 trail 撑爆（trail 是全量保留的）。
            let seen_pct = std::sync::atomic::AtomicU64::new(0);
            // 注意命名：外层 `total` 是**文件总数**（用于 [i/N]），
            // 闭包参数是**当前文件的字节总量**。两者不能同名，否则
            // 格式化里 {total} 会取到字节数。
            let staged = |done: u64, grand: Option<u64>| {
                progress(done, grand);
                // 没有总大小（源站不返回 Content-Length，辅助文件常见）时
                // 也必须上报，否则整个下载期间 trail 一条新事件都没有，
                // 前端就停在「等待阶段输出」。按 64 MiB 粒度报增量。
                let Some(grand) = grand.filter(|t| *t > 0) else {
                    let step = done / (64 << 20);
                    let prev = seen_pct.load(std::sync::atomic::Ordering::Relaxed);
                    if step > prev
                        && seen_pct
                            .compare_exchange(
                                prev,
                                step,
                                std::sync::atomic::Ordering::Relaxed,
                                std::sync::atomic::Ordering::Relaxed,
                            )
                            .is_ok()
                    {
                        sink(
                            DeployStage::Downloading,
                            StageOutcome::Running,
                            format!(
                                "[{}/{total}] {kind} {name}：已下载 {}",
                                i + 1,
                                mangosteen_platform::fs::human_bytes(done)
                            ),
                        );
                    }
                    return;
                };
                let pct = done.saturating_mul(100) / grand;
                let bucket = pct / 5 * 5;
                let prev = seen_pct.load(std::sync::atomic::Ordering::Relaxed);
                if bucket > prev && bucket <= 100 {
                    if seen_pct
                        .compare_exchange(
                            prev,
                            bucket,
                            std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        sink(
                            DeployStage::Downloading,
                            StageOutcome::Running,
                            format!(
                                "[{}/{total}] {kind} {name}：{}%（{} / {}）",
                                i + 1,
                                bucket,
                                mangosteen_platform::fs::human_bytes(done),
                                mangosteen_platform::fs::human_bytes(grand)
                            ),
                        );
                    }
                }
            };
            mangosteen_provider::orchestrator::download_model_node(
                &ssh,
                &file_plan,
                hf_token.as_deref(),
                Some(&staged),
                None,
            )?;
            let size = mangosteen_device::remote::remote_file_size(ssh, &path)?;
            sink(
                DeployStage::Downloading,
                StageOutcome::Running,
                format!(
                    "[{}/{total}] {kind} {name}：已就绪（{}）",
                    i + 1,
                    mangosteen_platform::fs::human_bytes(size)
                ),
            );
        }

        // 终检：不读 `.download.ok`，也不信任务表状态，只看远端真实文件。
        // 任一文件缺失/为空都禁止进入模型加载。
        for path in std::iter::once(cloud_plan.model_path.clone()).chain(
            cloud_plan
                .auxiliary_downloads
                .iter()
                .map(|a| a.path.clone()),
        ) {
            let size = mangosteen_device::remote::remote_file_size(ssh, &path)?;
            if size == 0 {
                return Err(MnError::Network {
                    stage: "模型下载".to_string(),
                    reason: format!("模板声明的远端模型文件不存在或为空：{path}"),
                });
            }
        }
        sink(
            DeployStage::Downloading,
            StageOutcome::Done,
            format!(
                "模型下载完成：共 {total} 个文件（主模型 + {} 个辅助模型）均已校验",
                cloud_plan.auxiliary_downloads.len()
            ),
        );
        Ok(())
    }

    /// 自建设备：连 SSH → 准备远端资源 → 建隧道。**不创建任何云资源**。
    fn prepare_device_node(
        &self,
        id: &str,
        mut plan: DeploymentPlan,
        device: DeviceTarget,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedCloud> {
        // 自定义节点不重置远端密码，但节点配置阶段仍必须完成 SSH 免密：
        // 先用用户保存的原始密码安装公钥，确认密钥登录成功后，后续
        // 依赖安装、引擎准备、模型下载和隧道全部改用密钥认证。
        let ssh = mangosteen_provider::orchestrator::configure_existing_ssh_access(&device.ssh)?;

        let cloud_plan = plan
            .cloud
            .clone()
            .ok_or_else(|| MnError::bad_request("部署方案缺少云端参数".to_string()))?;

        sink(
            crate::progress::DeployStage::Selecting,
            crate::progress::StageOutcome::Done,
            format!("节点已连接，节点 IP：{}", device.ssh.host),
        );
        sink(
            crate::progress::DeployStage::Configuring,
            crate::progress::StageOutcome::Running,
            "正在配置目标节点的运行环境".to_string(),
        );
        mangosteen_provider::orchestrator::configure_node(
            &ssh,
            &device.workdir,
            &self.config().cloud.llama_server_url,
        )?;
        // 节点配置完成后才开始模型下载。
        sink(
            crate::progress::DeployStage::Configuring,
            crate::progress::StageOutcome::Done,
            format!("节点配置完成：{}", device.ssh.host),
        );
        sink(
            crate::progress::DeployStage::Downloading,
            crate::progress::StageOutcome::Running,
            "开始在目标节点下载模型".to_string(),
        );
        self.download_all_node_models(&ssh, &cloud_plan, progress, sink)?;

        // 本地端口必须从调度器的 11435-11999 区间分配（全局唯一），绝不能用远端
        // engine_port（固定 8080）：两个不同节点上的模型 engine_port 可能同为 8080，
        // 本地隧道端口若跟着远端走必然撞车（实测业务隧道混乱）。
        // 本地唯一 ↔ 远端唯一的映射关系由台账 (local_port, engine_port) 记录。
        //
        // 关键：必须先用 scheduler.allocate_local_port() 分配端口，再传给隧道规格，
        // 避免隧道管理器内部自动分配时从错误起始值（如 8080）开始。
        let local_port = self.scheduler().allocate_local_port();
        let spec =
            mangosteen_provider::orchestrator::tunnel_spec_for(&ssh, cloud_plan.engine_port, true, Some(local_port));
        sink(
            crate::progress::DeployStage::Selecting,
            crate::progress::StageOutcome::Running,
            format!(
                "正在建立 SSH 隧道：{}@{}:{} → 本地端口映射（本地端口 {local_port}，最长等待 15 秒）",
                ssh.user, ssh.host, ssh.port
            ),
        );
        let local_port = self
            .tunnels()
            .establish(&spec, Duration::from_secs(15))?
            .local_port;
        sink(
            crate::progress::DeployStage::Selecting,
            crate::progress::StageOutcome::Done,
            format!("SSH 隧道已建立：127.0.0.1:{local_port} → {}:{} · 实例 {}", local_port, cloud_plan.engine_port, id),
        );

        if let Some(c) = plan.cloud.as_mut() {
            c.local_port = local_port;
        }
        self.state.update_plan(&id, plan.clone())?;

        Ok(PreparedCloud {
            id: id.to_string(),
            plan,
            ssh,
            workdir: device.workdir,
        })
    }

    /// 云厂商：按候选创建实例 → 准备远端资源 → 建隧道。
    fn prepare_vendor_node(
        &self,
        id: &str,
        mut plan: DeploymentPlan,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
        why_cloud: &str,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<PreparedCloud> {
        if self.cloud().is_none() {
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
            require_stock: self.config().scheduler.require_stock,
        };

        // provision_resources 内部会等待实例 Running，并确认正式 SSH 地址、
        // 密码认证和 ssh-copy-id 密钥登录。此期间仍属于“GPU 节点选择”，
        // 不能提前进入节点配置；只有 provision_resources 成功返回后，才有
        // 可靠 SSH 登录方式可以开始配置环境。
        sink(
            crate::progress::DeployStage::Selecting,
            crate::progress::StageOutcome::Running,
            "GPU 节点创建中：等待实例 Running、SSH 地址和登录方式确认".to_string(),
        );
        let mut perf = DeployPerfTrace::new(id);
        perf.enter("实例创建(等待Running+SSH确认)");
        perf.note(&format!(
            "约束: 显存≥{:.1}GiB 内存≥{:.1}GiB 磁盘≥{:.1}GiB",
            constraints.min_vram_bytes as f64 / (1 << 30) as f64,
            constraints.min_memory_bytes as f64 / (1 << 30) as f64,
            constraints.min_disk_bytes as f64 / (1 << 30) as f64
        ));
        let mut cloud = self.cloud();
        let orchestrator = cloud.as_mut().expect("checked above");
        // 预分配本地端口，避免网关重启后从错误起始值（如 8080）开始分配
        let local_port = self.scheduler().allocate_local_port();
        let p = orchestrator.provision_resources(&constraints, &cloud_plan, Some(progress), local_port)?;
        let instance = p
            .instance
            .clone()
            .ok_or_else(|| MnError::bad_request("云端编排未返回实例信息".to_string()))?;
        let node_ip = instance
            .network
            .ssh_host
            .clone()
            .unwrap_or_else(|| "未知 IP".to_string());
        let node_ssh_port = instance.network.ssh_port.unwrap_or(22);
        perf.note(&format!("实例已 Running: {}:{} · {}", node_ip, node_ssh_port, instance.id));
        perf.leave("实例创建完成");
        perf.enter("节点配置(依赖安装+llama-server下载)");
        let instance = p
            .instance
            .clone()
            .ok_or_else(|| MnError::bad_request("云端编排未返回实例信息".to_string()))?;
        let node_ip = instance
            .network
            .ssh_host
            .clone()
            .unwrap_or_else(|| "未知 IP".to_string());
        let node_ssh_port = instance.network.ssh_port.unwrap_or(22);
        // provider 已等待实例进入 Running；先把正式 IP 告知 UI，再进行节点配置。
        sink(
            crate::progress::DeployStage::Selecting,
            crate::progress::StageOutcome::Done,
            format!(
                "GPU 节点实例已创建并运行：{}:{} · {}",
                node_ip, node_ssh_port, instance.id
            ),
        );
        sink(
            crate::progress::DeployStage::Configuring,
            crate::progress::StageOutcome::Running,
            format!(
                "正在配置节点 {}:{}：准备推理框架与运行环境",
                node_ip, node_ssh_port
            ),
        );

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
        self.state.update_plan(&id, plan.clone())?;
        sink(
            crate::progress::DeployStage::Selecting,
            crate::progress::StageOutcome::Done,
            format!(
                "GPU 节点创建完成，SSH 登录已确认：{}:{} · 实例 {}",
                node_ip, node_ssh_port, instance.id
            ),
        );
        sink(
            crate::progress::DeployStage::Configuring,
            crate::progress::StageOutcome::Running,
            format!(
                "进入节点配置环境：{}:{}，开始 SSH 密码与免密登录配置",
                node_ip, node_ssh_port
            ),
        );
        // 直接复用 `provision_resources` 已经建立并验证过的 SSH 通道。
        //
        // 这里曾调用 `configure_ssh_access(&instance)` 重新走一遍
        // 「重置密码 → 装公钥 → 确认密钥登录」。那是纯粹的重复劳动，而且
        // 有害：配置里若带 `ssh_password`，这一遍会**再次重置 root 密码**，
        // 前面刚确认可用的密钥信任关系随之作废，紧随其后的依赖安装、模型
        // 下载全部连接失败——报错还会被包装成「隧道建立前退出」。
        //
        // `ProvisionedResources.ssh` 正是 `ensure_key_target` 的产物
        // （已验证 `identity_file` 可登录），这里用它就是「一次验证、处处复用」。
        let ssh = p.ssh.clone();
        // 节点配置内部严格执行：依赖安装与验证 → llama-server 下载与可执行性验证。
        // 只有整个函数成功返回，才允许推进到模型下载。
        mangosteen_provider::orchestrator::configure_node(
            &ssh,
            &p.workdir,
            &self.config().cloud.llama_server_url,
        )?;
        perf.leave("节点配置完成");
        sink(
            crate::progress::DeployStage::Configuring,
            crate::progress::StageOutcome::Done,
            format!(
                "节点配置完成：依赖已验证，llama-server 可执行程序已确认：{}:{}",
                node_ip, node_ssh_port
            ),
        );
        sink(
            crate::progress::DeployStage::Downloading,
            crate::progress::StageOutcome::Running,
            format!(
                "llama-server 配置完成，立即进入模型下载：{}",
                cloud_plan.model_path
            ),
        );
        perf.enter("模型下载");
        self.download_all_node_models(&ssh, &cloud_plan, progress, sink)?;
        perf.leave("模型下载完成");
        perf.finish(true);

        // 模型量化当前暂不激活：不执行任何量化处理，也不把阶段标记为
        // Skipped/Done；启动链路直接使用模板声明的 GGUF 文件。
        Ok(PreparedCloud {
            id: id.to_string(),
            plan,
            ssh,
            workdir: p.workdir,
        })
    }

    /// **启动模型阶段**：拉起引擎并等待 `/health`。
    pub fn commit_cloud(
        &self,
        prepared: PreparedCloud,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        // 性能统计：启动模型阶段全程计时
        let mut perf = DeployPerfTrace::new(&prepared.id);
        perf.enter("启动模型(拉引擎+健康检查)");
        let result = self.commit_cloud_inner(prepared, sink);
        perf.leave("启动模型完成");
        match &result {
            Ok(_) => perf.finish(true),
            Err(e) => {
                perf.note(&format!("启动失败: {e}"));
                perf.finish(false);
            }
        }
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
        &self,
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
        // 性能统计：启动模型子阶段全程计时
        let mut perf = DeployPerfTrace::new(&id);
        //
        // 云厂商实例在创建时已由 `SelectionConstraints.min_vram_bytes` 按规格
        // 保证显存，不需要也不应该再走一次 SSH 查询——SSH 一时不可用
        // （实例刚开机、网络抖动、端口转发未就绪）会让整个启动被误判为失败。
        //
        // 自定义节点没有经过规格选择，8GB 卡也可能被拿来跑 27B 全量 GPU 模型，
        // 最终只在 llama-server 内部 CUDA OOM，表现成 120 秒 connection reset。
        // 因此这里保留预检，但**查询失败只记录并跳过**，绝不阻断启动：
        // 真正的连接问题会在后续起引擎时以准确错误暴露。
        if cloud_plan.device_id.is_some() {
            match mangosteen_device::remote::remote_gpu_memory(&ssh) {
                Ok(Some((total_vram, free_vram))) => {
                    let required = cloud_plan.vram_bytes_per_gpu;
                    let safety = 512u64 * 1024 * 1024;
                    if free_vram < required.saturating_add(safety) {
                        let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                        let e = MnError::bad_request(format!(
                            "远端 GPU 显存不足，禁止启动推理引擎：总显存 {:.2} GiB，当前空闲 {:.2} GiB，预计至少需要 {:.2} GiB（含 512 MiB 运行余量）。请更换显存更大的节点，或降低量化/上下文长度；本次未启动 llama-server。",
                            gib(total_vram),
                            gib(free_vram),
                            gib(required.saturating_add(safety))
                        ));
                        self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
                        return Err(e);
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("[deploy] 显存预检跳过（查询失败，不阻断启动）：{e}");
                }
            }
        }
        // 启动前先确认远端确实有模型文件。下载阶段若因残留标记或命名不一致
        // 被跳过，这里要给出指向「模型下载」的明确结论，而不是让
        // llama-server 抛出难以定位的 GGUF 打不开错误。
        let model_size =
            mangosteen_device::remote::remote_file_size(&ssh, &cloud_plan.engine.model_path)?;
        if model_size == 0 {
            let e = MnError::Network {
                stage: "启动云端推理服务".to_string(),
                reason: format!(
                    "远端模型文件不存在：{}。请在「模型下载」阶段确认文件已完整落盘",
                    cloud_plan.engine.model_path
                ),
            };
            self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
            return Err(e);
        }
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

        perf.enter("拉起llama-server进程");
        let started =
            mangosteen_provider::orchestrator::start_node_engine(&ssh, &workdir, &launch_config);
        perf.leave("llama-server启动完成");
        if let Err(e) = started {
            self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
            return Err(e);
        }

        // 经隧道确认端到端可用。少了这一步，台账会标 Ready 而对话直接 502。
        //
        // prepare 阶段建立的是「远端端口尚未监听也允许存在」的引导隧道。
        // 启动模型后必须关闭并严格重建一次，否则 SSH 进程可能看似存在，
        // 但本地端口实际已经断开，最终只得到 curl (7) connection refused。
        perf.enter("重建隧道");
        let local_port = if cloud_plan.device_id.is_some() {
            let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
                &ssh,
                cloud_plan.engine_port,
                false,
                Some(cloud_plan.engine_port),
            );
            let _ = self.tunnels().close(&spec);
            self.tunnels()
                .establish(&spec, Duration::from_secs(30))?
                .local_port
        } else {
            let mut cloud = self.cloud();
            let o = cloud.as_mut().ok_or_else(|| {
                MnError::bad_request("未配置云端能力，无法重建模型隧道".to_string())
            })?;
            o.ensure_tunnel_to(&ssh, cloud_plan.engine_port)?
        };
        perf.leave("隧道重建完成");
        if let Some(c) = self
            .state
            .get(&id)?
            .and_then(|r| r.plan.cloud)
            .map(|mut c| {
                c.local_port = local_port;
                c
            })
        {
            let mut updated = self
                .state
                .get(&id)?
                .ok_or_else(|| MnError::bad_request("启动模型前台账记录已丢失".to_string()))?;
            updated.plan.cloud = Some(c);
            self.state.upsert(updated)?;
        }
        let health = format!("http://127.0.0.1:{local_port}/health");
        let timeout = Duration::from_secs(self.config().scheduler.launch_timeout_secs);
        perf.enter("健康检查(/health就绪)");
        if let Err(e) =
            mangosteen_device::remote::wait_http_ready(&health, timeout, Duration::from_secs(2))
        {
            perf.leave("健康检查失败");
            let diagnostic = mangosteen_device::remote::llama_server_start_diagnostics(
                &ssh,
                &workdir,
                cloud_plan.engine_port,
            )
            .unwrap_or_else(|diag_err| format!("远端诊断获取失败：{diag_err}"));
            let e = MnError::Network {
                stage: "等待云端推理服务".to_string(),
                reason: format!("{e}\n远端启动诊断：\n{diagnostic}"),
            };
            self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
            return Err(e);
        }
        perf.leave("健康检查通过");
        // 性能统计：启动阶段完成
        perf.finish(true);

        self.state.set_status(&id, DeploymentStatus::Ready, None)?;
        sink(
            DeployStage::Launching,
            StageOutcome::Done,
            format!(
                "推理进程已就绪（/health 通过）· 节点 {}@{}",
                ssh.user, ssh.host
            ),
        );
        // 启动阶段通过引导隧道完成健康检查；此处确认同一条本地网关到
        // 远端模型通道仍然可用，并将其作为独立的业务阶段上报。
        sink(
            DeployStage::Tunneling,
            StageOutcome::Running,
            format!("正在建立本地网关到远端模型的隧道：127.0.0.1:{local_port}"),
        );
        if let Err(e) = mangosteen_device::remote::wait_http_ready(
            &health,
            Duration::from_secs(10),
            Duration::from_secs(1),
        ) {
            self.rollback_failed_cloud(&id, &ssh, &workdir, &plan.origin, &e);
            return Err(e);
        }
        sink(
            DeployStage::Tunneling,
            StageOutcome::Done,
            format!("本地网关已接通远端模型：127.0.0.1:{local_port}"),
        );
        // 隧道建立完成后再落一次节点信息：此后 UI 展示的 IP／端口
        // 就是「当前真正可连的地址」，而不是 prepare 阶段的占位值。
        if let Some(mut rec) = self.state.get(&id)? {
            if let Some(c) = rec.plan.cloud.as_mut() {
                c.node_ip = Some(ssh.host.clone());
                c.node_ssh_port = Some(ssh.port);
                c.node_ssh_user = Some(ssh.user.clone());
                c.local_port = local_port;
            }
            self.state.upsert(rec)?;
        }
        self.state
            .get(&id)?
            .map(|r| r.plan)
            .ok_or_else(|| MnError::bad_request("台账记录丢失"))
    }

    /// 启动失败的回滚：**停实例不销毁**，保留现场供重试。
    ///
    /// 启动失败（模型文件缺失、参数不兼容、动态库缺失）通常是配置问题，
    /// 机器上的引擎与权重都还有用。关机停止 GPU 计费即可，销毁等于把
    /// 下次重试的成本全部重置。自定义节点只停进程，机器不动。
    fn rollback_failed_cloud(
        &self,
        id: &str,
        ssh: &SshTarget,
        workdir: &str,
        origin: &ComputeOrigin,
        err: &MnError,
    ) {
        // 本次部署自己的云端参数：engine_port 是「按端口停进程 / 关隧道」的依据。
        let own = self
            .state
            .get(id)
            .ok()
            .flatten()
            .and_then(|r| r.plan.cloud);

        // 1. 只停**本次部署自己的**引擎进程。共享节点（用户复用的已有实例、
        //    或同机多模型的自建设备）上其它模型可能正在推理，不带端口的
        //    全量停止会误杀整台机器的 llama-server。
        match own.as_ref().map(|c| c.engine_port) {
            Some(port) => {
                let _ = mangosteen_device::remote::stop_llama_server_on_port(
                    ssh, workdir, Some(port),
                );
            }
            None => {
                let _ = mangosteen_device::remote::stop_llama_server(ssh, workdir);
            }
        }

        // 2. 只关本次部署自己的隧道，绝不能 close_all——那会把共享节点上
        //    其它模型的本地转发一起断掉。
        if let Some(c) = own.as_ref() {
            let spec =
                mangosteen_provider::orchestrator::tunnel_spec_for(ssh, c.engine_port, false, Some(c.engine_port));
            if c.device_id.is_some() {
                let _ = self.tunnels().close(&spec);
            } else if let Some(o) = self.cloud().as_mut() {
                let _ = o.tunnels_mut().close(&spec);
            }
        }

        let ComputeOrigin::Cloud {
            provider,
            instance_id,
        } = origin;
        // 只有「云厂商实例」才允许关机；用户自己的机器不能动。
        //
        // ## 判据为什么不能用 `device_id.is_none()`
        //
        // 曾经这么写，实测踩到：用户把厂商实例的 IP 也登记成了自定义设备
        // （用于「登陆方式」展示），于是 `device_id` 有值 → 被判成「用户
        // 自己的机器」→ **永不关机**。表现是停止服务后 GPU 仍占用 19 GB、
        // 节点持续计费，而界面显示已停。
        //
        // 「是不是厂商实例」只取决于**它是否由厂商 API 托管**，而这正是
        // `provider` 与 `instance_id` 的组合语义：两者都非空 = 台账里记着
        // 厂商与实例号，可以调 API 关机。`device_id` 只是「用户给这台机器
        // 起了个名字」，与托管归属无关。
        let vendor_managed =
            !provider.trim().is_empty() && !instance_id.trim().is_empty();
        let shared_node = self
            .state
            .get(id)
            .ok()
            .flatten()
            .map(|r| self.has_other_active_on_node(id, &r))
            .unwrap_or(true);
        let mut note = err.to_string();
        if vendor_managed && !shared_node {
            if let Some(o) = self.cloud().as_mut() {
                if let Err(e) = o.power_off(provider, instance_id) {
                    note = format!("{err}；且实例 {instance_id} 关机失败，可能仍在计费：{e}");
                } else {
                    note = format!("{err}；实例 {instance_id} 已停止保存，可再次部署复用");
                }
            }
        } else if vendor_managed && shared_node {
            note = format!("{err}；节点上仍有其它模型的部署在运行，实例保持开机不受影响");
        }
        let _ = self
            .state
            .set_status(id, DeploymentStatus::Failed, Some(note));
    }
}
