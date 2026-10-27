//! Mangosteen 第一层 Web UI：离线单文件静态界面。
//!
//! 页面只通过 HTTP API 与第二层 `gateway` 通信，不依赖网关实现细节。

/// 单文件控制台 HTML。
pub const INDEX_HTML: &str = include_str!("index.html");
