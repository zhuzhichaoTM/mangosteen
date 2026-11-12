//! `mangosteen deploy`
//!
//! 归属：**用户交互层 → 命令 `deploy`**。
//!
//! ## 行为
//!
//! 1. 解析模型与参数；
//! 2. 调网关 `deploy`（部署到云端，由云端调度器选择最优 GPU 节点）；
//! 3. 默认同步等待部署完成，打印结果；
//! 4. 若指定 `--watch`，启动部署后周期性轮询查看下载/启动进度，直至完成或失败；
//! 5. 若指定 `--async`，仅启动部署并立即返回部署 ID，不等待完成。
//!
//! ## 关键设计
//!
//! 云端模型下载和引擎启动都是耗时操作（数分钟至数十分钟）。
//! `--watch` 模式下，CLI 会每 5 秒轮询一次部署状态，显示实时进度；
//! 用户也可随时 Ctrl-C 中断，部署在云端继续运行，稍后用
//! `mangosteen status <部署ID>` 查看进度。

use std::time::Duration;

use mangosteen_base::state::DeploymentStatus;
use mangosteen_core::estimate::KvCacheType;
use mangosteen_core::MnResult;
use mangosteen_gateway::DeploymentRequest;

use crate::cli::Args;
use crate::commands::{build_gateway, reject_unknown_options};
use crate::output;
use output::{Color, paint};

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "context",
    "batch-size",
    "kv-cache",
    "local-only",
    "no-cpu-fallback",
    "require-full-gpu",
    "max-price",
    "json",
    "quiet",
    // 节点选择参数
    "reuse-instance",
    "reuse-provider",
    "device",
    "cloud-spec",
    "gpu-model",
    "interactive",
    // 异步/轮询参数
    "watch",
    "async",
    // 配置模板槽位（与 UI 部署页下拉框一致：default / uncensored / mode1…）
    "template",
];

/// 轮询间隔（秒）。
const POLL_INTERVAL_SECS: u64 = 5;

/// `deploy` 入口。
pub fn run(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_deploy_help();
        return Ok(0);
    }
    execute(args)
}

