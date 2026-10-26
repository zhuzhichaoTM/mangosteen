//! 时间工具。集中收敛「取当前时间」，便于测试注入固定时钟。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 Unix 秒。系统时钟早于 epoch 时返回 0，不 panic。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 当前 Unix 毫秒。
pub fn now_unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 把 Unix 秒格式化为 `YYYY-MM-DD HH:MM:SS UTC`，避免为 CLI 引入 chrono 依赖。
pub fn format_unix_utc(secs: i64) -> String {
    if secs <= 0 {
        return "-".to_string();
    }
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02} UTC")
}

/// 由「自 1970-01-01 起的天数」反解公历年月日（Howard Hinnant 算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_1970() {
        assert_eq!(format_unix_utc(0), "-");
        assert_eq!(format_unix_utc(1), "1970-01-01 00:00:01 UTC");
    }

    #[test]
    fn known_timestamps() {
        // 2024-01-01T00:00:00Z = 1704067200
        assert_eq!(format_unix_utc(1_704_067_200), "2024-01-01 00:00:00 UTC");
        // 2026-10-24T08:30:00Z
        assert_eq!(format_unix_utc(1_792_830_600), "2026-10-24 08:30:00 UTC");
    }

    #[test]
    fn now_is_plausible() {
        let n = now_unix();
        assert!(n > 1_700_000_000, "system clock looks wrong: {n}");
    }
}
