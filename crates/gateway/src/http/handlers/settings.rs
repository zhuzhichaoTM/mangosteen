//! 配置读写处理器（`/api/settings`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;

impl GatewayServer {
    pub(crate) fn get_settings(&self) -> Response {
        let cfg = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            gw.config().clone()
        };

        let providers: Vec<serde_json::Value> = cfg
            .providers
            .iter()
            .map(|(name, p)| {
                serde_json::json!({
                    "name": name,
                    "driver": p.driver,
                    "enabled": p.enabled,
                    "display_name": p.display_name,
                    // 只回显凭证「形式」，绝不回显内容
                    "secret_kind": match &p.secret {
                        mangosteen_core::config::SecretRef::Env(v) => format!("env:{v}"),
                        mangosteen_core::config::SecretRef::File(f) => {
                            format!("file:{}", f.display())
                        }
                        mangosteen_core::config::SecretRef::Inline(_) => {
                            "inline:已配置".to_string()
                        }
                        mangosteen_core::config::SecretRef::None => "none:无需密钥".to_string(),
                    },
                    "secret_configured": p.secret.resolve(mangosteen_platform::paths::real_env).is_ok(),
                })
            })
            .collect();

        Response::json(
            200,
            &serde_json::json!({
                "gateway": {
                    "host": cfg.gateway.host,
                    "port": cfg.gateway.port,
                    "require_api_key": cfg.gateway.require_api_key,
                },
                "scheduler": {
                    "local_first": cfg.scheduler.local_first,
                    "max_fallbacks": cfg.scheduler.max_fallbacks,
                    "launch_timeout_secs": cfg.scheduler.launch_timeout_secs,
                    "max_price_per_hour": cfg.scheduler.max_price_per_hour,
                },
                "local": {
                    // 推理引擎：当前值与全部可选项（含是否已适配、所需格式）
                    "engine": cfg.local.engine.id(),
                    "engine_display": cfg.local.engine.display_name(),
                    "engine_supported": cfg.local.engine.is_supported(),
                    "engines": mangosteen_core::config::InferenceEngine::all()
                        .iter()
                        .map(|e| serde_json::json!({
                            "id": e.id(),
                            "name": e.display_name(),
                            "supported": e.is_supported(),
                            "format": e.required_format(),
                            "binary": e.binary_name(),
                        }))
                        .collect::<Vec<_>>(),
                    "engine_path": cfg.local.engine_path.as_ref().map(|p| p.display().to_string()),
                    "default_context": cfg.local.default_context,
                    "min_context": cfg.local.min_context,
                    "kv_cache": cfg.local.kv_cache.as_cli_value(),
                    "batch_size": cfg.local.batch_size,
                    "allow_cpu_fallback": cfg.local.allow_cpu_fallback,
                    "allow_partial_offload": cfg.local.allow_partial_offload,
                    "auto_library_path": cfg.local.auto_library_path,
                },
                "storage": {
                    "models_dir": cfg.storage.models_dir.display().to_string(),
                    "cache_dir": cfg.storage.cache_dir.display().to_string(),
                    "default_source": cfg.storage.default_source,
                    "verify_hash": cfg.storage.verify_hash,
                    "max_model_bytes": cfg.storage.max_model_bytes,
                },
                "providers": providers,
                "model_sources": cfg.model_sources.keys().collect::<Vec<_>>(),
            }),
        )
    }
    /// 写入设置。
    ///
    /// 只接受**白名单字段**，且写入前必须通过 `Config::validate()`。
    /// 不做「任意 TOML 覆盖」——那等于给页面一个任意配置写入口，
    /// 一旦网关被暴露到局域网就是提权面。
    pub(crate) fn put_settings(&self, req: &ParsedRequest) -> Response {
        let patch: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };

        let (mut cfg, store) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            (gw.config().clone(), gw.config_store())
        };

        let mut applied: Vec<String> = Vec::new();

        // ---- scheduler.local_first ----
        if let Some(v) = patch
            .get("scheduler")
            .and_then(|s| s.get("local_first"))
            .and_then(|v| v.as_bool())
        {
            cfg.scheduler.local_first = v;
            applied.push("scheduler.local_first".into());
        }
        if let Some(v) = patch
            .get("scheduler")
            .and_then(|s| s.get("max_price_per_hour"))
            .and_then(|v| v.as_f64())
        {
            cfg.scheduler.max_price_per_hour = Some(v);
            applied.push("scheduler.max_price_per_hour".into());
        }

        // ---- local.* ----
        if let Some(l) = patch.get("local") {
            if let Some(v) = l.get("default_context").and_then(|v| v.as_u64()) {
                cfg.local.default_context = v as u32;
                applied.push("local.default_context".into());
            }
            if let Some(v) = l.get("kv_cache").and_then(|v| v.as_str()) {
                match v {
                    "f16" => cfg.local.kv_cache = mangosteen_core::estimate::KvCacheType::F16,
                    "q8_0" => cfg.local.kv_cache = mangosteen_core::estimate::KvCacheType::Q8_0,
                    "q4_0" => cfg.local.kv_cache = mangosteen_core::estimate::KvCacheType::Q4_0,
                    other => {
                        return Response::json(
                            400,
                            &ErrorResponse::bad_request(
                                format!("未知 kv_cache `{other}`，可选 f16 / q8_0 / q4_0"),
                                Some("local.kv_cache"),
                            ),
                        )
                    }
                }
                applied.push("local.kv_cache".into());
            }
            if let Some(v) = l.get("batch_size").and_then(|v| v.as_u64()) {
                cfg.local.batch_size = v as u32;
                applied.push("local.batch_size".into());
            }
            if let Some(v) = l.get("allow_cpu_fallback").and_then(|v| v.as_bool()) {
                cfg.local.allow_cpu_fallback = v;
                applied.push("local.allow_cpu_fallback".into());
            }
            if let Some(v) = l.get("allow_partial_offload").and_then(|v| v.as_bool()) {
                cfg.local.allow_partial_offload = v;
                applied.push("local.allow_partial_offload".into());
            }
            // 推理引擎切换
            if let Some(v) = l.get("engine").and_then(|v| v.as_str()) {
                match mangosteen_core::config::InferenceEngine::parse(v) {
                    Some(e) => {
                        cfg.local.engine = e;
                        applied.push("local.engine".into());
                    }
                    None => {
                        return Response::json(
                            400,
                            &ErrorResponse::bad_request(
                                format!(
                                    "未知推理引擎 `{v}`，可选 {}",
                                    mangosteen_core::config::InferenceEngine::all()
                                        .iter()
                                        .map(|e| e.id())
                                        .collect::<Vec<_>>()
                                        .join(" / ")
                                ),
                                Some("local.engine"),
                            ),
                        )
                    }
                }
            }
        }

        // ---- storage.default_source ----
        if let Some(v) = patch
            .get("storage")
            .and_then(|s| s.get("default_source"))
            .and_then(|v| v.as_str())
        {
            cfg.storage.default_source = v.to_string();
            applied.push("storage.default_source".into());
        }

        // ---- providers.<name>.enabled ----
        if let Some(ps) = patch.get("providers").and_then(|v| v.as_object()) {
            for (name, p) in ps {
                if let Some(enabled) = p.get("enabled").and_then(|v| v.as_bool()) {
                    if let Some(slot) = cfg.providers.get_mut(name) {
                        slot.enabled = enabled;
                        applied.push(format!("providers.{name}.enabled"));
                    }
                }
            }
        }

        if applied.is_empty() {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "没有任何可识别的字段被修改。支持的字段见 GET /api/settings",
                    None,
                ),
            );
        }

        // 校验后才落盘：非法配置绝不能写进文件，否则用户下次启动直接失败
        if let Err(e) = cfg.validate() {
            return Response::error(&e);
        }
        match store.save(&cfg) {
            Ok(path) => {
                // 同步更新内存中的配置：否则用户改完设置要重启才生效
                if let Ok(mut gw) = self.gateway.lock() {
                    gw.replace_config(cfg);
                }
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "path": path.display().to_string(),
                        "applied": applied,
                    }),
                )
            }
            Err(e) => Response::error(&e),
        }
    }
}
