//! 响应组装与请求参数小工具。
//!
//! 归属：**第二层 · API Server**。这些函数只做数据整形（任务状态 JSON、
//! 查询串布尔标记、错误摘要），不触发任何部署或推理动作。

/// 评估结果的单行摘要（含资源占用明细）。
///
/// 与 `gateway::assessed_detail` 同样的信息，但在 server 侧构造——
/// 那边是 `pub(crate)` 之外的私有函数，且这里要用在**持有任务表锁**的
/// 闭包里（不能再进网关锁拿数据）。
pub(crate) fn probe_assessed_detail(ev: &mangosteen_orchestration::assess::Evaluated) -> String {
    mangosteen_orchestration::deployment::assessed_detail(ev)
}

/// 从模型引用推一个人类可读的名字（用于粗估兜底）。
///
/// 去掉 `.gguf` 后缀与量化尾缀，得到如 `qwen2.5-0.5b-instruct`。
/// 量化尾缀写法多样（`-q4_k_m` / `.Q4_K_M`），复用 `ModelRef` 自己的
/// 解析器，避免两处规则分叉。
pub(crate) fn model_name_for_estimate(model: &mangosteen_core::source::ModelRef) -> String {
    let f = model.local_file_name();
    let stem = f.strip_suffix(".gguf").unwrap_or(&f).to_string();
    match model.quant_from_filename() {
        Some(q) => {
            let lower = stem.to_ascii_lowercase();
            let qs = q.as_str().to_ascii_lowercase();
            match lower.rfind(&qs) {
                Some(pos) => {
                    let cut = stem[..pos].trim_end_matches(['-', '.', '_']).to_string();
                    if cut.is_empty() {
                        stem
                    } else {
                        cut
                    }
                }
                None => stem,
            }
        }
        None => stem,
    }
}

/// 取错误信息第一行（弹窗/阶段说明里只放一句能读懂的话）。
pub(crate) fn first_line(msg: &str) -> String {
    msg.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("未知原因")
        .trim()
        .to_string()
}

/// 任务状态 → JSON。字段名是 UI 的契约，改名会让进度条白屏。
pub(crate) fn job_json(j: &crate::pulljobs::PullJob) -> serde_json::Value {
    // 步骤条：把「全部步骤」与「每步最新状态」一次给全。
    //
    // 为什么在后端算而不是前端拼：前端只拿到「当前阶段」时，无法区分
    // 「这步还没到」与「这步跳过了」——后者必须显示原因。把状态判定
    // 放在这里，UI 只管画。
    let trail = &j.deploy_trail;
    let steps: Vec<serde_json::Value> = mangosteen_orchestration::progress::DeployStage::STEPS
        .iter()
        .map(|s| {
            // 取该阶段的最后一条事件作为它的状态
            let last = trail.iter().rev().find(|e| e.stage == s.id());
            let outcome = last.map(|e| e.outcome.as_str()).unwrap_or("pending");
            serde_json::json!({
                "id": s.id(),
                "label": s.label(),
                "index": s.index(),
                "outcome": outcome,
                "detail": last.map(|e| e.detail.clone()).unwrap_or_default(),
                "at_ms": last.map(|e| e.at_ms),
            })
        })
        .collect();

    serde_json::json!({
        "id": j.id,
        "spec": j.spec,
        "file": j.file,
        "state": j.state.as_str(),
        "done_bytes": j.done_bytes,
        "total_bytes": j.total_bytes,
        "percent": j.percent(),
        "bytes_per_sec": j.bytes_per_sec(),
        "model_id": j.model_id,
        "error": j.error,
        "note": j.note,
        // warning ≠ error：下载成功但文件不是模型（如 imatrix）
        "warning": j.warning,
        // 自动部署相关：UI 用它展示「准备中 → 部署中 → 已启动」
        "deploy_after": j.deploy_after,
        "deploy_state": j.deploy_state,
        // 结构化进度：当前阶段 + 序号 + 总步数 + 每步状态
        "deploy_stage": j.deploy_stage.map(|s| serde_json::json!({
            "id": s.id(),
            "label": s.label(),
            "index": s.index(),
            "total": mangosteen_orchestration::progress::DeployStage::total(),
        })),
        "deploy_detail": j.deploy_detail,
        "deploy_steps": steps,
        "deploy_trail": trail.iter().map(|e| serde_json::json!({
            "stage": e.stage,
            "label": e.label,
            "outcome": e.outcome.as_str(),
            "detail": e.detail,
            "at_ms": e.at_ms,
        })).collect::<Vec<_>>(),
        "deployment_id": j.deployment_id,
        "already_present": j.already_present,
        // 能否「停止」：只有还在跑的任务可以中止并回滚。
        // 已终结的（done/failed/cancelled）不该再给停止按钮——
        // 点了也没意义，还会让用户以为能撤销一个已经完成的部署。
        "can_abort": !j.state.is_terminal() && j.deploy_after,
    })
}

/// 从查询串中取布尔标记。
///
/// 接受 `1` / `true` / `yes` / 空值（`?flag`）作为真，其余为假。
/// 单独成函数是因为路由分支只拿得到 `ParsedRequest`，而 Query 解析
/// 不应依赖整个请求对象。
pub(crate) fn query_flag(query: &str, key: &str) -> bool {
    for part in query.split('&') {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        if k == key {
            let v = v.trim().to_ascii_lowercase();
            return matches!(v.as_str(), "" | "1" | "true" | "yes" | "on");
        }
    }
    false
}
