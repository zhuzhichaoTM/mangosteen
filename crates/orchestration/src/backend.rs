//! 后端算力目标：编排层选定的推理服务地址。
//!
//! 属于第三层产出的路由目标；第二层 HTTP 只做转发，不关心其来源。

/// 后端算力目标：网关转发时的目的地。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendTarget {
    /// 监听地址（本地为 `127.0.0.1`，云端经隧道后也是 `127.0.0.1`）
    pub host: String,
    /// 端口
    pub port: u16,
    /// 该后端的模型 ID（用于 `auto` 替换与响应回填）
    pub model_id: String,
    /// 算力来源描述（写入响应与健康检查）
    pub origin_label: String,
    /// 上游 API Key（safety 分支，改动点 2）。
    ///
    /// 有值时网关转发（流式 curl 与非流式 http_post_json）会附
    /// `Authorization: Bearer <key>`——适配启用 `--api-key-file` 的
    /// llama-server。同源部署（本地 127.0.0.1）无 key 时为 `None`，
    /// 行为与旧版完全一致。
    pub api_key: Option<String>,
    /// TLS 是否启用（P1 传输加密）：true 时 base_url 用 https——
    /// llama-server 由引导链证书以 `--ssl-key-file/--ssl-cert-file`
    /// 启动，网关转发走 https（自签证书，curl `-k`）。
    pub tls: bool,
}

impl BackendTarget {
    /// base URL（不含 `/v1`）。TLS 启用时 https（自签证书）。
    pub fn base_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// OpenAI 兼容 base URL。
    pub fn openai_base_url(&self) -> String {
        format!("{}/v1", self.base_url())
    }

    /// 完整上游 URL。
    pub fn url_for(&self, path: &str) -> String {
        format!(
            "{}/v1/{}",
            self.base_url().trim_end_matches('/'),
            path.trim_start_matches("/v1/").trim_start_matches('/')
        )
    }
}
