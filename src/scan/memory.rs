use std::time::Duration;

const CHECK_EVERY: Duration = Duration::from_millis(250);
const DEFAULT_SHARE_PERCENT: u64 = 60;
const FALLBACK_LIMIT_MB: u64 = 8 * 1024;
pub const EXIT_CODE: i32 = 3;

pub fn field_kb(text: &str, name: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

pub fn process_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
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

pub fn watch(limit_mb: u64) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let Some(used) = process_mb() else {
                return;
            };
            if used > limit_mb {
                eprintln!(
                    "error: memory use reached {used} MB, over extract.max_memory_mb = {limit_mb}; \
                     stopping the scan. Run scan again: the files that were being read are \
                     retried one at a time, and a file that does this twice is recorded as an error"
                );
                std::process::exit(EXIT_CODE);
            }
        }
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
        let used = process_mb().unwrap();
        assert!(used > 0 && used < default_limit_mb() * 10);
    }
}
