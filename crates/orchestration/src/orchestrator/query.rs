//! 运行状态查询：台账列表、活跃后端、云端监控与整体状态快照。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;

impl Orchestrator {
    /// 列出全部部署。
    pub fn list_deployments(&self) -> MnResult<Vec<DeploymentRecord>> {
        self.state.list()
    }
    /// 取单个部署。
    pub fn get_deployment(&self, id: &str) -> MnResult<Option<DeploymentRecord>> {
        self.state.get(id)
    }
    /// 探活：返回每个活跃部署的真实状态（并顺带修正台账）。
    ///
    /// 本地引擎可能已崩溃、云端实例可能被厂商回收，
    /// 因此「台账说 Ready」不等于「现在真的能服务」。
    pub fn refresh(&mut self) -> MnResult<Vec<ActiveDeployment>> {
        let records = self.state.list_active()?;
        let mut out = Vec::new();

        // ---- 先收敛历史遗留的重复实例 ----
        //
        // 「同一模型只允许一个实例」是产品约束。但台账里可能已有历史重复
        // （旧版本不拦、或多次 deploy 绕过）。若不收敛，UI 会同时显示两个
        // 同模型实例，「停止」按钮也只能指向其中一个，另一个变成孤儿。
        //
        // 策略：每个模型 ID 只保留最先创建的那个，其余真停掉（本地杀进程、
        // 云端销毁实例）。这条在刷新路径里做，所以每次探活都会自愈。
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut keep: Vec<mangosteen_base::state::DeploymentRecord> = Vec::new();
        let mut dup_ids: Vec<String> = Vec::new();
        for r in records {
            let mid = mangosteen_core::model::ModelEntry::derive_id(
                &r.plan.model.name,
                r.plan.model.quant,
            );
            match seen.get(&mid) {
                // 已有一个同模型的活跃实例 → 这个算重复
                Some(_) => dup_ids.push(r.plan.id.clone()),
                None => {
                    seen.insert(mid, r.plan.id.clone());
                    keep.push(r);
                }
            }
        }
        for id in &dup_ids {
            // 尽力停止；失败也不阻断刷新（台账状态仍会被探活修正）
            let _ = self.stop(id);
        }

        for r in keep {
            let id = r.plan.id.clone();
            // 各分支都会赋值，这里只是给编译器一个初值
            #[allow(unused_assignments)]
            let mut ready = false;

            if r.plan.origin.is_local() {
                if let Some(h) = self.engines.get_mut(&id) {
                    let st = h.status(Duration::from_secs(0));
                    ready = st.is_serving();
                    if st.is_dead() {
                        // 引擎已死 → 台账改为 Failed，避免误报
                        self.state.set_status(
                            &id,
                            DeploymentStatus::Failed,
                            Some(format!("本地引擎已退出：{}", st.describe())),
                        )?;
                    }
                } else {
                    // 无内存句柄：该部署可能是**另一个进程**（如 `mangosteen deploy`
                    // 之后另起 `mangosteen serve`）启动的。此时不能直接判定为未就绪，
                    // 必须通过 HTTP 探活——这正是「网关唯一收口」在多进程下的必要补充。
                    ready = probe_local_health(&self.engines_registry, &r.plan);
                    if !ready && r.status == DeploymentStatus::Ready {
                        // 台账说 Ready 但探不通 → 进程确实没了，修正台账
                        self.state.set_status(
                            &id,
                            DeploymentStatus::Failed,
                            Some(format!(
                                "本地引擎在 {} 无响应（进程可能已退出）",
                                r.plan
                                    .local_access_port()
                                    .map(|p| format!("127.0.0.1:{p}"))
                                    .unwrap_or_else(|| "原端口".into())
                            )),
                        )?;
                    }
                }
            } else {
                // 云端：只信台账状态（真实探测需要网络往返，放到推理时兜底）
                ready = r.status == DeploymentStatus::Ready;
            }

            out.push(ActiveDeployment {
                id: id.clone(),
                // 用完整模型 ID（`name:quant`）而不是裸模型名：
                // `/v1/models`、推理请求、推理响应三处必须给出同一个标识，
                // 否则上层应用做一致性校验会失败。同一模型的不同量化版本
                // 也才能区分开。
                model: mangosteen_core::model::ModelEntry::derive_id(
                    &r.plan.model.name,
                    r.plan.model.quant,
                ),
                origin: r.plan.origin.clone(),
                status: r.status,
                port: r.plan.local_access_port(),
                ready,
            });
        }
        Ok(out)
    }
    /// 找出当前可用于推理的后端。
    ///
    /// 选择规则：优先本地（本地优先原则的延伸），否则取最近就绪的云端部署。
    pub fn active_backend(&mut self) -> MnResult<BackendTarget> {
        let active = self.refresh()?;

        // 1. 本地优先
        if let Some(a) = active
            .iter()
            .find(|a| a.origin.is_local() && a.ready && a.port.is_some())
        {
            return Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: a.port.unwrap(),
                model_id: a.model.clone(),
                origin_label: "本地算力".to_string(),
            });
        }
        // 2. 云端
        if let Some(a) = active
            .iter()
            .find(|a| !a.origin.is_local() && a.ready && a.port.is_some())
        {
            return Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: a.port.unwrap(),
                model_id: a.model.clone(),
                origin_label: a.origin.label(),
            });
        }
        Err(MnError::bad_request(
            "当前没有就绪的推理后端。请先执行 `mangosteen deploy <model>`",
        ))
    }
    /// 按模型 ID 选择后端；`None`/`"auto"` 表示自动（本地优先）。
    ///
    /// 为什么需要它：UI 的「模型信息」下拉让用户选具体模型，但如果网关
    /// 永远取「第一个就绪的」，那个选择就是摆设——用户选了 A，实际
    /// 路由到 B，回答风格对不上却看不出原因。
    ///
    /// 指定了具体模型却**没在运行**时必须明确报错，而不是悄悄退回自动：
    /// 静默换模型比报错更难排查（用户会以为「就是这个模型答的」）。
    pub fn active_backend_for(&mut self, want: Option<&str>) -> MnResult<BackendTarget> {
        let Some(id) = want
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "auto")
        else {
            return self.active_backend();
        };

        let active = self.refresh()?;
        // 精确匹配模型 ID；也接受「不带量化后缀」的宽松匹配，
        // 因为 UI 显示与台账 ID 偶尔会在量化标记上不一致。
        let hit = active
            .iter()
            .find(|a| a.ready && a.port.is_some() && a.model == id)
            .or_else(|| {
                active
                    .iter()
                    .find(|a| a.ready && a.port.is_some() && a.model.split(':').next() == Some(id))
            });

        match hit {
            Some(a) => Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: a.port.unwrap(),
                model_id: a.model.clone(),
                origin_label: a.origin.label(),
            }),
            None => {
                let running: Vec<&str> = active
                    .iter()
                    .filter(|a| a.ready)
                    .map(|a| a.model.as_str())
                    .collect();
                Err(MnError::bad_request(format!(
                    "模型 `{id}` 未在运行，无法对话。\n当前运行中：{}",
                    if running.is_empty() {
                        "（无）".to_string()
                    } else {
                        running.join(", ")
                    }
                )))
            }
        }
    }
    /// 当前活跃的云端部署（**只读台账，不发网络请求**）。
    ///
    /// 设备页需要「现在用的是哪台云机器」才能去查监控。这里刻意只返回
    /// 元信息，把厂商接口调用留给调用方在**锁外**执行——云端往返是
    /// 百毫秒级，绝不能放在网关锁内（否则对话/状态等所有 API 都被拖慢）。
    ///
    /// 若有多个云端部署，取台账里**最近更新**的那个（与「当前在用」的直觉
    /// 一致），避免同时打多个厂商接口。
    pub fn active_cloud_deployment(&self) -> MnResult<Option<CloudDeploymentRef>> {
        let Some(rec) = self
            .state
            .list_active()?
            .into_iter()
            .filter(|r| !r.plan.origin.is_local())
            .max_by_key(|r| r.updated_at)
        else {
            return Ok(None);
        };

        let (provider, instance_id) = match &rec.plan.origin {
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => (provider.clone(), instance_id.clone()),
            ComputeOrigin::Local => return Ok(None),
        };

        Ok(Some(CloudDeploymentRef {
            deployment_id: rec.plan.id.clone(),
            provider,
            instance_id,
            model: mangosteen_core::model::ModelEntry::derive_id(
                &rec.plan.model.name,
                rec.plan.model.quant,
            ),
            status: rec.status,
        }))
    }
    /// 查询某云端实例的实时监控（**会发起网络请求**）。
    ///
    /// 调用方必须确保**不在持有网关锁时调用**它——单次往返约 0.5 秒，
    /// 持锁会拖慢所有其它 API。后台采集线程（[`the gateway cloud monitor`]）
    /// 的做法是「持锁取元信息 → 放锁 → 再持锁调用本方法」，看似持锁，
    /// 但它运行在独立线程且频率被压到 5 秒一次，实际影响可接受；
    /// HTTP 请求路径**绝不**调用本方法，只读缓存。
    pub fn cloud_instance_metrics(
        &self,
        provider: &str,
        instance_id: &str,
    ) -> MnResult<Option<mangosteen_core::cloud::InstanceMetrics>> {
        let Some(orch) = self.cloud.as_ref() else {
            // 未配置云服务商：不是错误，设备页显示「未启用云端」即可
            return Ok(None);
        };
        orch.instance_metrics(Some(provider), instance_id)
    }
    /// 状态快照。
    pub fn status(&mut self) -> MnResult<GatewayStatus> {
        let engine_path = self.local_engine.locate().ok();
        // 只读缓存：`--version` 在部分安装上要 20~30 秒，绝不能在请求路径上跑
        let engine_version = engine_path
            .as_ref()
            .and_then(|p| self.cached_engine_version(p));

        Ok(GatewayStatus {
            version: mangosteen_core::identity::VERSION.to_string(),
            hardware: self.assessor.hardware().describe(),
            // 「已就绪」而非「已启用」：凭证缺失的服务商不能降级，
            // 展示成已就绪会误导用户以为本地不足时一定能上云。
            providers: mangosteen_provider::build_registry(&self.config)
                .registry
                .names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            active: self.refresh()?,
            engine_path,
            engine_version,
        })
    }
}
