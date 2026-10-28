//! 部署生命周期：停止服务（保留节点）、恢复服务、中止回滚与整体关停。
//!
//! 归属：网关 crate 内部（第三层编排职责的第二层门面实现）。
//! `Gateway` 的字段在 `super`（gateway 模块）中定义，子模块可直接访问。

use super::*;

impl Gateway {
    /// 停止部署（释放本地进程 / 销毁云端实例）。
    pub fn stop(&mut self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;

        match &record.plan.origin {
            ComputeOrigin::Local => {
                if let Some(mut h) = self.engines.remove(id) {
                    h.shutdown()?;
                } else if let Some(pid) = record.pid {
                    // 进程不是本实例启动的（如 deploy 在另一个进程里），
                    // 按台账记录的 PID 终止，避免留下孤儿进程占用端口。
                    mangosteen_platform::process::terminate_pid(pid, Duration::from_secs(3))?;
                }
            }
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                if let Some(o) = self.cloud.as_mut() {
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
    /// **停止服务**（云端：停推理进程 + 关机实例；本地：停引擎进程）。
    ///
    /// ## 与 [`Gateway::stop`] 的区别
    ///
    /// | | `stop_service` | `stop` |
    /// |---|---|---|
    /// | 云端实例 | **保留**（关机，停止 GPU 计费） | 销毁 |
    /// | 磁盘/权重 | 保留，再启动只需几秒 | 全部丢失，要重新上传 |
    /// | 台账状态 | `stopped` | `destroyed` |
    ///
    /// 这才是「停止」应有的语义：用户想暂停一下，不是想删机器。
    /// 真正释放资源（停止一切计费）请用 `DELETE /api/deployments/{id}`。
    pub fn stop_service(&mut self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;

        match &record.plan.origin {
            ComputeOrigin::Local => {
                if let Some(mut h) = self.engines.remove(id) {
                    h.shutdown()?;
                } else if let Some(pid) = record.pid {
                    mangosteen_platform::process::terminate_pid(pid, Duration::from_secs(3))?;
                }
            }
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                let o = self.cloud.as_mut().ok_or_else(|| {
                    MnError::bad_request("未配置云端能力，无法操作云端部署".to_string())
                })?;
                // 1) 先停推理进程。失败不致命——它可能本来就没跑（上次已停），
                //    但**要记下来**，因为「进程停不掉」意味着关机时可能丢数据。
                let mut warnings = Vec::new();
                if let Err(e) = o.stop_service(provider, instance_id) {
                    warnings.push(format!("停止推理进程：{e}"));
                }
                // 2) 再关机。这一步失败必须报错——机器还在跑就意味着继续计费。
                o.power_off(provider, instance_id)?;
                // 3) 关掉隧道：机器都关了，隧道只会反复重连
                if let Err(e) = o.tunnels_mut().close_all() {
                    warnings.push(format!("关闭隧道：{e}"));
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
        self.state.set_status(id, DeploymentStatus::Stopped, None)?;
        Ok(())
    }
    /// **启动服务**（云端：开机 + 起推理进程；本地：重新拉起引擎）。
    ///
    /// 与重新部署的区别：不重新建实例、不重新传权重，只把停掉的东西拉起来。
    /// 云端权重与二进制都还在盘上，所以通常十几秒就能用。
    pub fn start_service(&mut self, id: &str) -> MnResult<()> {
        let record = self
            .state
            .get(id)?
            .ok_or_else(|| MnError::bad_request(format!("部署 `{id}` 不存在")))?;
        let plan = record.plan.clone();

        match &plan.origin {
            ComputeOrigin::Local => Err(MnError::bad_request(
                "本地部署没有「保留实例再启动」的概念：请直接重新部署该模型".to_string(),
            )),
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => {
                let cloud_plan = plan.cloud.clone().ok_or_else(|| {
                    MnError::bad_request("台账缺少云端方案，无法启动".to_string())
                })?;
                self.state
                    .set_status(id, DeploymentStatus::Provisioning, None)?;
                let launch_config = self.cloud_launch_config(&cloud_plan)?;
                let o = self.cloud.as_mut().ok_or_else(|| {
                    MnError::bad_request("未配置云端能力，无法操作云端部署".to_string())
                })?;
                // 1) 开机并等到 Running
                if let Err(e) = o.power_on(provider, instance_id, &cloud_plan) {
                    let msg = e.to_string();
                    self.state
                        .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                    return Err(e);
                }
                // 2) 起推理进程
                if let Err(e) = o.start_service(provider, instance_id, &cloud_plan, &launch_config)
                {
                    let msg = e.to_string();
                    self.state
                        .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                    return Err(e);
                }
                // 3) 重建隧道 + 等 /health
                let base = match o.establish_tunnel(provider, instance_id, &cloud_plan) {
                    Ok(b) => b,
                    Err(e) => {
                        let msg = e.to_string();
                        self.state
                            .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                        return Err(e);
                    }
                };
                let health = format!("{base}/health");
                if let Err(e) = mangosteen_device::remote::wait_http_ready(
                    &health,
                    o.start_timeout(),
                    o.poll_interval(),
                ) {
                    let msg = e.to_string();
                    self.state
                        .set_status(id, DeploymentStatus::Failed, Some(msg.clone()))?;
                    return Err(e);
                }
                self.state.set_status(id, DeploymentStatus::Ready, None)?;
                Ok(())
            }
        }
    }
    /// 中止部署并**回滚**它产生的全部痕迹。
    ///
    /// ## 与 [`Gateway::stop`] 的区别
    ///
    /// | | `stop` | `abort_deployment` |
    /// |---|---|---|
    /// | 语义 | 停掉一个**已成功**的部署 | 撤销一次**进行中/刚完成**的部署 |
    /// | 台账 | 保留记录，状态改 `destroyed` | **删除记录** |
    /// | 端口 | 释放（进程退出后自然可用） | 显式从预留表移除 |
    /// | 文件 | 不动 | 按 `delete_model_file` 决定 |
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
    /// 调用方（[`crate::pulljobs`] 的 abort 钩子）知道文件真正落在哪。
    ///
    /// 只有**本次部署下载的**文件才该删。若模型原本就在本地
    /// （用户点的是「启动」而非「下载并启动」），删掉它等于卸载用户
    /// 已有的资产——那不是「停止部署」，是「删除模型」。
    pub fn abort_deployment(
        &mut self,
        id: &str,
        delete_file: Option<&std::path::Path>,
    ) -> MnResult<()> {
        let record = self.state.get(id)?;

        // 1) 停引擎 / 销毁云端实例（可能不存在：prepare 后立即被中止）
        if let Some(rec) = &record {
            match &rec.plan.origin {
                ComputeOrigin::Local => {
                    if let Some(mut h) = self.engines.remove(id) {
                        let _ = h.shutdown();
                    } else if let Some(pid) = rec.pid {
                        // 忽略失败：进程可能已经退了
                        let _ = mangosteen_platform::process::terminate_pid(
                            pid,
                            Duration::from_secs(3),
                        );
                    }
                }
                ComputeOrigin::Cloud {
                    provider,
                    instance_id,
                } => {
                    if let Some(o) = self.cloud.as_mut() {
                        // 云端实例不销毁会**持续计费**，失败必须暴露出来
                        let errors = o.teardown(provider, instance_id);
                        if !errors.is_empty() {
                            return Err(MnError::Network {
                                stage: "回滚云端资源".to_string(),
                                reason: format!(
                                    "实例 {} 可能仍在运行并计费：{}",
                                    instance_id,
                                    errors.join("；")
                                ),
                            });
                        }
                    }
                }
            }

            // 2) 释放端口预留
            if let Some(port) = rec.plan.local_access_port() {
                self.scheduler.release_port(port);
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

        // 4) 删除台账记录（不是标记 destroyed——回滚要让这次尝试「没发生过」）
        self.state.remove(id)?;
        Ok(())
    }
    /// 停止全部部署。
    pub fn stop_all(&mut self) -> Vec<(String, String)> {
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
    pub fn shutdown(&mut self) -> Vec<(String, String)> {
        self.stop_all()
    }
}
