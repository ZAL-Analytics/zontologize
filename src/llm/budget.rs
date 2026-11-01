use sqlx::types::chrono::{DateTime, Utc};

use super::Windows;
use super::openrouter::Prices;

pub const MIN_CALLS: i64 = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Calibration {
    pub calls: i64,
    pub chars: f64,
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cost_usd: f64,
    pub seconds: f64,
    pub five_hour_per_usd: Option<f64>,
    pub seven_day_per_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Defaults {
    pub chars_per_token: f64,
    pub output_tokens: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Estimate {
    pub requests: u64,
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub usd: Option<f64>,
    pub seconds: Option<f64>,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    pub calibrated: bool,
}

pub fn estimate(
    requests: u64,
    chars: u64,
    calibration: &Calibration,
    prices: Option<Prices>,
    defaults: Defaults,
    concurrency: usize,
) -> Estimate {
    let calibrated = calibration.calls >= MIN_CALLS && calibration.chars > 0.0;
    let calls = calibration.calls.max(1) as f64;
    let tokens_per_char = if calibrated {
        calibration.input_tokens / calibration.chars
    } else {
        1.0 / defaults.chars_per_token
    };
    let output_per_request = if calibrated {
        calibration.output_tokens / calls
    } else {
        defaults.output_tokens
    };
    let input_tokens = chars as f64 * tokens_per_char;
    let output_tokens = requests as f64 * output_per_request;
    let learned_tokens = calibration.input_tokens + calibration.output_tokens;
    let usd = match prices {
        Some(prices) => Some(input_tokens * prices.input + output_tokens * prices.output),
        None if calibrated && learned_tokens > 0.0 => {
            Some(calibration.cost_usd * (input_tokens + output_tokens) / learned_tokens)
        }
        None => None,
    };
    let seconds =
        calibrated.then(|| calibration.seconds / calls * requests as f64 / concurrency as f64);
    let share = |per_usd: Option<f64>| Some(usd? * per_usd?);
    Estimate {
        requests,
        input_tokens,
        output_tokens,
        usd,
        seconds,
        five_hour: share(calibration.five_hour_per_usd),
        seven_day: share(calibration.seven_day_per_usd),
        calibrated,
    }
}

pub fn windows_needed(share: f64, used_now: f64, max_use: f64) -> u64 {
    let room_now = (max_use - used_now).max(0.0);
    if share <= room_now {
        return 1;
    }
    1 + ((share - room_now) / max_use).ceil() as u64
}

pub fn window_full(windows: &Windows, max_use: f64, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    [windows.five_hour, windows.seven_day]
        .into_iter()
        .flatten()
        .filter(|w| w.used >= max_use && w.resets_at > now)
        .map(|w| w.resets_at)
        .max()
}

pub fn cap_reached(
    spent_usd: f64,
    requests: u64,
    max_usd: f64,
    max_requests: u64,
) -> Option<String> {
    if max_usd > 0.0 && spent_usd >= max_usd {
        return Some(format!(
            "spent ${spent_usd:.4} of the ${max_usd:.4} cap; rerun with a higher --max-usd"
        ));
    }
    if max_requests > 0 && requests >= max_requests {
        return Some(format!(
            "sent {requests} of {max_requests} requests; rerun with a higher --max-requests"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Window;
    use sqlx::types::chrono::TimeZone;

    const DEFAULTS: Defaults = Defaults {
        chars_per_token: 4.0,
        output_tokens: 100.0,
    };

    #[test]
    fn estimate_uses_defaults_until_calibrated() {
        let prices = Prices {
            input: 1e-6,
            output: 2e-6,
        };
        let e = estimate(
            10,
            40_000,
            &Calibration::default(),
            Some(prices),
            DEFAULTS,
            2,
        );
        assert!(!e.calibrated);
        assert_eq!(e.input_tokens, 10_000.0);
        assert_eq!(e.output_tokens, 1_000.0);
        assert!((e.usd.unwrap() - 0.012).abs() < 1e-12);
        assert_eq!(e.seconds, None);
        let calibration = Calibration {
            calls: 20,
            chars: 20_000.0,
            input_tokens: 10_000.0,
            output_tokens: 4_000.0,
            cost_usd: 0.28,
            seconds: 40.0,
            five_hour_per_usd: Some(0.5),
            seven_day_per_usd: None,
        };
        let e = estimate(10, 40_000, &calibration, None, DEFAULTS, 2);
        assert!(e.calibrated);
        assert_eq!(e.input_tokens, 20_000.0);
        assert_eq!(e.output_tokens, 2_000.0);
        assert!((e.usd.unwrap() - 0.44).abs() < 1e-9);
        assert_eq!(e.seconds, Some(10.0));
        assert!((e.five_hour.unwrap() - 0.22).abs() < 1e-9);
        assert_eq!(e.seven_day, None);
    }

    #[test]
    fn windows_needed_counts_the_current_one() {
        assert_eq!(windows_needed(0.1, 0.2, 0.8), 1);
        assert_eq!(windows_needed(0.7, 0.2, 0.8), 2);
        assert_eq!(windows_needed(2.0, 0.9, 0.8), 4);
    }

    #[test]
    fn full_window_waits_for_its_reset() {
        let now = Utc.with_ymd_and_hms(2030, 1, 1, 12, 0, 0).unwrap();
        let five = Utc.with_ymd_and_hms(2030, 1, 1, 15, 0, 0).unwrap();
        let seven = Utc.with_ymd_and_hms(2030, 1, 5, 0, 0, 0).unwrap();
        let windows = Windows {
            five_hour: Some(Window {
                used: 0.85,
                resets_at: five,
            }),
            seven_day: Some(Window {
                used: 0.3,
                resets_at: seven,
            }),
        };
        assert_eq!(window_full(&windows, 0.8, now), Some(five));
        assert_eq!(window_full(&windows, 0.9, now), None);
        assert_eq!(window_full(&windows, 0.8, five), None);
        let both = Windows {
            seven_day: Some(Window {
                used: 0.95,
                resets_at: seven,
            }),
            ..windows
        };
        assert_eq!(window_full(&both, 0.8, now), Some(seven));
    }

    #[test]
    fn caps_stop_and_zero_means_none() {
        assert!(cap_reached(0.5, 10, 0.0, 0).is_none());
        assert!(cap_reached(0.5, 10, 0.5, 0).unwrap().contains("--max-usd"));
        assert!(
            cap_reached(0.1, 3, 1.0, 3)
                .unwrap()
                .contains("--max-requests")
        );
        assert!(cap_reached(0.1, 2, 1.0, 3).is_none());
    }
}
