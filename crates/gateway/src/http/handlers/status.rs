//! 状态与诊断处理器（`/health`、`/v1/models`、`/api/status`、`/api/doctor`、`/api/sysinfo`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::server::*;
use crate::protocol;
use mangosteen_core::openai::HealthResponse;

impl GatewayServer {
    pub(crate) fn health(&self) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
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
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let entries: Vec<(String, String)> = match gw.models().list() {
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
        let l = protocol::build_model_list(&entries, mangosteen_core::time::now_unix());
        Response::json(200, &l)
    }
    pub(crate) fn status(&self) -> Response {
        let mut gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        match gw.status() {
            Ok(s) => {
                let payload = serde_json::json!({
                    "version": s.version,
                    "hardware": s.hardware,
                    "providers": s.providers,
                    "engine_path": s.engine_path.map(|p| p.display().to_string()),
                    "engine_version": s.engine_version,
                    "active": s.active.iter().map(|a| serde_json::json!({
                        "id": a.id,
                        "model": a.model,
                        "origin": a.origin.label(),
                        "status": a.status.as_str(),
                        "port": a.port,
                        "ready": a.ready,
                    })).collect::<Vec<_>>(),
                });
                Response::json(200, &payload)
            }
            Err(e) => Response::error(&e),
        }
    }
    /// 环境自检（供 UI 的「环境自检」按钮使用）。
    ///
    /// 与 `mangosteen doctor` 共用网关层的诊断能力，避免两处判断分叉。
    pub(crate) fn doctor(&self) -> Response {
        // 取配置与缓存的引擎版本：两者都在锁内读一次，随后锁外使用
        let (cfg, cached_version) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            (gw.config().clone(), gw.engine_version())
        };

        let mut checks: Vec<serde_json::Value> = Vec::new();

        // 1. 引擎
        //
        // 注意：**不能**调 `diagnose_engine`——它会同步执行
        // `llama-server --version`，在 Homebrew 版上要 27 秒，
        // 直接把本接口拖到超时。这里改成用预热缓存回答，
        // 缓存为空时如实说「仍在探测」，而不是让请求挂住。
        let diag = crate::diagnose_engine_cached(Some(&cfg), cached_version);
        // 三种状态必须分开，否则「还在探测」会被误报成「引擎坏了」：
        //   有 binary + 有版本  → ok
        //   有 binary + 无版本  → 探测中（预热还没跑完），不是失败
        //   无 binary           → 确实没装
        match (&diag.binary, &diag.version) {
            (Some(b), Some(v)) => checks.push(serde_json::json!({
                "name": "本地推理引擎",
                "status": "ok",
                "detail": format!("{} —— {v}", b.display()),
            })),
            (Some(b), None) => checks.push(serde_json::json!({
                "name": "本地推理引擎",
                "status": "warn",
                "detail": format!("{} 已找到，正在后台探测版本…", b.display()),
                "hint": "引擎版本探测需加载全部动态库，本机约需 20~30 秒；稍后刷新即可",
            })),
            _ => checks.push(serde_json::json!({
                "name": "本地推理引擎",
                "status": "fail",
                "detail": "未找到 llama-server",
                "hint": "brew install llama.cpp，或用 local.engine_path 指定路径",
            })),
        }

        // 2. 引擎库路径（能跑起来的原因值得说清楚）
        if let Some(v) = &diag.library_path {
            checks.push(serde_json::json!({
                "name": "引擎运行库路径",
                "status": "ok",
                "detail": format!("注入 {}={v}", diag.library_path_var),
            }));
        }

        // 3. 本地优先策略
        checks.push(serde_json::json!({
            "name": "本地优先策略",
            "status": if cfg.scheduler.local_first { "ok" } else { "warn" },
            "detail": if cfg.scheduler.local_first {
                "已启用（符合产品基线）".to_string()
            } else {
                "被配置关闭，将不再优先尝试本地部署".to_string()
            },
            "hint": if cfg.scheduler.local_first { serde_json::Value::Null } else {
                serde_json::json!("把 scheduler.local_first 改回 true")
            },
        }));

        // 4. 云服务商
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
                "hint": "本地资源不足时将无法降级。检查 config.providers 凭证",
            }));
        } else {
            checks.push(serde_json::json!({
                "name": "云服务商",
                "status": "ok",
                "detail": format!("已就绪：{}", registered.join(", ")),
            }));
        }

        // 5. 模型库
        let model_count = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
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
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            let models_dir = gw.config().storage.models_dir.clone();
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
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            let hw = gw.assessor().hardware().clone();
            let models_dir = gw.config().storage.models_dir.clone();
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
