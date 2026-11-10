//! 状态与诊断处理器（`/health`、`/v1/models`、`/api/status`、`/api/doctor`、`/api/sysinfo`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::server::*;
use crate::protocol;
use mangosteen_core::openai::HealthResponse;

impl GatewayServer {
    pub(crate) fn health(&self) -> Response {
        let gw = self.gateway.as_ref();
        let backend = gw.active_backend().ok();
        let h = HealthResponse {
            status: "ok".to_string(),
            version: mangosteen_core::identity::VERSION.to_string(),
            backend: backend.as_ref().map(|b| b.origin_label.clone()),
            backend_ready: backend.is_some(),
            model: backend.as_ref().map(|b| b.model_id.clone()),
        };
        Response::json(200, &h)
    }
    pub(crate) fn models(&self) -> Response {
        let gw = self.gateway.as_ref();
        let mut entries: Vec<(String, String)> = match gw.models().list() {
            Ok(list) => list
                .into_iter()
                .map(|m| {
                    let owner = if m.is_local() {
                        "local".to_string()
                    } else {
                        "cloud".to_string()
                    };
                    (m.id, owner)
                })
                .collect(),
            Err(e) => return Response::error(&e),
        };
        // 手动接入的远端推理服务没有本地模型台账记录，仍必须出现在
        // OpenAI /v1/models 中，否则 UI 会认为没有可对话模型。
        let cfg = gw.config();
        let mut remotes = cfg.remote_inferences.clone();
        if let Some(remote) = cfg.remote_inference.clone() {
            remotes.push(remote);
        }
        for remote in remotes {
            if !entries.iter().any(|(id, _)| id == &remote.model_id) {
                entries.push((remote.model_id, "cloud".to_string()));
            }
        }
        // 活跃部署的模型也必须出现：只部署在云端（或引擎直跑）的模型
        // 在本地 ModelStore 没有记录，OpenAI 客户端拉模型列表会看不到
        // 可对话模型。只读台账（list_active 不触发厂商网络）。
        if let Ok(recs) = gw.state().list_active() {
            for r in recs {
                let id = mangosteen_core::model::ModelEntry::derive_id(
                    &r.plan.model.name,
                    r.plan.model.quant,
                );
                if !entries.iter().any(|(eid, _)| eid == &id) {
                    let owner = if r.plan.cloud.is_some() {
                        "cloud"
                    } else {
                        "local"
                    };
                    entries.push((id, owner.to_string()));
                }
            }
        }
        let l = protocol::build_model_list(&entries, mangosteen_core::time::now_unix());
        Response::json(200, &l)
    }
    pub(crate) fn status(&self) -> Response {
        let gw = self.gateway.as_ref();
        match gw.status() {
            Ok(s) => {
                let mut active: Vec<serde_json::Value> = s.active.iter().map(|a| serde_json::json!({
                    "id": a.id,
                    "model": a.model,
                    "origin": a.origin.label(),
                    "origin_kind": "cloud",
                    "node_ip": match &a.origin {
                        mangosteen_core::plan::ComputeOrigin::Cloud { .. } => gw
                            .get_deployment(&a.id)
                            .ok()
                            .flatten()
                            .and_then(|r| r.plan.cloud.as_ref().and_then(|c| c.node_ip.clone())),
                    },
                    "remote_port": match &a.origin {
                        mangosteen_core::plan::ComputeOrigin::Cloud { .. } => gw
                            .get_deployment(&a.id)
                            .ok()
                            .flatten()
                            .and_then(|r| r.plan.cloud.as_ref().map(|c| c.engine_port)),
                    },
                    "status": a.status.as_str(),
                    "port": a.port,
                    "ready": a.ready,
                })).collect();
                let mut remotes = gw.config().remote_inferences.clone();
                if let Some(remote) = gw.config().remote_inference.clone() {
                    remotes.push(remote);
                }
                for remote in remotes {
                    if !active.iter().any(|item| {
                        item.get("model").and_then(|v| v.as_str()) == Some(remote.model_id.as_str())
                    }) {
                        active.push(serde_json::json!({
                            "id": format!("remote-{}", remote.model_id),
                            "model": remote.model_id,
                            "origin": "远端推理服务",
                            "origin_kind": "cloud",
                            "node_ip": remote.host,
                            "remote_port": remote.remote_port,
                            "status": "ready",
                            "port": serde_json::Value::Null,
                            "ready": true,
                        }));
                    }
                }
                let payload = serde_json::json!({
                    "version": s.version,
                    "hardware": s.hardware,
                    "providers": s.providers,
                    "active": active,
                });
                Response::json(200, &payload)
            }
            Err(e) => Response::error(&e),
        }
    }
    /// 环境自检（供 UI 的「环境自检」按钮使用）。
    ///
    /// 与 `mangosteen doctor` 共用网关层的诊断能力，避免两处判断分叉。
    /// 本地 llama-server 启动路径已撤销（本地推理交由 Ollama 承担），
    /// 自检不再包含本地引擎项，聚焦云端部署能力。
    pub(crate) fn doctor(&self) -> Response {
        let cfg = {
            let gw = self.gateway.as_ref();
            gw.config().clone()
        };

        let mut checks: Vec<serde_json::Value> = Vec::new();

        // 1. 云服务商（云端部署是唯一部署路径，凭证缺失必须显眼提示）
        let (registered, skipped) = crate::probe_providers(&cfg);
        if registered.is_empty() {
            checks.push(serde_json::json!({
                "name": "云服务商",
                "status": "warn",
                "detail": if cfg.providers.is_empty() {
                    "未配置任何服务商".to_string()
                } else {
                    format!("{} 个已启用但均不可用：{}", skipped.len(),
                        skipped.iter().map(|(n, r)| format!("{n}（{r}）")).collect::<Vec<_>>().join("；"))
                },
                "hint": "云端部署无法进行。检查 config.providers 凭证",
            }));
        } else {
            checks.push(serde_json::json!({
                "name": "云服务商",
                "status": "ok",
                "detail": format!("已就绪：{}", registered.join(", ")),
            }));
        }

        // 2. 模型库
        let model_count = {
            let gw = self.gateway.as_ref();
            gw.models().list().map(|l| l.len()).unwrap_or(0)
        };
        checks.push(serde_json::json!({
            "name": "模型库",
            "status": if model_count > 0 { "ok" } else { "warn" },
            "detail": format!("{model_count} 个模型"),
            "hint": if model_count > 0 { serde_json::Value::Null } else {
                serde_json::json!("执行 `mangosteen models pull <仓库>/<文件.gguf>` 下载模型")
            },
        }));

        Response::json(200, &serde_json::json!({ "checks": checks }))
    }
    /// 系统资源快照（device 页签数据源）。
    ///
    /// 每次都重新采样，不做缓存：监控界面的价值就是「现在」。
    /// 采样成本是两次 sysctl/读 `/proc`，微秒级。
    pub(crate) fn sysinfo(&self) -> Response {
        let snap = self.sampler.sample();

        // 各活跃部署的进程资源占用
        let deps = {
            let gw = self.gateway.as_ref();
            let models_dir = mangosteen_platform::paths::models_dir(&mangosteen_platform::paths::real_env);
            gw.list_deployments()
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    let pid = r.pid;
                    serde_json::json!({
                        "id": r.plan.id,
                        "model": mangosteen_core::model::ModelEntry::derive_id(
                            &r.plan.model.name, r.plan.model.quant),
                        "origin": r.plan.origin.label(),
                        "status": r.status.as_str(),
                        "port": r.plan.local_access_port(),
                        "pid": pid,
                        "rss_bytes": pid.and_then(mangosteen_platform::resmon::process_rss_bytes),
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|v| (v, models_dir.clone()))
                .collect::<Vec<_>>()
        };

        let dep_json: Vec<serde_json::Value> = deps.into_iter().map(|(v, _)| v).collect();

        let hardware = {
            let gw = self.gateway.as_ref();
            let hw = gw.assessor().hardware().clone();
            let models_dir = mangosteen_platform::paths::models_dir(&mangosteen_platform::paths::real_env);
            (hw, models_dir)
        };

        let disk_free = mangosteen_platform::resmon::available_space(&hardware.1).ok();

        // 云端监控：只读缓存（后台线程每 5 秒刷新一次）。
        //
        // 绝不在请求路径里调厂商接口：设备页每 2 秒轮询一次，
        // 每次都打云端会让页面明显卡顿并触发厂商限流。
        let cloud = self.cloud_section();

        Response::json(
            200,
            &serde_json::json!({
                "cpu_usage": snap.cpu_usage,
                "physical_cores": snap.physical_cores,
                "logical_cores": snap.logical_cores,
                "total_memory_bytes": snap.total_memory_bytes,
                "available_memory_bytes": snap.available_memory_bytes,
                "memory_usage": snap.memory_usage(),
                "sampled_at_ms": snap.sampled_at_ms,
                "os": hardware.0.os,
                "arch": hardware.0.arch,
                "cpu_model": hardware.0.host.cpu_model,
                "accelerators": hardware.0.accelerators.iter().map(|a| serde_json::json!({
                    "name": a.name,
                    "vendor": format!("{:?}", a.vendor),
                    "vram_bytes": a.usable_accelerator_memory(),
                })).collect::<Vec<_>>(),
                "disk_free_bytes": disk_free,
                "deployments": dep_json,
                "cloud": cloud,
            }),
        )
    }
    /// 设备页的「云端算力」段。
    ///
    /// 返回 `active: false` 表示**当前用的是本地算力**——这不是错误，
    /// 前端据此隐藏云端区域。只有真的查询失败时才带 `error`。
    pub(crate) fn cloud_section(&self) -> serde_json::Value {
        let (snapshot, error, age_ms) = self.cloud_monitor.read();

        let Some(snap) = snapshot else {
            // 无快照：要么本地部署，要么首轮采集还没完成。
            // 用 `pending` 让前端区分「不适用」与「稍等就有」。
            return serde_json::json!({
                "active": false,
                "pending": age_ms.is_none(),
                "error": error,
            });
        };

        // 指标保持 0~1 比例，与本地 `cpu_usage` 语义一致，格式化交给前端。
        let metrics = snap.metrics.as_ref().map(|m| {
            serde_json::json!({
                "cpu_usage": m.cpu_usage,
                "memory_usage": m.memory_usage,
                "gpu_usage": m.gpu_usage,
                "gpu_memory_usage": m.gpu_memory_usage,
                "disk_usage": m.disk_usage,
                "data_disk_usage": m.data_disk_usage,
                "sampled_at": m.sampled_at,
                "has_any": m.has_any(),
            })
        });

        serde_json::json!({
            "active": true,
            "pending": false,
            "deployment_id": snap.deployment_id,
            "provider": snap.provider,
            "instance_id": snap.instance_id,
            "model": snap.model,
            "status": snap.status.as_str(),
            "metrics": metrics,
            // 该服务商是否提供监控（`metrics` 为 null 即不支持）
            "supported": snap.metrics.is_some(),
            "age_ms": age_ms,
            "error": error.or(snap.error),
        })
    }
}
