//! 模型下载任务处理器（`POST /api/models/pull`、`/api/models/jobs*`）。
//!
//! 归属：第二层 API Server。下载本身在后台线程执行（见 [`crate::pulljobs`]），
//! 这里只负责受理、查询与取消；任务进度由 `job_json` 统一序列化。

use std::sync::Arc;

use crate::http::request::REMOTE_HEADER_BYTES;
use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;
use mangosteen_core::MnError;

impl GatewayServer {
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
            .unwrap_or("cloud")
            .to_string();
        // 用户选定的已保存自定义节点；有值时直接使用该节点，不创建云实例。
        let device_id = parsed
            .get("device_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // 用户选定的云端 GPU 规格（空表示自动选点）
        let cloud_spec = parsed
            .get("cloud_spec")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let config_template = parsed
            .get("config_template")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        // 配置模板正文（YAML 全文）。这是部署的唯一真源：下哪个仓库的哪个
        // 文件、用什么参数启动，全部由它决定。只传名称会让后端只能靠模型名
        // 硬编码特判，用户选 Uncensored 却下载 default 权重正是这么来的。
        let config_template_text = parsed
            .get("config_template_text")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let model_id = parsed
            .get("model_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let gpu_model = parsed
            .get("gpu_model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        // 用户在节点选择弹窗里选定的「已有节点」：有值时部署直接复用这台
        // 云实例（停机先开机、运行中同机另起端口），不再创建新实例。
        // provider 必须成对传回：实例 ID 跨厂商不保证唯一。
        let reuse_instance_id = parsed
            .get("reuse_instance_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let reuse_provider = parsed
            .get("reuse_provider")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let auxiliary_models = parsed
            .get("auxiliary_models")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let raw = item.as_str()?.trim();
                        let file_name = raw.split_whitespace().next()?.to_string();
                        if !file_name.to_ascii_lowercase().ends_with(".gguf") {
                            return None;
                        }
                        Some(mangosteen_orchestration::assess::AuxiliaryModelSpec {
                            file_name,
                            url: String::new(),
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

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
        let (cfg, models, gw_for_deploy) = {
            let gw = self.gateway.as_ref();
            (
                gw.config().clone(),
                // ModelStore 是 Clone 的（内部只有路径 + 策略）。
                // 克隆一份给闭包用，下载线程全程不持网关锁。
                gw.models().clone(),
                Arc::clone(&self.gateway),
            )
        };

        let registry = match cfg.model_source_registry() {
            Ok(r) => r,
            Err(e) => return Response::error(&e),
        };
        // 部署页选择的模型 ID 对应配置白名单；白名单的 source_spec
        // 是生效模板的唯一模型资产定义。只有没有 model_id 的旧 CLI 调用
        // 才回退到请求中的裸 source spec。
        let effective_spec = model_id
            .as_ref()
            .and_then(|id| cfg.model_whitelist.get(id).map(|m| m.source_spec.as_str()))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(spec);
        let (source, model) = match registry.resolve(effective_spec) {
            Ok(v) => v,
            Err(e) => return Response::error(&e),
        };

        // 记下直连下载地址：本地登记后，云端部署要靠它让远端自己下载。
        // 拼不出地址（自定义源缺模板）时保持 None，调用方会给出明确错误。
        let remote_url = source.download_url(&model).ok();

        let register: crate::pulljobs::RegisterHook =
            Arc::new(move |path: &std::path::Path, source| {
                let id = models
                    .register_local_with_source(path, None, source)?
                    .entry
                    .id;
                if let Some(url) = &remote_url {
                    // 记录失败不影响本次下载成功：模型已登记，只是云端部署
                    // 会缺下载地址（届时会明确报错）。
                    let _ = models.set_remote_url(&id, url);
                }
                Ok(id)
            });

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

        // 磁盘空间预检（已知大小时才有意义）。
        // 仅「本地部署」需要校验**本地**模型库目录：模型会下载到本机 `models_dir`。
        // 云端 / 自定义 SSH 节点的模型在**远端**下载，本地磁盘剩余空间与本次部署
        // 完全无关——绝不能用本机剩余空间去拦云端部署，否则本地盘一紧张，
        // 远端 32G V100 这种明明够用的机器也开不起来。
        if deploy_target == "local" {
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
        }

        // 本地已有完整文件 → 不必启动下载线程，直接标记完成
        let file_name = model.local_file_name();
        let final_path = cfg.storage.models_dir.join(&file_name);
        let already = match (opts.expected_bytes, std::fs::metadata(&final_path)) {
            (Some(expected), Ok(meta)) => meta.len() == expected,
            _ => false,
        };

        // 重新部署必须完整走「GPU 选择 → 节点配置 → 模型下载 → 启动模型 → 隧道」
        // 全部阶段。即使台账里存在同模型的 stopped 实例，也不能在 HTTP 受理
        // 阶段直接走 resume：用户本次可能已经选择了另一种 GPU，且节点配置、
        // SSH 免密、依赖和模型文件都必须重新校验。
        // 资源去重只允许发生在具体阶段内部（例如节点配置阶段发现依赖已存在），
        // 不能用它跳过阶段本身。
        let reuse_id: Option<String> = None;

        let job_id = self.pull_jobs.create_with(spec, &file_name, deploy_after);
        if deploy_after && cloud_spec.is_some() {
            // UI 已完成模型评估并明确选择 GPU 规格；任务创建后从“节点选择”开始。
            // 后台准备阶段仍可能重复做一次评估，但 PullJobs 会拒绝阶段回退，
            // 因此用户不会看到流程重新跳回“模型评估”。
            self.pull_jobs.set_stage(
                &job_id,
                mangosteen_orchestration::progress::DeployStage::Selecting,
                mangosteen_orchestration::progress::StageOutcome::Running,
                format!(
                    "已选择 GPU 节点 {}，等待实例创建完成",
                    cloud_spec.as_deref().unwrap_or("")
                ),
            );
        }

        // 部署回调：把每个阶段实时写回任务表。
        //
        // 闭包在**持有网关锁**期间被调用（`prepare` / `commit_local` 要
        // `&mut Gateway`），所以 sink 里只能操作内存任务表，
        // 绝不做网络/磁盘 IO——否则几 GB 下载期间整个 API 都会被拖死。
        let purge_model_id = model_id.clone();
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
            let prepare: crate::pulljobs::PrepareHook = Arc::new(move |should_cancel| {
                // 评估：本地文件 > 远端真实元数据 > 按体积粗估
                let req = {
                    let mut r =
                        mangosteen_orchestration::assess::DeploymentRequest::for_model("__probe__");
                    r.target = tgt;
                    // 云端要据此在远端下载；本地部署用不到，带着也无害
                    r.model_download = dl_spec.clone();
                    r.model_id = model_id.clone();
                    r.cloud_spec = cloud_spec.clone();
                    r.device_id = device_id.clone();
                    r.gpu_model = gpu_model.clone();
                    r.reuse_instance_id = reuse_instance_id.clone();
                    r.reuse_provider = reuse_provider.clone();
                    r.auxiliary_models = auxiliary_models.clone();
                    r.config_template = config_template.clone();
                    r.config_template_text = config_template_text.clone();
                    r
                };

                // 阶段 1：模型评估（真实执行，完成后输出加载所需算力资源）
                jobs_p.set_stage(
                    &jid_p,
                    mangosteen_orchestration::progress::DeployStage::Assessing,
                    mangosteen_orchestration::progress::StageOutcome::Running,
                    "正在解析模型元数据并评估模型加载所需算力资源".to_string(),
                );

                let evaluated = if local_final_path.is_file() {
                    // 本地已有 → 读盘解析，最准且零网络延迟
                    let g = gw_p.as_ref();
                    let ev = g.assessor_public().evaluate_file(&local_final_path, &req);
                    match ev {
                        Ok(ev) => {
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Done,
                                "模型评估完成，已输出模型加载所需算力资源".to_string(),
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
                            let g = gw_p.as_ref();
                            let ev = g.evaluate_remote(&meta.info, &req);
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Done,
                                "模型评估完成，已输出模型加载所需算力资源（远端元数据）"
                                    .to_string(),
                            );
                            ev
                        }
                        Err(e) => {
                            // 预取失败不阻断部署，但必须如实标注为估算。
                            let g = gw_p.as_ref();
                            let ev = g.evaluate_estimated(&est_name, est_quant, est_size, &req);
                            jobs_p.set_stage(
                                &jid_p,
                                mangosteen_orchestration::progress::DeployStage::Assessing,
                                mangosteen_orchestration::progress::StageOutcome::Done,
                                format!(
                                    "模型评估完成，使用估算值输出模型加载所需算力资源：{}",
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
                if should_cancel() {
                    return Err(MnError::Cancelled {
                        stage: "节点配置环境".to_string(),
                    });
                }
                let g = gw_p.as_ref();
                g.prepare_with_progress(evaluated, &req, &sink)
            });

            // 回滚与云端提交都需要一份独立网关句柄。
            // 本地 `commit`（启动本地引擎）已随纯云端架构移除。
            let gw_for_abort = Arc::clone(&gw);

            // ---- 回滚：撤销这次部署产生的全部痕迹 ----
            //
            // 由 pulljobs 在检测到用户取消时调用。要点：
            //  · 停引擎（可能已经起来了）
            //  · 删台账记录（不是标 destroyed——回滚要「当没发生过」）
            //  · 释放端口预留（否则反复「部署→停止」会吃光端口区间）
            //  · 按 `delete_file` 决定是否删模型文件
            let gw_a = Arc::clone(&gw_for_abort);
            // ---- 云端提交：启动引擎并探活 ----
            //
            // 云端没有本地下载，`prepare` 完成后立刻进入这里。
            let jobs_cc = jobs.clone();
            let jid_cc = jid.clone();
            let gw_cc = Arc::clone(&gw_for_abort);
            let commit_cloud: crate::pulljobs::CommitCloudHook = Arc::new(move |prepared| {
                let sink_jobs = jobs_cc.clone();
                let sink_id = jid_cc.clone();
                let sink = move |stage: mangosteen_orchestration::progress::DeployStage,
                                 outcome: mangosteen_orchestration::progress::StageOutcome,
                                 detail: String| {
                    sink_jobs.set_stage(&sink_id, stage, outcome, detail);
                };
                let g = gw_cc.as_ref();
                let plan = g.commit_cloud(prepared, &sink)?;
                Ok(plan.id)
            });

            let gw_rs = Arc::clone(&gw_for_abort);
            let jobs_rs = jobs.clone();
            let jid_rs = jid.clone();
            let resume: crate::pulljobs::ResumeHook = Arc::new(move |dep_id| {
                let sink_jobs = jobs_rs.clone();
                let sink_id = jid_rs.clone();
                let sink = move |stage: mangosteen_orchestration::progress::DeployStage,
                                 outcome: mangosteen_orchestration::progress::StageOutcome,
                                 detail: String| {
                    sink_jobs.set_stage(&sink_id, stage, outcome, detail);
                };
                let g = gw_rs.as_ref();
                g.start_service_with_progress(dep_id, &sink)?;
                Ok(dep_id.to_string())
            });

            let abort: crate::pulljobs::AbortHook = Arc::new(move |dep_id, delete_file| {
                let Some(dep_id) = dep_id else { return Ok(()) };
                let g = gw_a.as_ref();
                g.abort_deployment(dep_id, delete_file)
            });

            let gw_invalid = Arc::clone(&gw_for_abort);
            let invalidate: crate::pulljobs::InvalidateHook = Arc::new(move |dep_id, reason| {
                let g = gw_invalid.as_ref();
                g.invalidate_deployment(dep_id, reason)
            });

            let refresh_gateway = Arc::clone(&self.gateway);
            let refresh_snapshot = Arc::clone(&self.backend_snapshot);
            let refresh_routes: crate::pulljobs::RefreshRoutesHook = Arc::new(move || {
                let gw = refresh_gateway.as_ref();
                let Ok(records) = gw.list_deployments() else {
                    return;
                };
                let Ok(mut snapshot) = refresh_snapshot.write() else {
                    return;
                };
                snapshot.clear();
                for record in records {
                    if record.status != mangosteen_base::state::DeploymentStatus::Ready {
                        continue;
                    }
                    let Some(port) = record.plan.local_access_port() else {
                        continue;
                    };
                    let model_id = mangosteen_core::model::ModelEntry::derive_id(
                        &record.plan.model.name,
                        record.plan.model.quant,
                    );
                    snapshot.insert(
                        model_id.clone(),
                        crate::protocol::BackendTarget {
                            host: "127.0.0.1".to_string(),
                            port,
                            model_id,
                            origin_label: record.plan.origin.label(),
                        },
                    );
                }
            });

            Some(crate::pulljobs::DeployHooks {
                prepare,
                commit_cloud,
                resume,
                invalidate,
                refresh_routes,
                abort,
            })
        } else {
            None
        };

        // 重新部署必须走完整阶段：已有资源只能在对应阶段内部校验并复用，
        // 不能在 HTTP 受理阶段直接跳过 GPU 选择、节点配置和下载流程。
        // 资源去重只允许发生在具体阶段内部（例如节点配置阶段发现依赖已存在），
        // 不能用它跳过阶段本身。
        //
        // 部署入口先清理同模型的历史台账记录和旧推理进程，防止旧的 ready/
        // provisioning 记录在轮询时把新任务误判成“部署已完成”。模型文件不删，
        // 下载阶段会重新做真实文件校验并复用完整资源。
        if deploy_after {
            if let Some(id) = purge_model_id.as_deref() {
                if let Err(e) = self.gateway.purge_model_deployments(id) {
                    return Response::error(&e);
                }
            }
        }

        if already {
            // 已有本地文件也必须经过完整部署阶段。这里只记录文件已存在；
            // 不再提前把任务标记 Done/Downloading Skipped 后直接 prepare。
            // worker 会在模型下载阶段重新做远端文件完整性与可用性确认。
            if deploy_after {
                self.pull_jobs.set_note(
                    &job_id,
                    "本地已存在模型文件，仍执行完整部署流程并在下载阶段重新校验",
                );
                let resumed = cfg
                    .storage
                    .models_dir
                    .join(format!("{file_name}.part"))
                    .is_file();
                // 阶段状态只能由 worker 在完成真实校验后上报。这里不能提前
                // 下载阶段只由真实 worker/prepare 流程上报，不能在 HTTP 受理时提前
                // 后续阶段已经“完成”，掩盖 auxiliary_models 尚未校验的问题。
                crate::pulljobs::spawn(
                    self.pull_jobs.clone(),
                    job_id.clone(),
                    source,
                    model,
                    opts,
                    register,
                    dep,
                    reuse_id,
                );
                if resumed {
                    self.pull_jobs
                        .set_note(&job_id, "检测到未完成的下载，将从断点继续并重新校验");
                }
                self.pull_jobs.prune(10 * 60 * 1000);
                return Response::json(
                    200,
                    &serde_json::json!({ "ok": true, "job_id": job_id, "reused": false }),
                );
            }

            // 纯下载请求仍保留原有的本地文件幂等语义。
            let model_id = match register(&final_path, Some(source.name.as_str())) {
                Ok(id) => id,
                Err(e) => {
                    self.pull_jobs
                        .mark_failed(&job_id, format!("登记模型失败：{e}"));
                    return Response::error(&e);
                }
            };
            let size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
            self.pull_jobs.mark_done(&job_id, &model_id, size, true);
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
                reuse_id,
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
}
