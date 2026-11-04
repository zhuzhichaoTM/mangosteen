//! 模型配置模板读写处理器（`/api/model-templates`）。
//!
//! 归属：第二层 API Server。
//!
//! ## 为什么单独一组接口，而不塞进 `/api/settings`
//!
//! 模板是**部署的唯一真源**（下哪个仓库的哪个文件、用什么参数启动），
//! 它的读写需要：
//! * 保存前用对应模型的 schema 试解析，把「模板写错了」挡在写入之前；
//! * 返回时带上解析出的摘要（下载地址、文件名），让用户在界面上直接
//!   确认「我改的配置真的会生效」。
//!
//! 这两件事都不属于通用配置补丁的语义，因此独立成接口。

use crate::http::request::percent_decode_path;
use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;

impl GatewayServer {
    /// 列出全部模型的配置模板。
    ///
    /// 返回结构：
    /// ```json
    /// { "models": [ { "model": "qwen3.8-27b", "display_name": "Qwen3.8-27B",
    ///                 "templates": [ { "slot": "mode1", "name": "Uncensored",
    ///                                  "text": "...", "summary": {...} } ] } ],
    ///   "supported": ["qwen2.5-0.5b", "qwen3.8-27b"] }
    /// ```
    pub(crate) fn list_model_templates(&self) -> Response {
        let gw = self.gateway.as_ref();
        let cfg = gw.config();
        let models: Vec<serde_json::Value> = cfg
            .model_whitelist
            .iter()
            .map(|(model, wl)| {
                let templates: Vec<serde_json::Value> = wl
                    .templates
                    .iter()
                    .map(|(slot, text)| {
                        serde_json::json!({
                            "slot": slot,
                            "name": template_display_name(text),
                            // 是否内置模板：内置的由系统随版本提供，用户新增的才是
                            // 他自己的配置。设置页只列用户新增的，内置的只在
                            // 部署页下拉框里出现（不可误删）。
                            "builtin": is_builtin_template(model, slot),
                            "text": text,
                            // 解析摘要：用户在设置页就能看到「这份配置会下哪个文件」，
                            // 不必等部署时才发现选错了模板。
                            "summary": template_summary(model, text),
                        })
                    })
                    .collect();
                serde_json::json!({
                    "model": model,
                    "display_name": wl.display_name,
                    "templates": templates,
                })
            })
            .collect();
        Response::json(
            200,
            &serde_json::json!({
                "models": models,
                "supported": mangosteen_core::template::supported_models(),
            }),
        )
    }