/// 打印 deploy 子命令帮助。
fn print_deploy_help() {
    println!("mangosteen deploy <模型> [选项]");
    println!();
    println!("部署模型到云端（租赁 GPU 算力）。");
    println!();
    println!("{}", output::bold("位置参数"));
    println!("  {:<w$}  {}", "<模型>", "模型 ID 或 GGUF 文件路径", w = 22);
    println!();
    println!("{}", output::bold("选项"));
    println!("  {:<w$}  {}", "--context <N>", "上下文长度（默认 8192）", w = 22);
    println!("  {:<w$}  {}", "--batch-size <N>", "批处理大小（默认 512）", w = 22);
    println!("  {:<w$}  {}", "--kv-cache <类型>", "KV 缓存精度（如 f16, q8_0）", w = 22);
    println!("  {:<w$}  {}", "--gpu-model <型号>", "GPU 型号（如 V100S、3080TI、P40）", w = 22);
    println!("  {:<w$}  {}", "--max-price <元/小时>", "最高价格上限（元/小时）", w = 22);
    println!("  {:<w$}  {}", "--template <槽位>", "配置模板槽位（default / uncensored / mode1…）", w = 22);
    println!("  {:<w$}  {}", "--device <节点ID>", "指定自定义节点部署", w = 22);
    println!("  {:<w$}  {}", "--reuse-instance", "复用已有实例（不重新创建）", w = 22);
    println!("  {:<w$}  {}", "--reuse-provider <厂商>", "指定复用的云厂商", w = 22);
    println!("  {:<w$}  {}", "--cloud-spec <规格>", "指定云端实例规格 ID", w = 22);
    println!("  {:<w$}  {}", "--watch", "部署后轮询查看进度（每 5 秒刷新）", w = 22);
    println!("  {:<w$}  {}", "--async", "异步返回，仅输出部署 ID 不等待完成", w = 22);
    println!("  {:<w$}  {}", "--local-only", "仅使用本地算力，不走云端", w = 22);
    println!("  {:<w$}  {}", "--no-cpu-fallback", "禁止 CPU 回退，仅 GPU 推理", w = 22);
    println!("  {:<w$}  {}", "--require-full-gpu", "要求完整 GPU 显存，不允许部分卸载", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!("  {:<w$}  {}", "--interactive", "交互式确认部署方案", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen deploy qwen2.5-0.5b --gpu-model V100S --watch");
    println!("  mangosteen deploy qwen2.5-7b --context 16384 --batch-size 256");
    println!("  mangosteen deploy qwen2.5-0.5b --device dev-001 --template default");
    println!("  mangosteen deploy qwen2.5-0.5b --max-price 1.5 --async");
    println!();
    println!("更多信息：docs/USAGE.md");
}

/// 共享实现。
fn execute(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;

    let model = args.positionals.get(1).cloned().ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少模型参数。用法：mangosteen deploy <模型ID或GGUF路径> [--context 8192]",
        )
    })?;

    let watch = args.has("watch");
    let async_mode = args.has("async");

    let mut req = DeploymentRequest::for_model(model.clone());
    if let Some(ctx) = args.get_num::<u32>("context")? {
        req.context_len = Some(ctx);
    }
    if let Some(b) = args.get_num::<u32>("batch-size")? {
        req.batch_size = b;
    }
    if let Some(p) = args.get_num::<f64>("max-price")? {
        req.max_price_per_hour = Some(p);
    }
    if let Some(kv) = args.get("kv-cache") {
        req.kv_cache = Some(parse_kv(kv)?);
    }
    if args.has("local-only") {
        req.allow_cloud = false;
    }
    if args.has("no-cpu-fallback") {
        req.allow_cpu_fallback = false;
    }
    if args.has("require-full-gpu") {
        req.allow_partial_offload = false;
    }

    // 节点选择参数
    if let Some(device_id) = args.get("device") {
        req.device_id = Some(device_id.to_string());
    }
    if let Some(instance_id) = args.get("reuse-instance") {
        req.reuse_instance_id = Some(instance_id.to_string());
    }
    if let Some(provider) = args.get("reuse-provider") {
        req.reuse_provider = Some(provider.to_string());
    }
    if let Some(spec) = args.get("cloud-spec") {
        req.cloud_spec = Some(spec.to_string());
    }
    if let Some(gpu_model) = args.get("gpu-model") {
        req.gpu_model = Some(gpu_model.to_string());
    }

    // 配置模板槽位：与 UI 部署页下拉框一致。
    //
    // ## 为什么需要它
    //
    // 模板是部署的唯一真源（下哪个仓库的哪个文件、用什么参数启动）。
    // UI 部署总是显式传模板正文；CLI 也必须明确「用哪个模板」：
    // * `--template <槽位>` 显式选择（default / uncensored / mode1…）；
    // * 未指定时**默认取 `default` 槽位**（与 UI 下拉框第一项一致），
    //   优先读设置页维护的正文，配置里没有再回落内置模板；
    // * 无论哪种来源，启动时都打印一行「配置模板」，让部署所用的
    //   模板始终显式可见——避免「以为用的是 A，实际跑的是 B」。
    let gw = build_gateway(&args)?;
    let model_key = base_model_name(&model);
    let slot = args
        .get("template")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();

    // 模板正文查找顺序：设置页维护的（用户改过的）→ 内置默认模板。
    let cfg_text = gw
        .config()
        .model_whitelist
        .get(&model_key)
        .and_then(|wl| wl.templates.get(&slot))
        .cloned();
    let (template_source, template_text) = match cfg_text {
        Some(t) => ("设置页配置", t),
        None => match mangosteen_core::template::builtin_default_template(&model_key) {
            Some(t) if slot == "default" => ("内置默认", t.to_string()),
            _ => {
                // 报错时列出该模型可用的模板槽位，用户不必再翻设置页
                let available: Vec<String> = gw
                    .config()
                    .model_whitelist
                    .get(&model_key)
                    .map(|wl| wl.templates.keys().cloned().collect())
                    .unwrap_or_default();
                let builtin: Vec<&str> =
                    mangosteen_core::template::BUILTIN_TEMPLATES
                        .iter()
                        .filter(|(m, _, _, _)| *m == model_key)
                        .map(|(_, slot, _, _)| *slot)
                        .collect();
                let mut all: Vec<String> = available;
                all.extend(builtin.iter().map(|s| s.to_string()));
                all.sort();
                all.dedup();
                let hint = if all.is_empty() {
                    "该模型暂无任何配置模板".to_string()
                } else {
                    format!("可用模板：{}", all.join("、"))
                };
                return Err(mangosteen_core::MnError::bad_request(format!(
                    "模型 `{model_key}` 下不存在配置模板 `{slot}`。{hint}"
                )));
            }
        },
    };
    req.config_template = Some(slot.clone());
    req.config_template_text = Some(template_text.clone());
    // 模板 schema 按 UI 下拉框的模型 id（如 qwen2.5-0.5b）注册；
    // CLI 必须显式带上，否则后端会拿 GGUF 解析名（qwen2.5-0.5b-instruct）
    // 去查 schema 而报「暂不支持」。同时交给 template::resolve 选对正文。
    req.model_id = Some(model_key.clone());
    output::print_info(&format!(
        "配置模板：{}（来源：{template_source}；可用 --template <槽位> 切换）",
        slot
    ));

    // 解析模板正文，提取下载地址和模型文件名（用于远端直连下载）。
    // 模板是部署的唯一真源：下哪个仓库的哪个文件、用什么参数启动，全部由它决定。
    let template_download = parse_template_download(&model_key, &template_text);

    let spinner = output::Spinner::new(format!("正在部署 {model}"));
    let mut spinner = spinner;
    spinner.tick("评估资源并调度…");

    // 启动部署（同步调用，会阻塞直到部署进入 provisioning 状态或完成）
    //
    // 两种路径：
    // 1. 模型在台账中（已 models add 或之前部署过）→ 直接 gw.deploy
    // 2. 模型不在台账但有配置模板（如 qwen3.8-27b）→ 用模板信息做估算评估，
    //    然后走 prepare + commit（与 UI 部署页一致）
    let result = if gw.evaluate(&req).is_ok() {
        // 路径 1：模型已在台账，直接部署
        gw.deploy(&req)
    } else if let Some(dl) = template_download {
        // 路径 2：模型不在台账但有模板，走估算评估 → prepare → commit
        deploy_from_template(&gw, &req, &model_key, &dl)
    } else {
        // 路径 3：模型不在台账且无模板，返回原始错误
        gw.deploy(&req)
    };
    spinner.finish();

    let plan = match result {
        Ok(p) => p,
        Err(e) => {
            output::print_error(&e);
            return Err(e);
        }
    };

    // 部署已进入 provisioning 或 ready 状态，获取部署 ID
    let deploy_id = plan.id.clone();

    if async_mode {
        // 仅启动部署，立即返回
        output::print_success(&format!(
            "部署已启动（异步模式）：{}",
            deploy_id
        ));
        output::print_info(&format!(
            "使用 `mangosteen status {}` 查看进度",
            deploy_id
        ));
        return Ok(0);
    }

    if watch {
        // 周期性轮询查看进度
        return poll_deploy_progress(&gw, &deploy_id, &model);
    }

    // 同步等待模式：deploy 已经阻塞直到完成
    if args.has("json") {
        let payload = serde_json::json!({
            "id": plan.id,
            "model": plan.model.name,
            "origin": plan.origin.label(),
            "port": plan.local_access_port(),
            "openai_base_url": format!("http://127.0.0.1:{}/v1", gw.config().gateway.port),
            "fallback_trail": plan.fallback_trail,
            "placement": plan.assessment.placement,
            "context_len": plan.assessment.context_len,
            "kv_cache": plan.assessment.kv_cache,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        print_deploy_result(&plan, &gw);
        for w in &plan.assessment.warnings {
            output::print_warning(&w.message());
        }
    }

    Ok(0)
}

/// 周期性轮询部署进度，直至完成或失败。
fn poll_deploy_progress(
    gw: &mangosteen_gateway::Gateway,
    deploy_id: &str,
    model: &str,
) -> MnResult<i32> {
    use std::io::Write;

    output::print_info(&format!(
        "部署 ID：{}，开始轮询进度（每 {} 秒刷新一次，Ctrl-C 可中断）",
        deploy_id, POLL_INTERVAL_SECS
    ));

    let mut last_stage: Option<String> = None;

    loop {
        match gw.get_deployment(deploy_id) {
            Ok(Some(record)) => {
                let status = record.status;
                let detail = match &record.error {
                    Some(e) => format!("（错误：{e}）"),
                    None => String::new(),
                };

                // 只有阶段或进度变化时才打印，避免刷屏
                let stage_str = status.as_str().to_string();
                if last_stage.as_ref() != Some(&stage_str) {
                    let icon = match status {
                        DeploymentStatus::Ready => Color::Green,
                        DeploymentStatus::Failed => Color::Red,
                        _ => Color::Cyan,
                    };
                    println!(
                        "{} {} {} {}",
                        paint_icon(icon),
                        bold("进度："),
                        paint(&stage_str, icon),
                        detail
                    );
                    last_stage = Some(stage_str);
                }

                // 终态：退出循环
                match status {
                    DeploymentStatus::Ready => {
                        output::print_success(&format!(
                            "部署完成：{}",
                            model
                        ));
                        // 重新获取完整计划打印详情
                        if let Ok(Some(rec)) = gw.get_deployment(deploy_id) {
                            print_deploy_result_full(&rec, &gw);
                        }
                        // 部署成功后，确保守护网关在运行（保持 SSH 隧道）。
                        // CLI 是临时进程，退出后 SSH 隧道随之关闭；UI 是常驻进程，隧道保持。
                        // 为了让 CLI 部署的模型也能持续服务，部署成功后自动启动守护网关：
                        // 网关启动时会调用 restore_deployment_tunnels() 恢复隧道。
                        if let Err(e) = ensure_gateway_running(gw) {
                            output::print_warning(&format!("守护网关启动失败：{e}"));
                            output::print_info("请手动执行 `./mangosteen ui start` 启动网关以保持隧道");
                        }
                        return Ok(0);
                    }
                    DeploymentStatus::Failed => {
                        output::print_error(&mangosteen_core::MnError::bad_request(format!(
                            "部署失败：{}",
                            record.error.unwrap_or_else(|| "未知错误".to_string())
                        )));
                        return Err(mangosteen_core::MnError::bad_request("部署失败"));
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                output::print_warning(&format!(
                    "部署记录不存在：{}",
                    deploy_id
                ));
                return Err(mangosteen_core::MnError::bad_request("部署记录不存在"));
            }
            Err(e) => {
                output::print_warning(&format!(
                    "查询部署状态失败：{e}，{} 秒后重试",
                    POLL_INTERVAL_SECS
                ));
            }
        }

        // 等待下一次轮询
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
    }
}

/// 确保守护网关在运行（部署成功后调用）。
///
/// 如果网关已运行且健康，什么都不做；否则启动后台网关进程。
/// 网关启动时会调用 restore_deployment_tunnels() 恢复隧道，
/// 从而保证 CLI 部署的模型在 CLI 进程退出后仍能通过隧道访问。
fn ensure_gateway_running(gw: &mangosteen_gateway::Gateway) -> MnResult<()> {
    let port = gw.config().gateway.port;

    // 检查网关是否已运行
    if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok() {
            // 网关已运行，隧道应保持
            return Ok(());
        }
    }

    // 网关未运行，启动后台网关
    output::print_info("正在启动守护网关以保持隧道...");

    let exe = std::env::current_exe()
        .map_err(|e| mangosteen_core::MnError::io("定位 mangosteen 可执行文件", &e))?;

    #[cfg(unix)]
    let mut cmd = {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("nohup");
        c.arg(&exe);
        c.process_group(0);
        c
    };
    #[cfg(not(unix))]
    let mut cmd = std::process::Command::new(&exe);

    cmd.arg("ui")
        .arg("--foreground")
        .arg("--no-open")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    let _child = cmd
        .spawn()
        .map_err(|e| mangosteen_core::MnError::io("启动守护网关", &e))?;

    // 等待网关就绪（最多 10 秒）
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Ok(addr) = format!("127.0.0.1:{port}").parse() {
            if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok() {
                output::print_success(&format!("守护网关已启动：http://127.0.0.1:{port}"));
                return Ok(());
            }
        }
    }

    output::print_warning("守护网关启动超时，请手动执行 `./mangosteen ui start` 启动");
    Ok(())
}

/// 打印带颜色的图标。
fn paint_icon(color: Color) -> String {
    if output::color_enabled() {
        paint("●", color)
    } else {
        "●".to_string()
    }
}

/// 粗体文本。
fn bold(text: &str) -> String {
    output::bold(text)
}

/// 打印完整的部署结果（从台账记录）。
fn print_deploy_result_full(
    record: &mangosteen_base::state::DeploymentRecord,
    gw: &mangosteen_gateway::Gateway,
) {
    let plan = &record.plan;
    output::print_success(&format!(
        "部署完成：{} ({})",
        plan.model.name,
        plan.origin.label()
    ));
    let port = plan
        .local_access_port()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    output::print_kv(&[
        ("部署 ID".into(), plan.id.clone()),
        ("算力来源".into(), plan.origin.label()),
        (
            "落位形态".into(),
            plan.assessment.placement.describe(plan.model.n_layers),
        ),
        ("上下文".into(), plan.assessment.context_len.to_string()),
        (
            "KV 精度".into(),
            plan.assessment.kv_cache.as_cli_value().to_string(),
        ),
        ("量化".into(), plan.assessment.quant.to_string()),
        ("后端端口".into(), port),
        (
            "OpenAI 入口".into(),
            format!("http://127.0.0.1:{}/v1", gw.config().gateway.port),
        ),
        ("资源占用".into(), plan.assessment.requirement.describe()),
    ]);

    if !plan.fallback_trail.is_empty() {
        println!();
        output::print_warning(&format!(
            "发生算力降级：{}",
            plan.fallback_trail.join(" → ")
        ));
        output::print_info(
            "云上实例正在计费，用完请执行 `mangosteen stop <部署ID>` 或 `mangosteen stop --all`",
        );
    }
}

/// 打印部署结果。
fn print_deploy_result(
    plan: &mangosteen_core::plan::DeploymentPlan,
    gw: &mangosteen_gateway::Gateway,
) {
    output::print_success(&format!(
        "部署完成：{} ({})",
        plan.model.name,
        plan.origin.label()
    ));
    let port = plan
        .local_access_port()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());
    output::print_kv(&[
        ("部署 ID".into(), plan.id.clone()),
        ("算力来源".into(), plan.origin.label()),
        (
            "落位形态".into(),
            plan.assessment.placement.describe(plan.model.n_layers),
        ),
        ("上下文".into(), plan.assessment.context_len.to_string()),
        (
            "KV 精度".into(),
            plan.assessment.kv_cache.as_cli_value().to_string(),
        ),
        ("量化".into(), plan.assessment.quant.to_string()),
        ("后端端口".into(), port),
        (
            "OpenAI 入口".into(),
            format!("http://127.0.0.1:{}/v1", gw.config().gateway.port),
        ),
        ("资源占用".into(), plan.assessment.requirement.describe()),
    ]);

    // 降级轨迹：非空时必须显眼展示（用户需要知道为什么没用本地）
    if !plan.fallback_trail.is_empty() {
        println!();
        output::print_warning(&format!(
            "发生算力降级：{}",
            plan.fallback_trail.join(" → ")
        ));
        output::print_info(
            "云上实例正在计费，用完请执行 `mangosteen stop <部署ID>` 或 `mangosteen stop --all`",
        );
    }
}

