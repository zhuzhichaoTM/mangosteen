//! 本地部署提交：启动引擎、等待健康检查与失败降级。
//!
//! 归属：第三层 · 编排调度核心（部署编排子模块）。

use super::*;

impl Orchestrator {
    /// 「提交」阶段：启动本地引擎并等待健康检查。
    ///
    /// 调用方需保证**模型文件此刻已在磁盘上**（无论原本就有还是刚下载完）。
    /// 重复调用会因端口占用或引擎已启动而失败，不做幂等处理——
    /// 静默复用会让「上次启动到一半崩了」这类问题被掩盖。
    pub fn commit_local(
        &mut self,
        prepared: PreparedLocal,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        let result = self.commit_local_inner(prepared, sink);
        if let Err(e) = &result {
            sink(
                crate::progress::DeployStage::Failed,
                crate::progress::StageOutcome::Failed,
                format!("{e}"),
            );
        }
        result
    }
    /// 「提交」阶段主体：启动引擎并等待健康检查。
    ///
    /// 本地启动失败后的降级也在这一层：`prepare` 里已经落过台账，
    /// 所以失败必须显式把状态改成 `Failed`，否则会留下一条永远
    /// `provisioning` 的记录，UI 上表现为「卡在部署中」。
    pub(crate) fn commit_local_inner(
        &mut self,
        prepared: PreparedLocal,
        sink: crate::progress::ProgressSink<'_>,
    ) -> MnResult<DeploymentPlan> {
        use crate::progress::{DeployStage, StageOutcome};

        let PreparedLocal {
            id,
            evaluated,
            plan,
            req,
        } = prepared;

        // 模型路径可能刚被 `with_model_path` 补上（下载场景：prepare 时
        // 文件还不存在）。台账在 prepare 阶段已按空路径落过盘，这里必须
        // 同步更新，否则后续「按台账重新启动 / 探活」会拿到空路径。
        if let Some(rec) = self.state.get(&id)? {
            if rec.plan.local.as_ref().map(|l| l.model_path.as_str())
                != Some(plan.model_path.as_str())
            {
                let mut updated = rec.clone();
                if let Some(l) = updated.plan.local.as_mut() {
                    l.model_path = plan.model_path.clone();
                }
                self.state.upsert(updated)?;
            }
        }

        // ---- 4. 量化策略（本链路不做自动量化）----
        //
        // 用户下的已经是量化版 GGUF，再量化一次只降精度。如实标「跳过」，
        // 不假装跑过——假装会让用户以为多花了几分钟。
        sink(
            DeployStage::Quantizing,
            StageOutcome::Skipped,
            format!(
                "无需量化：{} 已是 {} 量化版本",
                evaluated.spec.name,
                evaluated.spec.quant.as_str()
            ),
        );

        // ---- 5. 启动 ----
        sink(
            DeployStage::Launching,
            StageOutcome::Running,
            format!("正在启动 {} 并等待健康检查", plan.engine.display_name()),
        );
        match self.launch_local(&id, &plan) {
            Ok(handle) => {
                let pid = handle.pid();
                self.engines.insert(id.clone(), handle);
                self.state.set_pid(&id, pid)?;
                self.state.set_status(&id, DeploymentStatus::Ready, None)?;
                sink(
                    DeployStage::Launching,
                    StageOutcome::Done,
                    // pid 为 0 表示外部复用的服务（Ollama），
                    // 不是本进程拉起的子进程
                    if pid == 0 {
                        format!("外部引擎已就绪（端口 {}）", plan.port)
                    } else {
                        format!("引擎已就绪（PID {pid}，端口 {}）", plan.port)
                    },
                );
                self.state
                    .get(&id)?
                    .map(|r| r.plan)
                    .ok_or_else(|| MnError::bad_request("台账记录丢失"))
            }
            Err(local_err) => {
                let reason = format!("{local_err}");
                self.state
                    .set_status(&id, DeploymentStatus::Failed, Some(reason.clone()))?;

                // 用户选了「本地算力」→ 绝不降级上云。
                // 静默降级会直接产生计费，是最不能接受的意外。
                if req.target.forces_local() {
                    return Err(MnError::InsufficientLocalResource {
                        detail: format!(
                            "本地部署失败：{reason}。\
                             你选择了「本地算力」，不会自动改用云端（避免意外计费）"
                        ),
                    });
                }

                // 允许降级 → 重新决策。`prepare` 里已经算过一次本地方案，
                // 这里要的是「本地失败后的降级结论」，因此走 degrade 分支。
                let mut trail = vec![format!("local:{}", short(&reason))];
                let fallback = self
                    .scheduler
                    .degrade_after_local_failure(&evaluated, &req, &reason, 0)?;
                match fallback {
                    Decision::Cloud {
                        plan: cloud_plan,
                        reason: why,
                    } => {
                        trail.push(format!("cloud:{}", short(&why)));
                        sink(
                            DeployStage::Selecting,
                            StageOutcome::Done,
                            format!("本地失败，已降级到云端：{why}"),
                        );
                        self.deploy_cloud(
                            &evaluated,
                            *cloud_plan,
                            trail,
                            &format!("本地部署失败（{reason}）"),
                            sink,
                        )
                    }
                    Decision::Reject { reason } => Err(MnError::InsufficientLocalResource {
                        detail: format!(
                            "本地部署失败且无法降级：{reason}（原始错误：{local_err}）"
                        ),
                    }),
                    Decision::Local { .. } => {
                        // 理论上不会走到这里（degrade 只产出 Cloud/Reject）
                        Err(MnError::InsufficientLocalResource {
                            detail: format!("本地部署失败：{local_err}，降级决策异常"),
                        })
                    }
                }
            }
        }
    }
    /// 执行本地部署。
    pub(crate) fn launch_local(
        &self,
        deployment_id: &str,
        plan: &mangosteen_core::plan::LocalPlan,
    ) -> MnResult<EngineHandle> {
        // ---- 取插件，不写引擎分支 ----
        //
        // 这里**只问「当前引擎对应哪个插件」**。启动方式、是否拉进程、
        // 怎么探活全部由插件自己决定（llama.cpp 拉子进程，Ollama 复用
        // 常驻守护进程）。以前这里是 `if engine == Ollama { ... }`，
        // 每接一个新框架都要改本函数——正是基线禁止的「改动主链路」。
        let engine = self.config.local.engine;
        let Some(plugin) = self.engines_registry.get(engine) else {
            return Err(MnError::bad_request(format!(
                "推理引擎 {} 尚未适配（未注册插件）。\
                 请在设置页改用已支持的引擎，或使用命令行自行启动 {}。",
                engine.display_name(),
                engine.display_name()
            )));
        };
        plugin.precheck()?;

        let timeout = Duration::from_secs(self.config.scheduler.launch_timeout_secs);
        let mut attempt = 0;
        let max_attempts = 2; // 端口冲突时换一个端口重试一次
        let mut current = plan.clone();

        loop {
            match plugin.launch(&current, timeout) {
                Ok(h) => return Ok(h),
                Err(e) => {
                    attempt += 1;
                    // 端口被占用是最常见的瞬时失败，换端口重试即可。
                    // 复用型引擎（Ollama）不占这个端口，重试也无害。
                    if attempt < max_attempts && is_port_conflict(&e) {
                        let next = next_port(current.port, &self.config.local);
                        current.port = next;
                        let _ = deployment_id;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}
