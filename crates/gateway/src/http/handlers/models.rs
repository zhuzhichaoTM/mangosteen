//! 模型资产处理器（`/api/models*`、模型下载任务）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use std::sync::Arc;

use crate::http::request::REMOTE_HEADER_BYTES;
use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;
use mangosteen_core::MnError;

impl GatewayServer {
    pub(crate) fn list_models(&self) -> Response {
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };
        let list = match gw.models().list() {
            Ok(l) => l,
            Err(e) => return Response::error(&e),
        };

        let items: Vec<serde_json::Value> = list
            .iter()
            .map(|m| {
                let (path, exists, size) = match &m.location {
                    mangosteen_core::model::ModelLocation::Local { path } => {
                        let meta = std::fs::metadata(path).ok();
                        (
                            Some(path.display().to_string()),
                            meta.is_some(),
                            meta.map(|x| x.len()),
                        )
                    }
                    mangosteen_core::model::ModelLocation::Cloud { path, .. } => {
                        (Some(path.clone()), true, None)
                    }
                };

                // 能否部署：只做**本地计算**（资源是否够），不探测引擎、
                // 不查云端库存。原因是这里要列表展示，必须毫秒级返回。
                //
                // 文件不存在就明确不可部署——让用户看到灰色的「部署」而不是
                // 点了才失败。真正的可行性评估仍在点「部署」时做（含引擎与
                // 云端费用），这里只负责把明显不能部署的置灰。
                let (deployable, reason) = if !exists {
                    (false, Some("模型文件不存在或已被删除".to_string()))
                } else {
                    let dreq = mangosteen_orchestration::assess::DeploymentRequest::for_model(
                        m.id.clone(),
                    );
                    match gw.evaluate(&dreq) {
                        Ok(ev) => {
                            if ev.is_local_viable() {
                                (true, None)
                            } else {
                                (
                                    false,
                                    Some("本地资源不足（点「部署」可查看云端方案）".to_string()),
                                )
                            }
                        }
                        Err(e) => (false, Some(e.to_string())),
                    }
                };

                serde_json::json!({
                    "id": m.id,
                    "name": m.spec.name,
                    "quant": m.spec.quant.to_string(),
                    "size_bytes": size.or(Some(m.spec.file_size_bytes)).filter(|v| *v > 0),
                    "path": path,
                    "file_exists": exists,
                    "is_local": m.is_local(),
                    "sha256": m.sha256,
                    "deployable": deployable,
                    "deploy_blocked_reason": reason,
                })
            })
            .collect();