    /// 新增或更新一个配置模板。
    ///
    /// 请求体：`{ "model": "键名a", "slot": "键名b", "text": "YAML 正文" }`
    ///
    /// ## 写入前必须试解析
    ///
    /// 模板语法错、缺 `model`/`source` 时立刻拒绝并指出行号——让错误停在
    /// 设置页，而不是等到部署跑到「模型下载」才失败。这一步同时保证
    /// 「保存成功的模板一定能被部署链路消费」。
    pub(crate) fn put_model_template(&self, req: &ParsedRequest) -> Response {
        let body: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(model) = body
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 model（模型名，键名a）", Some("model")),
            );
        };
        let Some(slot) = body
            .get("slot")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 slot（配置模板名，键名b）", Some("slot")),
            );
        };
        let Some(text) = body
            .get("text")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        else {
            return Response::json(
                400,
                &ErrorResponse::bad_request("缺少 text（配置模板正文）", Some("text")),
            );
        };

        // 键名 (模型名, 模板名) 不得与**系统内置**模板重叠。
        //
        // 内置模板随版本发布，是部署的基线方案；允许用户用同名覆盖会造成
        // 两个后果：① 升级时我们无法安全地更新内置配置（分不清是系统版本
        // 还是用户改过）；② 用户以为「我新建了一份」，实际是悄悄改掉了
        // 基线，出问题时无从对照。因此要求自定义必须另起名字。
        //
        // 用户自定义之间同名则允许——那是「我再存一次」，后面的替换前面的，
        // 符合编辑预期。
        if is_builtin_template(model, slot) {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    format!(
                        "键名与系统内置模板重叠：模型 `{model}` 下已内置模板 `{slot}`。\
                         自定义模板请换一个模板名（如 `{slot}-custom`）"
                    ),
                    Some("slot"),
                ),
            );
        }

        // 试解析：语法错误 / 缺必需字段 / 模型不支持，都在写入前拦下。
        if let Err(e) = mangosteen_core::template::resolve(model, text) {
            return Response::json(
                400,
                &ErrorResponse::bad_request(format!("模板校验失败：{e}"), Some("text")),
            );
        }

        let (mut cfg, store) = {
            let gw = self.gateway.as_ref();
            (gw.config().as_ref().clone(), gw.config_store())
        };

        let entry = cfg
            .model_whitelist
            .entry(model.to_string())
            .or_insert_with(|| mangosteen_core::config::WhitelistedModel {
                display_name: model.to_string(),
                source_spec: String::new(),
                template: slot.to_string(),
                templates: Default::default(),
            });
        entry.templates.insert(slot.to_string(), text.to_string());

        if let Err(e) = cfg.validate() {
            return Response::error(&e);
        }
        if let Err(e) = persist_config(self, cfg.clone(), &store) {
            return Response::error(&e);
        }
        Response::json(
            200,
            &serde_json::json!({
                "ok": true,
                "model": model,
                "slot": slot,
                "name": template_display_name(text),
                "summary": template_summary(model, text),
            }),
        )
    }

    /// 删除一个配置模板。
    pub(crate) fn delete_model_template(&self, model: &str, slot: &str) -> Response {
        let model = percent_decode_path(model.trim());
        let slot = percent_decode_path(slot.trim());
        let (mut cfg, store) = {
            let gw = self.gateway.as_ref();
            (gw.config().as_ref().clone(), gw.config_store())
        };
        let Some(wl) = cfg.model_whitelist.get_mut(&model) else {
            return Response::json(
                404,
                &ErrorResponse::bad_request(format!("模型 `{model}` 不存在"), Some("model")),
            );
        };
        // 内置模板不可删除：它是系统随版本提供的基线配置，删掉会让该模型
        // 失去默认部署方案。用户想改内容请直接编辑（保存即覆盖）。
        if is_builtin_template(&model, &slot) {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    format!("`{slot}` 是内置模板，不能删除（可直接编辑内容）"),
                    Some("slot"),
                ),
            );
        }
        if wl.templates.remove(&slot).is_none() {
            return Response::json(
                404,
                &ErrorResponse::bad_request(
                    format!("模型 `{model}` 下没有模板 `{slot}`"),
                    Some("slot"),
                ),
            );
        }
        if let Err(e) = persist_config(self, cfg.clone(), &store) {
            return Response::error(&e);
        }
        Response::json(200, &serde_json::json!({ "ok": true }))
    }
}

/// 模板显示名：正文首行 `template:` 的值，取不到时返回 `default`。
fn template_display_name(text: &str) -> String {
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("template:") {
            let v = rest.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "default".to_string()
}

/// 是否为内置模板（由系统随版本提供）。
///
/// 判定依据是 `BUILTIN_TEMPLATES`——那是内置模板的唯一来源。
/// 用户自己新增的槽位一律返回 false，因此可以安全地被编辑/删除。
fn is_builtin_template(model: &str, slot: &str) -> bool {
    mangosteen_core::template::BUILTIN_TEMPLATES
        .iter()
        .any(|(m, s, _, _)| m.eq_ignore_ascii_case(model) && *s == slot)
}

/// 模板解析摘要：把「这份配置会做什么」摊平成 JSON，供界面直接展示。
///
/// 解析失败不报错——列表接口要能展示有问题的存量模板，让用户看到并修正。
fn template_summary(model: &str, text: &str) -> serde_json::Value {
    match mangosteen_core::template::resolve(model, text) {
        Ok(s) => serde_json::json!({
            "ok": true,
            "repo": mangosteen_core::template::parse_raw(text)
                .ok()
                .and_then(|r| r.get("repository").map(str::to_string)),
            "model_file": s.model_file,
            "download_url": s.download_url,
            "context": s.context,
            "gpu_layers": s.gpu_layers,
            "mmproj": s.mmproj,
            "draft_model": s.draft_model,
            "aux_count": s.aux_files.len(),
        }),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    }
}

/// 落盘并热更新内存配置。
///
/// 顺序不能反：先写盘（失败则内存不动，不产生「看起来生效、重启丢失」
/// 的假象），成功后再替换内存中的配置。
fn persist_config(
    server: &GatewayServer,
    cfg: mangosteen_core::config::Config,
    store: &mangosteen_base::config::ConfigStore,
) -> mangosteen_core::MnResult<()> {
    store.save(&cfg)?;
    server.gateway.replace_config(cfg);
    Ok(())
}
