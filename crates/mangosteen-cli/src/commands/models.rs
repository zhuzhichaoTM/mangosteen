//! `mangosteen models`
//!
//! 归属：**用户交互层 → 命令 `models`**。
//!
//! 子命令：`pull` / `add` / `list` / `show` / `verify` / `remove` / `search` / `sources`。
//! 全部操作经由模型库（基础能力层），`add` 会解析 GGUF 头部并把真实架构参数
//! 记入台账——后续资源评估才有准确依据。

use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{load_config, model_store, reject_unknown_options, state_store};
use crate::output;

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "json",
    "version",
    "delete-file",
    "reset",
    "quiet",
    "source",
    "no-verify",
    "force",
];

/// 各子命令允许的选项（更严格，避免 `models list --force` 这类无意义组合）。
const PULL_ALLOWED: &[&str] = &["config", "json", "source", "no-verify", "force", "quiet"];
const SEARCH_ALLOWED: &[&str] = &["config", "json", "source", "quiet"];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, ALLOWED)?;
    let sub = args.pos(1).unwrap_or("list").to_string();
    match sub.as_str() {
        "pull" | "download" => pull(args),
        "add" => add(args),
        "list" | "ls" => list(args),
        "show" | "info" => show(args),
        "verify" => verify(args),
        "remove" | "rm" => remove(args),
        "search" | "ls-remote" => search(args),
        "sources" => sources(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `models {other}`。可用：pull / add / list / show / verify / remove / \
             search / sources"
        ))),
    }
}

