const DEFAULT_SHARE_PERCENT: u64 = 60;
const FALLBACK_LIMIT_MB: u64 = 8 * 1024;

pub fn field_kb(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

pub fn pid_mb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss = field_kb(&status, "VmRSS")?;
    let swap = field_kb(&status, "VmSwap").unwrap_or(0);
    Some((rss + swap) / 1024)
}

pub fn default_limit_mb() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| field_kb(&text, "MemTotal"))
        .map_or(FALLBACK_LIMIT_MB, |kb| {
            kb / 1024 * DEFAULT_SHARE_PERCENT / 100
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fields_in_kb() {
        let text = "Name:\tzontologize\nVmRSS:\t  204800 kB\nVmSwap:\t    1024 kB\n";
        assert_eq!(field_kb(text, "VmRSS"), Some(204800));
        assert_eq!(field_kb(text, "VmSwap"), Some(1024));
        assert_eq!(field_kb(text, "VmData"), None);
        assert_eq!(field_kb("VmRSSX: 5 kB\n", "VmRSS"), None);
    }

    #[test]
    fn own_process_is_measured() {
        let used = pid_mb(std::process::id()).unwrap();
        assert!(used > 0 && used < default_limit_mb() * 10);
    }
}
