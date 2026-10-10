//! 基础工具：哈希、时间戳、记录 ID。刻意不引入 chrono / uuid 这类依赖：宿主会按插件加载
//! sidecar，体积和编译时间都要压着，而这几件事各自十几行就能做完。

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// 防篡改链的基本原语：任意字符串的 SHA-256，输出 64 位小写十六进制。库里已有的 digest
/// 都出自这里，所以算法和拼接方式（见 store.rs 的 `content_digest`）上线后就不能再改，否则历史记录全部校验失败。
pub fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    to_hex(&hasher.finalize())
}

pub fn to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

/// 当前 Unix 毫秒时间戳。系统时钟早于 1970 时 `duration_since` 会返回 Err（个别容器里真会出现），
/// 这里退化成 0 而不是 panic：时间戳不准只是一条记录难看，panic 却会让 SDK 的 worker 线程死掉。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_millis() as i64)
        .unwrap_or_default()
}

/// 生成 `sg_xxxxxxxxxxxxxxxx` 形式的记录 ID：取种子哈希的前 16 位十六进制。种子里混了进程号、
/// 进程内自增序号和毫秒时间戳（见 store.rs 的 `next_record_id`），所以同一毫秒内的并发提交也不会撞 ID。
pub fn short_id(prefix: &str, seed: &str) -> String {
    // sha256_hex 恒为 64 个 ASCII 字符，按字节切前 16 个不可能切在字符中间。
    format!("{prefix}{}", &sha256_hex(seed)[..16])
}

/// 毫秒时间戳 → `YYYY-MM-DD HH:MM:SSZ`（UTC），不引入日期库。CSV 导出的 `createdAtUtc` /
/// `decidedAtUtc` 两列用它；main.rs 还取结果的前 10 个字符当导出文件名里的日期，所以这个格式的前缀长度不能动。
pub fn format_ms_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)，即 Howard Hinnant 的 civil_from_days。
/// 用 400 年（146097 天）为一个 era 分段，因此不需要闰年查表；`div_euclid` / `rem_euclid`
/// 保证 1970 年以前的负数天数也能正确换算。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    (if month <= 2 { year + 1 } else { year }, month as u32, day as u32)
}

/// 主机名。用来把 operator 的兜底从"哪个账号"升级成"哪台机器上的哪个账号"：多人共用一台机、
/// 或同名账号分散在多台机时，光有 `USER` 是分不开的。
/// 刻意不加依赖（`libc::gethostname` 要多一个 crate），按平台走最省事的来源：Linux 读
/// `/proc/sys/kernel/hostname`，其余平台退回环境变量 `HOSTNAME` / `COMPUTERNAME`。都拿不到就
/// 返回 `None`——调用方必须能接受，因为 GUI 启动的进程经常没有 `HOSTNAME`，macOS/Windows 也没有 `/proc`。
pub fn hostname() -> Option<String> {
    if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        if let Some(name) = normalize_hostname(&contents) {
            return Some(name);
        }
    }
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|value| normalize_hostname(&value))
}

/// 去空白、拒空串，并限制长度：这个名字会进审计记录和 CSV，不该被一个异常长的值撑爆。
fn normalize_hostname(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty() && trimmed.chars().count() <= 64).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch_boundaries() {
        assert_eq!(format_ms_utc(0), "1970-01-01 00:00:00Z");
        assert_eq!(format_ms_utc(1_700_000_000_000), "2023-11-14 22:13:20Z");
        assert_eq!(format_ms_utc(-86_400_000), "1969-12-31 00:00:00Z");
    }

    #[test]
    fn handles_leap_day() {
        // 2024-02-29 12:20:00Z，闰日当天
        assert_eq!(format_ms_utc(1_709_209_200_000), "2024-02-29 12:20:00Z");
    }

    #[test]
    fn hostname_rejects_blank_and_oversized_values() {
        assert_eq!(normalize_hostname("  \n "), None);
        assert_eq!(normalize_hostname("dbx-box\n").as_deref(), Some("dbx-box"));
        assert_eq!(normalize_hostname(&"h".repeat(65)), None);
        assert_eq!(normalize_hostname(&"h".repeat(64)).map(|name| name.len()), Some(64));
    }

    #[test]
    fn hostname_never_panics() {
        // 结果取决于运行环境（CI 容器里可能真拿不到），所以只断言它不炸，
        // 且拿到的值一定是修剪过的。
        if let Some(name) = hostname() {
            assert_eq!(name, name.trim());
            assert!(!name.is_empty());
        }
    }
}
