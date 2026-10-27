//! 销毁指定实例（`StopCompShareInstance` → `TerminateCompShareInstance`）。
//!
//! 用法：
//!
//! ```bash
//! export COMPSHARE_PUBLIC_KEY='...'
//! export COMPSHARE_PRIVATE_KEY='...'
//! cargo run -p mangosteen-cloud --example destroy_instance -- uhost-xxxx
//! ```
//!
//! ## 为什么需要这个示例
//!
//! 厂商的 `TerminateCompShareInstance` **只接受 `Stopped` 状态**的实例。
//! 运行中的实例必须先关机、等它真的变成 `Stopped`，才能删除。
//!
//! 这个「先关机再删除」的补偿逻辑已经封装在
//! [`mangosteen_cloud::compshare::CompshareDriver::destroy`]（`ProviderDriver` 实现）里，
//! 本示例只是把它暴露成一个可执行的命令行工具。
//!
//! ## 费用安全
//!
//! 实例每多跑一分钟就多计费一分钟，所以销毁流程刻意做成
//! 「关机 → 轮询确认 Stopped → 删除 → 再确认」：
//! 任何一步没确认就继续下一步，都可能留下计费中的孤儿实例。

use std::time::Instant;

use mangosteen_cloud::compshare::{CompshareClient, Credential, CurlTransport};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let id = std::env::args()
        .nth(1)
        .ok_or("用法：destroy_instance <UHostId>")?;

    let credential = Credential::new(
        env("COMPSHARE_PUBLIC_KEY").ok_or("缺少 COMPSHARE_PUBLIC_KEY")?,
        env("COMPSHARE_PRIVATE_KEY").ok_or("缺少 COMPSHARE_PRIVATE_KEY")?,
    )?;

    // 先查实例属于哪个可用区：删除接口需要 Region/Zone
    let probe = CompshareClient::new(
        mangosteen_cloud::compshare::DEFAULT_ENDPOINT,
        credential.clone(),
        CurlTransport::new(30),
        "",
        "",
    )?;
    let raw = probe.call(
        "DescribeCompShareInstance",
        {
            let mut p = mangosteen_cloud::compshare::Params::new();
            p.set_list("UHostIds", std::slice::from_ref(&id));
            p
        },
        mangosteen_cloud::compshare::client::Scope::None,
    )?;
    let arr = raw
        .get("UHostSet")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(uhost) = arr.first() else {
        println!("实例 {id} 不存在（可能已释放）。无需处理。");
        return Ok(());
    };
    let region = uhost
        .get("Region")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let zone = uhost
        .get("Zone")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state = uhost
        .get("State")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let name = uhost
        .get("Name")
        .and_then(|v| v.as_str())
        .unwrap_or("-")
        .to_string();

    println!("=== 待销毁实例 ===");
    println!("实例 ID : {id}");
    println!("名称    : {name}");
    println!("位置    : {region}/{zone}");
    println!("当前状态: {state}");
    println!();

    let client = CompshareClient::new(
        mangosteen_cloud::compshare::DEFAULT_ENDPOINT,
        credential,
        CurlTransport::new(40),
        &region,
        &zone,
    )?;

    let t0 = Instant::now();

    // ---------- 1. 若非 Stopped，先关机 ----------
    if !state.eq_ignore_ascii_case("Stopped") {
        println!("[1/3] 关机中（厂商要求 Terminate 仅对 Stopped 生效）…");
        client.stop_instance(&id)?;

        // 轮询直到真的 Stopped。关机是异步的，不等确认就删会失败。
        let deadline = Instant::now() + std::time::Duration::from_secs(300);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3));
            let r = client.describe_instances(Some(std::slice::from_ref(&id)), 10, 0)?;
            let st = r
                .uhost_set
                .first()
                .map(|u| u.state.clone())
                .unwrap_or_default();
            print!("\r      当前状态：{st}    ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            if st.eq_ignore_ascii_case("Stopped") {
                println!();
                break;
            }
            if Instant::now() >= deadline {
                println!();
                return Err("等待关机超时（300s）。请到控制台确认后再试。".into());
            }
        }
        println!("[1/3] ✓ 已关机");
    } else {
        println!("[1/3] ✓ 已是 Stopped，无需关机");
    }

    // ---------- 2. 删除 ----------
    println!("[2/3] 删除实例…");
    let resp = client.terminate_instance(&id, true)?;
    println!(
        "[2/3] ✓ 已提交删除（InRecycle={}）",
        if resp.in_recycle.is_empty() {
            "-"
        } else {
            &resp.in_recycle
        }
    );

    // ---------- 3. 确认已消失 ----------
    println!("[3/3] 确认实例已释放…");
    let deadline = Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let r = client.describe_instances(Some(std::slice::from_ref(&id)), 10, 0)?;
        if r.uhost_set.is_empty() {
            println!("[3/3] ✓ 实例已从列表消失，计费已停止");
            break;
        }
        let st = r.uhost_set[0].state.clone();
        print!("\r      当前状态：{st}    ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        if Instant::now() >= deadline {
            println!();
            println!("[3/3] ⚠ 仍在列表中（可能进入回收站）。请到控制台确认。");
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }

    println!();
    println!("完成，耗时 {:.1}s", t0.elapsed().as_secs_f64());
    println!("实例 {id} 已销毁，不再计费。");
    Ok(())
}
