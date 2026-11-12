//! `mangosteen models`
//!
//! 归属：**用户交互层 → 命令 `models`**。
//!
//! 管理模型配置模板（设置页「模型配置」的 CLI 对应）。
//! 模板是部署的唯一真源：下哪个仓库的哪个文件、用什么参数启动，全部由它决定。
//!
//! 子命令：
//! | 子命令 | 作用 |
//! |--------|------|
//! | `list` | 列出全部模型的配置模板 |
//! | `add` | 新增或更新一个配置模板 |
//! | `remove` | 删除一个自定义配置模板 |
//! | `show` | 显示指定模板的正文和解析摘要 |

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{build_gateway, reject_unknown_options};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "json",
    "quiet",
    "force",
    "slot",
    "file",
];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_models_help();
        return Ok(0);
    }
    reject_unknown_options(&args, ALLOWED)?;

    let sub = args.pos(1).unwrap_or("list").to_string();
    match sub.as_str() {
        "list" | "ls" => {
            if args.has("help") {
                print_models_list_help();
                return Ok(0);
            }
            list(args)
        }
        "add" => {
            if args.has("help") {
                print_models_add_help();
                return Ok(0);
            }
            add(args)
        }
        "remove" | "rm" => {
            if args.has("help") {
                print_models_remove_help();
                return Ok(0);
            }
            remove(args)
        }
        "show" => {
            if args.has("help") {
                print_models_show_help();
                return Ok(0);
            }
            show(args)
        }
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `models {other}`。可用：list / add / remove / show"
        ))),
    }
}

/// `models list` - 列出全部模型的配置模板。
fn list(args: Args) -> MnResult<i32> {
    let gw = build_gateway(&args)?;
    let cfg = gw.config();

    if args.has("json") {
        let models: Vec<serde_json::Value> = cfg
            .model_whitelist
            .iter()
            .map(|(model, wl)| {
                let templates: Vec<serde_json::Value> = wl
                    .templates
                    .iter()
                    .map(|(slot, text)| {
                        let builtin = mangosteen_core::template::BUILTIN_TEMPLATES
                            .iter()
                            .any(|(m, s, _, _)| m.eq_ignore_ascii_case(model) && *s == slot);
                        serde_json::json!({
                            "slot": slot,
                            "builtin": builtin,
                            "display_name": template_display_name(text),
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
        let json = serde_json::to_string_pretty(&serde_json::json!({ "models": models }))
            .map_err(|e| mangosteen_core::MnError::bad_request(format!("序列化失败：{e}")))?;
        println!("{}", json);
        return Ok(0);
    }

    println!("{}", output::bold("模型配置模板"));
    println!();

    for (model, wl) in &cfg.model_whitelist {
        println!("  {} ({})", output::paint(model, output::Color::Cyan), wl.display_name);
        for (slot, text) in &wl.templates {
            let builtin = mangosteen_core::template::BUILTIN_TEMPLATES
                .iter()
                .any(|(m, s, _, _)| m.eq_ignore_ascii_case(model) && *s == slot);
            let tag = if builtin { "内置" } else { "自定义" };
            let name = template_display_name(text);
            println!("    {} [{}] {}", slot, tag, name);
        }
        println!();
    }

    println!("{}", output::dim("用 `mangosteen models show <模型> --slot <槽位>` 查看模板详情"));
    println!("{}", output::dim("用 `mangosteen models add <模型> --slot <槽位> --file <文件>` 新增/更新模板"));
    println!("{}", output::dim("用 `mangosteen models remove <模型> --slot <槽位>` 删除自定义模板"));

    Ok(0)
}

/// `models add` - 新增或更新一个配置模板。
fn add(args: Args) -> MnResult<i32> {
    let model = args
        .pos(2)
        .map(|s| s.to_string())
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少模型参数。用法：mangosteen models add <模型> --slot <槽位> --file <文件>"
        ))?;

    let slot = args
        .get("slot")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少 --slot 参数（配置模板名，如 default / uncensored）"
        ))?;

    let file_path = args
        .get("file")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少 --file 参数（模板文件路径）"
        ))?;

    let text = std::fs::read_to_string(&file_path)
        .map_err(|e| mangosteen_core::MnError::bad_request(
            format!("读取文件失败：{e}")
        ))?;

    if text.trim().is_empty() {
        return Err(mangosteen_core::MnError::bad_request("模板文件内容为空"));
    }

    // 校验模板语法
    mangosteen_core::template::resolve(&model, &text)
        .map_err(|e| mangosteen_core::MnError::bad_request(format!("模板校验失败：{e}")))?;

    // 检查是否与内置模板重名
    let is_builtin = mangosteen_core::template::BUILTIN_TEMPLATES
        .iter()
        .any(|(m, s, _, _)| m.eq_ignore_ascii_case(&model) && *s == slot);
    if is_builtin && !args.has("force") {
        return Err(mangosteen_core::MnError::bad_request(
            format!("模板 `{slot}` 是内置模板，不能覆盖。如要修改，请用 --force 强制覆盖，或换一个槽位名")
        ));
    }

    // 保存到配置
    let (mut cfg, store) = {
        let gw = build_gateway(&args)?;
        (gw.config().as_ref().clone(), gw.config_store())
    };

    let entry = cfg
        .model_whitelist
        .entry(model.clone())
        .or_insert_with(|| mangosteen_core::config::WhitelistedModel {
            display_name: model.clone(),
            source_spec: String::new(),
            template: slot.clone(),
            templates: Default::default(),
        });
    entry.templates.insert(slot.clone(), text.clone());

    cfg.validate()?;
    store.save(&cfg)?;

    output::print_success(&format!(
        "模板已保存：{} / {}",
        model, slot
    ));
    println!();
    println!("{}", output::bold("解析摘要"));
    if let Ok(spec) = mangosteen_core::template::resolve(&model, &text) {
        println!("  模型文件      {}", spec.model_file);
        println!("  下载地址      {}", spec.download_url);
        println!("  上下文         {}", spec.context);
        println!("  GPU 层数      {}", spec.gpu_layers);
        if let Some(mmproj) = &spec.mmproj {
            println!("  多模态投影    {}", mmproj);
        }
        if let Some(draft) = &spec.draft_model {
            println!("  草稿模型      {}", draft);
        }
        if !spec.aux_files.is_empty() {
            println!("  辅助文件      {} 个", spec.aux_files.len());
        }
    }

    Ok(0)
}

