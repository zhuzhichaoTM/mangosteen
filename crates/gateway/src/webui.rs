//! 内置 Web 控制台（四页签）。
//!
//! 归属：**第一层·用户交互层**，由第二层 API Server 同源托管。
//! 页面资源外置为单文件 HTML，构建时通过 `include_str!` 嵌入，
//! 不增加前端构建链或运行时文件依赖。

/// 控制台页面。
pub const INDEX_HTML: &str = include_str!("assets/index.html");
