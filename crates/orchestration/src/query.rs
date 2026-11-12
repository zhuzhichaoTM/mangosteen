//! 运行状态查询：台账列表、活跃后端、云端监控与整体状态快照。
//!
//! 归属：第三层 · 编排调度核心。
//! `Orchestrator` 的字段在 `super` 中定义，子模块可直接访问。

use super::*;

/// 从台账里挑出「可复用」的那一条（纯函数，便于单独验证判定口径）。
///
/// 判定条件见 [`Orchestrator::reusable_deployment`]。
fn pick_reusable(
    records: Vec<DeploymentRecord>,
    model_id: &str,
    gpu_model: Option<&str>,
) -> Option<DeploymentRecord> {
    let mut best: Option<DeploymentRecord> = None;
    for rec in records {
        let id = mangosteen_core::model::ModelEntry::derive_id(
            &rec.plan.model.name,
            rec.plan.model.quant,
            rec.plan.model.alias.as_deref(),
        );
        if id != model_id {
            continue;
        }
        let ComputeOrigin::Cloud { instance_id, .. } = &rec.plan.origin;
        // 「有实例」的两种形态：
        //   · 云厂商实例 → 云端容器 ID 非空
        //   · 自定义节点 → 没有容器 ID，但设备绑定存在（机器本身就是实例）
        // 只认容器 ID 会让自定义节点永远被判成「没部署过」，
        // 于是每次点部署都重走一遍配置与下载。
        let has_device = rec
            .plan
            .cloud
            .as_ref()
            .and_then(|c| c.device_id.as_ref())
            .map(|d| !d.trim().is_empty())
            .unwrap_or(false);
        if instance_id.trim().is_empty() && !has_device {
            continue;
        }
        // 正在跑的会撞唯一性约束，交给原有检查报错；终态不能复活
        if matches!(
            rec.status,
            DeploymentStatus::Ready | DeploymentStatus::Destroyed
        ) {
            continue;
        }
        if let Some(want) = gpu_model.filter(|s| !s.is_empty()) {
            let actual = rec
                .plan
                .cloud
                .as_ref()
                .map(|c| c.gpu_model.as_str())
                .unwrap_or("");
            if !actual.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        match &best {
            Some(b) if b.updated_at >= rec.updated_at => {}
            _ => best = Some(rec),
        }
    }
    best
}

/// 把同模型的多条记录收敛成一条：保留**最新创建**的那条。
///
/// 纯函数，便于单独验证「保留哪一条」的口径——这个口径判错会直接表现为
/// 「部署成功后节点立刻被关闭」，必须能单独测试。
///
/// ## 为什么按 `created_at` 而不是列表顺序
///
/// 台账按追加顺序存储，先出现的恰恰是**旧记录**（可能早已 stopped/failed）。
/// 如果按「列表里先出现的」保留，就会停掉用户刚部署成功、正在服务的那台，
/// 表现是「UI 提示部署完成、模型已就绪」紧接着节点被关。
///
/// 同一秒内多次创建时 `created_at` 可能相同，此时用 `updated_at` 兜底；
/// 两者都相同则保留索引更大的（更晚追加），仍然符合「取最新」。
///
/// 返回 `(保留的记录, 判定为重复的部署 ID 列表)`。
fn dedupe_by_model(records: Vec<DeploymentRecord>) -> (Vec<DeploymentRecord>, Vec<String>) {
    // 模型 ID → 当前最优记录的索引
    let mut best: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (idx, r) in records.iter().enumerate() {
        let mid = mangosteen_core::model::ModelEntry::derive_id(
            &r.plan.model.name,
            r.plan.model.quant,
            r.plan.model.alias.as_deref(),
        );
        match best.get(&mid) {
            Some(&cur) => {
                let (cur_r, new_r) = (&records[cur], r);
                // Ready 状态的记录优先保留（部署成功，正在服务中，绝不能被 stop）
                let cur_ready = cur_r.status == DeploymentStatus::Ready;
                let new_ready = new_r.status == DeploymentStatus::Ready;
                let newer = match (cur_ready, new_ready) {
                    (true, false) => false, // 已有 Ready，新记录不是 Ready，保留旧记录
                    (false, true) => true,  // 新记录是 Ready，优先保留新记录
                    _ => (new_r.plan.created_at, new_r.updated_at)
                        > (cur_r.plan.created_at, cur_r.updated_at), // 都非 Ready，按时间戳比较
                };
                if newer {
                    best.insert(mid, idx);
                }
            }
            None => {
                best.insert(mid, idx);
            }
        }
    }
    let keep_idx: std::collections::HashSet<usize> = best.values().copied().collect();

    let mut keep = Vec::new();
    let mut dup_ids = Vec::new();
    for (idx, r) in records.into_iter().enumerate() {
        if keep_idx.contains(&idx) {
            keep.push(r);
        } else {
            dup_ids.push(r.plan.id.clone());
        }
    }
    (keep, dup_ids)
}

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
    pub fn refresh(&self) -> MnResult<Vec<ActiveDeployment>> {
        let records = self.state.list_active()?;
        eprintln!(
            "[refresh] list_active returned {} records",
            records.len()
        );
        for r in &records {
            eprintln!(
                "[refresh] record: id={} status={:?} model={}",
                r.plan.id, r.status, r.plan.model.name
            );
        }
        let mut out = Vec::new();

        // ---- 先收敛历史遗留的重复实例 ----
        //
        // 「同一模型只允许一个实例」是产品约束。但台账里可能已有历史重复
        // （旧版本不拦、或多次 deploy 绕过）。若不收敛，UI 会同时显示两个
        // 同模型实例，「停止」按钮也只能指向其中一个，另一个变成孤儿。
        //
        // 保留哪一条由 [`dedupe_by_model`] 决定：**最新创建**的那条。
        //
        // 这条曾经写成「保留列表里第一条」，结果是 UI 提示「部署完成、
        // 模型已就绪」后节点立刻被关——因为台账里旧记录排在前面被保留，
        // 刚部署成功的新记录反而被判成重复并被 `stop()`。
        // `refresh()` 挂在状态轮询上，所以故障在部署成功后第一次刷新就发生。
        let (keep, dup_ids) = dedupe_by_model(records);
        eprintln!(
            "[refresh] dedupe_by_model: keep={} dups={:?}",
            keep.len(),
            dup_ids
        );
        for id in &dup_ids {
            eprintln!(
                "[refresh] stopping duplicate deployment id={}",
                id
            );
            // 尽力停止；失败也不阻断刷新（台账状态仍会被探活修正）
            let _ = self.stop(id);
        }

        for r in keep {
            let id = r.plan.id.clone();
            // 云端：只信台账状态（真实探测需要网络往返，放到推理时兜底）
            let ready = r.run_status == mangosteen_base::state::RunStatus::Active;

            // 远端推理端口（云端实例上的 llama-server 监听端口）
            let remote_port = r.plan.cloud.as_ref().map(|c| c.engine_port);

            out.push(ActiveDeployment {
                id: id.clone(),
                // 用完整模型 ID（`name:quant`）而不是裸模型名：
                // `/v1/models`、推理请求、推理响应三处必须给出同一个标识，
                /// 否则上层应用做一致性校验会失败。同一模型的不同量化版本
                /// 也才能区分开。
                model: mangosteen_core::model::ModelEntry::derive_id(
                    &r.plan.model.name,
                    r.plan.model.quant,
                    r.plan.model.alias.as_deref(),
                ),
                origin: r.plan.origin.clone(),
                status: r.status,
                run_status: r.run_status.clone(),
                port: r.plan.local_access_port(),
                remote_port,
                ready,
            });
        }
        Ok(out)
    }
    /// 找出当前可用于推理的后端。
    ///
    /// 纯云端架构下只取最近就绪的云端部署（本地推理由 Ollama 承担，
    /// 不在 Mangosteen 的部署编排范围内）。
    pub fn active_backend(&self) -> MnResult<BackendTarget> {
        if let Some((model_id, port)) = self.routes().iter().next() {
            return Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: *port,
                model_id: model_id.clone(),
                origin_label: "remote-inference".to_string(),
            });
        }
        let active = self.refresh()?;

        // 云端：取最近就绪且已分配端口的部署
        if let Some(a) = active.iter().find(|a| a.ready && a.port.is_some()) {
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
    /// 按模型 ID 选择后端；`None`/`"auto"` 表示自动（取最近就绪的云端部署）。
    ///
    /// 为什么需要它：UI 的「模型信息」下拉让用户选具体模型，但如果网关
    /// 永远取「第一个就绪的」，那个选择就是摆设——用户选了 A，实际
    /// 路由到 B，回答风格对不上却看不出原因。
    ///
    /// 指定了具体模型却**没在运行**时必须明确报错，而不是悄悄退回自动：
    /// 静默换模型比报错更难排查（用户会以为「就是这个模型答的」）。
    pub fn active_backend_for(&self, want: Option<&str>) -> MnResult<BackendTarget> {
        if let Some(id) = want.filter(|x| *x != "auto") {
            if let Some(port) = self.routes().get(id) {
                return Ok(BackendTarget {
                    host: "127.0.0.1".to_string(),
                    port: *port,
                    model_id: id.to_string(),
                    origin_label: "remote-inference".to_string(),
                });
            }
        } else if let Some((model_id, port)) = self.routes().iter().next() {
            return Ok(BackendTarget {
                host: "127.0.0.1".to_string(),
                port: *port,
                model_id: model_id.clone(),
                origin_label: "remote-inference".to_string(),
            });
        }
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
    /// 找一个「已部署但没在跑」的部署，可被复用来重启。
    ///
    /// ## 为什么需要它
    ///
    /// 用户点「部署」时，如果这个模型已经有实例（上次部署过、现在停止），
    /// 重新创建实例等于把最贵的部分（几 GB 权重 + 依赖安装 + 引擎）重做一遍。
    /// 只要实例还在，正确的动作是开机重启。
    ///
    /// ## 判定口径
    ///
    /// - 模型 ID 相同（含量化后缀，不同量化是不同模型）；
    /// - 有实例：云端容器 ID 非空，或绑定了自定义设备；
    /// - 状态**不是** `ready`（在跑的会撞唯一性约束，交给原有检查报错）；
    /// - 状态不是 `destroyed`（终态不能复活）。
    ///
    /// 多个候选时取最近更新的那个，与 [`Self::active_cloud_deployment`] 一致。
    pub fn reusable_deployment(
        &self,
        model_id: &str,
        gpu_model: Option<&str>,
    ) -> MnResult<Option<DeploymentRecord>> {
        Ok(pick_reusable(self.state.list()?, model_id, gpu_model))
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
            .max_by_key(|r| r.updated_at)
        else {
            return Ok(None);
        };

        let (provider, instance_id) = match &rec.plan.origin {
            ComputeOrigin::Cloud {
                provider,
                instance_id,
            } => (provider.clone(), instance_id.clone()),
        };

        Ok(Some(CloudDeploymentRef {
            deployment_id: rec.plan.id.clone(),
            provider,
            instance_id,
            model: mangosteen_core::model::ModelEntry::derive_id(
                &rec.plan.model.name,
                rec.plan.model.quant,
                rec.plan.model.alias.as_deref(),
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
        let cloud = self.cloud();
        let Some(orch) = cloud.as_ref() else {
            // 未配置云服务商：不是错误，设备页显示「未启用云端」即可
            return Ok(None);
        };
        orch.instance_metrics(Some(provider), instance_id)
    }
    /// 状态快照。（只读，不修改 state.json 数据）
    pub fn status(&self) -> MnResult<GatewayStatus> {
        let records = self.state.list()?;
        let active: Vec<ActiveDeployment> = records
            .iter()
            .filter(|r| matches!(r.status, DeploymentStatus::Provisioning | DeploymentStatus::Ready))
            .map(|r| ActiveDeployment {
                id: r.plan.id.clone(),
                model: mangosteen_core::model::ModelEntry::derive_id(
                    &r.plan.model.name,
                    r.plan.model.quant,
                    r.plan.model.alias.as_deref(),
                ),
                origin: r.plan.origin.clone(),
                status: r.status,
                run_status: r.run_status.clone(),
                port: r.plan.local_access_port(),
                remote_port: r.plan.cloud.as_ref().map(|c| c.engine_port),
                ready: r.run_status == mangosteen_base::state::RunStatus::Active,
            })
            .collect();
        Ok(GatewayStatus {
            version: mangosteen_core::identity::VERSION.to_string(),
            hardware: self.assessor.hardware().describe(),
            providers: mangosteen_provider::build_registry(&self.config())
                .registry
                .names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            active,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mangosteen_base::state::{DeploymentRecord, DeploymentStatus};
    use mangosteen_core::estimate::{Assessment, KvCacheType, Placement, ResourceRequirement};
    use mangosteen_core::model::{ModelSpec, Quantization};
    use mangosteen_core::plan::{CloudPlan, DeploymentPlan, LocalPlan};

    /// 构造一份只关心「复用判定」字段的云端方案记录。
    fn record(id: &str, instance_id: &str, status: DeploymentStatus, at: i64) -> DeploymentRecord {
        let spec = ModelSpec {
            name: "qwen".into(),
            alias: None,
            params_b: 0.5,
            quant: Quantization::Q4KM,
            n_layers: 24,
            n_heads: 14,
            n_kv_heads: 2,
            embedding_dim: 896,
            vocab_size: 151_936,
            native_context: 32_768,
            file_size_bytes: 491_400_032,
        };
        let assessment = Assessment {
            placement: Placement::FullGpu,
            requirement: ResourceRequirement {
                weights_bytes: 400 << 20,
                kv_cache_bytes: 100 << 20,
                compute_buffer_bytes: 64 << 20,
                runtime_overhead_bytes: 200 << 20,
            },
            context_len: 8192,
            kv_cache: KvCacheType::F16,
            quant: Quantization::Q4KM,
            threads: 4,
            batch_size: 512,
            accelerator_budget_bytes: 24 << 30,
            host_budget_bytes: 48 << 30,
            warnings: vec![],
        };
        let engine = LocalPlan::from_assessment(
            &spec,
            "/root/mangosteen/models/qwen.gguf".to_string(),
            &assessment,
            8080,
        );
        DeploymentPlan {
            id: id.to_string(),
            model: spec,
            origin: ComputeOrigin::Cloud {
                provider: "compshare".to_string(),
                instance_id: instance_id.to_string(),
            },
            cloud: Some(CloudPlan {
                provider: "compshare".to_string(),
                gpu_model: "RTX 4090".to_string(),
                gpu_count: 1,
                vram_bytes_per_gpu: 24 << 30,
                cpu_cores: 8,
                memory_bytes: 32 << 30,
                disk_bytes: 50 << 30,
                image: "cuda128".to_string(),
                model_path: "/root/mangosteen/models/qwen.gguf".to_string(),
                engine_port: 8080,
                mmproj_path: None,
                draft_model_path: None,
                auxiliary_downloads: Vec::new(),
                local_port: 0,
                engine,
                max_price_per_hour: None,
                download_url: Some("https://example.com/qwen.gguf".to_string()),
                download_bytes: Some(491_400_032),
                remote_model: None,
                node_ip: None,
                requested_spec_id: None,
                requested_gpu_model: None,
                node_ssh_port: None,
                node_ssh_user: None,
                device_id: None,
                reuse_instance_id: None,
                reuse_provider: None,
            }),
            assessment,
            created_at: 1_700_000_000,
            fallback_trail: vec![],
        }
        .into_record(status, at)
    }

    trait IntoRecord {
        fn into_record(self, status: DeploymentStatus, at: i64) -> DeploymentRecord;
    }
    impl IntoRecord for DeploymentPlan {
        fn into_record(self, status: DeploymentStatus, at: i64) -> DeploymentRecord {
            let mut r = DeploymentRecord::from_plan(self);
            r.status = status;
            r.updated_at = at;
            r
        }
    }

    /// 「可复用」的判定口径：同模型 + 有实例 + 没在跑 + 未销毁。
    ///
    /// 这条判定决定用户点「部署」时是复用开机还是重新建机器——
    /// 判错一次就是几 GB 重下加一段实例创建等待。
    #[test]
    fn reusable_needs_instance_and_must_not_be_running() {
        // derive_id 不再附加量化后缀：模型名 "qwen" → ID "qwen"
        let model = "qwen";

        // 空台账
        assert!(pick_reusable(vec![], model, None).is_none());

        // 既没有云端实例 ID、也没绑定自定义设备 → 不能复用
        let none_yet = record("dep-empty", "", DeploymentStatus::Stopped, 10);
        assert!(
            pick_reusable(vec![none_yet], model, None).is_none(),
            "既无实例也无设备时必须走创建流程"
        );

        // 自定义节点：没有容器 ID，但绑定设备 → 可复用（机器本身就是实例）
        let mut custom = record("dep-custom", "", DeploymentStatus::Stopped, 15);
        if let Some(c) = custom.plan.cloud.as_mut() {
            c.device_id = Some("dev-1".to_string());
        }
        assert_eq!(
            pick_reusable(vec![custom], model, None).map(|r| r.plan.id),
            Some("dep-custom".to_string()),
            "自定义节点部署停止后也应能直接重启"
        );

        // 有实例且已停止 → 可复用
        let parked = record("dep-stopped", "uhost-1", DeploymentStatus::Stopped, 20);
        assert_eq!(
            pick_reusable(vec![parked], model, None).map(|r| r.plan.id),
            Some("dep-stopped".to_string())
        );

        // 失败但有实例 → 也可复用（修完配置就能开机重试）
        let failed = record("dep-failed", "uhost-2", DeploymentStatus::Failed, 30);
        assert_eq!(
            pick_reusable(vec![failed], model, None).map(|r| r.plan.id),
            Some("dep-failed".to_string())
        );

        // 正在运行 → 不复用，交给唯一性约束报错
        let running = record("dep-ready", "uhost-3", DeploymentStatus::Ready, 40);
        assert!(
            pick_reusable(vec![running], model, None).is_none(),
            "运行中的实例不该走复用路径"
        );

        // 已销毁 → 不能复活
        let gone = record("dep-gone", "uhost-4", DeploymentStatus::Destroyed, 50);
        assert!(pick_reusable(vec![gone], model, None).is_none());

        // GPU 型号不匹配 → 必须创建新实例
        let mismatch = record("dep-mismatch", "uhost-x", DeploymentStatus::Stopped, 80);
        assert!(pick_reusable(vec![mismatch], model, Some("P40")).is_none());
        // GPU 型号一致（大小写不敏感）→ 可以复用
        let matched = record("dep-matched", "uhost-y", DeploymentStatus::Stopped, 90);
        assert_eq!(
            pick_reusable(vec![matched], model, Some("rtx 4090")).map(|r| r.plan.id),
            Some("dep-matched".to_string())
        );

        let matched = record("dep-matched", "uhost-y", DeploymentStatus::Stopped, 90);
        assert_eq!(
            pick_reusable(vec![matched], model, Some("rtx 4090")).map(|r| r.plan.id),
            Some("dep-matched".to_string())
        );

        // 多个候选取最近更新
        let a = record("dep-a", "uhost-a", DeploymentStatus::Stopped, 60);
        let b = record("dep-b", "uhost-b", DeploymentStatus::Stopped, 70);
        assert_eq!(
            pick_reusable(vec![a, b], model, None).map(|r| r.plan.id),
            Some("dep-b".to_string())
        );
    }

    /// 去重必须保留**最新创建**的记录，而不是列表里最早的那条。
    ///
    /// 这是用户实际踩到的故障：部署成功后 UI 提示「模型已就绪」，紧接着
    /// 节点被关。根因是旧逻辑保留列表第一条（旧记录），把刚创建的新记录
    /// 判成重复并停止。
    #[test]
    fn dedupe_keeps_newest_not_first() {
        // 旧记录：创建时间早，但排在列表前面（台账追加顺序）
        let old = record("dep-old", "uhost-old", DeploymentStatus::Stopped, 10);
        // 新记录：创建时间晚，排在列表后面（刚部署成功、正在服务）
        let new = record("dep-new", "uhost-new", DeploymentStatus::Ready, 20);

        let (keep, dups) = dedupe_by_model(vec![old, new]);
        assert_eq!(
            keep.iter().map(|r| r.plan.id.as_str()).collect::<Vec<_>>(),
            vec!["dep-new"],
            "必须保留最新创建的记录"
        );
        assert_eq!(dups, vec!["dep-old".to_string()], "旧记录应被判为重复");
    }

    /// 同一秒创建（created_at 相同）时按 updated_at 取更新者。
    #[test]
    fn dedupe_same_created_at_falls_back_to_updated_at() {
        let mut a = record("dep-a", "uhost-a", DeploymentStatus::Stopped, 10);
        let mut b = record("dep-b", "uhost-b", DeploymentStatus::Ready, 20);
        a.plan.created_at = 100;
        b.plan.created_at = 100;

        let (keep, dups) = dedupe_by_model(vec![a, b]);
        assert_eq!(keep[0].plan.id, "dep-b");
        assert_eq!(dups, vec!["dep-a".to_string()]);
    }

    /// 不同模型互不去重：去重键是「模型 ID」，不是实例 ID。
    #[test]
    fn dedupe_scopes_per_model_id() {
        let qwen_a = record("qa", "uhost-a", DeploymentStatus::Ready, 1);
        let qwen_b = record("qb", "uhost-b", DeploymentStatus::Ready, 2);
        // 关键：两个不同实例但同模型（record 里的模型名都是 "qwen"）
        let (keep, dups) = dedupe_by_model(vec![qwen_a, qwen_b]);
        assert_eq!(keep.len(), 1, "同模型两条记录只留一条");
        assert_eq!(dups.len(), 1);
    }
}
