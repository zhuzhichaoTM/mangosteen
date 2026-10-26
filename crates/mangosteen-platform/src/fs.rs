//! 文件系统工具：原子写、权限、路径校验。
//!
//! 归属：**平台适配层**（同时服务基础能力层·文件访问控制模块）。
//!
//! ## 关键能力
//!
//! * **原子写**：先写临时文件再 `rename`，避免断电/崩溃导致配置或台账半截损坏。
//! * **权限收敛**：存放密钥的配置文件按 `0600` 权限创建（Windows 用 ACL 继承）。
//! * **路径规范化**：在真实路径上解析符号链接后再做白名单判断，防止绕过。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use mangosteen_core::{MnError, MnResult};

/// 原子写文件。
///
/// 步骤：同目录写 `.<name>.tmp` → `fsync` → `rename` 覆盖。
/// 同目录保证 `rename` 是同一文件系统内的原子操作。
pub fn atomic_write(path: &Path, contents: &[u8]) -> MnResult<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|e| MnError::io(format!("创建目录 {}", parent.display()), &e))?;

    let file_name = path
        .file_name()
        .ok_or_else(|| MnError::bad_request(format!("非法路径：{}", path.display())))?;
    let tmp = parent.join(format!(".{}.tmp", file_name.to_string_lossy()));

    {
        let mut f = fs::File::create(&tmp)
            .map_err(|e| MnError::io(format!("创建临时文件 {}", tmp.display()), &e))?;
        f.write_all(contents)
            .map_err(|e| MnError::io(format!("写入临时文件 {}", tmp.display()), &e))?;
        // 落盘后再 rename，否则崩溃可能留下空文件
        f.sync_all()
            .map_err(|e| MnError::io(format!("fsync {}", tmp.display()), &e))?;
    }

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        MnError::io(
            format!("原子替换 {} → {}", tmp.display(), path.display()),
            &e,
        )
    })
}

/// 原子写文本。
pub fn atomic_write_str(path: &Path, contents: &str) -> MnResult<()> {
    atomic_write(path, contents.as_bytes())
}

/// 读取文本文件，路径不存在时返回 `None`（区别于其它 IO 错误）。
pub fn read_to_string_opt(path: &Path) -> MnResult<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MnError::io(format!("读取 {}", path.display()), &e)),
    }
}

/// 以「仅属主可读写」权限写文件。用于持久化密钥类内容。
pub fn atomic_write_private(path: &Path, contents: &[u8]) -> MnResult<()> {
    atomic_write(path, contents)?;
    set_private_permissions(path)
}

/// 把文件权限收紧为 `0600`。
///
/// Windows 无 POSIX 权限位，改为依赖 `%APPDATA%` 目录本身的 ACL 继承，
/// 此处为无操作（但保留接口，便于未来加 ACL 调用）。
pub fn set_private_permissions(path: &Path) -> MnResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 配置文件可能含 API Key，权限必须收紧到 0600
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| MnError::io(format!("设置权限 {}", path.display()), &e))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 解析真实路径（跟随符号链接）。
///
/// 用于访问控制判断：若不做这一步，攻击者可在允许目录里放一个指向
/// `/etc/passwd` 的符号链接绕过白名单。
pub fn canonicalize_for_access(path: &Path) -> PathBuf {
    // 目标不存在时（如准备写入新文件）退化为规范化父目录 + 文件名
    if let Ok(c) = fs::canonicalize(path) {
        return c;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(cp) = fs::canonicalize(parent) {
            return cp.join(name);
        }
    }
    mangosteen_core::config::lexical_normalize(path)
}

/// 检查 `path` 的真实路径是否位于 `allowed_roots` 之下（任一即可）。
pub fn is_within_any(path: &Path, allowed_roots: &[&Path]) -> bool {
    let real = canonicalize_for_access(path);
    allowed_roots.iter().any(|root| {
        let r = canonicalize_for_access(root);
        real.starts_with(&r)
    })
}

/// 确保目录存在。
pub fn ensure_dir(path: &Path) -> MnResult<()> {
    fs::create_dir_all(path).map_err(|e| MnError::io(format!("创建目录 {}", path.display()), &e))
}