/// `models remove` - 删除一个自定义配置模板。
fn remove(args: Args) -> MnResult<i32> {
    let model = args
        .pos(2)
        .map(|s| s.to_string())
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少模型参数。用法：mangosteen models remove <模型> --slot <槽位>"
        ))?;

    let slot = args
        .get("slot")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少 --slot 参数（配置模板名）"
        ))?;

    // 检查是否是内置模板
    let is_builtin = mangosteen_core::template::BUILTIN_TEMPLATES
        .iter()
        .any(|(m, s, _, _)| m.eq_ignore_ascii_case(&model) && *s == slot);
    if is_builtin && !args.has("force") {
        return Err(mangosteen_core::MnError::bad_request(
            format!("`{slot}` 是内置模板，不能删除。如要删除，请用 --force 强制删除")
        ));
    }

    let (mut cfg, store) = {
        let gw = build_gateway(&args)?;
        (gw.config().as_ref().clone(), gw.config_store())
    };

    let Some(wl) = cfg.model_whitelist.get_mut(&model) else {
        return Err(mangosteen_core::MnError::bad_request(
            format!("模型 `{model}` 不存在")
        ));
    };

    if wl.templates.remove(&slot).is_none() {
        return Err(mangosteen_core::MnError::bad_request(
            format!("模型 `{model}` 下没有模板 `{slot}`")
        ));
    }

    store.save(&cfg)?;

    output::print_success(&format!(
        "模板已删除：{} / {}",
        model, slot
    ));

    Ok(0)
}

