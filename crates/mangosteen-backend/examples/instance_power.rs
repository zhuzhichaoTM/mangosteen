//! 启动/停止一个已存在的云端实例（不创建、不销毁）。
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! cargo run -p mangosteen-backend --example instance_power -- start uhost-xxxx
//! cargo run -p mangosteen-backend --example instance_power -- stop  uhost-xxxx
//! ```
//!
//! ## 为什么不直接调 CLI
//!
//! 项目约束「网关唯一收口」：CLI 不直连厂商。这个示例属于**排障工具**，
//! 与 `describe_instances` / `destroy_instance` 同类，用于验证适配层。
//! 正常业务路径是 `mangosteen deploy` / `mangosteen stop`。

use mangosteen_backend::{
    CompshareClient, CompshareConfig, Credential, CurlTransport, DEFAULT_ENDPOINT,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let action = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "start".to_string());
    let id = std::env::args()
        .nth(2)
        .ok_or("用法：instance_power <start|stop> <uhost-id>")?;

    let pk = std::env::var("COMPSHARE_PUBLIC_KEY").map_err(|_| "请设置 COMPSHARE_PUBLIC_KEY")?;
    let sk = std::env::var("COMPSHARE_PRIVATE_KEY").map_err(|_| "请设置 COMPSHARE_PRIVATE_KEY")?;

    let cfg = CompshareConfig::default();
    let client = CompshareClient::new(
        DEFAULT_ENDPOINT,
        Credential::new(pk, sk)?,
        CurlTransport::new(60),
        cfg.region,
        cfg.zone,
    )?;

    println!("=== 实例 {id} ===");
    let before = client.describe_instances(Some(std::slice::from_ref(&id)), 100, 0)?;
    let cur = before
        .uhost_set
        .iter()
        .find(|u| u.id == id)
        .ok_or("实例不存在")?;
    println!("当前状态 : {}", cur.state);
    println!("GPU      : {} × {}", cur.gpu_type, cur.gpu);
    println!("内存     : {} MB", cur.memory_mb);
    println!("SSH      : {}", cur.ssh_login_command);
    println!();

    match action.as_str() {
        "start" => {
            println!("→ StartCompShareInstance");
            let r = client.start_instance(&id, None)?;
            println!("  RetCode = {}", r.ret_code);
        }
        "stop" => {
            println!("→ StopCompShareInstance");
            let r = client.stop_instance(&id)?;
            println!("  RetCode = {}", r.ret_code);
        }
        other => return Err(format!("未知动作 {other}（只支持 start / stop）").into()),
    }

    // 轮询到目标状态（厂商是异步的）
    let target = if action == "start" {
        "Running"
    } else {
        "Stopped"
    };
    println!();
    println!("→ 等待状态变为 {target}（最多 180 秒）");
    for i in 1..=60 {
        std::thread::sleep(std::time::Duration::from_secs(3));
        let r = client.describe_instances(Some(std::slice::from_ref(&id)), 100, 0)?;
        let u = match r.uhost_set.iter().find(|u| u.id == id) {
            Some(u) => u,
            None => continue,
        };
        println!("  [{}] {}", i * 3, u.state);
        if u.state.eq_ignore_ascii_case(target) {
            println!();
            println!("=== 已到达 {target} ===");
            println!("SSH 命令 : {}", u.ssh_login_command);
            println!("公网 IP  : {}", u.primary_ip().unwrap_or("—"));
            if let Some(p) = &u.instance_price {
                println!("实例价   : {p}（{}）", u.charge_type);
            }
            return Ok(());
        }
        if u.state.eq_ignore_ascii_case("Install") {
            // 镜像安装阶段，继续等
        }
    }
    Err(format!("等待 {target} 超时").into())
}
