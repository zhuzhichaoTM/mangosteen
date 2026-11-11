//! `mangosteen device`
//!
//! 归属：**用户交互层 → 命令 `device`**。
//!
//! 管理自定义远端节点（设置页「自定义节点」的 CLI 对应）。
//! 自定义节点是用户自己的机器，部署时优先复用，不走云厂商创建。
//!
//! 子命令：
//! | 子命令 | 作用 |
//! |--------|------|
//! | `list` | 列出所有自定义节点，与 UI "自定义节点" 对应 |
//! | `add` | 新增一个自定义节点 |
//! | `remove` | 删除一个自定义节点 |
//! | `test` | 测试指定节点的 SSH 连通性 |

use mangosteen_core::config::SecretRef;
use mangosteen_core::MnResult;

use crate::cli::Args;
use crate::commands::{build_gateway, load_config, reject_unknown_options};
use crate::output;

use mangosteen_platform::paths;

/// 允许的选项。
const ALLOWED: &[&str] = &[
    "config",
    "json",
    "yes",
    "quiet",
    "user",
    "workdir",
    "password",
    "name",
    "env",
    "enable",
    "disable",
];

/// 入口。
pub fn run(args: Args) -> MnResult<i32> {
    if args.has("help") {
        print_device_help();
        return Ok(0);
    }
    reject_unknown_options(&args, ALLOWED)?;

    let sub = args.pos(1).unwrap_or("list").to_string();
    match sub.as_str() {
        "list" | "ls" => list(args),
        "add" => add(args),
        "remove" | "rm" => remove(args),
        "test" => test(args),
        other => Err(mangosteen_core::MnError::bad_request(format!(
            "未知子命令 `device {other}`。可用：list / add / remove / test"
        ))),
    }
}

/// `device list` - 列出所有自定义节点。
fn list(args: Args) -> MnResult<i32> {
    let gw = build_gateway(&args)?;
    let cfg = gw.config();
    let devices = &cfg.remote_devices;

    if args.has("json") {
        let arr: Vec<serde_json::Value> = devices
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "name": d.name,
                    "host": d.host,
                    "port": d.port,
                    "user": d.user,
                    "workdir": d.workdir,
                    "password_configured": d.has_password(),
                    "created_at": d.created_at,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"devices": arr}))
                .unwrap_or_default()
        );
        return Ok(0);
    }

    if devices.is_empty() {
        println!("{}", output::dim("（当前没有自定义节点）"));
        println!();
        println!(
            "  {}",
            output::dim("执行 `mangosteen device add <主机> <端口> --user <用户>` 新增一个节点")
        );
        return Ok(0);
    }

    let rows: Vec<Vec<String>> = devices
        .iter()
        .map(|d| {
            let id = if d.id.trim().is_empty() {
                mangosteen_core::config::RemoteDevice::derive_id(&d.host, d.port)
            } else {
                d.id.clone()
            };
            let name = if d.name.trim().is_empty() {
                format!("{}:{}", d.host, d.port)
            } else {
                d.name.clone()
            };
            vec![
                id,
                name,
                d.host.clone(),
                d.port.to_string(),
                d.user.clone(),
                d.workdir.clone(),
                if d.has_password() { "已配置" } else { "未配置" }.to_string(),
            ]
        })
        .collect();
    output::print_table(
        &["ID", "名称", "主机", "端口", "用户", "工作目录", "密码"],
        &rows,
    );
    println!();
    println!(
        "  {}",
        output::dim("用 `mangosteen device test <ID>` 检查连通性")
    );
    Ok(0)
}

