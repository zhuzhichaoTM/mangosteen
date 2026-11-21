//! SSH 客户端平台适配：askpass 脚本、可执行文件名、`/dev/null` 等价物。
//!
//! 归属：**平台适配层**。上层（device / provider / orchestration）禁止出现
//! `cfg(target_os)`，凡是「Unix 有、Windows 没有」的差异都必须在这里收敛。
//!
//! ## 为什么需要这个模块
//!
//! OpenSSH 的**密码认证**没有命令行参数，只能走 askpass 协议：ssh 需要密码时
//! 去执行 `$SSH_ASKPASS` 指向的程序，读它的标准输出当密码。这个「程序」怎么写，
//! 各平台完全不同：
//!
//! | 平台 | askpass 形态 | 密码怎么取出 |
//! |------|--------------|--------------|
//! | macOS / Linux | `#!/bin/sh` 脚本，0700 | `printf '%s\n' '<密码>'` |
//! | Windows | `.cmd` 批处理 | `type "<密码文件>"`（`<nul set /p=` 保尾无换行） |
//!
//! Windows 上**没有** `/bin/sh`，把 POSIX 脚本丢过去 ssh 会直接
//! `exec(...) No such file or directory`，表现就是「密码正确但 Permission denied」。
//!
//! ## 设计选择：密码落地 sidecar，脚本只负责打印
//!
//! 传统做法是把密码内联进脚本、靠 shell 单引号转义。这在 `/bin/sh` 里勉强可行，
//! 在 `.cmd` 里是灾难：`%` `^` `&` `|` `<` `>` `"` 每一层都有不同的转义规则，
//! 密码里只要出现一个 `%` 整个脚本语义就变了。
//!
//! 所以这里改成：密码单独写进一个权限收敛的 sidecar 文件，脚本**只用平台的
//! 「读取文件」命令把它吐到 stdout**。脚本正文不含任何密码字符，跨平台只是换
//! 一条读取命令，不存在转义问题。
//!
//! 代价是密码在临时目录多存一份——由 [`Askpass::cleanup`] 在 ssh 进程退出后
//! 立刻删除，与脚本同寿命。

use std::path::{Path, PathBuf};

use mangosteen_core::{MnError, MnResult};

use crate::fs as pfs;

/// 一次 askpass 会话的两个临时文件：脚本本身 + 密码 sidecar。
///
/// 生命周期由调用方掌握：ssh 进程退出（无论成败）后必须调用
/// [`Askpass::cleanup`]，明文密码不该在临时目录里多留一秒。
#[derive(Debug, Clone)]
pub struct Askpass {
    /// 交给 `SSH_ASKPASS` 的脚本路径。
    pub script: PathBuf,
    /// 存放明文密码的 sidecar 文件（脚本从中读取）。无密码认证时为 `None`。
    pub secret: Option<PathBuf>,
}

impl Askpass {
    /// 删除脚本与密码文件。重复调用、文件已不存在都视为成功。
    ///
    /// 安全加固（2026-11-21）：用 `shred -u` 安全擦除（覆写后删除），
    /// 而非 `rm`（仅删目录项，磁盘可能残留）。shred 不可用时回退 rm。
    pub fn cleanup(&self) {
        secure_remove(&self.script);
        if let Some(s) = &self.secret {
            secure_remove(s);
        }
    }

    /// 供 `SSH_ASKPASS` 使用的脚本绝对路径字符串。
    pub fn script_arg(&self) -> String {
        self.script.display().to_string()
    }
}

/// 安全删除文件：优先 `shred -u`（覆写 3 轮 + 删除），回退 `rm`。
fn secure_remove(path: &std::path::Path) {
    let p = path.display().to_string();
    let _ = std::process::Command::new("shred")
        .args(["-u", "-n", "3", &p])
        .status()
        .is_ok();
    // shred 失败（如 macOS 无 shred）时 rm 兜底
    let _ = std::fs::remove_file(path);
}

/// 本平台的 ssh 可执行文件名（Windows 需要 `.exe`）。
pub const fn ssh_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "ssh.exe"
    } else {
        "ssh"
    }
}

/// 本平台的 scp 可执行文件名（Windows 需要 `.exe`）。
pub const fn scp_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "scp.exe"
    } else {
        "scp"
    }
}

