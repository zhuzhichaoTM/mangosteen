//! 模型资产处理器（`/api/models*`、模型下载任务）。
//!
//! 归属：第二层 API Server。处理器通过 `pub(crate)` 字段访问网关状态，
//! 对外行为与拆分前完全一致。

use crate::http::server::*;
use mangosteen_core::openai::ErrorResponse;

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