        Response::json(200, &serde_json::json!({ "models": items }))
    }
    /// 搜索模型。
    ///
    /// 一个接口、两种模式，按输入自动判别——这样 UI 只需要一个搜索框，
    /// 用户既能「按关键字找模型」，也能「按仓库路径看文件」：
    ///
    /// | 输入 | 判定 | 返回 |
    /// |------|------|------|
    /// | `qwen` / `qwen gguf` | 关键字 | `{ mode: "repos", repos: [...] }` |
    /// | `Qwen/Qwen2.5-0.5B-Instruct-GGUF` | 仓库全名（含 `/`） | `{ mode: "files", files: [...] }` |
    ///
    /// 判别依据是「是否含 `/`」：仓库全名必然是 `owner/name` 两段，
    /// 而关键字几乎不会带斜杠。这个规则简单、可预测，也不会误判。
    pub(crate) fn search_models(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };

        // 兼容两种字段名：`query`（新，关键字）与 `repo`（旧，仓库路径）
        let raw = parsed
            .get("query")
            .or_else(|| parsed.get("repo"))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if raw.is_empty() {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "缺少 query 字段（关键字，如 `qwen2.5 gguf`；或仓库全名 `owner/name`）",
                    Some("query"),
                ),
            );
        }

        let cfg = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            gw.config().clone()
        };

        let registry = match cfg.model_source_registry() {
            Ok(r) => r,
            Err(e) => return Response::error(&e),
        };
        let source = match parsed.get("source").and_then(|v| v.as_str()) {
            Some(s) => match registry.get(s) {
                Some(x) => x.clone(),
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(format!("未知模型源 `{s}`"), Some("source")),
                    )
                }
            },
            None => match registry.get(registry.default_source()) {
                Some(x) => x.clone(),
                None => return Response::text(500, "默认模型源未定义"),
            },
        };

        // 去掉可能带的 `源名:` 前缀
        let text = raw
            .strip_prefix(&format!("{}:", source.name))
            .unwrap_or(&raw)
            .to_string();

        let opts = mangosteen_base::DownloadOptions::new(
            cfg.storage.models_dir.clone(),
            cfg.storage.max_model_bytes,
        );

        let is_repo_path = text.contains('/');

        if is_repo_path {
            // ---- 模式一：已知仓库，列出其中可下载的 GGUF ----
            return match mangosteen_base::download::fetch_remote_files(&source, &text, &opts) {
                Ok(files) => Response::json(
                    200,
                    &serde_json::json!({
                        "mode": "files",
                        "source": source.name,
                        "repo": text,
                        "files": files.iter().map(|f| serde_json::json!({
                            "name": f.name,
                            "size": f.size,
                            "sha256": f.sha256,
                            // 格式标识（gguf / safetensors / pytorch / …）。
                            // 不做过滤，只告知，由用户按所用引擎自行判断。
                            "format": f.format,
                        })).collect::<Vec<_>>(),
                    }),
                ),
                Err(e) => Response::error(&e),
            };
        }

        // ---- 模式二：关键字，搜索仓库 ----
        // 默认一次取 100 个：用户在 UI 里要能「看到足够多的候选再翻页」，
        // 30 个太少（一个热门关键字就有几百个仓库）。
        // 上限也设 100——再多会让上游变慢，且前端分页已能满足浏览需求。
        let limit = parsed
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 100) as usize)
            .unwrap_or(100);

        match mangosteen_base::download::search_repos(&source, &text, limit, &opts) {
            Ok(repos) => Response::json(
                200,
                &serde_json::json!({
                    "mode": "repos",
                    "source": source.name,
                    "query": text,
                    "repos": repos.iter().map(|r| serde_json::json!({
                        "repo": r.repo,
                        "downloads": r.downloads,
                        "likes": r.likes,
                        "tags": r.tags,
                        "has_gguf": r.has_gguf,
                    })).collect::<Vec<_>>(),
                }),
            ),
            Err(e) => Response::error(&e),
        }
    }
    /// 提交下载任务（deploy 页签的「下载」按钮）。
    ///
    /// **立即返回** `job_id`，实际下载在后台线程进行；进度用
    /// `GET /api/models/jobs` 轮询。理由见 [`crate::pulljobs`] 模块文档。
    ///
    /// 幂等与去重：
    /// - 本地已有完整文件 → 任务立刻标记完成并带 `note`，不会重下
    /// - 同一模型已有进行中的任务 → 直接返回那个 `job_id`，不会并发下两份
    pub(crate) fn pull_model(&self, req: &ParsedRequest) -> Response {
        let parsed: serde_json::Value = match serde_json::from_str(&req.body_str()) {
            Ok(v) => v,
            Err(e) => {
                return Response::json(
                    400,
                    &ErrorResponse::bad_request(format!("请求体不是合法 JSON：{e}"), None),
                )
            }
        };
        let Some(spec) = parsed.get("model").and_then(|v| v.as_str()) else {
            return Response::json(
                400,
                &ErrorResponse::bad_request(
                    "缺少 model 字段（形如 `owner/仓库/文件.gguf`）",
                    Some("model"),
                ),
            );
        };
        // UI 的「部署」按钮走这条路径：下载完**自动启动**。
        // CLI 的 `models pull` 不带此参数，只下载不启动。
        let deploy_after = parsed
            .get("deploy")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 若要求自动部署，还要带上部署诉求（设备选择等）
        let deploy_target = parsed
            .get("target")
            .and_then(|v| v.as_str())
            .unwrap_or("local")
            .to_string();

        // 同一规格正在下载 → 复用，避免两个 curl 争抢同一个 .part
        if let Some(existing) = self.pull_jobs.find_active_by_spec(spec) {
            return Response::json(
                200,
                &serde_json::json!({
                    "ok": true,
                    "job_id": existing.id,
                    "reused": true,
                    "note": "该模型正在下载中，已复用现有任务",
                }),
            );
        }

        // ---- 持锁期间只做「取快照」，不做任何网络操作 ----
        let (cfg, register, gw_for_deploy) = {
            let gw = match self.gateway.lock() {
                Ok(g) => g,
                Err(_) => return Response::text(500, "网关状态锁中毒"),
            };
            let cfg = gw.config().clone();
            // 登记闭包：下载完成后把文件写进模型台账。
            // 用闭包而非可变的 Gateway 引用，是为了让下载线程完全不持锁。
            // ModelStore 是 Clone 的（内部只有路径 + 策略），
            // 克隆一份进闭包即可，不需要共享可变状态。
            let models = gw.models().clone();
            let reg: crate::pulljobs::RegisterHook = Arc::new(move |path: &std::path::Path| {
                models.register_local(path, None).map(|r| r.entry.id)
            });
            // 部署闭包要用它，但**在这里不能持锁**——只捕获 Arc，
            // 真正执行时才短暂加锁（见下面闭包体）。
            (cfg, reg, Arc::clone(&self.gateway))
        };

        let registry = match cfg.model_source_registry() {
            Ok(r) => r,
            Err(e) => return Response::error(&e),
        };
        let (source, model) = match registry.resolve(spec) {
            Ok(v) => v,
            Err(e) => return Response::error(&e),
        };

        let mut opts = mangosteen_base::DownloadOptions::new(
            cfg.storage.models_dir.clone(),
            cfg.storage.max_model_bytes,
        );

        // 先取元数据：拿官方大小与 SHA-256，并提前发现「文件不存在」。
        // 这一步在网络上是几百毫秒，放在请求里可接受——它能让用户在
        // 提交前就知道文件不对，而不是等任务跑起来才失败。
        let target = model.local_file_name();
        match mangosteen_base::download::fetch_remote_files(&source, &model.repo, &opts) {
            Ok(files) => match files
                .iter()
                .find(|f| f.name == target || f.name.ends_with(&format!("/{target}")))
            {
                Some(f) => {
                    opts.expected_bytes = Some(f.size);
                    opts.expected_sha256 = f.sha256.clone();
                }
                None => {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!(
                            "仓库 {} 中不存在文件 `{target}`。先用「搜索」查看该仓库可下载的文件",
                            model.repo
                        ),
                            Some("model"),
                        ),
                    )
                }
            },
            Err(e) => {
                // 元数据取不到不算致命：仍然允许下载（只是没有大小与哈希校验）。
                // 但要让用户知道这一点，而不是以为一切正常。
                let _ = e;
            }
        }

        // 磁盘空间预检（已知大小时才有意义）
        if let Some(need) = opts.expected_bytes {
            if let Ok(avail) = mangosteen_platform::fs::available_space(&cfg.storage.models_dir) {
                let required = need + need / 20 + (128 << 20);
                if avail < required {
                    return Response::json(
                        400,
                        &ErrorResponse::bad_request(
                            format!(
                                "磁盘空间不足：需要约 {}，可用 {}",
                                mangosteen_platform::fs::human_bytes(required),
                                mangosteen_platform::fs::human_bytes(avail)
                            ),
                            None,
                        ),
                    );
                }
            }
        }

        // 本地已有完整文件 → 不必启动下载线程，直接标记完成
        let file_name = model.local_file_name();
        let final_path = cfg.storage.models_dir.join(&file_name);
        let already = match (opts.expected_bytes, std::fs::metadata(&final_path)) {
            (Some(expected), Ok(meta)) => meta.len() == expected,
            _ => false,
        };

        let job_id = self.pull_jobs.create_with(spec, &file_name, deploy_after);

        // 部署回调：把每个阶段实时写回任务表。
        //
        // 闭包在**持有网关锁**期间被调用（`prepare` / `commit_local` 要
        // `&mut Gateway`），所以 sink 里只能操作内存任务表，
        // 绝不做网络/磁盘 IO——否则几 GB 下载期间整个 API 都会被拖死。
        let dep: Option<crate::pulljobs::DeployHooks> = if deploy_after {
            let jobs = self.pull_jobs.clone();
            let jid = job_id.clone();
            let gw = gw_for_deploy;
            let tgt = if deploy_target == "cloud" {
                mangosteen_orchestration::assess::ComputeTarget::Cloud
            } else {
                mangosteen_orchestration::assess::ComputeTarget::Local
            };
            // 远端元数据预取所需：源、模型引用、下载选项快照。
            // 这一份给 prepare 阶段用（HTTP Range 只取头部）。
            let probe_source = source.clone();
            let probe_model = model.clone();
            let probe_opts = opts.clone();
            // 模型名与量化用于「预取失败」时的粗估兜底
            let est_name = model_name_for_estimate(&model);
            let est_quant = model
                .quant_from_filename()
                .unwrap_or(mangosteen_core::model::Quantization::Q4KM);
            let est_size = opts.expected_bytes.unwrap_or(0);

            // ---- 第 1 段：准备（评估 → 选设备 → 配置）----
            let jobs_p = jobs.clone();
            let jid_p = jid.clone();
            let gw_p = Arc::clone(&gw);
            // 本地已存在的最终文件路径：优先读它做精确评估。
            // 远端预取要走网络（实测 5 秒），文件就在本地时没必要绕这一圈。
            let local_final_path = final_path.clone();

            // 云端直连下载所需的「从哪下、多大」。
            //
            // 设备选云端时，模型的下载发生在**云端**，所以要把源地址一起带给
            // 编排层；否则云端只能退化成「要求本地先下一份再上传」——那与
            // 「云端弹性补位」的定位相悖（本地没下过就不许上云）。
            //
            // 地址用与本地下载完全相同的 `download_url` 规则拼出，
            // 保证云端拿到的就是用户选的那个文件。
            let dl_spec: Option<mangosteen_orchestration::assess::ModelDownloadSpec> =
                match probe_source.download_url(&probe_model) {
                    Ok(url) => Some(mangosteen_orchestration::assess::ModelDownloadSpec {
                        url,
                        expected_bytes: probe_opts.expected_bytes,
                    }),
                    Err(e) => {
                        // 拼不出地址不是致命错误：本地已有文件时仍可上传兜底。
                        // 但要留痕，否则云端报「没有可用下载地址」时无从追查。
                        eprintln!("[deploy] 无法拼出下载地址，云端将只能走上传：{e}");
                        None
                    }
                };
            let prepare: crate::pulljobs::PrepareHook = Arc::new(move || {
                // 评估：本地文件 > 远端真实元数据 > 按体积粗估
                let req = {
                    let mut r =
                        mangosteen_orchestration::assess::DeploymentRequest::for_model("__probe__");
                    r.target = tgt;
                    // 云端要据此在远端下载；本地部署用不到，带着也无害
                    r.model_download = dl_spec.clone();
                    r
                };

                // 阶段 1/6：模型评估
                jobs_p.set_stage(
                    &jid_p,
                    mangosteen_orchestration::progress::DeployStage::Assessing,
                    mangosteen_orchestration::progress::StageOutcome::Running,
                    "正在读取模型元数据并评估资源需求".to_string(),
                );

                let evaluated = if local_final_path.is_file() {
                    // 本地已有 → 读盘解析，最准且零网络延迟
                    let g = gw_p
                        .lock()
                        .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                    let ev = g.assessor_public().evaluate_file(&local_final_path, &req);
                    drop(g);
                    match ev {
                        Ok(ev) => {
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Done,
                                format!("{}（元数据来自本地文件）", probe_assessed_detail(&ev)),
                            );
                            ev
                        }
                        Err(e) => {
                            // 本地文件坏了 → 也会失败，但给准确原因
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Failed,
                                format!("本地模型文件无法解析：{e}"),
                            );
                            return Err(e);
                        }
                    }
                } else {
                    match mangosteen_base::download::probe_remote_metadata(
                        &probe_source,
                        &probe_model,
                        &probe_opts,
                        REMOTE_HEADER_BYTES,
                    ) {
                        Ok(meta) => {
                            let g = gw_p
                                .lock()
                                .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                            let ev = g.evaluate_remote(&meta.info, &req);
                            drop(g);
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Done,
                                format!("{}（元数据来自远端文件头）", probe_assessed_detail(&ev)),
                            );
                            ev
                        }
                        Err(e) => {
                            // 预取失败不阻断部署，但必须如实标注为估算。
                            let g = gw_p
                                .lock()
                                .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                            let ev = g.evaluate_estimated(&est_name, est_quant, est_size, &req);
                            drop(g);
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Done,
                                format!(
                                    "{}（⚠ 元数据预取失败，以下为按体积的估算值：{}）",
                                    probe_assessed_detail(&ev),
                                    first_line(&e.to_string())
                                ),
                            );
                            ev
                        }
                    }
                };

                // 阶段 2/6、3/6 由 prepare_with_progress 内部上报
                let sink_jobs = jobs_p.clone();
                let sink_id = jid_p.clone();
                let sink = move |stage: mangosteen_orchestration::progress::DeployStage,
                                 outcome: mangosteen_orchestration::progress::StageOutcome,
                                 detail: String| {
                    sink_jobs.set_stage(&sink_id, stage, outcome, detail);
                };
                let mut g = gw_p
                    .lock()
                    .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                g.prepare_with_progress(evaluated, &req, &sink)
            });

            // ---- 第 3 段：提交（启动引擎）----
            let jobs_c = jobs.clone();
            let jid_c = jid.clone();
            // commit 与 abort 都要网关句柄；abort 另需一份独立克隆，
            // 因为 commit 的闭包已经把 gw 移进去了。
            let gw_for_abort = Arc::clone(&gw);
            let gw_c = gw;
            // commit 需要知道文件落在哪（prepare 时还没有这个文件）
            let final_path_for_commit = final_path.clone();
            let commit: crate::pulljobs::CommitHook = Arc::new(move |prepared| {
                // prepare 在下载**之前**执行，那时文件还不存在，
                // plan.model_path 是空的。这里补上真实落盘路径，
                // 否则引擎会报「模型文件不存在：」（冒号后为空）。
                let prepared =
                    prepared.with_model_path(final_path_for_commit.to_string_lossy().to_string());
                let sink_jobs = jobs_c.clone();
                let sink_id = jid_c.clone();
                let sink = move |stage: mangosteen_orchestration::progress::DeployStage,
                                 outcome: mangosteen_orchestration::progress::StageOutcome,
                                 detail: String| {
                    sink_jobs.set_stage(&sink_id, stage, outcome, detail);
                };
                let mut g = gw_c
                    .lock()
                    .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                let plan = g.commit_local(prepared, &sink)?;
                Ok(plan.id)
            });

            // ---- 回滚：撤销这次部署产生的全部痕迹 ----
            //
            // 由 pulljobs 在检测到用户取消时调用。要点：
            //  · 停引擎（可能已经起来了）
            //  · 删台账记录（不是标 destroyed——回滚要「当没发生过」）
            //  · 释放端口预留（否则反复「部署→停止」会吃光端口区间）
            //  · 按 `delete_file` 决定是否删模型文件
            let gw_a = gw_for_abort;
            let abort: crate::pulljobs::AbortHook = Arc::new(move |dep_id, delete_file| {
                let Some(dep_id) = dep_id else { return Ok(()) };
                let mut g = gw_a
                    .lock()
                    .map_err(|_| MnError::bad_request("网关状态锁中毒"))?;
                g.abort_deployment(dep_id, delete_file)
            });

            Some(crate::pulljobs::DeployHooks {
                prepare,
                commit,
                abort,
            })
        } else {
            None
        };

        if already {
            // 走一遍登记（可能只是台账丢了，文件还在）
            let model_id = match register(&final_path) {
                Ok(id) => id,
                Err(e) => {
                    self.pull_jobs
                        .mark_failed(&job_id, format!("登记模型失败：{e}"));
                    return Response::error(&e);
                }
            };
            let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
            self.pull_jobs.mark_done(&job_id, &model_id, size, true);
            if deploy_after {
                // 文件已在本地 → 下载阶段直接标「跳过」（不是假装下载过）
                self.pull_jobs.set_stage(
                    &job_id,
                    mangosteen_orchestration::progress::DeployStage::Downloading,
                    mangosteen_orchestration::progress::StageOutcome::Skipped,
                    format!(
                        "本地已存在完整文件（{}），跳过下载",
                        mangosteen_base::download::human_bytes(size)
                    ),
                );
                self.pull_jobs.mark_deploying(&job_id);
                // 已在本地 → 直接跑「准备 → 提交」，不经过下载。
                // 准备里仍会做评估与设备选择，用户能看到同样的六步。
                if let Some(hooks) = dep.as_ref() {
                    match (hooks.prepare)() {
                        Ok(crate::gateway::PreparedDeployment::Done(plan)) => {
                            self.pull_jobs.set_deployment(&job_id, Some(&plan.id), None);
                        }
                        Ok(crate::gateway::PreparedDeployment::Local(p)) => {
                            match (hooks.commit)(*p) {
                                Ok(dep_id) => {
                                    self.pull_jobs.set_deployment(&job_id, Some(&dep_id), None)
                                }
                                Err(e) => self.pull_jobs.set_deployment(
                                    &job_id,
                                    None,
                                    Some(format!(
                                        "模型已就绪，但自动启动失败：{e}。\
                                         可在「已下载模型」里点「启动」重试"
                                    )),
                                ),
                            }
                        }
                        Err(e) => self.pull_jobs.set_deployment(
                            &job_id,
                            None,
                            Some(format!(
                                "模型已就绪，但部署准备失败：{e}。\
                                 可在「已下载模型」里点「启动」重试"
                            )),
                        ),
                    }
                }
            }
        } else {
            let resumed = cfg
                .storage
                .models_dir
                .join(format!("{file_name}.part"))
                .is_file();
            crate::pulljobs::spawn(
                self.pull_jobs.clone(),
                job_id.clone(),
                source,
                model,
                opts,
                register,
                dep,
            );
            if resumed {
                // 续传是磁盘上 .part 的能力，这里只补一句让用户安心
                self.pull_jobs
                    .set_note(&job_id, "检测到未完成的下载，将从断点继续");
            }
        }

        // 清理过期任务，避免表无限增长
        self.pull_jobs.prune(10 * 60 * 1000);

        Response::json(
            200,
            &serde_json::json!({
                "ok": true,
                "job_id": job_id,
                "reused": false,
            }),
        )
    }
    pub(crate) fn list_pull_jobs(&self, req: &ParsedRequest) -> Response {
        if let Some(id) = req.query_param("id") {
            return match self.pull_jobs.get(&id) {
                Some(j) => Response::json(200, &job_json(&j)),
                None => Response::not_found(&id),
            };
        }
        let all = self.pull_jobs.list();
        Response::json(
            200,
            &serde_json::json!({
                "jobs": all.iter().map(job_json).collect::<Vec<_>>(),
            }),
        )
    }
    /// 取消下载任务。`POST /api/models/jobs/{id}/cancel`
    pub(crate) fn cancel_pull_job(&self, id: &str) -> Response {
        match self.pull_jobs.cancel(id) {
            true => Response::json(200, &serde_json::json!({ "ok": true, "id": id })),
            false => Response::json(
                400,
                &ErrorResponse::bad_request(format!("任务 `{id}` 不存在或已结束"), Some("id")),
            ),
        }
    }
    /// 删除模型（可选删除磁盘文件）。
    ///
    /// `?delete_file=1` 时同时删除磁盘上的 GGUF。默认只从台账移除，
    /// 因为模型动辄数 GiB，误删重下代价很高。
    pub(crate) fn remove_model(&self, id: &str, req: &ParsedRequest) -> Response {
        let delete_file = query_flag(&req.query, "delete_file");
        let gw = match self.gateway.lock() {
            Ok(g) => g,
            Err(_) => return Response::text(500, "网关状态锁中毒"),
        };

        // 删之前记下文件路径，删完**核实**是否真的不在了。
        //
        // 为什么不能直接回显 `delete_file` 参数：`ModelStore::remove` 在
        // 「路径不在授权范围」时会静默跳过删除却仍返回成功。直接回显
        // 会让 UI 报「已删除（含磁盘文件）」而文件还在占几个 GB——
        // 用户以为清理干净了，实际没有。
        let file_path = gw
            .models()
            .get(id)
            .ok()
            .flatten()
            .and_then(|m| match m.location {
                mangosteen_core::model::ModelLocation::Local { path } => Some(path),
                _ => None,
            });

        match gw.models().remove(id, delete_file) {
            Ok(true) => {
                let file_deleted = match (&file_path, delete_file) {
                    (Some(p), true) => !p.exists(),
                    _ => false,
                };
                Response::json(
                    200,
                    &serde_json::json!({
                        "ok": true,
                        "id": id,
                        "file_deleted": file_deleted,
                        // 请求删文件但实际没删掉 → 明确告诉用户残留路径，
                        // 而不是让他自己去找
                        "leftover": if delete_file && !file_deleted {
                            file_path.map(|p| p.display().to_string())
                        } else {
                            None
                        },
                    }),
                )
            }
            Ok(false) => Response::not_found(id),
            Err(e) => Response::error(&e),
        }
    }
}