/// `device add <host> <port>` - 新增一个自定义节点。
fn add(args: Args) -> MnResult<i32> {
    let host = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少主机名。用法：mangosteen device add <主机> <端口> [--user <用户>] [--workdir <目录>] [--password <密码>] [--name <名称>]",
        )
    })?;
    let port_str = args.pos(3).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少端口号。用法：mangosteen device add <主机> <端口> [--user <用户>] [--workdir <目录>] [--password <密码>] [--name <名称>]",
        )
    })?;
    let port: u16 = port_str
        .parse()
        .map_err(|_| {
            mangosteen_core::MnError::bad_request(format!(
                "端口号必须是数字，收到 `{port_str}`"
            ))
        })?;

    let user = args
        .get("user")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "root".to_string());
    let workdir = args
        .get("workdir")
        .map(|s| s.to_string())
        .unwrap_or_else(|| "/root/mangosteen".to_string());
    let name = args.get("name").map(|s| s.to_string());

    let gw = build_gateway(&args)?;
    let mut cfg = gw.config().as_ref().clone();
    let store = gw.config_store();

    // 检查是否已存在相同主机:端口的节点（同一台机器应更新而不是新增）
    let existing_index = cfg
        .remote_devices
        .iter()
        .position(|d| d.host == host && d.port == port);

    let mut device = mangosteen_core::config::RemoteDevice {
        id: if let Some(idx) = existing_index {
            cfg.remote_devices[idx].id.clone()
        } else {
            String::new()
        },
        name: name.clone().unwrap_or_default(),
        host: host.to_string(),
        port,
        user: user.clone(),
        password: SecretRef::None,
        workdir: workdir.clone(),
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };

    // 处理密码
    if let Some(password) = args.get("password") {
        let secret = password.trim();
        if secret.is_empty() {
            return Err(mangosteen_core::MnError::bad_request("密码不能为空"));
        }
        let env = &mangosteen_platform::paths::real_env as &dyn Fn(&str) -> Option<String>;
        let dir = mangosteen_platform::paths::config_dir(env).join("secrets");
        mangosteen_platform::fs::ensure_private_dir(&dir)?;
        let path = dir.join(format!("device-{}:{}", host, port));
        mangosteen_platform::fs::write_private_file(&path, secret)?;
        device.password = SecretRef::File(path);
    }

    // 规范化
    let device = device.normalized()?;

    if let Some(idx) = existing_index {
        cfg.remote_devices[idx] = device.clone();
    } else {
        cfg.remote_devices.push(device.clone());
    }

    cfg.validate()?;
    let written = store.save(&cfg)?;
    gw.replace_config(cfg);
    gw.reload_cloud_drivers();

    output::print_success(&format!(
        "自定义节点已{} → {}",
        if existing_index.is_some() { "更新" } else { "新增" },
        written.display()
    ));
    println!(
        "  ID: {}  主机: {}:{}  用户: {}  工作目录: {}",
        device.id, device.host, device.port, device.user, device.workdir
    );
    println!(
        "  {}",
        output::dim("用 `mangosteen device test <ID>` 检查连通性")
    );
    Ok(0)
}

/// `device remove <id>` - 删除一个自定义节点。
fn remove(args: Args) -> MnResult<i32> {
    let id = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少设备 ID。用法：mangosteen device remove <ID>",
        )
    })?;

    let gw = build_gateway(&args)?;
    let mut cfg = gw.config().as_ref().clone();
    let store = gw.config_store();

    let before = cfg.remote_devices.len();
    cfg.remote_devices.retain(|d| {
        let dev_id = if d.id.trim().is_empty() {
            mangosteen_core::config::RemoteDevice::derive_id(&d.host, d.port)
        } else {
            d.id.clone()
        };
        dev_id != id
    });

    if cfg.remote_devices.len() == before {
        return Err(mangosteen_core::MnError::bad_request(format!(
            "未找到 ID 为 `{id}` 的自定义节点。用 `mangosteen device list` 查看可用 ID"
        )));
    }

    cfg.validate()?;
    let written = store.save(&cfg)?;
    gw.replace_config(cfg);
    gw.reload_cloud_drivers();

    output::print_success(&format!("自定义节点 {id} 已删除 → {}", written.display()));
    Ok(0)
}