/// `models pull <源:owner/repo/file.gguf>`
///
/// 参照 Ollama 的拉取流程：先取远端元数据（官方 SHA-256 与大小）→
/// 下载到 `.part` → 校验 → 原子改名 → 自动登记台账。
fn pull(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, PULL_ALLOWED)?;

    let spec = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少模型标识。用法：\n  \
             mangosteen models pull <owner>/<仓库>/<文件.gguf>\n  \
             mangosteen models pull modelscope:Qwen/Qwen2.5-0.5B-Instruct-GGUF/\
             qwen2.5-0.5b-instruct-q4_k_m.gguf\n\
             可用 `mangosteen models search <owner>/<仓库>` 查看仓库里有哪些 GGUF",
        )
    })?;

    let (config, _src) = load_config(&args)?;
    let registry = config.model_source_registry()?;

    // `--source` 可强制指定源，覆盖标识里的前缀
    let (source, model) = match args.get("source") {
        Some(s) => {
            let mut r = mangosteen_core::source::ModelRef::parse(spec, s)?;
            r.source = s.to_ascii_lowercase();
            let src = registry
                .get(s)
                .ok_or_else(|| {
                    mangosteen_core::MnError::bad_request(format!(
                        "未知模型源 `{s}`。可用：{}",
                        registry
                            .all()
                            .iter()
                            .map(|x| x.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?
                .clone();
            (src, r)
        }
        None => registry.resolve(spec)?,
    };

    if !args.has("quiet") {
        output::print_info(&format!(
            "源：{}　仓库：{}　文件：{}",
            source.display_name.clone().unwrap_or_else(|| source.name.clone()),
            model.repo,
            model.file
        ));
    }

    // ---- 1. 取远端元数据（拿官方大小与 SHA-256）----
    let mut opts = mangosteen_base::DownloadOptions::new(
        config.storage.models_dir.clone(),
        config.storage.max_model_bytes,
    );

    let want_verify = !args.has("no-verify");
    if !args.has("quiet") {
        println!("  {}", output::dim("查询仓库元数据…"));
    }
    match mangosteen_base::download::fetch_remote_files(&source, &model.repo, &opts) {
        Ok(files) => {
            let target = model.local_file_name();
            match files.iter().find(|f| f.name == target || f.name.ends_with(&format!("/{target}"))) {
                Some(f) => {
                    opts.expected_bytes = Some(f.size);
                    if want_verify {
                        opts.expected_sha256 = f.sha256.clone();
                    }
                    if !args.has("quiet") {
                        output::print_kv(&[
                            (
                                "远端大小".into(),
                                mangosteen_platform::fs::human_bytes(f.size),
                            ),
                            (
                                "官方 SHA-256".into(),
                                f.sha256
                                    .clone()
                                    .map(|s| format!("{}…", &s[..16]))
                                    .unwrap_or_else(|| output::dim("源未提供，跳过校验")),
                            ),
                        ]);
                    }
                }
                None => {
                    // 元数据里没有该文件 → 明确报错并列出可选项，
                    // 比让用户等下载完才 404 友好得多
                    let cands: Vec<String> = files
                        .iter()
                        .take(20)
                        .map(|f| {
                            format!(
                                "    {}  ({})",
                                f.name,
                                mangosteen_platform::fs::human_bytes(f.size)
                            )
                        })
                        .collect();
                    return Err(mangosteen_core::MnError::bad_request(format!(
                        "仓库 {} 中不存在文件 `{}`。可用文件：\n{}",
                        model.repo,
                        target,
                        if cands.is_empty() {
                            "（该仓库没有 GGUF 文件）".to_string()
                        } else {
                            cands.join("\n")
                        }
                    )));
                }
            }
        }
        Err(e) => {
            // 元数据不可用不阻塞下载：源可能没有元数据接口，
            // 或临时故障。但必须告诉用户「校验会退化为本地实算」
            output::print_warning(&format!("无法获取仓库元数据（{e}）。将直接下载并本地校验"));
            opts.expected_bytes = None;
            opts.expected_sha256 = None;
        }
    }

    // ---- 2. 磁盘空间预检 ----
    if let Some(need) = opts.expected_bytes {
        if let Some(avail) = available_space(&config.storage.models_dir) {
            // 留 5% + 128MiB 余量，避免正好写满
            let required = need + need / 20 + (128 << 20);
            if avail < required {
                return Err(mangosteen_core::MnError::bad_request(format!(
                    "磁盘空间不足：需要约 {}，可用 {}。模型库目录：{}",
                    mangosteen_platform::fs::human_bytes(required),
                    mangosteen_platform::fs::human_bytes(avail),
                    config.storage.models_dir.display()
                )));
            }
        }
    }

    // ---- 3. 下载 ----
    // 进度由下载器自身输出（单一进度源），CLI 不再重复打印，
    // 否则两套进度会争抢终端同一行。
    let quiet = args.has("quiet");
    let json = args.has("json");
    let dl = mangosteen_base::download_model(&source, &model, &opts, !quiet && !json)?;

    // ---- 4. 记录期望哈希（供离线 verify）----
    if let Some(sha) = &dl.sha256 {
        mangosteen_base::download::write_sha_sidecar(&dl.path, sha)?;
    }

    // ---- 5. 登记台账 ----
    let store = model_store(&args)?;
    let reg = store.register_local(&dl.path, args.get("version"))?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "path": dl.path.display().to_string(),
                "bytes": dl.bytes,
                "sha256": dl.sha256,
                "resumed": dl.resumed,
                "model_id": reg.entry.id,
                "name": reg.entry.spec.name,
                "quant": reg.entry.spec.quant.to_string(),
                "params_b": reg.entry.spec.params_b,
                "native_context": reg.entry.spec.native_context,
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    output::print_success(&format!(
        "下载完成并已登记：{}（{}）",
        reg.entry.id,
        mangosteen_platform::fs::human_bytes(dl.bytes)
    ));
    if dl.resumed {
        output::print_info("本次使用了断点续传");
    }
    println!();
    println!(
        "  {}",
        output::dim(&format!(
            "下一步：mangosteen assess {}　或　mangosteen deploy {}",
            reg.entry.spec.name, reg.entry.spec.name
        ))
    );
    Ok(0)
}

/// `models search <owner/仓库>` —— 列出远端仓库里的 GGUF 文件。
fn search(args: Args) -> MnResult<i32> {
    reject_unknown_options(&args, SEARCH_ALLOWED)?;

    let repo = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少仓库名。用法：mangosteen models search <owner>/<仓库>，             例如 mangosteen models search Qwen/Qwen2.5-0.5B-Instruct-GGUF",
        )
    })?;

    let (config, _) = load_config(&args)?;
    let registry = config.model_source_registry()?;

    let source = match args.get("source") {
        Some(s) => registry
            .get(s)
            .ok_or_else(|| {
                mangosteen_core::MnError::bad_request(format!(
                    "未知模型源 `{s}`。可用：{}",
                    registry
                        .all()
                        .iter()
                        .map(|x| x.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?
            .clone(),
        None => registry
            .get(registry.default_source())
            .expect("默认源必存在")
            .clone(),
    };

    // 允许 `--source hf` 与 `hf:owner/repo` 两种写法
    let repo = repo.strip_prefix(&format!("{}:", source.name)).unwrap_or(repo);

    let opts = mangosteen_base::DownloadOptions::new(
        config.storage.models_dir.clone(),
        config.storage.max_model_bytes,
    );
    let files = mangosteen_base::download::fetch_remote_files(&source, repo, &opts)?;

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "source": source.name,
                "repo": repo,
                "files": files.iter().map(|f| serde_json::json!({
                    "name": f.name,
                    "size": f.size,
                    "sha256": f.sha256,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    if files.is_empty() {
        println!("  {}", output::dim("该仓库没有 GGUF 文件"));
        return Ok(0);
    }

    output::print_info(&format!(
        "{} · {} 个 GGUF 文件",
        source.display_name.clone().unwrap_or_else(|| source.name.clone()),
        files.len()
    ));
    println!();
    let rows: Vec<Vec<String>> = files
        .iter()
        .map(|f| {
            vec![
                f.name.clone(),
                mangosteen_platform::fs::human_bytes(f.size),
                f.sha256
                    .as_ref()
                    .map(|s| format!("{}…", &s[..12]))
                    .unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    output::print_table(&["文件", "大小", "SHA-256"], &rows);
    println!();
    println!(
        "  {}",
        output::dim(&format!(
            "拉取：mangosteen models pull {}/{}/<文件>",
            source.name, repo
        ))
    );
    Ok(0)
}

/// `models sources` —— 列出可用模型源。
fn sources(args: Args) -> MnResult<i32> {
    let (config, _) = load_config(&args)?;
    let registry = config.model_source_registry()?;

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "default": registry.default_source(),
                "sources": registry.all().iter().map(|s| serde_json::json!({
                    "name": s.name,
                    "display_name": s.display_name,
                    "kind": format!("{:?}", s.kind),
                    "base_url": s.base_url,
                    "default_revision": s.default_revision,
                })).collect::<Vec<_>>(),
            }))
            .unwrap_or_default()
        );
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = registry
        .all()
        .iter()
        .map(|s| {
            vec![
                s.name.clone(),
                s.display_name.clone().unwrap_or_else(|| "-".into()),
                s.base_url.clone(),
                s.default_revision.clone(),
                if s.name == registry.default_source() {
                    "默认"
                } else {
                    ""
                }
                .to_string(),
            ]
        })
        .collect();
    output::print_table(&["源名", "说明", "地址", "修订", ""], &rows);
    println!();
    println!(
        "  {}",
        output::dim("配置 [model_sources.<名>] 可覆盖或新增源；storage.default_source 指定默认源")
    );
    Ok(0)
}

/// 查询目录所在分区的可用空间（字节）。
fn available_space(dir: &std::path::Path) -> Option<u64> {
    mangosteen_platform::fs::available_space(dir).ok()
}

/// `models add <path>`
fn add(args: Args) -> MnResult<i32> {
    let path = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少模型文件路径。用法：mangosteen models add <path/to/model.gguf>",
        )
    })?;

    let store = model_store(&args)?;
    let version = args.get("version");
    let reg = store.register_local(std::path::Path::new(path), version)?;

    if args.has("json") {
        let payload = serde_json::json!({
            "id": reg.entry.id,
            "name": reg.entry.spec.name,
            "architecture": reg.info.architecture,
            "quant": reg.entry.spec.quant.to_string(),
            "params_b": reg.entry.spec.params_b,
            "n_layers": reg.entry.spec.n_layers,
            "n_heads": reg.entry.spec.n_heads,
            "n_kv_heads": reg.entry.spec.n_kv_heads,
            "embedding_dim": reg.entry.spec.embedding_dim,
            "native_context": reg.entry.spec.native_context,
            "file_size_bytes": reg.entry.spec.file_size_bytes,
            "sha256": reg.entry.sha256,
            "replaced": reg.replaced,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap_or_default());
        return Ok(0);
    }

    if reg.replaced {
        output::print_success(&format!("已更新模型登记：{}", reg.entry.id));
    } else {
        output::print_success(&format!("已登记模型：{}", reg.entry.id));
    }
    output::print_kv(&[
        ("模型名".into(), reg.entry.spec.name.clone()),
        ("架构".into(), reg.info.architecture.clone()),
        (
            "量化".into(),
            format!(
                "{}（实测 {:.2} bpw）",
                reg.entry.spec.quant,
                reg.entry.spec.effective_bits_per_weight()
            ),
        ),
        (
            "参数量".into(),
            format!("{:.1}B", reg.entry.spec.params_b),
        ),
        (
            "层数 / 注意力".into(),
            format!(
                "{} 层 · {} heads · {} KV heads{}",
                reg.entry.spec.n_layers,
                reg.entry.spec.n_heads,
                reg.entry.spec.n_kv_heads,
                if reg.entry.spec.is_gqa() { "（GQA）" } else { "" }
            ),
        ),
        (
            "原生上下文".into(),
            reg.entry.spec.native_context.to_string(),
        ),
        (
            "文件大小".into(),
            mangosteen_platform::fs::human_bytes(reg.entry.spec.file_size_bytes),
        ),
        ("SHA-256".into(), reg.entry.sha256.clone().unwrap_or_else(|| output::dim("未计算"))),
    ]);
    println!();
    println!(
        "  {}",
        output::dim(&format!(
            "下一步：mangosteen assess {} 查看本地可行性",
            reg.entry.spec.name
        ))
    );
    Ok(0)
}

/// `models list`
fn list(args: Args) -> MnResult<i32> {
    let store = model_store(&args)?;
    let models = store.list()?;

    if args.has("json") {
        let arr: Vec<serde_json::Value> = models
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "name": m.spec.name,
                    "quant": m.spec.quant.to_string(),
                    "params_b": m.spec.params_b,
                    "location": match &m.location {
                        mangosteen_core::model::ModelLocation::Local { path } =>
                            format!("local:{}", path.display()),
                        mangosteen_core::model::ModelLocation::Cloud { instance_id, .. } =>
                            format!("cloud:{instance_id}"),
                    },
                    "size_bytes": m.spec.file_size_bytes,
                    "version": m.version,
                    "sha256": m.sha256,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"models": arr})).unwrap_or_default()
        );
        return Ok(0);
    }

    if models.is_empty() {
        println!("  {}", output::dim("（模型库为空）"));
        println!();
        println!(
            "  {}",
            output::dim("执行 `mangosteen models add <path/to/model.gguf>` 登记模型")
        );
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = models
        .iter()
        .map(|m| {
            vec![
                m.id.clone(),
                format!("{:.1}B", m.spec.params_b),
                m.spec.quant.to_string(),
                mangosteen_platform::fs::human_bytes(m.spec.file_size_bytes),
                m.spec.native_context.to_string(),
                if m.is_local() { "本地" } else { "云端" }.to_string(),
                m.version.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    output::print_table(
        &["模型 ID", "参数", "量化", "大小", "上下文", "位置", "版本"],
        &rows,
    );
    Ok(0)
}

/// `models show <id>`
fn show(args: Args) -> MnResult<i32> {
    let id = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request("缺少模型 ID。用法：mangosteen models show <id>")
    })?;
    let store = model_store(&args)?;

    let entry = match store.get(id)? {
        Some(e) => e,
        None => {
            let hits = store.find_by_name(id)?;
            match hits.len() {
                1 => hits.into_iter().next().unwrap(),
                0 => {
                    return Err(mangosteen_core::MnError::ModelNotFound {
                        path: std::path::PathBuf::from(id),
                    })
                }
                n => {
                    return Err(mangosteen_core::MnError::bad_request(format!(
                        "模型名 `{id}` 匹配到 {n} 个，请使用完整 ID"
                    )))
                }
            }
        }
    };

    if args.has("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&entry).unwrap_or_default()
        );
        return Ok(0);
    }

    output::print_kv(&[
        ("模型 ID".into(), entry.id.clone()),
        ("名称".into(), entry.spec.name.clone()),
        ("参数量".into(), format!("{:.2}B", entry.spec.params_b)),
        ("量化".into(), entry.spec.quant.to_string()),
        (
            "实测位宽".into(),
            format!("{:.2} bpw", entry.spec.effective_bits_per_weight()),
        ),
        ("层数".into(), entry.spec.n_layers.to_string()),
        (
            "注意力".into(),
            format!(
                "{} heads / {} KV heads (head_dim {})",
                entry.spec.n_heads,
                entry.spec.n_kv_heads,
                entry.spec.head_dim()
            ),
        ),
        ("隐藏维度".into(), entry.spec.embedding_dim.to_string()),
        ("词表".into(), entry.spec.vocab_size.to_string()),
        ("原生上下文".into(), entry.spec.native_context.to_string()),
        (
            "文件大小".into(),
            mangosteen_platform::fs::human_bytes(entry.spec.file_size_bytes),
        ),
        (
            "位置".into(),
            match &entry.location {
                mangosteen_core::model::ModelLocation::Local { path } => {
                    path.display().to_string()
                }
                mangosteen_core::model::ModelLocation::Cloud { instance_id, path } => {
                    format!("云端 {instance_id}:{path}")
                }
            },
        ),
        ("SHA-256".into(), entry.sha256.clone().unwrap_or_else(|| "-".into())),
        (
            "最后校验".into(),
            entry
                .verified_at
                .map(mangosteen_core::time::format_unix_utc)
                .unwrap_or_else(|| "-".into()),
        ),
        (
            "KV/token".into(),
            format!(
                "{} B",
                entry.spec.kv_bytes_per_token()
            ),
        ),
    ]);
    Ok(0)
}

/// `models verify <id>`
fn verify(args: Args) -> MnResult<i32> {
    let id = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request("缺少模型 ID。用法：mangosteen models verify <id>")
    })?;
    let store = model_store(&args)?;

    match store.verify(id) {
        Ok(true) => {
            output::print_success(&format!("{id} 哈希校验通过"));
            Ok(0)
        }
        Ok(false) => {
            output::print_info(&format!(
                "{id} 此前未记录哈希，已计算并写入台账（本次不构成校验结论）"
            ));
            Ok(0)
        }
        Err(e) => Err(e),
    }
}

/// `models remove <id>`
fn remove(args: Args) -> MnResult<i32> {
    let id = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request("缺少模型 ID。用法：mangosteen models remove <id>")
    })?;
    let store = model_store(&args)?;

    // 若该模型有活跃部署，拒绝删除（防止误删正在用的文件）
    let state = state_store(&args)?;
    let in_use: Vec<String> = state
        .list_active()?
        .into_iter()
        .filter(|r| r.plan.model.name == id || r.plan.id == id)
        .map(|r| r.plan.id)
        .collect();
    if !in_use.is_empty() {
        return Err(mangosteen_core::MnError::bad_request(format!(
            "模型 {id} 正被部署 {} 使用，请先停止部署",
            in_use.join(", ")
        )));
    }

    let delete_file = args.has("delete-file");
    if delete_file {
        // 删除磁盘文件是不可逆操作，必须显式 --yes
        if !args.has("yes") {
            output::print_warning(
                "⚠ 此操作非常危险，可能导致不可逆的数据丢失！--delete-file 会永久删除模型文件。",
            );
            let some = store.get(id)?;
            if let Some(e) = &some {
                if let mangosteen_core::model::ModelLocation::Local { path } = &e.location {
                    println!("  将删除：{}", path.display());
                }
            }
            println!();
            println!(
                "  确认执行请加 {}：{}",
                output::paint("--yes", output::Color::Cyan),
                output::bold(&format!("mangosteen models remove {id} --delete-file --yes"))
            );
            return Ok(2);
        }
    }

    let removed = store.remove(id, delete_file)?;
    if removed {
        if delete_file {
            output::print_success(&format!("{id} 已从台账移除，模型文件已删除"));
        } else {
            output::print_success(&format!("{id} 已从台账移除（磁盘文件保留）"));
        }
        Ok(0)
    } else {
        Err(mangosteen_core::MnError::bad_request(format!(
            "台账中没有模型 `{id}`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_subcommand_is_actionable() {
        let a = crate::cli::parse_args(&["models".to_string(), "frob".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("models frob"));
        assert!(e.to_string().contains("add"));
    }

    #[test]
    fn add_requires_path() {
        let a = crate::cli::parse_args(&["models".to_string(), "add".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("缺少模型文件路径"));
    }

    #[test]
    fn show_requires_id() {
        let a = crate::cli::parse_args(&["models".to_string(), "show".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("缺少模型 ID"));
    }

    #[test]
    fn remove_requires_id() {
        let a = crate::cli::parse_args(&["models".to_string(), "remove".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("缺少模型 ID"));
    }

    #[test]
    fn verify_requires_id() {
        let a = crate::cli::parse_args(&["models".to_string(), "verify".to_string()]).unwrap();
        let e = run(a).unwrap_err();
        assert!(e.to_string().contains("缺少模型 ID"));
    }
}