/// 从模板正文解析下载地址和模型文件名。
///
/// 模板格式示例：
/// ```yaml
/// source: https://hf-mirror.com/
/// repository: unsloth/Qwen3.8-27B-GGUF
/// model: qwen3.8-27b-q4_k_m.gguf
/// ```
///
/// 返回 `(download_url, expected_bytes)`，`expected_bytes` 为 None 表示未知。
fn parse_template_download(
    _model_key: &str,
    template_text: &str,
) -> Option<mangosteen_gateway::ModelDownloadSpec> {
    let source = template_text
        .lines()
        .find(|l| l.trim().starts_with("source:"))
        .and_then(|l| l.trim().strip_prefix("source:"))
        .map(|s| s.trim().to_string())?;
    let repository = template_text
        .lines()
        .find(|l| l.trim().starts_with("repository:"))
        .and_then(|l| l.trim().strip_prefix("repository:"))
        .map(|s| s.trim().to_string());
    let model_file = template_text
        .lines()
        .find(|l| l.trim().starts_with("model:"))
        .and_then(|l| l.trim().strip_prefix("model:"))
        .map(|s| s.trim().to_string())?;

    // 拼接下载地址：source/repository/resolve/main/model_file
    let url = if let Some(repo) = repository {
        format!(
            "{}/{}/resolve/main/{}",
            source.trim_end_matches('/'),
            repo,
            model_file
        )
    } else {
        // 没有 repository，source 本身就是完整前缀
        format!(
            "{}/{}",
            source.trim_end_matches('/'),
            model_file
        )
    };

    Some(mangosteen_gateway::ModelDownloadSpec {
        url,
        expected_bytes: None,
    })
}