/// 查询目录所在分区可用空间（字节）。
///
/// 下载前预检要用：模型动辄几 GiB，等写到一半发现磁盘满，比提前拒绝糟得多
/// （`.part` 会占着空间，用户还得手工清）。
///
/// 目录不存在时退回其最近的已存在父目录——首次使用时模型库目录还没建，
/// 此时父分区的剩余空间才是真实可用的。
pub fn available_space(path: &Path) -> MnResult<u64> {
    #[cfg(unix)]
    {
        use std::ffi::CString;

        let mut probe = if path.exists() {
            Some(path.to_path_buf())
        } else {
            path.ancestors().nth(1).map(|p| p.to_path_buf())
        };
        // 往上找到第一个真实存在的目录
        while let Some(p) = probe.clone() {
            if p.exists() {
                break;
            }
            probe = p.parent().map(|x| x.to_path_buf());
        }
        let Some(p) = probe else {
            return Err(MnError::io(
                format!("查询磁盘空间 {}", path.display()),
                &std::io::Error::from(std::io::ErrorKind::NotFound),
            ));
        };

        let c = CString::new(p.to_string_lossy().as_bytes()).map_err(|_| {
            MnError::io(
                format!("查询磁盘空间 {}", p.display()),
                &std::io::Error::from(std::io::ErrorKind::InvalidInput),
            )
        })?;
        // SAFETY: statvfs 只写本函数提供的栈上缓冲区；指针在调用期间有效。
        unsafe {
            let mut st: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c.as_ptr(), &mut st) == 0 {
                return Ok(st.f_bavail as u64 * st.f_frsize as u64);
            }
        }
        Err(MnError::Io {
            context: format!("查询磁盘空间 {}", p.display()),
            reason: "statvfs 调用失败".to_string(),
        })
    }
    #[cfg(not(unix))]
    {
        // Windows 无 statvfs；用 GetDiskFreeSpaceExW
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::ffi::OsStrExt;
            let mut buf: Vec<u16> = path.as_os_str().encode_wide().collect();
            buf.push(0);
            let mut free: u64 = 0;
            // SAFETY: 传入以 NUL 结尾的宽字符串与三个可写 u64
            let ok = unsafe {
                windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                    buf.as_ptr(),
                    &mut free as *mut u64 as *mut _,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                return Ok(free);
            }
            return Err(MnError::Io {
                context: format!("查询磁盘空间 {}", path.display()),
                reason: "GetDiskFreeSpaceExW 失败".to_string(),
            });
        }
        #[allow(unreachable_code)]
        Err(MnError::Io {
            context: format!("查询磁盘空间 {}", path.display()),
            reason: "当前平台不支持查询可用空间".to_string(),
        })
    }
}

/// 人类可读字节数。
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut val = bytes as f64;
    let mut idx = 0;
    while val >= 1024.0 && idx < UNITS.len() - 1 {
        val /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.2} {}", val, UNITS[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mn-fs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_creates_and_overwrites() {
        let d = tmpdir("atomic");
        let p = d.join("a.txt");
        atomic_write_str(&p, "one").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "one");
        atomic_write_str(&p, "two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        // 不应残留临时文件
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn atomic_write_creates_missing_parents() {
        let d = tmpdir("atomic-nested");
        let p = d.join("x").join("y").join("z.toml");
        atomic_write_str(&p, "v").unwrap();
        assert!(p.is_file());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn read_optional_distinguishes_missing() {
        let d = tmpdir("readopt");
        let p = d.join("nope.txt");
        assert_eq!(read_to_string_opt(&p).unwrap(), None);
        fs::write(&p, "hi").unwrap();
        assert_eq!(read_to_string_opt(&p).unwrap().as_deref(), Some("hi"));
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn private_write_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("perm");
        let p = d.join("secret.toml");
        atomic_write_private(&p, b"token=abc").unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "密钥文件权限必须是 0600，实际 {mode:o}");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn within_any_allows_subpaths() {
        let d = tmpdir("within");
        let sub = d.join("models");
        fs::create_dir_all(&sub).unwrap();
        let f = sub.join("m.gguf");
        fs::write(&f, "x").unwrap();

        assert!(is_within_any(&f, &[d.as_path()]));
        assert!(is_within_any(&f, &[sub.as_path()]));
        assert!(!is_within_any(&f, &[Path::new("/etc")]));
        fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    fn within_any_blocks_symlink_escape() {
        let d = tmpdir("symlink");
        let allowed = d.join("allowed");
        fs::create_dir_all(&allowed).unwrap();
        let outside = d.join("outside.txt");
        fs::write(&outside, "secret").unwrap();
        let link = allowed.join("link.gguf");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        // 词法看 link 在 allowed 内，但真实路径在外 → 必须拒绝
        assert!(
            !is_within_any(&link, &[allowed.as_path()]),
            "符号链接逃逸未被拦截"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn canonicalize_nonexistent_uses_parent() {
        let d = tmpdir("canon");
        let p = d.join("will-be-created.txt");
        let c = canonicalize_for_access(&p);
        assert!(c.starts_with(fs::canonicalize(&d).unwrap()));
        assert!(c.ends_with("will-be-created.txt"));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn ensure_dir_is_idempotent() {
        let d = tmpdir("ensure");
        let p = d.join("a/b/c");
        ensure_dir(&p).unwrap();
        ensure_dir(&p).unwrap();
        assert!(p.is_dir());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn human_bytes_formatting() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1536), "1.50 KiB");
        assert_eq!(human_bytes(1 << 30), "1.00 GiB");
        assert_eq!(human_bytes(24 << 30), "24.00 GiB");
        assert_eq!(human_bytes(1 << 40), "1.00 TiB");
    }

    #[test]
    fn atomic_write_rejects_empty_relative_name() {
        // 根路径没有文件名 → 明确报参数错误而非 panic
        let e = atomic_write(Path::new("/"), b"x").unwrap_err();
        assert!(matches!(e, MnError::BadRequest { .. }));
    }
}
