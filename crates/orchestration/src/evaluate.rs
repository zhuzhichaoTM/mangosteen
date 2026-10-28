//! 部署评估：本地文件 / 远端元数据 / 粗估三条入口。
//!
//! 归属：第三层 · 编排调度核心（部署编排子模块）。

use super::*;

impl Orchestrator {
    /// 用远端预取的 GGUF 元数据评估（模型尚未下载到本地）。
    ///
    /// 供 [`the pull job manager`] 的「评估 → 选设备 → 配置 → 下载 → 启动」
    /// 链路在**下载前**调用。`info` 由 `mangosteen_base::probe_remote_metadata`
    /// 通过 HTTP `Range` 拿到文件头部后解析而来。
    pub fn evaluate_remote(
        &self,
        info: &mangosteen_core::gguf::GgufInfo,
        req: &DeploymentRequest,
    ) -> Evaluated {
        self.assessor.evaluate_remote(info, req)
    }
    /// 按「名字 + 量化 + 体积」粗估评估（远端元数据预取失败时的兜底）。
    ///
    /// 调用方必须把结论标注为估算——见 [`Assessor::evaluate_estimated`]。
    pub fn evaluate_estimated(
        &self,
        name: &str,
        quant: mangosteen_core::model::Quantization,
        file_size_bytes: u64,
        req: &DeploymentRequest,
    ) -> Evaluated {
        self.assessor
            .evaluate_estimated(name, quant, file_size_bytes, req)
    }
    /// 评估：按模型 ID 查台账或按路径直接解析。
    pub fn evaluate(&self, req: &DeploymentRequest) -> MnResult<Evaluated> {
        // 1. 先按台账 ID / 名称查
        if let Some(entry) = self.models.get(&req.model)? {
            if let mangosteen_core::model::ModelLocation::Local { path } = &entry.location {
                if path.is_file() {
                    return self.assessor.evaluate_file(path, req);
                }
            }
            // 云端模型或本地文件已丢失 → 用台账里的规格评估（不读盘）
            return Ok(self.assessor.evaluate_spec(&entry.spec, req));
        }

        // 2. 按模糊名查
        let hits = self.models.find_by_name(&req.model)?;
        if hits.len() == 1 {
            if let mangosteen_core::model::ModelLocation::Local { path } = &hits[0].location {
                if path.is_file() {
                    return self.assessor.evaluate_file(path, req);
                }
            }
            return Ok(self.assessor.evaluate_spec(&hits[0].spec, req));
        }
        if hits.len() > 1 {
            return Err(MnError::bad_request(format!(
                "模型名 `{}` 匹配到 {} 个模型：{}。请使用完整 ID",
                req.model,
                hits.len(),
                hits.iter()
                    .map(|h| h.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        // 3. 当作文件路径
        let p = PathBuf::from(&req.model);
        if p.is_file() {
            return self.assessor.evaluate_file(&p, req);
        }

        Err(MnError::ModelNotFound { path: p })
    }
    /// 评估器引用（供上层在**不重入网关方法**时直接做一次评估）。
    ///
    /// 为什么需要：`pulljobs` 的部署钩子在 prepare 阶段要判断
    /// 「本地文件在不在，在就读盘精确评估」，而这个判断发生在闭包内、
    /// 已经短暂持有网关锁。走 [`Orchestrator::evaluate`] 会绕一圈（它按 ID
    /// 查台账），而这里手上的就是确切路径，直接用评估器更准。
    pub fn assessor_public(&self) -> &Assessor {
        &self.assessor
    }
}