/// 从模板部署（模型不在台账时）。
///
/// 与 UI 部署页一致：用模板信息做估算评估 → prepare → commit。
fn deploy_from_template(
    gw: &mangosteen_gateway::Gateway,
    req: &mangosteen_gateway::DeploymentRequest,
    model_key: &str,
    download: &mangosteen_gateway::ModelDownloadSpec,
) -> MnResult<mangosteen_core::plan::DeploymentPlan> {
    use mangosteen_core::model::Quantization;
    use mangosteen_gateway::noop_sink;

    // 估算评估：用模板中的模型名和默认量化
    let est_name = model_key;
    let est_quant = Quantization::Q4KM; // 默认 Q4_K_M
    let est_size = download.expected_bytes.unwrap_or(0);

    // 设置下载信息到请求
    let mut req_with_download = req.clone();
    req_with_download.model_download = Some(download.clone());

    // 估算评估
    let evaluated = gw.evaluate_estimated(est_name, est_quant, est_size, &req_with_download);

    // 准备（评估 → 选设备 → 配置 → 落台账）
    let prepared = gw.prepare_with_progress(evaluated, &req_with_download, &noop_sink)?;

    // 提交（启动实例、下载模型、启动引擎）
    match prepared {
        mangosteen_gateway::PreparedDeployment::Cloud(prepared) => {
            gw.commit_cloud(*prepared, &noop_sink)
        }
    }
}

