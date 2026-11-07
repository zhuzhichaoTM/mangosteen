//! 部署生命周期：停止服务（保留节点）、恢复服务、中止回滚与整体关停。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use std::time::{Duration, Instant};

use super::*;
use crate::deployment::{DeviceTarget, UnloadOutcome};

impl Orchestrator {
    /// API 网关启动后的业务自恢复：根据持久化台账恢复已部署模型的完整状态。
    ///
    /// 恢复步骤：
    /// 1. 查询云端实例实际状态（running/stopped）
    /// 2. 同步台账状态与实际状态
    /// 3. 对于应该运行的实例（厂商侧 running + 台账 ready/failed）：
    ///    - 重建 SSH 隧道
    ///    - 启动推理进程（如果进程未运行）
    ///    - 更新台账为 ready
    /// 4. 对于不应该运行的实例（厂商侧 stopped + 台账 ready）：
    ///    - 删除台账记录（停止后已删除，不应存在）
    ///
    /// 注意：由于采用方案A（停止后删除台账记录），stopped状态的记录不存在，
    /// 因此恢复机制无法恢复stopped状态的部署。只有ready/failed状态的记录会被恢复。
    ///
    /// GPU 节点和远端模型进程可能在 API 网关重启期间继续运行；本地
    /// `TunnelManager` 是进程内状态，重启后必须重新建立映射，否则台账虽是
    /// `ready`，对话仍找不到本地端口。
    pub fn restore_deployment_tunnels(&self) -> Vec<String> {
        let records = match self.state.list() {
            Ok(records) => records,
            Err(e) => return vec![format!("读取部署台账失败：{e}")],
        };
        let mut errors = Vec::new();

        for record in records {
            // 只处理云端部署（有 cloud plan）
            let Some(cloud_plan) = record.plan.cloud.clone() else {
                continue;
            };

            // 获取云端实例实际状态
            let instance_state = self.query_instance_state(&record);

            let id = record.plan.id.clone();
            let ComputeOrigin::Cloud {
                provider,
                instance_id,
            } = &record.plan.origin;

            // 情况1：云端实例已停止，但台账显示 ready/failed → 删除台账记录
            if matches!(instance_state, Some(mangosteen_core::cloud::InstanceState::Stopped))
                && matches!(
                    record.status,
                    DeploymentStatus::Ready | DeploymentStatus::Failed
                )
            {
                eprintln!("[restore] {id}：云端实例已停止，删除台账记录");
                if let Err(e) = self.state.remove(&id) {
                    errors.push(format!("{id}：删除台账记录失败：{e}"));
                }
                continue;
            }

            // 情况2：云端实例正在运行，台账为 ready/failed → 恢复隧道和进程
            if matches!(instance_state, Some(mangosteen_core::cloud::InstanceState::Running))
                && matches!(
                    record.status,
                    DeploymentStatus::Ready | DeploymentStatus::Failed
                )
            {
                eprintln!("[restore] {id}：云端实例正在运行，恢复隧道和推理进程");

                // 恢复隧道
                let tunnel_result = if let Some(target) = self.device_target_for(&cloud_plan) {
                    let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
                        &target,
                        cloud_plan.engine_port,
                        false,
                        Some(cloud_plan.engine_port),
                    );
                    self.tunnels()
                        .establish(&spec, Duration::from_secs(30))
                        .map(|t| t.local_port)
                } else {
                    let mut cloud = self.cloud();
                    let Some(orchestrator) = cloud.as_mut() else {
                        errors.push(format!("{id}：未配置云端能力，无法恢复隧道"));
                        continue;
                    };
                    orchestrator
                        .establish_tunnel(provider, instance_id, &cloud_plan)
                        .and_then(|base| {
                            base.rsplit(':')
                                .next()
                                .and_then(|p| p.trim_end_matches("/v1").parse::<u16>().ok())
                                .ok_or_else(|| {
                                    MnError::bad_request(format!("隧道地址无法解析：{base}"))
                                })
                        })
                };

                let local_port = match tunnel_result {
                    Ok(p) => p,
                    Err(e) => {
                        errors.push(format!("{id}：恢复隧道失败：{e}"));
                        continue;
                    }
                };

                self.scheduler().reserve_port(local_port);

                // 更新台账中的端口信息
                match self.state.get(&id) {
                    Ok(Some(mut current)) => {
                        if let Some(c) = current.plan.cloud.as_mut() {
                            c.local_port = local_port;
                        }
                        if let Err(e) = self.state.upsert(current) {
                            errors.push(format!("{id}：保存恢复端口失败：{e}"));
                        }
                    }
                    Ok(None) => errors.push(format!("{id}：台账记录在恢复期间消失")),
                    Err(e) => errors.push(format!("{id}：读取记录失败：{e}")),
                }

                // 检查并启动推理进程（如果进程未运行）
                if let Some(target) = self.device_target_for(&cloud_plan) {
                    // 自定义节点路径
                    let workdir = self.config().cloud.remote_workdir.clone();
                    let port = cloud_plan.engine_port;
                    let running = mangosteen_device::remote::llama_server_running_on_port(
                        &target, &workdir, port,
                    )
                    .unwrap_or(false);

                    if !running {
                        eprintln!("[restore] {id}：推理进程未运行，尝试启动");
                        let launch_config = match self.cloud_launch_config(&cloud_plan) {
                            Ok(cfg) => cfg,
                            Err(e) => {
                                errors.push(format!("{id}：获取启动配置失败：{e}"));
                                continue;
                            }
                        };
                        if let Err(e) = mangosteen_provider::orchestrator::start_node_engine(
                            &target,
                            &workdir,
                            &launch_config,
                        ) {
                            errors.push(format!("{id}：启动推理进程失败：{e}"));
                            continue;
                        }
                    }
                } else {
                    // 云厂商实例路径
                    let mut cloud = self.cloud();
                    if let Some(o) = cloud.as_mut() {
                        let running = o.is_service_running(provider, instance_id, cloud_plan.engine_port)
                            .unwrap_or(false);

                        if !running {
                            eprintln!("[restore] {id}：推理进程未运行，尝试启动");
                            let launch_config = match self.cloud_launch_config(&cloud_plan) {
                                Ok(cfg) => cfg,
                                Err(e) => {
                                    errors.push(format!("{id}：获取启动配置失败：{e}"));
                                    continue;
                                }
                            };
                            if let Err(e) = o.start_service(provider, instance_id, &cloud_plan, &launch_config) {
                                errors.push(format!("{id}：启动推理进程失败：{e}"));
                                continue;
                            }
                        }
                    }
                }

                // 等待健康检查通过
                let health = format!("http://127.0.0.1:{local_port}/health");
                let timeout = Duration::from_secs(self.config().scheduler.launch_timeout_secs);
                match mangosteen_device::remote::wait_http_ready(&health, timeout, Duration::from_secs(2)) {
                    Ok(_) => {
                        // 健康检查通过，更新台账为 ready
                        if let Err(e) = self.state.set_status(&id, DeploymentStatus::Ready, None) {
                            errors.push(format!("{id}：更新台账状态失败：{e}"));
                        } else {
                            eprintln!("[restore] {id}：恢复成功，状态已更新为 ready");
                        }
                    }
                    Err(e) => {
                        errors.push(format!("{id}：健康检查失败：{e}"));
                        // 健康检查失败，标记为 failed
                        let _ = self.state.set_status(
                            &id,
                            DeploymentStatus::Failed,
                            Some(format!("恢复后健康检查失败：{e}")),
                        );
                    }
                }
            }
        }
        errors
    }

    /// 查询云端实例的实际状态。
    fn query_instance_state(
        &self,
        record: &mangosteen_base::state::DeploymentRecord,
    ) -> Option<mangosteen_core::cloud::InstanceState> {
        let ComputeOrigin::Cloud {
            provider,
            instance_id,
        } = &record.plan.origin;

        if provider.trim().is_empty() || instance_id.trim().is_empty() {
            return None;
        }

        let mut cloud = self.cloud();
        let o = cloud.as_mut()?;
        let driver = o.registry().get(provider).ok()?;
        let inst = driver.describe(instance_id).ok()?;
        Some(inst.state)
    }

    /// 卸载一个部署：停止推理服务、关隧道、**停止实例**（不销毁）。
    ///
    /// ## 为什么不再销毁实例
    ///
    /// 产品要求「卸载只停不删」：机器上的权重、引擎和运行环境是重新部署时
    /// 最贵的部分（几 GB 下载 + 依赖安装）。直接销毁等于把下次部署的成本
    /// 全部重付一遍。停止实例既释放了 GPU 计费，又保留了可复用的现场。
    /// 实例的最终清理由系统按策略回收，或在用户删除自定义节点时处理。
    ///
    /// ## 与自定义节点的边界
    ///
    /// 自定义节点是用户自己的机器，**任何情况下都不替用户关机**——
    /// 那会影响用户的其它用途。只停我们拉起的推理进程。
    ///
    /// | 节点来源 | 卸载时的行为 |
    /// |---|---|
    /// | 云厂商创建的实例 | 停进程 → 关隧道 → **关机**（停 GPU 计费，实例与磁盘保留） |
    /// | 自定义节点（设置页登记） | 停进程 → 关隧道 → 机器不动 |
    ///
    /// 台账保留为 `stopped`：再次点「部署」会复用同一实例开机重启。
    pub fn unload(&self, id: &str) -> MnResult<UnloadOutcome> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;
        let cloud = record.plan.cloud.clone();
        let state = record.status;
        let shared_node = self.has_other_active_on_node(id, &record);

        // 0) 先把台账置为 `stopped`（带「正在停止」说明），再做停止动作。
        //
        // 为什么必须在停进程**之前**：停止远端进程到关隧道、写终态之间
        // 有一个秒级窗口——进程已死但本地隧道还在，后台探测（runprobe）
        // 在这个窗口里会把 `ready` 记录连续两轮探测失败后降级成
        // `failed`（「本地端口与远端引擎端口映射不存在或健康检查失败」），
        // UI 轮询拿到失败状态弹出错误提示，随后卸载收尾又把它改回
        // `stopped`——表现为「卸载成功却闪过一个报错」。
        //
        // `stopped` 是探测豁免态（用户的显式动作态），提前置位即可让
        // 探测线程跳过，消除这条竞态。若后续停止动作失败，仍会按各自
        // 的错误路径显式改回 `failed`，不影响真实故障的上报。
        if matches!(state, DeploymentStatus::Ready | DeploymentStatus::Failed) {
            let _ = self
                .state
                .set_status(id, DeploymentStatus::Stopped, Some("正在停止推理服务…".to_string()));
        }

        // 1) 先停远端推理进程：无论哪种来源，GPU 显存都该释放。
        if let Some(c) = cloud.as_ref() {
            match self.device_target_for(c) {
                Some(target) => {
                    let workdir = self.config().cloud.remote_workdir.clone();
                    let port = c.engine_port;
                    if let Err(e) = mangosteen_device::remote::stop_llama_server_on_port(
                        &target,
                        &workdir,
                        Some(port),
                    ) {
                        // 停止脚本失败不等于进程还在：SSH 抖动（连接层中断、
                        // 厂商网关限流）可能发生在「已发出 kill、确认命令
                        // 未返回」的半程。此时按实际状态判定——进程确实
                        // 不在了就视为停止成功，继续卸载；仍在运行才报错。
                        // 否则一次网络抖动会把已经成功的卸载误报成失败，
                        // 而 UI 也随之弹出多余的错误窗口。
                        let still_running =
                            mangosteen_device::remote::llama_server_running_on_port(
                                &target, &workdir, port,
                            )
                            .unwrap_or(false);
                        if still_running {
                            return Err(MnError::Network {
                                stage: "卸载（停止模型进程）".to_string(),
                                reason: format!(
                                    "模型远端端口 {port} 的 llama-server 停止失败：{e}"
                                ),
                            });
                        }
                    }
                    // 二次校验同样只作辅助证据：它的 SSH 错误不能推翻
                    // 主操作的成功结论（详见 stop_service_on_self_hosted）。
                    match mangosteen_device::remote::llama_server_running_on_port(
                        &target, &workdir, port,
                    ) {
                        Ok(true) => {
                            return Err(MnError::bad_request(format!(
                                "模型远端端口 {port} 的 llama-server 仍在运行，已阻止后续关机"
                            )));
                        }
                        // 已停止 / 校验没跑完（网络抖动）：都继续后续流程
                        Ok(false) | Err(_) => {}
                    }
                }
                None => {
                    // 云厂商实例：直接由驱动停止服务（内部走 ssh）。
                    //
                    // 方案3：停止后**等待端口确认空闲**，最多 10 秒。停止命令
                    // 返回成功不代表端口已释放（TIME_WAIT / 进程收尾），直接
                    // 关机或复用会撞 address in use。查询失败只记录不阻断：
                    // 它是辅助证据，不是主判据（见 stop_service_on_self_hosted）。
                    let ComputeOrigin::Cloud {
                        provider,
                        instance_id,
                    } = &record.plan.origin;
                    if let Some(o) = self.cloud().as_mut() {
                        if !provider.trim().is_empty() && !instance_id.trim().is_empty() {
                            let port = record
                                .plan
                                .cloud
                                .as_ref()
                                .map(|c| c.engine_port)
                                .unwrap_or(8080);
                            if let Err(e) = o.stop_service_on_port(provider, instance_id, port) {
                                // 停止命令失败不等于进程还在：SSH 抖动可能发生在
                                // 「已发出 kill、确认命令未返回」的半程（与上方
                                // 自建设备路径同一判据）。进程确实不在了就继续
                                // 卸载，否则一次网络抖动会把成功的卸载误报成失败。
                                let still_running = self
                                    .vendor_wait_target(provider, instance_id)
                                    .map(|wt| {
                                        mangosteen_device::remote::llama_server_running_on_port(
                                            &wt,
                                            &self.config().cloud.remote_workdir,
                                            port,
                                        )
                                        .unwrap_or(true) // 查询不到时按「仍在运行」保守处理
                                    })
                                    .unwrap_or(true);
                                if still_running {
                                    return Err(MnError::Network {
                                        stage: "卸载（停止模型进程）".to_string(),
                                        reason: format!("模型远端进程停止失败：{e}"),
                                    });
                                }
                                eprintln!(
                                    "[unload] 停止命令报错但进程已不在，视为停止成功：{e}"
                                );
                            }
                            // 端口空闲确认轮询（方案3）
                            let wait_target = self.vendor_wait_target(provider, instance_id);
                            let deadline = Instant::now() + Duration::from_secs(10);
                            loop {
                                let Some(wt) = wait_target.clone() else {
                                    eprintln!("[unload] 端口空闲确认跳过（无法获取 SSH 目标）");
                                    break;
                                };
                                match mangosteen_device::remote::llama_server_running_on_port(
                                    &wt,
                                    &self.config().cloud.remote_workdir,
                                    port,
                                ) {
                                    Ok(false) => break,
                                    Ok(true) if Instant::now() >= deadline => {
                                        return Err(MnError::Network {
                                            stage: "卸载（停止模型进程）".to_string(),
                                            reason: format!(
                                                "停止命令已返回，但端口 {port} 10 秒后仍有 llama-server 监听"
                                            ),
                                        });
                                    }
                                    Ok(true) => std::thread::sleep(Duration::from_millis(500)),
                                    Err(e) => {
                                        // 查询失败不推翻停止成功：网络抖动下的辅助证据
                                        eprintln!(
                                            "[unload] 端口空闲确认跳过（查询失败，不阻断）：{e}"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2) 关隧道并释放端口预留。
        //
        // 节点共享时不能 close_all：那会切断其它模型的业务转发。
        // 但**必须**关掉当前模型自己那条隧道——否则每卸载一个模型就
        // 泄漏一个 ssh 进程和一个本地端口，反复部署后端口区间被吃光，
        // 表现为「分配本地端口失败」。
        let engine_port = cloud.as_ref().map(|c| c.engine_port);
        if shared_node {
            if let Some(p) = engine_port {
                let _ = self.tunnels().close_by_remote_port(p);
                if let Some(o) = self.cloud().as_mut() {
                    let _ = o.tunnels_mut().close_by_remote_port(p);
                }
            }
        } else {
            let _ = self.tunnels().close_all();
            if let Some(o) = self.cloud().as_mut() {
                let _ = o.tunnels_mut().close_all();
            }
        }
        if let Some(port) = record.plan.local_access_port() {
            self.scheduler().release_port(port);
        }

        // 3) 云厂商实例：通过 nvidia-smi 检查是否有其他大模型进程占用GPU。
        //    - 无其他进程：直接关闭实例（不删除实例）
        //    - 有其他进程：不关闭实例，避免影响其他模型
        //    实例由云平台系统自动老化删除，或用户通过其他手段删除。
        //    节点共享时只停止当前模型进程，GPU 节点和其它模型保持运行。
        //    自建设备（device_id 非空）也是厂商托管的实例，同样允许关机——
        //    旧实现把「用户给机器起过名字」误判成「用户自己的物理机」，导致
        //    复用节点的实例永远不关机、持续计费。
        let is_custom_device = cloud
            .as_ref()
            .and_then(|c| c.device_id.as_deref())
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false);
        let keep_node = is_custom_device && shared_node;
        let mut stopped_instance = None;
        if !keep_node {
            let ComputeOrigin::Cloud {
                provider,
                instance_id,
            } = &record.plan.origin;
            let vendor_managed = !provider.trim().is_empty() && !instance_id.trim().is_empty();
            if vendor_managed {
                if let Some(o) = self.cloud().as_mut() {
                    // 检查是否有其他 GPU 进程占用
                    let has_other_processes = if let Some(c) = cloud.as_ref() {
                        if let Some(target) = self.device_target_for(c) {
                            match mangosteen_device::remote::has_gpu_processes(&target) {
                                Ok(true) => {
                                    eprintln!("[unload] GPU 上仍有其他进程运行，不关闭实例 {instance_id}");
                                    true
                                }
                                Ok(false) => {
                                    eprintln!("[unload] GPU 上无其他进程，将关闭实例 {instance_id}");
                                    false
                                }
                                Err(e) => {
                                    eprintln!("[unload] nvidia-smi 查询失败，保守处理不关闭实例：{e}");
                                    true // 查询失败时保守处理，不关闭实例
                                }
                            }
                        } else {
                            // 云厂商实例路径：通过 SSH 目标检查
                            match self.vendor_wait_target(provider, instance_id) {
                                Some(target) => {
                                    match mangosteen_device::remote::has_gpu_processes(&target) {
                                        Ok(true) => {
                                            eprintln!("[unload] GPU 上仍有其他进程运行，不关闭实例 {instance_id}");
                                            true
                                        }
                                        Ok(false) => {
                                            eprintln!("[unload] GPU 上无其他进程，将关闭实例 {instance_id}");
                                            false
                                        }
                                        Err(e) => {
                                            eprintln!("[unload] nvidia-smi 查询失败，保守处理不关闭实例：{e}");
                                            true
                                        }
                                    }
                                }
                                None => {
                                    eprintln!("[unload] 无法获取 SSH 目标，保守处理不关闭实例");
                                    true
                                }
                            }
                        }
                    } else {
                        eprintln!("[unload] 无 cloud plan，保守处理不关闭实例");
                        true
                    };

                    if !has_other_processes {
                        // 无其他进程，关闭实例（不删除）
                        eprintln!("[unload] 正在关闭实例 {instance_id}");
                        if let Err(e) = o.power_off(provider, instance_id) {
                            let _ = self.state.set_status(
                                id,
                                DeploymentStatus::Failed,
                                Some(format!("实例 {instance_id} 关机失败，可能仍在计费：{e}")),
                            );
                            return Err(MnError::Network {
                                stage: "卸载（停止实例）".to_string(),
                                reason: format!("实例 {instance_id} 关机失败，可能仍在计费：{e}"),
                            });
                        }
                        stopped_instance = Some(instance_id.clone());
                    } else {
                        // 有其他进程，不关闭实例
                        eprintln!("[unload] 实例 {instance_id} 上有其他进程，保持运行状态");
                    }
                }
            }
        }

        // 4) 删除台账记录（方案A：停止后删除记录）
        if let Err(e) = self.state.remove(id) {
            eprintln!("[unload] 删除台账记录失败：{e}");
            // 不阻断流程，台账删除失败不是致命错误
        }
        Ok(UnloadOutcome {
            stopped_instance,
            node_kept: keep_node,
        })
    }

    /// 判断该部署所在节点是否仍有其它活跃模型。
    ///
    /// 节点匹配优先使用云厂商 `(provider, instance_id)`；自定义节点使用
    /// `device_id`。`Ready` 和 `Provisioning` 都视为仍可能占用节点资源，
    /// 这样卸载一个模型时不会误关掉其它正在启动的模型。
    pub(crate) fn has_other_active_on_node(
        &self,
        current_id: &str,
        current: &mangosteen_base::state::DeploymentRecord,
    ) -> bool {
        let Ok(records) = self.state.list() else {
            // 读取失败时采取保守策略：不关机，避免误伤其它模型。
            return true;
        };
        let current_key = Self::node_key(current);
        records.into_iter().any(|other| {
            if other.plan.id == current_id || !other.status.is_active() {
                return false;
            }
            Self::node_key(&other) == current_key
        })
    }

    fn node_key(record: &mangosteen_base::state::DeploymentRecord) -> String {
        let cloud = record.plan.cloud.as_ref();
        if let Some(device_id) = cloud.and_then(|c| c.device_id.as_deref()) {
            if !device_id.trim().is_empty() {
                return format!("device:{device_id}");
            }
        }
        match &record.plan.origin {
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => format!("cloud:{provider}:{instance_id}"),
        }
    }

    /// 由云端方案反查自定义节点的 SSH 目标（云厂商实例返回 `None`）。
    fn device_target_for(
        &self,
        cloud: &mangosteen_core::plan::CloudPlan,
    ) -> Option<mangosteen_device::remote::SshTarget> {
        let device_id = cloud.device_id.as_deref()?;
        let cfg = self.config();
        cfg.remote_devices
            .iter()
            .find(|d| d.id == device_id)
            .map(|d| DeviceTarget::from_config(d, &cfg.cloud.remote_workdir).ssh)
    }

    /// 为「停止后的端口空闲确认」重建 SSH 目标（云厂商路径专用）。
    ///
    /// 复用 `CloudOrchestrator::service_target` 的取数路径：host/port 来自
    /// 厂商 `describe()`，密码按「实例自带 > 配置统一」解析。查不到 SSH
    /// 信息时返回 `None`，调用方据此跳过确认轮询——它是辅助证据，
    /// 不得因为重建失败而推翻停止动作本身的结论。
    fn vendor_wait_target(
        &self,
        provider: &str,
        instance_id: &str,
    ) -> Option<mangosteen_device::remote::SshTarget> {
        let mut cloud = self.cloud();
        let o = cloud.as_mut()?;
        o.service_target(provider, instance_id).ok()
    }

    /// 删除自定义节点前，销毁该节点上由本工具创建的云实例。
    ///
    /// 自定义节点本身是用户的机器，删除节点**不代表**要销毁实例；
    /// 但如果这台机器上跑着我们用云 API 创建过的实例（例如用户先由
    /// 本工具开机器、再把它的 IP 登记成自定义节点），删除节点时就必须
    /// 调云端接口把实例释放掉，否则会一直计费。
    pub fn destroy_instances_bound_to_device(
        &self,
        device_id: &str,
        host: &str,
    ) -> MnResult<Vec<String>> {
        let records = self.state.list()?;
        let mut stopped = Vec::new();
        for rec in records {
            let Some(c) = rec.plan.cloud.as_ref() else {
                continue;
            };
            let bound =
                c.device_id.as_deref() == Some(device_id) || c.node_ip.as_deref() == Some(host);
            if !bound {
                continue;
            }
            let ComputeOrigin::Cloud {
                provider,
                instance_id,
            } = &rec.plan.origin;
            if !provider.trim().is_empty() && !instance_id.trim().is_empty() {
                let mut cloud = self.cloud();
                let Some(o) = cloud.as_mut() else {
                    continue;
                };
                let engine_port = rec
                    .plan
                    .cloud
                    .as_ref()
                    .map(|c| c.engine_port)
                    .unwrap_or(8080);
                if let Err(e) = o.stop_service_on_port(provider, instance_id, engine_port) {
                    return Err(MnError::Network {
                        stage: "删除节点并停止实例".to_string(),
                        reason: format!("实例 {instance_id} 可能仍在计费：{e}"),
                    });
                }
                stopped.push(instance_id.clone());
            }
            let _ = self.tunnels().close_all();
            if let Some(port) = rec.plan.local_access_port() {
                self.scheduler().release_port(port);
            }
            self.state.remove(&rec.plan.id)?;
        }
        Ok(stopped)
    }

    /// 停止部署（释放本地进程 / 销毁云端实例）。
    ///
    /// 这是「实例删除由系统自行处理」的最终落点：删除节点、重复收敛与
    /// 网关关闭等系统侧清理走这里；面向用户的「卸载/取消」只停不删。
    pub fn stop(&self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;

        match &record.plan.origin {
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                if let Some(o) = self.cloud().as_mut() {
                    let errors = o.teardown(provider, instance_id);
                    if !errors.is_empty() {
                        return Err(MnError::Network {
                            stage: "销毁云端资源".to_string(),
                            reason: errors.join("；"),
                        });
                    }
                }
            }
        }
        self.state
            .set_status(id, DeploymentStatus::Destroyed, None)?;
        Ok(())
    }
    /// **停止服务**（云端：停推理进程 + 关机实例）。
    ///
    /// ## 与 [`Orchestrator::stop`] 的区别
    ///
    /// | | `stop_service` | `stop` |
    /// |---|---|---|
    /// | 云端实例 | **保留**（关机，停止 GPU 计费） | 销毁 |
    /// | 磁盘/权重 | 保留，再启动只需几秒 | 全部丢失，要重新上传 |
    /// | 台账状态 | `stopped` | `destroyed` |
    ///
    /// 这才是「停止」应有的语义：用户想暂停一下，不是想删机器。
    /// 真正释放资源（停止一切计费）请用 `DELETE /api/deployments/{id}`。
    pub fn stop_service(&self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;

        match &record.plan.origin {
            ComputeOrigin::Cloud {
                provider: origin_provider,
                instance_id,
            } => {
                // ## provider 必须能回退到 cloud.provider
                //
                // `origin.provider` 由编排阶段回填，自建设备（`device_id`）
                // 路径下可能留空——实测台账里就是 `origin.provider=""` 而
                // `cloud.provider="ssh"`。直接拿空串去查驱动会报
                // 「云端驱动 `` 未注册或未启用」，用户看到的是**停止失败、
                // 但机器仍在计费**。
                //
                // 判据顺序：origin.provider（厂商托管）→ cloud.provider
                //（自建设备/SSH 直连）→ 空则说明这条记录根本没有可操作的
                // 远端，交给下面的「只停进程」分支处理，不要抛配置错误。
                let provider = if !origin_provider.trim().is_empty() {
                    origin_provider.clone()
                } else {
                    record
                        .plan
                        .cloud
                        .as_ref()
                        .map(|c| c.provider.clone())
                        .unwrap_or_default()
                };

                // 自建设备（或 provider 为空）没有厂商 API 可调：
                // 只能用 SSH 停掉该端口的推理进程，机器绝不关机。
                // 这是「自定义节点是用户自己的机器」这条边界的落点。
                if provider.trim().is_empty() || provider == "ssh" {
                    return self.stop_service_on_self_hosted(id, &record);
                }

                let mut cloud = self.cloud();
                let o = cloud.as_mut().ok_or_else(|| {
                    MnError::bad_request("未配置云端能力，无法操作云端部署".to_string())
                })?;
                // 1) 先停推理进程。失败不致命——它可能本来就没跑（上次已停），
                //    但**要记下来**，因为「进程停不掉」意味着关机时可能丢数据。
                let mut warnings = Vec::new();
                if let Err(e) = o.stop_service(&provider, instance_id) {
                    warnings.push(format!("停止推理进程：{e}"));
                }
                // 2) 再关机。这一步失败必须报错——机器还在跑就意味着继续计费。
                o.power_off(&provider, instance_id)?;
                // 3) 关掉隧道：机器都关了，隧道只会反复重连
                if let Err(e) = o.tunnels_mut().close_all() {
                    warnings.push(format!("关闭隧道：{e}"));
                }
                // 4) 清理台账里的节点连接信息：实例已经关机，公网 IP/SSH 端口
                //    可能随下次开机漂移。留着旧值会让 UI 与后续复用误以为
                //    节点仍可连，产生「停止后仍显示旧 IP」的历史残留。
                //    下次启动由 service_target 实时查询，不依赖这里。
                if let Some(rec) = self.state.get(id)? {
                    let mut plan = rec.plan.clone();
                    if let Some(c) = plan.cloud.as_mut() {
                        c.node_ip = None;
                        c.node_ssh_port = None;
                    }
                    self.state.update_plan(id, plan)?;
                }
                let note = if warnings.is_empty() {
                    None
                } else {
                    Some(format!("实例已关机，但：{}", warnings.join("；")))
                };
                self.state.set_status(id, DeploymentStatus::Stopped, note)?;
                return Ok(());
            }
        }
    }

    /// 自建设备的「停止服务」：只停远端推理进程，**绝不关机**。
    ///
    /// ## 边界
    ///
    /// 自定义节点是用户自己的机器（可能就是用户的工作站），关机是破坏性
    /// 操作，不属于本工具的权限。所以这里只按端口停掉 `llama-server`，
    /// 释放显存、让出端口，机器保持运行。
    ///
    /// ## 为什么必须有这条路径
    ///
    /// `stop_service` 曾经无条件下调厂商 `power_off`，而自建设备的
    /// `origin.provider` 是空串（或 `ssh`）——于是报
    /// 「云端驱动 `` 未注册或未启用」，用户看到停止失败、进程却仍在跑、
    /// 显存仍被占用。这条分支把「自建设备」这一合法形态接住。
    fn stop_service_on_self_hosted(
        &self,
        id: &str,
        record: &mangosteen_base::state::DeploymentRecord,
    ) -> MnResult<()> {
        let mut warnings = Vec::new();
        if let Some(cloud) = record.plan.cloud.as_ref() {
            let port = cloud.engine_port;
            match self.device_target_for(cloud) {
                // 走自建设备（设置页登记的 SSH 目标）
                Some(target) => {
                    let workdir = self.config().cloud.remote_workdir.clone();
                    mangosteen_device::remote::stop_llama_server_on_port(
                        &target,
                        &workdir,
                        Some(port),
                    )
                    .map_err(|e| MnError::Network {
                        stage: "停止推理进程".to_string(),
                        reason: format!("远端端口 {port} 的 llama-server 停止失败：{e}"),
                    })?;
                    // 二次确认：按端口判据必须真的没了，不能只信脚本退出码。
                    //
                    // ## 校验失败 ≠ 停止失败
                    //
                    // 实测：停止脚本已经把进程杀干净（远端 GPU 归零、进程数 0），
                    // 紧接着的这次校验却因为 SSH 传输层抖动返回 255，旧实现用
                    // `?` 直接往外抛——于是「明明停成功了却报失败」，用户重试
                    // 也无法消除。校验是**辅助证据**，它的网络错误不能推翻
                    // 主操作的成功结论，只能降级成一条警告。
                    match mangosteen_device::remote::llama_server_running_on_port(
                        &target, &workdir, port,
                    ) {
                        Ok(true) => {
                            return Err(MnError::Network {
                                stage: "停止推理进程".to_string(),
                                reason: format!(
                                    "远端端口 {port} 的 llama-server 仍在运行，已阻止标记为「已停止」"
                                ),
                            });
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warnings.push(format!("停止后校验未完成（{e}），进程可能已停止"));
                        }
                    }
                }
                // 既没有 device 也没有可用的节点信息：进程可能根本没起过，
                // 或者台账已与远端脱节。不谎报成功，如实说明。
                None => {
                    warnings.push("台账缺少可连接的节点信息，未执行远端停止".to_string());
                }
            }
        }

        // 关隧道（保留实例）：自建设备机器不动，但本地转发必须收掉，
        // 否则每停一次就泄漏一条 ssh 与一个本地端口。
        if let Some(port) = record.plan.cloud.as_ref().map(|c| c.engine_port) {
            let _ = self.tunnels().close_by_remote_port(port);
            if let Some(o) = self.cloud().as_mut() {
                let _ = o.tunnels_mut().close_by_remote_port(port);
            }
        }
        if let Some(port) = record.plan.local_access_port() {
            self.scheduler().release_port(port);
        }
        // 清掉节点连接信息（IP/端口可能漂移，下次启动实时查询）
        if let Some(rec) = self.state.get(id)? {
            let mut plan = rec.plan.clone();
            if let Some(c) = plan.cloud.as_mut() {
                c.node_ip = None;
                c.node_ssh_port = None;
            }
            self.state.update_plan(id, plan)?;
        }

        let note = if warnings.is_empty() {
            Some("已停止推理服务；自定义节点的机器保留".to_string())
        } else {
            Some(format!("已停止推理服务，但：{}", warnings.join("；")))
        };
        self.state.set_status(id, DeploymentStatus::Stopped, note)?;
        Ok(())
    }
    /// **启动服务**（开机 + 起推理进程 + 重建隧道 + 探活）。
    ///
    /// 与重新部署的区别：不重新建实例、不重新装依赖、不重新下权重，
    /// 只把停掉的东西拉起来。云端权重与二进制都还在盘上，所以通常十几秒就能用。
    ///
    /// 支持两类节点：
    ///
    /// | 节点来源 | 启动动作 |
    /// |---|---|
    /// | 云厂商实例 | 开机 → 起 `llama-server` → 重建隧道 → 等 `/health` |
    /// | 自定义节点 | 起 `llama-server` → 重建隧道 → 等 `/health`（机器本就开着） |
    pub fn start_service(&self, id: &str) -> MnResult<()> {
        self.start_service_with_progress(id, &crate::progress::noop_sink)
    }

    /// 带进度上报的启动服务（部署页点「部署」复用实例时走这里）。
    pub fn start_service_with_progress(
        &self,
        id: &str,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<()> {
        use crate::progress::{DeployStage, StageOutcome};

        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;
        let mut plan = record.plan.clone();
        let cloud_plan = plan
            .cloud
            .clone()
            .ok_or_else(|| MnError::bad_request("台账缺少云端方案，无法启动".to_string()))?;

        // ---- 端口冲突防御（多模型同节点）----
        //
        // 复用路径沿用台账里的 engine_port，但该端口可能已被**其它活跃部署**
        // 占用（模型 A 停止后模型 B 部署时抢走了同一个端口）。启动前检查：
        // 冲突就重新分配一个空闲端口并回写台账，否则 llama-server 起不来
        // （address in use）或两个模型共用一条隧道（业务转发串线）。
        let port_owner = self
            .state
            .list()
            .unwrap_or_default()
            .into_iter()
            .find(|r| {
                r.plan.id != id
                    && r.status.is_active()
                    && r.plan.cloud.as_ref().map(|c| c.engine_port) == Some(cloud_plan.engine_port)
                    && r.plan.cloud.as_ref().and_then(|c| c.node_ip.clone())
                        == cloud_plan.node_ip.clone()
            });
        let mut cloud_plan = cloud_plan;
        if let Some(other) = port_owner {
            // 排除自己后重新分配：allocate_remote_engine_port 看的是全部活跃
            // 部署，而本部署此刻仍是 active（provisioning/ready），直接调用
            // 会把自己的旧端口也当作已占用——正确行为本来就是避开它，
            // 因此这里先临时把状态当非活跃处理：手动构造排除集。
            let mut used: std::collections::BTreeSet<u16> = self
                .state
                .list()
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.status.is_active() && r.plan.id != id)
                .filter_map(|r| r.plan.cloud.map(|c| c.engine_port))
                .collect();
            used.insert(cloud_plan.engine_port);
            let new_port = (mangosteen_device::remote::LLAMA_CPP_PORT..=8191u16)
                .find(|p| !used.contains(p))
                .unwrap_or(mangosteen_device::remote::LLAMA_CPP_PORT);
            eprintln!(
                "[start] 端口 {} 已被部署 {} 占用，本部署改用 {}",
                cloud_plan.engine_port, other.plan.id, new_port
            );
            cloud_plan.engine_port = new_port;
            cloud_plan.engine.port = new_port;
            if let Some(c) = plan.cloud.as_mut() {
                c.engine_port = new_port;
                c.engine.port = new_port;
            }
            self.state.update_plan(id, plan.clone())?;
        }

        sink(
            DeployStage::Selecting,
            StageOutcome::Done,
            format!(
                "复用已有实例：{}（不重新创建、不重新下载）",
                cloud_plan
                    .node_ip
                    .clone()
                    .unwrap_or_else(|| "已登记节点".to_string())
            ),
        );
        // 依赖与引擎已在节点上（上次部署装过），这里直接跳过配置阶段，
        // 只在下方校验二进制存在性。
        sink(
            DeployStage::Configuring,
            StageOutcome::Skipped,
            "节点环境沿用上次部署：依赖与 llama-server 已就绪，跳过重复安装".to_string(),
        );
        sink(
            DeployStage::Downloading,
            StageOutcome::Skipped,
            "模型文件沿用上次部署：跳过重新下载".to_string(),
        );

        self.state
            .set_status(id, DeploymentStatus::Provisioning, None)?;

        let mark_failed = |st: &mangosteen_base::state::StateStore, e: &MnError| {
            let _ = st.set_status(id, DeploymentStatus::Failed, Some(e.to_string()));
        };

        match &plan.origin {
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                let launch_config = self.cloud_launch_config(&cloud_plan)?;
                // ---- 自定义节点：机器本就开着，只需起进程 + 建隧道 ----
                if let Some(target) = self.device_target_for(&cloud_plan) {
                    // 即使是复用节点，也必须重新验证连接：节点可能换过密码、
                    // 重启过 sshd、清理过 authorized_keys，不能因为台账存在就跳过。
                    let target =
                        match mangosteen_provider::orchestrator::configure_existing_ssh_access(
                            &target,
                        ) {
                            Ok(verified) => verified,
                            Err(e) => {
                                mark_failed(&self.state, &e);
                                return Err(e);
                            }
                        };
                    sink(
                        DeployStage::Configuring,
                        StageOutcome::Running,
                        format!("正在重新测试复用节点连接：{}:{}", target.host, target.port),
                    );
                    if let Err(e) = mangosteen_device::remote::ssh_exec(&target, "true", 30) {
                        mark_failed(&self.state, &e);
                        return Err(e);
                    }
                    sink(
                        DeployStage::Configuring,
                        StageOutcome::Done,
                        "复用节点 SSH 连通性与免密登录确认成功".to_string(),
                    );
                    sink(
                        DeployStage::Launching,
                        StageOutcome::Running,
                        format!("正在启动推理引擎：{}:{}", target.host, target.port),
                    );
                    let workdir = self.config().cloud.remote_workdir.clone();
                    if let Err(e) = mangosteen_provider::orchestrator::start_node_engine(
                        &target,
                        &workdir,
                        &launch_config,
                    ) {
                        mark_failed(&self.state, &e);
                        return Err(e);
                    }
                    let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
                        &target,
                        cloud_plan.engine_port,
                        false,
                        Some(cloud_plan.engine_port),
                    );
                    let _ = self.tunnels().close(&spec);
                    let local_port = match self
                        .tunnels()
                        .establish(&spec, Duration::from_secs(30))
                    {
                        Ok(t) => t.local_port,
                        Err(e) => {
                            mark_failed(&self.state, &e);
                            return Err(e);
                        }
                    };
                    if let Err(e) =
                        self.finish_start(id, local_port, &target.host, target.port, sink)
                    {
                        mark_failed(&self.state, &e);
                        return Err(e);
                    }
                    return Ok(());
                }

                // ---- 云厂商实例：开机 → 起进程 → 建隧道 ----
                let mut cloud = self.cloud();
                let o = cloud.as_mut().ok_or_else(|| {
                    MnError::bad_request("未配置云端能力，无法操作云端部署".to_string())
                })?;
                sink(
                    DeployStage::Selecting,
                    StageOutcome::Running,
                    format!("正在开机实例 {instance_id} 并等待进入运行状态"),
                );
                if let Err(e) = o.power_on(provider, instance_id, &cloud_plan) {
                    mark_failed(&self.state, &e);
                    return Err(e);
                }
                // 重复节点/停止后重启也必须重新验证 SSH：不能因为实例 ID
                // 存在就跳过连通性测试。确认密码登录、公钥安装和免密登录
                // 都成功后，才允许进入启动模型阶段。
                let verified_ssh =
                    match o.service_target(provider, instance_id).and_then(|target| {
                        mangosteen_provider::orchestrator::configure_existing_ssh_access(&target)
                    }) {
                        Ok(target) => target,
                        Err(e) => {
                            mark_failed(&self.state, &e);
                            return Err(e);
                        }
                    };
                if let Err(e) = mangosteen_device::remote::ssh_exec(&verified_ssh, "true", 30) {
                    mark_failed(&self.state, &e);
                    return Err(e);
                }
                sink(
                    DeployStage::Configuring,
                    StageOutcome::Done,
                    "复用节点 SSH 连通性与免密登录确认成功".to_string(),
                );
                sink(
                    DeployStage::Launching,
                    StageOutcome::Running,
                    format!("实例已运行，正在启动推理引擎：{instance_id}"),
                );
                if let Err(e) = o.start_service(provider, instance_id, &cloud_plan, &launch_config)
                {
                    mark_failed(&self.state, &e);
                    return Err(e);
                }
                let base = match o.establish_tunnel(provider, instance_id, &cloud_plan) {
                    Ok(b) => b,
                    Err(e) => {
                        mark_failed(&self.state, &e);
                        return Err(e);
                    }
                };
                let local_port = base
                    .rsplit(':')
                    .next()
                    .and_then(|p| p.trim_end_matches("/v1").parse::<u16>().ok())
                    .ok_or_else(|| {
                        MnError::bad_request(format!("隧道返回了无法解析的地址：{base}"))
                    })?;
                let host = verified_ssh.host.clone();
                let ssh_port = verified_ssh.port;
                if let Err(e) = self.finish_start(id, local_port, &host, ssh_port, sink) {
                    mark_failed(&self.state, &e);
                    return Err(e);
                }
                Ok(())
            }
        }
    }

    /// 启动成功后的公共收尾：等 `/health`、写台账、上报阶段。
    fn finish_start(
        &self,
        id: &str,
        local_port: u16,
        node_host: &str,
        node_ssh_port: u16,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<()> {
        use crate::progress::{DeployStage, StageOutcome};

        let health = format!("http://127.0.0.1:{local_port}/health");
        let timeout = Duration::from_secs(self.config().scheduler.launch_timeout_secs);
        mangosteen_device::remote::wait_http_ready(&health, timeout, Duration::from_secs(2))?;
        sink(
            DeployStage::Launching,
            StageOutcome::Done,
            format!("推理进程已就绪（/health 通过）· 本地端口 {local_port}"),
        );

        // 回写实际端口与节点信息：UI 展示的必须是「现在真能连的地址」
        if let Some(mut rec) = self.state.get(id)? {
            let mut plan = rec.plan.clone();
            if let Some(c) = plan.cloud.as_mut() {
                c.local_port = local_port;
                if !node_host.trim().is_empty() {
                    c.node_ip = Some(node_host.to_string());
                    c.node_ssh_port = Some(node_ssh_port);
                }
            }
            rec.plan = plan;
            // 只更新方案（保留状态），状态由状态机推进到 Ready
            self.state.update_plan(id, rec.plan)?;
        }
        // Provisioning → Ready 是合法迁移，走状态机而不是直接写字段，
        // 否则非法状态组合会悄悄落盘。
        self.state.set_status(id, DeploymentStatus::Ready, None)?;
        sink(
            DeployStage::Tunneling,
            StageOutcome::Done,
            format!("本地网关已接通远端模型：127.0.0.1:{local_port}"),
        );
        Ok(())
    }

    /// 中止部署并**回滚**它产生的痕迹（实例只停不删）。
    ///
    /// ## 与 [`Orchestrator::stop`] 的区别
    ///
    /// | | `stop` | `abort_deployment` |
    /// |---|---|---|
    /// | 语义 | 系统侧彻底回收 | 用户取消一次**进行中/刚完成**的部署 |
    /// | 台账 | 保留记录，状态改 `destroyed` | 实例已创建→标 `stopped` 保留；未创建→**删除记录** |
    /// | 端口 | 释放 | 显式从预留表移除 |
    /// | 文件 | 不动 | 按 `delete_model_file` 决定 |
    ///
    /// ## 为什么实例只停不删
    ///
    /// 产品要求「取消/卸载都只停实例」。停机能立刻止住 GPU 计费，同时
    /// 保留已装好的依赖、引擎与已下载的权重——用户重试时不必再等几 GB。
    /// 真正销毁由系统按策略回收，或在用户删除自定义节点时处理。
    ///
    /// 台账在实例已存在时会保留为 `stopped`：`instance_id` 是「下次部署
    /// 复用同一台机器」的唯一依据，删掉记录等于丢失这条线索。
    ///
    /// ## 为什么要显式释放端口
    ///
    /// `prepare` 阶段调 `scheduler.decide` 时就把端口记进了 `reserved`
    /// 列表。若回滚不还回去，反复「部署→停止」会一点点吃光端口区间，
    /// 最终表现是「端口耗尽，部署失败」——而用户根本没跑过几个模型。
    ///
    /// ## `delete_file` 的边界
    ///
    /// 传 `Some(path)` 才删，且**以传入路径为准**而不是台账里的
    /// `model_path`：远端部署的 prepare 发生在下载之前，台账里那条
    /// 记录写的是空路径（文件当时还不存在），照它删等于什么也没删。
    /// 调用方（[`the pull job manager`] 的 abort 钩子）知道文件真正落在哪。
    ///
    /// 只有**本次部署下载的**文件才该删。若模型原本就在本地
    /// （用户点的是「启动」而非「下载并启动」），删掉它等于卸载用户
    /// 已有的资产——那不是「停止部署」，是「删除模型」。
    pub fn abort_deployment(
        &self,
        id: &str,
        delete_file: Option<&std::path::Path>,
    ) -> MnResult<()> {
        let record = self.state.get(id)?;

        // 1) 停止云端实例（**不销毁**）
        //
        // 产品要求：部署中途取消只停实例，不删机器。停机能立刻止住 GPU 计费，
        // 又保留已装好的依赖、引擎与已下载的权重，用户再次部署时能直接复用。
        // 销毁由系统按策略回收，避免「取消一次就白下几个 GB」。
        if let Some(rec) = &record {
            match &rec.plan.origin {
                ComputeOrigin::Cloud {
                    provider,
                    instance_id,
                } => {
                    let vendor_managed = !provider.trim().is_empty()
                        && !instance_id.trim().is_empty()
                        && rec
                            .plan
                            .cloud
                            .as_ref()
                            .and_then(|c| c.device_id.as_ref())
                            .is_none();
                    if vendor_managed {
                        if let Some(o) = self.cloud().as_mut() {
                            // 先停推理进程（可能已经起来了），再关机。
                            let _ = o.stop_service_on_port(
                                provider,
                                instance_id,
                                rec.plan
                                    .cloud
                                    .as_ref()
                                    .map(|c| c.engine_port)
                                    .unwrap_or(8080),
                            );
                            // 关机失败意味着仍在计费：这是唯一必须让用户知道的事。
                            if let Err(e) = o.power_off(provider, instance_id) {
                                return Err(MnError::Network {
                                    stage: "回滚（停止实例）".to_string(),
                                    reason: format!(
                                        "实例 {instance_id} 关机失败，可能仍在计费：{e}"
                                    ),
                                });
                            }
                        }
                    }
                }
            }

            // 2) 释放端口预留
            if let Some(port) = rec.plan.local_access_port() {
                self.scheduler().release_port(port);
            }
            // 关隧道：机器已关，留着只会反复重连
            let _ = self.tunnels().close_all();
            if let Some(o) = self.cloud().as_mut() {
                let _ = o.tunnels_mut().close_all();
            }
        }

        // 3) 按需删文件（以调用方给的路径为准，见方法文档）
        if let Some(p) = delete_file {
            if p.is_file() {
                let _ = std::fs::remove_file(p);
            }
            // 顺手清掉续传残留（`<name>.gguf.part`）
            let part = p.with_extension("gguf.part");
            if part.is_file() {
                let _ = std::fs::remove_file(&part);
            }
            // 还有 `<name>.gguf.sha256` 之类的旁注文件
            let mut side = p.as_os_str().to_owned();
            side.push(".sha256");
            let side = PathBuf::from(side);
            if side.is_file() {
                let _ = std::fs::remove_file(&side);
            }
        }

        // 4) 台账：实例已创建 → 标 `stopped` 保留（实例 ID 是复用依据）；
        //    还没创建 → 删除记录（这次尝试确实什么都没留下）。
        let has_instance = record
            .as_ref()
            .map(|r| {
                let cloud_id = match &r.plan.origin {
                    ComputeOrigin::Cloud { instance_id, .. } => !instance_id.trim().is_empty(),
                };
                // 自定义节点没有容器 ID，但绑定设备存在；机器本身就是「实例」。
                let has_device = r
                    .plan
                    .cloud
                    .as_ref()
                    .and_then(|c| c.device_id.as_ref())
                    .map(|d| !d.trim().is_empty())
                    .unwrap_or(false);
                cloud_id || has_device
            })
            .unwrap_or(false);
        if has_instance {
            // 状态迁移：Planned/Provisioning/Ready/Failed 都必须能落到 Stopped
            // （见 `DeploymentRecord::transition` 的允许集合）。
            self.state.set_status(
                id,
                DeploymentStatus::Stopped,
                Some("部署已取消；实例已停止保存，可再次部署复用".to_string()),
            )?;
        } else {
            self.state.remove(id)?;
        }
        Ok(())
    }
    /// 部署前清理该模型的**历史残留部署记录**，让每次部署都从头开始。
    ///
    /// ## 为什么必须在部署前清
    ///
    /// 用户重复部署同一模型时，台账里可能留着上一次的记录（`ready` /
    /// `provisioning` / `stopped` / `failed`）。这些记录会带来两类错误：
    ///
    /// 1. **唯一性检查误判**：同模型存在「活跃」记录时直接拒绝部署，
    ///    但那个实例往往早就没了；
    /// 2. **UI 直接显示上次结果**：进度轮询读到残留的 `ready` 记录，
    ///    表现为「点部署瞬间提示部署完成」——实际一个资源都没创建。
    ///
    /// 所以每次部署前先把这个模型的历史记录清干净。
    ///
    /// ## 只清状态，不删资源
    ///
    /// - **不删远端文件**：模型权重、mmproj、MTP 是下次部署最贵的部分，
    ///   下载阶段会重新校验并复用；
    /// - **不关机、不销毁实例**：节点复用由部署流程自己按 GPU 型号匹配；
    ///   这里只停掉该模型自己的推理进程（按它自己的端口），
    ///   避免新进程起来时端口/显存被旧进程占住。
    ///
    /// 返回被清理的记录条数。
    pub fn purge_model_deployments(&self, model_id: &str) -> MnResult<usize> {
        let records = self.state.list()?;
        let mut removed = 0usize;
        for rec in records {
            let id = mangosteen_core::model::ModelEntry::derive_id(
                &rec.plan.model.name,
                rec.plan.model.quant,
            );
            if id != model_id {
                continue;
            }
            // 只停本模型自己的推理进程（按其 engine_port 定位），
            // 绝不 pkill 全节点，避免误杀同节点其它模型。
            let mut remote_targets: Vec<mangosteen_device::remote::SshTarget> = Vec::new();
            if let Some(cloud) = rec.plan.cloud.as_ref() {
                let port = cloud.engine_port;
                if let Some(target) = self.device_target_for(cloud) {
                    // 自定义节点：直接 SSH 停进程
                    let workdir = self.config().cloud.remote_workdir.clone();
                    let _ = mangosteen_device::remote::stop_llama_server_on_port(
                        &target,
                        &workdir,
                        Some(port),
                    );
                    remote_targets.push(target);
                } else {
                    let ComputeOrigin::Cloud {
                        provider,
                        instance_id,
                    } = &rec.plan.origin;
                    // 云厂商实例：走驱动停进程，但**不关机**
                    if !provider.trim().is_empty() && !instance_id.trim().is_empty() {
                        if let Some(o) = self.cloud().as_mut() {
                            let _ = o.stop_service_on_port(provider, instance_id, port);
                            // 厂商实例：取 SSH 目标用于清理下载残留（失败不阻断）
                            if let Ok(t) = o.service_target(provider, instance_id) {
                                remote_targets.push(t);
                            }
                        }
                    }
                }
            }
            // 清理该模型在远端的下载残留（断点文件/脚本/pid/日志）：
            // 上次中断的 aria2c/curl 残留会让新下载「0 字节停滞」误判。
            // 残留文件按部署方案里的 model_path 派生；辅助文件（mmproj/MTP）
            // 同样清理。失败只留日志，不阻断清理流程。
            for t in &remote_targets {
                if let Some(cloud) = rec.plan.cloud.as_ref() {
                    for p in std::iter::once(cloud.model_path.clone())
                        .chain(cloud.auxiliary_downloads.iter().map(|a| a.path.clone()))
                    {
                        match mangosteen_device::remote::clean_model_download_artifacts(t, &p) {
                            Ok(n) if n > 0 => {
                                eprintln!("[purge] 已清理 {} 的 {} 个下载残留", p, n);
                            }
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("[purge] 清理 {} 下载残留失败（不阻断）：{e}", p)
                            }
                        }
                    }
                }
            }
            // 释放该记录占用的本地端口预留，否则反复部署会吃光端口区间。
            if let Some(port) = rec.plan.local_access_port() {
                self.scheduler().release_port(port);
            }
            self.state.remove(&rec.plan.id)?;
            removed += 1;
        }
        Ok(removed)
    }

    /// 根据本地映射端口与远端引擎端口的实时探测结果刷新台账。
    ///
    /// `mapped=true` 表示本地端口能访问对应远端 `/health`，且方案中记录了
    /// 远端引擎端口；这时记录可视为 Ready（**含 Failed 的探测自愈**：
    /// 瞬时探测失败翻成的 Failed 在端口恢复服务后必须能翻回 Ready，
    /// 否则记录永久卡死，对话页签看不到该模型）。
    /// `mapped=false` 时，旧的 Ready 记录标为 Failed，避免历史台账
    /// 继续冒充可用实例。
    ///
    /// 正在部署的 Planned/Provisioning 不在这里处理，避免后台 5 秒探测在
    /// 节点创建、下载或启动期间把正常进行中的任务提前判成失败。
    ///
    /// Stopped 同样跳过：它是用户的显式动作态（停止服务后隧道必然不通），
    /// 不能因为「探测不到」降级成 Failed——否则破坏「停止保留实例、
    /// 可再启动」的生命周期。
    pub fn refresh_deployment_mapping(&self, id: &str, mapped: bool) -> MnResult<()> {
        let Some(record) = self.state.get(id)? else {
            return Ok(());
        };
        if record.status == DeploymentStatus::Destroyed
            || matches!(
                record.status,
                DeploymentStatus::Planned
                    | DeploymentStatus::Provisioning
                    | DeploymentStatus::Stopped
            )
        {
            return Ok(());
        }
        let next = if mapped {
            DeploymentStatus::Ready
        } else {
            DeploymentStatus::Failed
        };
        if record.status != next {
            let detail = if mapped {
                "本地端口与远端引擎端口映射存在，健康检查通过".to_string()
            } else {
                "本地端口与远端引擎端口映射不存在或健康检查失败".to_string()
            };
            self.state.set_status(id, next, Some(detail))?;
        }
        Ok(())
    }

    /// 标记云端实例已失效：保留历史记录但不再参与复用，
    /// 让下一次部署可以按用户当前 GPU 选择创建新实例。
    pub fn invalidate_deployment(&self, id: &str, reason: &str) -> MnResult<()> {
        let Some(record) = self.state.get(id)? else {
            return Ok(());
        };
        if record.status != DeploymentStatus::Destroyed {
            self.state
                .set_status(id, DeploymentStatus::Destroyed, Some(reason.to_string()))?;
        }
        Ok(())
    }

    /// 停止全部部署。
    pub fn stop_all(&self) -> Vec<(String, String)> {
        let ids: Vec<String> = self
            .state
            .list_active()
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.plan.id)
            .collect();
        let mut errors = Vec::new();
        for id in ids {
            if let Err(e) = self.stop(&id) {
                errors.push((id, e.to_string()));
            }
        }
        errors
    }
    /// 关闭网关：停止全部部署，释放资源。
    pub fn shutdown(&self) -> Vec<(String, String)> {
        self.stop_all()
    }
}
