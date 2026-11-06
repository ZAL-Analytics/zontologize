const DEFAULT_SHARE_PERCENT: u64 = 60;
const FALLBACK_LIMIT_MB: u64 = 8 * 1024;

fn field_kb(text: &str, name: &str) -> Option<u64> {
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
    let total_kb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| field_kb(&text, "MemTotal"));
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| cgroup_max(&text));
    limit_mb(total_kb, cgroup)
}

fn cgroup_max(cgroups: &str) -> Option<u64> {
    let path = cgroups.lines().find_map(|line| line.strip_prefix("0::"))?;
    let file = format!("/sys/fs/cgroup{}/memory.max", path.trim_end_matches('/'));
    std::fs::read_to_string(file).ok()?.trim().parse().ok()
}

fn limit_mb(total_kb: Option<u64>, cgroup_bytes: Option<u64>) -> u64 {
    let total_mb = total_kb.map(|kb| kb / 1024);
    let cgroup_mb = cgroup_bytes.map(|bytes| bytes / 1024 / 1024);
    let available = match (total_mb, cgroup_mb) {
        (Some(total), Some(cgroup)) => total.min(cgroup),
        (total, cgroup) => match total.or(cgroup) {
            Some(mb) => mb,
            None => return FALLBACK_LIMIT_MB,
        },
    };
    available * DEFAULT_SHARE_PERCENT / 100
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

    #[test]
    fn cgroup_limit_lowers_the_default() {
        assert_eq!(limit_mb(Some(16 * 1024 * 1024), None), 16 * 1024 * 60 / 100);
        assert_eq!(
            limit_mb(Some(16 * 1024 * 1024), Some(1024 * 1024 * 1024)),
            1024 * 60 / 100
        );
        assert_eq!(limit_mb(None, None), FALLBACK_LIMIT_MB);
        assert_eq!(cgroup_max("1:cpu:/\n"), None);
    }
}