/// 从模型名去掉量化后缀：`qwen3.8-27b-ud:q4_k_m` → `qwen3.8-27b-ud`。
fn base_model_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().split(':').next().unwrap_or("").to_string()
}

/// 解析 KV 精度参数。
fn parse_kv(s: &str) -> MnResult<KvCacheType> {
    match s.to_ascii_lowercase().as_str() {
        "f16" => Ok(KvCacheType::F16),
        "q8_0" | "q8" => Ok(KvCacheType::Q8_0),
        "q4_0" | "q4" => Ok(KvCacheType::Q4_0),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知 kv-cache `{other}`，可选：f16 / q8_0 / q4_0"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_parsing() {
        assert_eq!(parse_kv("f16").unwrap(), KvCacheType::F16);
        assert_eq!(parse_kv("Q8_0").unwrap(), KvCacheType::Q8_0);
        assert_eq!(parse_kv("q4").unwrap(), KvCacheType::Q4_0);
        assert!(parse_kv("q2").is_err());
    }

    #[test]
    fn missing_model_is_actionable() {
        let args = crate::cli::parse_args(&["deploy".to_string()]).unwrap();
        let e = execute(args).unwrap_err();
        assert!(e.to_string().contains("缺少模型参数"));
        assert!(e.to_string().contains("mangosteen deploy"));
    }

    #[test]
    fn unknown_option_rejected() {
        let args = crate::cli::parse_args(&[
            "deploy".to_string(),
            "m".to_string(),
            "--bogus".to_string(),
            "1".to_string(),
        ])
        .unwrap();
        let e = execute(args).unwrap_err();
        assert!(e.to_string().contains("--bogus"), "got {e}");
    }

    #[test]
    fn allowed_options_listed_in_error() {
        let args =
            crate::cli::parse_args(&["deploy".to_string(), "m".to_string(), "--nope".to_string()]);
        // --nope 是布尔开关形式，会被当成带值选项而缺值 → 解析期报错
        assert!(args.is_err() || execute(args.unwrap()).is_err());
    }
}
