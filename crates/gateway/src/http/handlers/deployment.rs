//! 部署与生命周期处理器（`/api/preview`、`/api/deploy`、`/api/deployments*`、`/api/assess`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;

impl GatewayServer {
    /// 部署预估：不创建任何资源，只回答「会发生什么、要花多少钱」。
    ///
    /// UI 的确认弹窗靠它拿数据。**这个接口绝不产生费用**——
    /// 云端实例只会在用户勾选确认后由 `/api/deploy` 创建。
    pub(crate) fn preview(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(model) = parsed.get("model").and_then(|m| m.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 model 字段", Some("model")),
            );
        };

        let mut dreq =
            mangosteen_orchestration::assess::DeploymentRequest::for_model(model.to_string());
        if let Some(c) = parsed.get("context_len").and_then(|v| v.as_u64()) {
            dreq.context_len = Some(c as u32);
        }
        if let Some(p) = parsed.get("max_price_per_hour").and_then(|v| v.as_f64()) {
            dreq.max_price_per_hour = Some(p);
        }
        // 用户在确认弹窗里选定的 GPU 规格。空字符串 / null 表示自动选点。
        if let Some(spec) = parsed.get("cloud_spec").and_then(|v| v.as_str()) {
            let spec = spec.trim();
            if !spec.is_empty() {
                dreq.cloud_spec = Some(spec.to_string());
            }
        }
        // 设备选择：local / cloud / auto（缺省 auto = 本地优先）
        if let Some(t) = parsed.get("target").and_then(|v| v.as_str()) {
            match mangosteen_orchestration::assess::ComputeTarget::parse(t) {
                Some(x) => dreq.target = x,
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知 target `{t}`，可选 local / cloud / auto"),
                            Some("target"),
                        ),
                    )
                }
            }
        }

        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.preview(&dreq) {
            Ok(p) => Response::json(200, &p),
            Err(e) => Response::error(&e),
        }
    }
    pub(crate) fn deployments(&self) -> Response {
        // 运行态来自后台探测缓存（微秒级读锁）；未启用探测时回退台账状态。
        // 绝不在这里现场探测：部署页几秒轮询一次，现场探测会把页面拖卡。
        let probe = self.runtime_probe.as_ref();
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.list_deployments() {
            Ok(list) => {
                let arr: Vec<serde_json::Value> = list
                    .into_iter()
                    .map(|r| {
                        // 实例 ID：区分「未部署」与「已部署」的唯一依据。
                        //
                        // 云厂商创建的实例用云端容器 ID；自定义节点没有容器
                        // ID，回退到设备 ID——否则「自定义节点部署成功」会被
                        // 判成「未部署」，用户既看不到节点也无法卸载。
                        let cloud_instance = match &r.plan.origin {
                            mangosteen_core::plan::ComputeOrigin::Cloud { instance_id, .. } => {
                                instance_id.clone()
                            }
                        };
                        let device_id = r.plan.cloud.as_ref().and_then(|c| c.device_id.clone());
                        let device_name = device_id.as_deref().and_then(|id| {
                            gw.config()
                                .remote_devices
                                .iter()
                                .find(|d| d.id == id)
                                .map(|d| if d.name.trim().is_empty() { id.to_string() } else { d.name.clone() })
                        });
                        let instance_id = if !cloud_instance.trim().is_empty() {
                            cloud_instance.clone()
                        } else {
                            device_name.clone().unwrap_or_default()
                        };
                        let deployed = !instance_id.trim().is_empty();
                        // 运行态：只有已部署才有意义；已停止/失败的记录不探测。
                        let running = match probe.and_then(|p| p.is_running(&r.plan.id)) {
                            Some(v) => v,
                            // 没探测过（刚启动/刚部署完）→ 按台账状态判断
                            None => deployed && r.status.as_str() == "ready",
                        };
                        serde_json::json!({
                            "id": r.plan.id,
                            // 实例 ID（云端容器 ID；自定义节点为设备 ID）
                            "instance_id": instance_id,
                            // 云端容器 ID 原文（自定义节点为空），供需要区分的调用方使用
                            "cloud_instance_id": cloud_instance,
                            "device_id": device_id,
                            // 是否已部署：UI 用它区分「未部署」与「已部署」
                            "deployed": deployed,
                            // 是否正在运行（底层会话进程保活探测结果）
                            "running": running,
                            "model": r.plan.model.name,
                            // 完整模型 ID（含量化后缀）。`model` 只有裸名，
                            // 同家族多个量化版本并存时分不清实际在跑哪一个。
                            "model_id": mangosteen_core::model::ModelEntry::derive_id(
                                &r.plan.model.name,
                                r.plan.model.quant,
                            ),
                            "quant": r.plan.model.quant.to_string(),
                            "origin": r.plan.origin.label(),
                            // 稳定机器可读类型：UI 用它渲染本地/云端符号，
                            // 不依赖本地化文案做判断。
                            "origin_kind": "cloud",
                            // 云端节点公网 IP。
                            "node_ssh_port": r.plan.cloud.as_ref().and_then(|c| c.node_ssh_port),
                            "node_ssh_user": r.plan.cloud.as_ref().and_then(|c| c.node_ssh_user.clone()),
                            "remote_port": r.plan.cloud.as_ref().map(|c| c.engine_port),
                            "gpu_model": r.plan.cloud.as_ref().map(|c| c.gpu_model.clone()),
                            "requested_spec_id": r
                                .plan
                                .cloud
                                .as_ref()
                                .and_then(|c| c.requested_spec_id.clone()),
                            "requested_gpu_model": r
                                .plan
                                .cloud
                                .as_ref()
                                .and_then(|c| c.requested_gpu_model.clone()),
                            "node_ip": r
                                .plan
                                .cloud
                                .as_ref()
                                .and_then(|c| c.node_ip.clone()),
                            "status": r.status.as_str(),
                            "port": r.plan.local_access_port(),
                            // 进程 PID：用户想用 top / kill 直接操作引擎进程时
                            // 必须有它，否则只能按端口反查。
                            "pid": r.pid,
                            "error": r.error,
                            // 最近更新时间：UI 用它判断哪个部署记录是「最新方案」
                            "updated_at": r.updated_at,
                            "fallback_trail": r.plan.fallback_trail,
                        })
                    })
                    .collect();
                Response::json(200, &serde_json::json!({ "deployments": arr }))
            }
            Err(e) => Response::error(&e),
        }
    }
    pub(crate) fn deployment(&self, id: &str) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.get_deployment(id) {
            Ok(Some(r)) => {
                let payload = serde_json::json!({
                    "id": r.plan.id,
                    "model": r.plan.model.name,
                    "origin": r.plan.origin.label(),
                    "status": r.status.as_str(),
                    "port": r.plan.local_access_port(),
                    "error": r.error,
                    "fallback_trail": r.plan.fallback_trail,
                    "assessment": r.plan.assessment,
                });
                Response::json(200, &payload)
            }
            Ok(None) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }
    /// 卸载部署（`POST /api/deployments/{id}/unload`）。
    ///
    /// **只停不删**：停推理进程、关隧道、云实例关机（停 GPU 计费），
    /// 实例与磁盘保留，下次点「部署」会复用同一实例开机重启。
    /// 实例的最终销毁由系统按策略回收，或在用户删除自定义节点时处理。
    pub(crate) fn unload(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let result = gw.unload(id);
        drop(gw);
        // 卸载后必须重建业务转发快照：该模型已不可服务，不能继续留在
        // 快照里把对话路由到一个已关闭的隧道端口。
        if result.is_ok() {
            self.rebuild_backend_snapshot();
        }
        match result {
            Ok(outcome) => Response::json(
                200,
                &serde_json::json!({
                    "ok": true,
                    "id": id,
                    "stopped_instance": outcome.stopped_instance,
                    "node_kept": outcome.node_kept,
                    "message": if outcome.node_kept {
                        "已停止推理服务；自定义节点的机器与文件保留"
                    } else {
                        "已停止推理服务并关机，实例保留可再次部署"
                    },
                }),
            ),
            Err(e) => Response::json(500, &serde_json::json!({"error": e.to_string()})),
        }
    }

    /// 停止服务：云端关机（停 GPU 计费，实例与权重保留），本地停引擎进程。
    pub(crate) fn stop_service(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let result = gw.stop_service(id);
        drop(gw);
        if result.is_ok() {
            self.rebuild_backend_snapshot();
        }
        match result {
            Ok(()) => Response::json(
                200,
                &serde_json::json!({"ok": true, "id": id, "status": "stopped"}),
            ),
            Err(e) => Response::json(500, &serde_json::json!({"error": e.to_string()})),
        }
    }
    /// 启动服务：云端开机并重新拉起推理进程。
    pub(crate) fn start_service(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.start_service(id) {
            Ok(()) => Response::json(
                200,
                &serde_json::json!({"ok": true, "id": id, "status": "ready"}),
            ),
            Err(e) => Response::json(500, &serde_json::json!({"error": e.to_string()})),
        }
    }
    /// 登陆方式：实时向云厂商 API 查询实例的 SSH 登录指令。
    ///
    /// 台账里的 `node_ssh_port` 是部署时刻快照，实例停机再开机后端口可能
    /// 被厂商重新映射（22/23/高位端口都可能）；「登陆方式」必须以厂商
    /// 当前返回为准。编排层自带 60 秒缓存，UI 轮询不会打爆厂商接口。
    pub(crate) fn ssh_info(&self, id: &str) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.instance_ssh_info(id) {
            Ok(Some(info)) => {
                let available = info.state == "running";
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "available": available,
                        "deployment_id": info.deployment_id,
                        "instance_id": info.instance_id,
                        "provider": info.provider,
                        "state": info.state,
                        "ssh_command": info.ssh_command,
                        "host": info.host,
                        "port": info.port,
                        "user": info.user,
                        "source": info.source,
                    }),
                )
            }
            Ok(None) => Response::json(200, &serde_json::json!({"ok": true, "available": false})),
            Err(e) => Response::json(
                200,
                &serde_json::json!({"ok": false, "available": false, "error": e.to_string()}),
            ),
        }
    }
    pub(crate) fn stop(&self, id: &str) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let result = gw.stop(id);
        drop(gw);
        // 停止服务后必须重建业务转发快照：该模型已不可服务，不能继续留在
        // 快照里把对话路由到一个已关闭的隧道端口。
        if result.is_ok() {
            self.rebuild_backend_snapshot();
        }
        match result {
            Ok(()) => Response::json(200, &serde_json::json!({"ok": true, "id": id})),
            Err(e) => Response::error(&e),
        }
    }
    pub(crate) fn deploy(&self, req: &ParsedRequest) -> Response {
        let body = req.body_str();
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let model = match parsed.get("model").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request("缺少 model 字段", Some("model")),
                )
            }
        };

        let mut dreq = mangosteen_orchestration::assess::DeploymentRequest::for_model(model);
        if let Some(c) = parsed.get("context_len").and_then(|v| v.as_u64()) {
            dreq.context_len = Some(c as u32);
        }
        if let Some(b) = parsed.get("batch_size").and_then(|v| v.as_u64()) {
            dreq.batch_size = b as u32;
        }
        if let Some(a) = parsed.get("allow_cloud").and_then(|v| v.as_bool()) {
            dreq.allow_cloud = a;
        }
        if let Some(id) = parsed.get("device_id").and_then(|v| v.as_str()) {
            dreq.device_id = Some(id.to_string());
        }
        if let Some(spec) = parsed.get("cloud_spec").and_then(|v| v.as_str()) {
            let spec = spec.trim();
            if !spec.is_empty() {
                dreq.cloud_spec = Some(spec.to_string());
            }
        }
        if let Some(gpu) = parsed.get("gpu_model").and_then(|v| v.as_str()) {
            let gpu = gpu.trim();
            if !gpu.is_empty() {
                dreq.gpu_model = Some(gpu.to_string());
            }
        }
        // 用户显式指定的复用实例（与 /api/models/pull 同语义：实例 ID
        // 跨厂商不保证唯一，provider 必须成对传回）。
        if let Some(id) = parsed.get("reuse_instance_id").and_then(|v| v.as_str()) {
            let id = id.trim();
            if !id.is_empty() {
                dreq.reuse_instance_id = Some(id.to_string());
            }
        }
        if let Some(p) = parsed.get("reuse_provider").and_then(|v| v.as_str()) {
            let p = p.trim();
            if !p.is_empty() {
                dreq.reuse_provider = Some(p.to_string());
            }
        }
        // 算力来源（部署页签的「本地 / 云端」选择）。
        //
        // Mangosteen 当前为纯云端部署：模型一律部署到远端 GPU 节点
        // （云厂商实例或已保存的 SSH 设备），本地推理由 Ollama 承担。
        // 因此：
        // * `target=auto` / `target=cloud` = 走云端部署；
        // * `target=local`                = 明确拒绝（本地已不再部署）。
        //
        // 未知值直接报错而不是回退默认：静默忽略会让用户以为选择生效了。
        if let Some(t) = parsed.get("target") {
            let raw = t.as_str().unwrap_or("");
            match mangosteen_orchestration::assess::ComputeTarget::parse(raw) {
                Some(v) => dreq.target = v,
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知 target `{raw}`，可选 auto / local / cloud"),
                            Some("target"),
                        ),
                    )
                }
            }
        }
        if let Some(p) = parsed.get("max_price_per_hour").and_then(|v| v.as_f64()) {
            dreq.max_price_per_hour = Some(p);
        }
        if let Some(k) = parsed.get("kv_cache").and_then(|v| v.as_str()) {
            match k {
                "f16" => dreq.kv_cache = Some(mangosteen_core::estimate::KvCacheType::F16),
                "q8_0" => dreq.kv_cache = Some(mangosteen_core::estimate::KvCacheType::Q8_0),
                "q4_0" => dreq.kv_cache = Some(mangosteen_core::estimate::KvCacheType::Q4_0),
                other => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!("未知 kv_cache `{other}`，可选 f16 / q8_0 / q4_0"),
                            Some("kv_cache"),
                        ),
                    )
                }
            }
        }

        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.deploy(&dreq) {
            Ok(plan) => {
                let cloud = plan.cloud.as_ref();
                let payload = serde_json::json!({
                    "id": plan.id,
                    "model": plan.model.name,
                    "origin": plan.origin.label(),
                    "origin_kind": "cloud",
                    "port": plan.local_access_port(),
                    "fallback_trail": plan.fallback_trail,
                    "openai_base_url": format!(
                        "http://127.0.0.1:{}/v1",
                        gw.config().gateway.port
                    ),
                    // 设备配置（云端）：让用户在部署完成后立刻看到
                    // 「哪台机器、怎么登录、推理进程在哪」。
                    "provider": cloud.map(|c| c.provider.clone()),
                    "instance_id": match &plan.origin {
                        mangosteen_core::plan::ComputeOrigin::Cloud { instance_id, .. } =>
                            Some(instance_id.clone()),
                    },
                    "gpu_model": cloud.map(|c| c.gpu_model.clone()),
                    "node_ip": cloud.and_then(|c| c.node_ip.clone()),
                    "node_ssh_port": cloud.and_then(|c| c.node_ssh_port),
                    "node_ssh_user": cloud.and_then(|c| c.node_ssh_user.clone()),
                    "requested_spec_id": cloud.and_then(|c| c.requested_spec_id.clone()),
                    "requested_gpu_model": cloud.and_then(|c| c.requested_gpu_model.clone()),
                });
                Response::json(201, &payload)
            }
            Err(e) => Response::error(&e),
        }
    }
    pub(crate) fn assess(&self, req: &ParsedRequest) -> Response {
        let body = req.body_str();
        let parsed: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let model = match parsed.get("model").and_then(|m| m.as_str()) {
            Some(m) => m.to_string(),
            None => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request("缺少 model 字段", Some("model")),
                )
            }
        };
        let mut dreq =
            mangosteen_orchestration::assess::DeploymentRequest::for_model(model).local_only();
        if let Some(c) = parsed.get("context_len").and_then(|v| v.as_u64()) {
            dreq.context_len = Some(c as u32);
        }

        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.evaluate(&dreq) {
            Ok(e) => Response::json(
                200,
                &serde_json::json!({
                    "model": e.spec,
                    "placement": e.assessment.placement,
                    "context_len": e.assessment.context_len,
                    "kv_cache": e.assessment.kv_cache,
                    "threads": e.assessment.threads,
                    "requirement": e.assessment.requirement,
                    "local_viable": e.is_local_viable(),
                    "warnings": e.assessment.warnings.iter().map(|w| w.message()).collect::<Vec<_>>(),
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }
}
