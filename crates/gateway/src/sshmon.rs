//! SSH 登录信息的后台刷新。
//!
//! 归属：**API 网关层**（部署页「登陆方式」的数据源）。
//!
//! ## 为什么不能放在请求路径里
//!
//! `GET /api/deployments/{id}/ssh-info` 曾被部署页轮询，而它的实现是
//! 「持网关锁 → 调厂商 `describe()`」。厂商接口慢或网络抖动时（实测
//! 出现 `Connection refused` / `Operation timed out` / `Network is
//! unreachable` 连续重试），锁被占住数十秒，**整个 HTTP 层排队**：
//!
//! | 现象 | 真实原因 |
//! |------|----------|
//! | 下载进度不动、消息中断 | `/api/models/jobs` 拿不到锁 |
//! | 页面提示「连接失败」 | `/api/status` 超时 |
//! | 对话流式输出卡住 | 流式响应也走同一把锁 |
//!
//! 与 `cloudmon` 同一结论：**任何网络调用都不得在网关锁内执行**，
//! 更不该由 HTTP 请求驱动。
//!
//! ## 现在的结构
//!
//! ```text
//! 后台线程（每 20 秒）：
//!   ① 持锁：只读台账，列出云端部署（微秒级）
//!   ② 释放锁：逐个调厂商刷新，写入 ssh_info_cache
//! HTTP 端点：
//!   只读 ssh_info_cache（纯内存），未命中就返回 pending
//! ```
//!
//! 代价是「登陆方式」在实例刚创建后可能有十几秒空窗（显示 pending）。
//! 这个代价是刻意的：宁可比厂商晚几十秒，也不能让整个网关跟着卡。
//!
//! ## 刷新频率
//!
//! 「登陆方式」只在实例停机再开机后才可能变（端口重新映射），
//! 属于分钟级变化。20 秒足够，且远低于厂商接口的限流阈值。

use std::sync::Arc;
use std::time::Duration;

use crate::gateway::Gateway;

/// 刷新间隔。
const REFRESH_INTERVAL: Duration = Duration::from_secs(20);

/// 启动后台刷新线程。
///
/// 返回 `JoinHandle` 便于测试或优雅退出时观测；生产路径直接丢弃即可
/// （线程随进程结束）。
pub fn spawn_ssh_info_refresher(gateway: Arc<Gateway>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || loop {
        // ① 只读台账。绝不在这里碰网络。
        let ids: Vec<String> = {
            let gw = gateway.as_ref();
            gw.deployments_needing_ssh_info()
        };

        // ② 逐个刷新。网关以 Arc 只读共享，Orchestrator 的云锁是
        // 细粒度内部锁，厂商调用不会阻塞 HTTP 路径。
        for id in ids {
            let gw = gateway.as_ref();
            let _ = gw.refresh_ssh_info(&id);
            // 两个部署之间喘口气，避免打爆厂商限流
            std::thread::sleep(Duration::from_millis(50));
        }

        std::thread::sleep(REFRESH_INTERVAL);
    })
}