/// `models show` - 显示指定模板的正文和解析摘要。
fn show(args: Args) -> MnResult<i32> {
    let model = args
        .pos(2)
        .map(|s| s.to_string())
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少模型参数。用法：mangosteen models show <模型> --slot <槽位>"
        ))?;

    let slot = args
        .get("slot")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| mangosteen_core::MnError::bad_request(
            "缺少 --slot 参数（配置模板名）"
        ))?;

    let gw = build_gateway(&args)?;
    let cfg = gw.config();

    let Some(wl) = cfg.model_whitelist.get(&model) else {
        return Err(mangosteen_core::MnError::bad_request(
            format!("模型 `{model}` 不存在")
        ));
    };

    let Some(text) = wl.templates.get(&slot) else {
        return Err(mangosteen_core::MnError::bad_request(
            format!("模型 `{model}` 下没有模板 `{slot}`")
        ));
    };

    let is_builtin = mangosteen_core::template::BUILTIN_TEMPLATES
        .iter()
        .any(|(m, s, _, _)| m.eq_ignore_ascii_case(&model) && *s == slot);

    if args.has("json") {
        let summary = match mangosteen_core::template::resolve(&model, text) {
            Ok(s) => serde_json::json!({
                "ok": true,
                "model_file": s.model_file,
                "download_url": s.download_url,
                "context": s.context,
                "gpu_layers": s.gpu_layers,
                "mmproj": s.mmproj,
                "draft_model": s.draft_model,
                "aux_count": s.aux_files.len(),
            }),
            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
        };
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "model": model,
            "slot": slot,
            "builtin": is_builtin,
            "text": text,
            "summary": summary,
        }))
        .map_err(|e| mangosteen_core::MnError::bad_request(format!("序列化失败：{e}")))?;
        println!("{}", json);
        return Ok(0);
    }

    println!("{}", output::bold(&format!("模型：{} ({})", model, wl.display_name)));
    println!("{}", output::bold(&format!("槽位：{} [{}]", slot, if is_builtin { "内置" } else { "自定义" })));
    println!();
    println!("{}", output::bold("模板正文"));
    println!("{}", text);
    println!();
    println!("{}", output::bold("解析摘要"));
    match mangosteen_core::template::resolve(&model, text) {
        Ok(spec) => {
            println!("  模型文件      {}", spec.model_file);
            println!("  下载地址      {}", spec.download_url);
            println!("  上下文        {}", spec.context);
            println!("  GPU 层数      {}", spec.gpu_layers);
            if let Some(mmproj) = &spec.mmproj {
                println!("  多模态投影    {}", mmproj);
            }
            if let Some(draft) = &spec.draft_model {
                println!("  草稿模型      {}", draft);
            }
            if !spec.aux_files.is_empty() {
                println!("  辅助文件      {} 个", spec.aux_files.len());
            }
        }
        Err(e) => {
            output::print_error(&mangosteen_core::MnError::bad_request(format!("解析失败：{e}")));
        }
    }

    Ok(0)
}

/// 模板显示名：正文首行 `template:` 的值。
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

fn print_models_help() {
    println!("mangosteen models <子命令> [选项]");
    println!();
    println!("管理模型配置模板（设置页「模型配置」的 CLI 对应）。");
    println!("模板是部署的唯一真源：下哪个仓库的哪个文件、用什么参数启动，全部由它决定。");
    println!();
    println!("{}", output::bold("子命令"));
    println!("  {:<w$}  {}", "list", "列出全部模型的配置模板", w = 12);
    println!("  {:<w$}  {}", "add", "新增或更新一个配置模板", w = 12);
    println!("  {:<w$}  {}", "remove", "删除一个自定义配置模板", w = 12);
    println!("  {:<w$}  {}", "show", "显示指定模板的正文和解析摘要", w = 12);
    println!();
    println!("{}", output::bold("全局选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 格式输出", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen models list");
    println!("  mangosteen models show qwen3.8-27b --slot uncensored");
    println!("  mangosteen models add qwen3.8-27b --slot custom --file ./my-template.yaml");
    println!("  mangosteen models remove qwen3.8-27b --slot custom");
    println!();
    println!("更多信息：docs/USAGE.md");
}

fn print_models_list_help() {
    println!("mangosteen models list [选项]");
    println!();
    println!("列出全部模型的配置模板。");
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--json", "以 JSON 格式输出", w = 12);
    println!();
    println!("示例：mangosteen models list");
}

fn print_models_add_help() {
    println!("mangosteen models add <模型> --slot <槽位> --file <文件> [选项]");
    println!();
    println!("新增或更新一个配置模板。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<模型>", "模型名称（如 qwen3.8-27b）", w = 12);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--slot <槽位>", "配置模板名（如 default / uncensored / custom）", w = 16);
    println!("  {:<w$}  {}", "--file <文件>", "模板文件路径（YAML 格式）", w = 16);
    println!("  {:<w$}  {}", "--force", "强制覆盖内置模板", w = 16);
    println!();
    println!("示例：mangosteen models add qwen3.8-27b --slot custom --file ./my-template.yaml");
}

fn print_models_remove_help() {
    println!("mangosteen models remove <模型> --slot <槽位> [选项]");
    println!();
    println!("删除一个自定义配置模板。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<模型>", "模型名称", w = 12);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--slot <槽位>", "配置模板名", w = 16);
    println!("  {:<w$}  {}", "--force", "强制删除内置模板", w = 16);
    println!();
    println!("示例：mangosteen models remove qwen3.8-27b --slot custom");
}

fn print_models_show_help() {
    println!("mangosteen models show <模型> --slot <槽位> [选项]");
    println!();
    println!("显示指定模板的正文和解析摘要。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<模型>", "模型名称", w = 12);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--slot <槽位>", "配置模板名", w = 16);
    println!("  {:<w$}  {}", "--json", "以 JSON 格式输出", w = 16);
    println!();
    println!("示例：mangosteen models show qwen3.8-27b --slot uncensored");
}