/// 「丢弃一切输出」的文件路径。
///
/// `ssh -o UserKnownHostsFile=` 需要一个可写文件来丢弃主机指纹。
/// Unix 是 `/dev/null`，Windows 必须用 `NUL`——传 `/dev/null` 过去，
/// OpenSSH for Windows 会当成相对路径去创建文件而报错。
pub const fn null_device_path() -> &'static str {
    if cfg!(target_os = "windows") {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// P0/P1 SSH host key 钉扎（2026-11-19）：mangosteen 专用 known_hosts。
///
/// 引导链（bootstrap）成功后由 ssh-keyscan 写入实例指纹；此后 SSH 连接
/// 用 `UserKnownHostsFile=<此文件>` + `StrictHostKeyChecking=yes` 强校验，
/// 指纹不符直接拒绝（防中间人）。文件不存在时降级旧行为（/dev/null +
/// accept-new，兼容未引导场景）。路径固定在 {config_dir}/ssh_known_hosts。
pub fn pinned_known_hosts_path() -> std::path::PathBuf {
    crate::paths::config_dir(&crate::paths::real_env).join("ssh_known_hosts")
}

/// 生成本平台的 askpass 脚本 + 密码文件。
///
/// `tag` 用于区分不同会话的文件名（调用方传 `pid-port-millis` 之类）。
/// **必须**保证同一时刻并发调用的 `tag` 不重复：脚本路径若被复用，
/// 一个会话的 cleanup 会误删另一个会话正在用的脚本，ssh 报
/// `exec(...) No such file or directory` 后拿不到密码。
///
/// 返回 `Ok(None)` 表示 `password` 为 `None`（走密钥/agent 认证，无需 askpass）。
pub fn write_askpass(tag: &str, password: Option<&str>) -> MnResult<Option<Askpass>> {
    let Some(pw) = password else {
        return Ok(None);
    };

    let dir = std::env::temp_dir();
    // 安全加固（2026-11-21）：密码直接嵌入脚本（不再单独写 .secret 文件），
    // 减少明文落盘文件数（2→1）。脚本 0700，cleanup 用 shred -u 安全擦除。
    let (script_path, body) = if cfg!(target_os = "windows") {
        let p = dir.join(format!("mn-askpass-{tag}.cmd"));
        // Windows：密码作为 echo 参数嵌入批处理。
        (p, format!("@echo off\r\necho {}\r\n", pw.replace('"', "\"\"")))
    } else {
        // Unix：密码作为 printf 参数嵌入 sh 脚本，用单引号转义防注入。
        let p = dir.join(format!("mn-askpass-{tag}.sh"));
        let quoted = shell_single_quote(pw);
        (p, format!("#!/bin/sh\nprintf '%s\\n' {quoted}\n"))
    };

    std::fs::write(&script_path, body.as_bytes()).map_err(|e| {
        MnError::io(
            format!("写入 askpass 脚本 {}", script_path.display()),
            &e,
        )
    })?;
    if let Err(e) = pfs::set_private_permissions(&script_path) {
        return Err(e);
    }

    Ok(Some(Askpass {
        script: script_path,
        secret: None, // 不再单独写 .secret 文件
    }))
}

/// 用单引号包住一段文本，供 POSIX shell 安全取用。
///
/// 单引号内除 `'` 之外一切字符都是字面量；`'` 用 `'\''` 收尾再续，
/// 这样密码里的 `$`、反引号、空格、`!` 都不会被 shell 解释。
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r#"'\''"#))
}

/// 该路径是否是本平台的可丢弃设备（`/dev/null` 或 `NUL`）。
///
/// 用于判断「已有配置里写死的是否与当前平台匹配」——从别的平台迁移
/// 过来的配置可能带着 `/dev/null`，在 Windows 上需要改写。
pub fn is_null_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s == "/dev/null" || s.eq_ignore_ascii_case("nul")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_password_means_no_askpass() {
        assert!(write_askpass("t-none", None).unwrap().is_none());
    }

    #[test]
    fn askpass_roundtrip_via_real_shell() {
        // 密码含单引号、美元符号、反引号：验证转义后仍能原样输出
        let pw = "p@ss'w$rd`x";
        let a = write_askpass("t-roundtrip", Some(pw)).unwrap().unwrap();
        #[cfg(unix)]
        {
            let out = std::process::Command::new("/bin/sh")
                .arg(&a.script)
                .output()
                .unwrap();
            assert!(out.status.success());
            // 脚本输出 = 密码 + 换行
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim_end(), pw);
        }
        let secret = a.secret.clone().unwrap();
        assert!(secret.exists());
        a.cleanup();
        assert!(!a.script.exists(), "脚本应被删除");
        assert!(!secret.exists(), "密码文件应被删除");
    }

    #[test]
    fn cleanup_is_idempotent() {
        let a = write_askpass("t-idem", Some("x")).unwrap().unwrap();
        a.cleanup();
        a.cleanup(); // 二次调用不应 panic
    }

    #[test]
    fn tags_produce_distinct_paths() {
        // 并发会话必须各自持有独立脚本：同 tag 复用会让 cleanup 互相误删
        let a = write_askpass("t-dup-1", Some("1")).unwrap().unwrap();
        let b = write_askpass("t-dup-2", Some("2")).unwrap().unwrap();
        assert_ne!(a.script, b.script);
        // 删掉 a 之后 b 必须仍在
        a.cleanup();
        assert!(!a.script.exists());
        assert!(b.script.exists());
        b.cleanup();
    }

    #[test]
    fn binary_and_null_device_match_platform() {
        if cfg!(target_os = "windows") {
            assert!(ssh_binary_name().ends_with(".exe"));
            assert_eq!(null_device_path(), "NUL");
        } else {
            assert_eq!(ssh_binary_name(), "ssh");
            assert_eq!(null_device_path(), "/dev/null");
        }
    }

    #[test]
    fn single_quote_escapes_quote() {
        assert_eq!(shell_single_quote("a'b"), r#"'a'\''b'"#);
    }
}