/// `device test <id>` - 测试指定节点的 SSH 连通性。
fn test(args: Args) -> MnResult<i32> {
    let id = args.pos(2).ok_or_else(|| {
        mangosteen_core::MnError::bad_request(
            "缺少设备 ID。用法：mangosteen device test <ID>",
        )
    })?;

    let gw = build_gateway(&args)?;

    // 先查找设备显示信息
    let cfg = gw.config();
    let device = cfg
        .remote_devices
        .iter()
        .find(|d| {
            let dev_id = if d.id.trim().is_empty() {
                mangosteen_core::config::RemoteDevice::derive_id(&d.host, d.port)
            } else {
                d.id.clone()
            };
            dev_id == id
        })
        .ok_or_else(|| {
            mangosteen_core::MnError::bad_request(format!(
                "未找到 ID 为 `{id}` 的自定义节点。用 `mangosteen device list` 查看可用 ID"
            ))
        })?;

    print!("正在检查 {} ... ", device.host);
    gw.test_device(id)?;
    println!("{}", output::paint("连通", output::Color::Green));
    Ok(0)
}

/// 打印 device 子命令帮助。
fn print_device_help() {
    println!("mangosteen device <子命令> [选项]");
    println!();
    println!("管理自定义远端节点（设置页「自定义节点」的 CLI 对应）。");
    println!("自定义节点是用户自己的机器，部署时优先复用，不走云厂商创建。");
    println!();
    println!("{}", output::bold("子命令"));
    println!("  {:<w$}  {}", "list", "列出所有自定义节点", w = 18);
    println!("  {:<w$}  {}", "add", "新增一个自定义节点", w = 18);
    println!("  {:<w$}  {}", "remove", "删除一个自定义节点", w = 18);
    println!("  {:<w$}  {}", "test", "测试指定节点的 SSH 连通性", w = 18);
    println!();
    println!("{}", output::bold("全局选项"));
    println!("  {:<w$}  {}", "--config <path>", "指定配置文件", w = 22);
    println!("  {:<w$}  {}", "--json", "以 JSON 输出（便于脚本处理）", w = 22);
    println!("  {:<w$}  {}", "--quiet", "抑制 banner 与进度提示", w = 22);
    println!();
    println!("{}", output::bold("device add 选项"));
    println!("  {:<w$}  {}", "--user <用户>", "SSH 登录用户名（默认 root）", w = 22);
    println!("  {:<w$}  {}", "--workdir <目录>", "远端工作目录（默认 /root/mangosteen）", w = 22);
    println!("  {:<w$}  {}", "--password <密码>", "SSH 登录密码", w = 22);
    println!("  {:<w$}  {}", "--name <名称>", "节点显示名称", w = 22);
    println!("  {:<w$}  {}", "--env", "从环境变量读取密码", w = 22);
    println!("  {:<w$}  {}", "--enable", "启用节点", w = 22);
    println!("  {:<w$}  {}", "--disable", "禁用节点", w = 22);
    println!();
    println!("{}", output::bold("示例"));
    println!("  mangosteen device list");
    println!("  mangosteen device add 192.168.1.100:22 --user root --name 我的节点");
    println!("  mangosteen device remove dev-192.168.1.100:22");
    println!("  mangosteen device test dev-192.168.1.100:22");
    println!();
    println!("更多信息：docs/USAGE.md");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_derive_id_is_stable() {
        let id1 = mangosteen_core::config::RemoteDevice::derive_id("192.168.1.100", 22);
        let id2 = mangosteen_core::config::RemoteDevice::derive_id("192.168.1.100", 22);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("dev-192.168.1.100:22"));
    }

    #[test]
    fn device_derive_id_sanitizes_host() {
        let id = mangosteen_core::config::RemoteDevice::derive_id("my-host.example.com", 2222);
        assert!(id.starts_with("dev-my-host.example.com:2222"));
        // 不应包含非法字符
        assert!(!id.contains(' '));
    }
}
