//! 部署生命周期：停止服务（保留节点）、恢复服务、中止回滚与整体关停。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;
use crate::deployment::{DeviceTarget, UnloadOutcome};

impl Orchestrator {
    /// API 网关启动后的业务自恢复：根据持久化台账重建已部署模型的隧道。
    ///
    /// GPU 节点和远端模型进程可能在 API 网关重启期间继续运行；本地
    /// `TunnelManager` 是进程内状态，重启后必须重新建立映射，否则台账虽是
    /// `ready`，对话仍找不到本地端口。该方法只恢复隧道，不重新创建节点、
    /// 不重新下载模型、不改变模型部署状态。
    pub fn restore_deployment_tunnels(&self) -> Vec<String> {
        let records = match self.state.list() {
            Ok(records) => records,
            Err(e) => return vec![format!("读取部署台账失败：{e}")],
        };
        let mut errors = Vec::new();
        for record in records {
            // `Failed` 记录同样重建隧道：瞬时探测失败会把实际仍在服务的
            // 部署标成 failed，网关重启后若不重建隧道，探测自愈永远等不到
            // 健康端口，模型在对话页签消失。状态这里不翻转，仍由后台探测
            // 在 `/health` 通过时翻回 `ready`（真死的部署保持 failed 无害）。
            if !matches!(
                record.status,
                DeploymentStatus::Ready | DeploymentStatus::Failed
            ) {
                continue;
            }
            let Some(cloud_plan) = record.plan.cloud.clone() else {
                continue;
            };
            let id = record.plan.id.clone();
            let result = if let Some(target) = self.device_target_for(&cloud_plan) {
                let spec = mangosteen_provider::orchestrator::tunnel_spec_for(
                    &target,
                    cloud_plan.engine_port,
                    false,
                );
                self.tunnels()
                    .establish(&spec, Duration::from_secs(30))
                    .map(|t| t.local_port)
            } else {
                let (provider, instance_id) = match &record.plan.origin {
                    ComputeOrigin::Cloud {
                        provider,
                        instance_id,
                    } => (provider.as_str(), instance_id.as_str()),
                };
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
            match result {
                Ok(local_port) => {
                    self.scheduler().reserve_port(local_port);
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
                }
                Err(e) => errors.push(format!("{id}：恢复模型隧道失败：{e}")),
            }
        }
        errors
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
                        return Err(MnError::Network {
                            stage: "卸载（停止模型进程）".to_string(),
                            reason: format!("模型远端端口 {port} 的 llama-server 停止失败：{e}"),
                        });
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
                    // 云厂商实例：直接由驱动停止服务（内部走 ssh）
                    let ComputeOrigin::Cloud {
                        provider,
                        instance_id,
                    } = &record.plan.origin;
                    if let Some(o) = self.cloud().as_mut() {
                        if !provider.trim().is_empty() && !instance_id.trim().is_empty() {
                            if let Err(e) = o.stop_service_on_port(
                                provider,
                                instance_id,
                                record
                                    .plan
                                    .cloud
                                    .as_ref()
                                    .map(|c| c.engine_port)
                                    .unwrap_or(8080),
                            ) {
                                return Err(MnError::Network {
                                    stage: "卸载（停止模型进程）".to_string(),
                                    reason: format!("模型远端进程停止失败：{e}"),
                                });
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

        // 3) 云厂商实例：只有该节点没有其它运行模型时才关机。
        //    节点共享时只停止当前模型进程，GPU 节点和其它模型保持运行。
        let keep_node = cloud
            .as_ref()
            .map(|c| c.device_id.is_some())
            .unwrap_or(false)
            || shared_node;
        let mut stopped_instance = None;
        if !keep_node {
            let ComputeOrigin::Cloud {
                provider,
                instance_id,
            } = &record.plan.origin;
            let vendor_managed = !provider.trim().is_empty() && !instance_id.trim().is_empty();
            if vendor_managed {
                if let Some(o) = self.cloud().as_mut() {
                    // 关机失败意味着仍在计费，必须让用户知道。实例已注册，
                    // 台账保留，用户可重试卸载。
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
                }
            }
        }

        // 4) 台账改为 `stopped`（不是删除）：实例还在，下次部署可复用。
        //    已经是 stopped 时保持原状态——重复卸载应当幂等。
        if state != DeploymentStatus::Stopped {
            self.state.set_status(
                id,
                DeploymentStatus::Stopped,
                Some(if keep_node {
                    "已停止推理服务；自定义节点的机器保留".to_string()
                } else {
                    "已停止推理服务并关机，实例保留可复用".to_string()
                }),
            )?;
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
        let plan = record.plan.clone();
        let cloud_plan = plan
            .cloud
            .clone()
            .ok_or_else(|| MnError::bad_request("台账缺少云端方案，无法启动".to_string()))?;

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
                } else {
                    let ComputeOrigin::Cloud {
                        provider,
                        instance_id,
                    } = &rec.plan.origin;
                    // 云厂商实例：走驱动停进程，但**不关机**
                    if !provider.trim().is_empty() && !instance_id.trim().is_empty() {
                        if let Some(o) = self.cloud().as_mut() {
                            let _ = o.stop_service_on_port(provider, instance_id, port);
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
