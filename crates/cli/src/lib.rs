//! # Mangosteen CLI
//!
//! **用户交互层**。提供两种操作方式：
//!
//! * **命令行**：`mangosteen <子命令>`，适合脚本化与快速操作；
//! * **TUI**：`mangosteen tui`，全屏交互界面，适合日常浏览与操作。
//!
//! ## 铁律
//!
//! 本 crate **只与 API 网关层交互**：
//!
//! ```text
//! ✅ 允许：use mangosteen_gateway::{Gateway, GatewayBuilder, DeploymentRequest}
//! ❌ 禁止：use mangosteen_local::LlamaCppEngine          ← 直连本地引擎
//! ❌ 禁止：use mangosteen_cloud::CloudOrchestrator       ← 直连云厂商抽象
//! ❌ 禁止：直接发 HTTP 给云厂商 API
//! ```
//!
//! 但允许依赖 `mangosteen-core`（领域类型，如 `Config`、`ModelSpec`）与
//! `mangosteen-base`（台账读取）——它们是**共享的领域模型**，不是算力执行能力。
//!
//! 这条约束由 `tests/layering.rs` 检查。

pub mod cli;
pub mod commands;
pub mod output;
pub mod tui;

/// 进程退出。
pub fn run() -> i32 {
    match cli::run(std::env::args().collect()) {
        Ok(code) => code,
        Err(e) => {
            output::print_error(&e);
            e.exit_code()
        }
    }
}
