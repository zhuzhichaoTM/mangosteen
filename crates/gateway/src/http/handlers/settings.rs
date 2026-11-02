//! 配置读写处理器（`/api/settings`）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::request::percent_decode_path;
use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;

impl GatewayServer {
    pub(crate) fn get_devices(&self) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let devices: Vec<serde_json::Value> = gw
            .config()
            .remote_devices
            .iter()
            .map(|d| serde_json::json!({
                "id": d.id,
                "name": d.name,
                "host": d.host,
                "port": d.port,
                "user": d.user,
                "workdir": d.workdir,
                "password_configured": !matches!(d.password, mangosteen_core::config::SecretRef::None),
            }))
            .collect();
        Response::json(200, &serde_json::json!({ "devices": devices }))
    }

    pub(crate) fn add_device(&self, req: &ParsedRequest) -> Response {
        let body: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let mut body = body;
        let password = body
            .as_object_mut()
            .and_then(|o| o.remove("password"))
            .and_then(|v| v.as_str().map(str::to_string));
        let mut device: mangosteen_core::config::RemoteDevice = match serde_json::from_value(body) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("设备参数无效：{e}"), None),
                )
            }
        };
        if let Some(password) = password.filter(|v| !v.is_empty()) {
            device.password = mangosteen_core::config::SecretRef::Inline(password);
        }
        if device.created_at == 0 {
            device.created_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
        }
        let device = match device.normalized() {
            Ok(v) => v,
            Err(e) => return Response::error(&e),
        };
        let (mut cfg, store) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            (gw.config().clone(), gw.config_store())
        };
        if let Some(slot) = cfg.remote_devices.iter_mut().find(|d| d.id == device.id) {
            *slot = device.clone();
        } else {
            cfg.remote_devices.push(device.clone());
        }
        if let Err(e) = cfg.validate() {
            return Response::error(&e);
        }
        match store.save(&cfg) {
            Ok(path) => {
                if let Ok(mut gw) = self.gateway.lock() {
                    gw.replace_config(cfg);
                }
                Response::json(
                    200,
                    &serde_json::json!({ "ok": true, "id": device.id, "host": device.host, "port": device.port, "user": device.user, "path": path.display().to_string() }),
                )
            }
            Err(e) => Response::error(&e),
        }
    }

    pub(crate) fn test_device(&self, id: &str) -> Response {
        let id = percent_decode_path(id);
        let device = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            gw.config()
                .remote_devices
                .iter()
                .find(|d| d.id == id)
                .cloned()
        };
        let Some(device) = device else {
            return Response::json(404, &ErrorResponse::bad_request("设备不存在", Some("id")));
        };
        let target =
            mangosteen_orchestration::DeviceTarget::from_config(&device, "/root/mangosteen");
        match mangosteen_device::remote::ssh_exec(&target.ssh, "true", 30) {
            Ok(_) => Response::json(
                200,
                &serde_json::json!({
                    "ok": true,
                    "message": format!("节点 {} 连接成功", device.name)
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }
    pub(crate) fn remove_device(&self, id: &str) -> Response {
        let id = percent_decode_path(id);
        let (mut cfg, store) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            (gw.config().clone(), gw.config_store())
        };
        let before = cfg.remote_devices.len();
        let removed = cfg
            .remote_devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| (d.id.clone(), d.host.clone()));
        cfg.remote_devices.retain(|d| d.id != id);
        if before == cfg.remote_devices.len() {
            return Response::json(404, &ErrorResponse::bad_request("设备不存在", Some("id")));
        }
        if let Err(e) = cfg.validate() {
            return Response::error(&e);
        }

        // 删除节点前先释放该节点上由本工具创建的云实例。
        //
        // 自定义节点本身是用户的机器，单纯从列表移除并不等于销毁实例；
        // 但如果这台机器上跑着我们用云 API 开出来的实例，不释放就会一直
        // 计费。这里按 device_id / 节点 IP 反查部署台账并销毁对应实例；
        // 销毁失败则中止删除，避免出现「节点没了但机器还在扣钱」。
        let destroyed = {
            let mut gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            match removed.as_ref() {
                Some((device_id, host)) => {
                    match gw.destroy_instances_bound_to_device(device_id, host) {
                        Ok(list) => list,
                        Err(e) => return Response::error(&e),
                    }
                }
                None => Vec::new(),
            }
        };

        match store.save(&cfg) {
            Ok(_) => {
                if let Ok(mut gw) = self.gateway.lock() {
                    gw.replace_config(cfg);
                }
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "id": id,
                        "destroyed_instances": destroyed,
                    }),
                )
            }
            Err(e) => Response::error(&e),
        }
    }

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
                    // 公钥是公开标识（参与签名但不算机密），可回显；
                    // 私钥绝不回显，只给 secret_configured 状态。
                    "public_key": p.options.get("public_key"),
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
                    "local_first": false,
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
                "model_whitelist": cfg.model_whitelist.iter().map(|(id, m)| serde_json::json!({"id": id, "display_name": m.display_name, "source_spec": m.source_spec, "template": m.template})).collect::<Vec<_>>(),
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

        // ---- providers.<name>.enabled / options / secret_value ----
        //
        // options 合并写入（驱动专属参数，如优云的 public_key/region）；
        // secret_value 是私钥明文入口：写入 secrets 目录（0700/0600）后
        // 把 SecretRef 指到该文件，配置文件里**绝不落明文**，也不回显。
        if let Some(ps) = patch.get("providers").and_then(|v| v.as_object()) {
            for (name, p) in ps {
                let Some(slot) = cfg.providers.get_mut(name) else {
                    continue;
                };
                if let Some(enabled) = p.get("enabled").and_then(|v| v.as_bool()) {
                    slot.enabled = enabled;
                    applied.push(format!("providers.{name}.enabled"));
                }
                if let Some(opts) = p.get("options").and_then(|v| v.as_object()) {
                    for (k, v) in opts {
                        if let Some(s) = v.as_str() {
                            slot.options.insert(k.clone(), s.to_string());
                            applied.push(format!("providers.{name}.options.{k}"));
                        }
                    }
                }
                if let Some(secret) = p.get("secret_value").and_then(|v| v.as_str()) {
                    let secret = secret.trim();
                    if !secret.is_empty() {
                        match write_provider_secret(name, secret) {
                            Ok(path) => {
                                slot.secret =
                                    mangosteen_core::config::SecretRef::File(path);
                                applied.push(format!("providers.{name}.secret"));
                            }
                            Err(e) => return Response::error(&e),
                        }
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
                // 厂商密钥/参数有变更时热重建云端驱动注册表（不动隧道，
                // 不影响活跃部署）；未生效时给出原因供 UI 提示。
                let providers_reload = if applied.iter().any(|a| a.starts_with("providers."))
                {
                    match self.gateway.lock() {
                        Ok(mut gw) => {
                            let (hot, skipped) = gw.reload_cloud_drivers();
                            Some(serde_json::json!({
                                "hot_reload": hot,
                                "skipped": skipped,
                            }))
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };
                let mut body = serde_json::json!({
                    "ok": true,
                    "path": path.display().to_string(),
                    "applied": applied,
                });
                if let Some(r) = providers_reload {
                    body["providers_reload"] = r;
                }
                Response::json(200, &body)
            }
            Err(e) => Response::error(&e),
        }
    }
}

/// 把厂商私钥写入用户 secrets 目录（0700 目录 / 0600 文件），返回路径。
///
/// 为什么不写 Inline 到 config.toml：配置文件会被备份/复制/进仓库，
/// 明文密钥一旦扩散无法收回；secrets 目录按基线独立存放（0700）。
fn write_provider_secret(
    provider: &str,
    secret: &str,
) -> mangosteen_core::MnResult<std::path::PathBuf> {
    let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
    let dir = mangosteen_platform::paths::config_dir(env).join("secrets");
    mangosteen_platform::fs::ensure_private_dir(&dir)?;
    let path = dir.join(format!("{provider}_private_key"));
    mangosteen_platform::fs::write_private_file(&path, secret)?;
    Ok(path)
}
