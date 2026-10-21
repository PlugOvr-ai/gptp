//! Local interface health from /sys/class/net/<if>/statistics.
//!
//! Drop counters separate congestion from physical-layer trouble:
//! rx_crc_errors / rx_frame_errors mean cabling, connectors, or PHY —
//! common in vehicles and invisible to ping — while rx_missed/fifo point
//! at overload. Sampled once per second; increases raise alarms in the
//! event log (rate-limited per counter).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::state::{CounterStat, LinkHealth, Status};

const RATE_COUNTERS: [&str; 4] = ["rx_bytes", "tx_bytes", "rx_packets", "tx_packets"];
const ERROR_COUNTERS: [&str; 10] = [
    "rx_dropped",
    "tx_dropped",
    "rx_errors",
    "tx_errors",
    "rx_missed_errors",
    "rx_fifo_errors",
    "rx_over_errors",
    "rx_crc_errors",
    "rx_frame_errors",
    "collisions",
];

pub fn run(iface: String, state: Arc<Mutex<Status>>, quit: Arc<AtomicBool>) {
    let mut last: HashMap<&'static str, u64> = HashMap::new();
    let mut last_alarm: HashMap<&'static str, Instant> = HashMap::new();
    let mut first = true;
    let mut last_at = Instant::now();

    while !quit.load(Ordering::SeqCst) {
        let mut health = LinkHealth::default();
        let now = Instant::now();
        let secs = now.duration_since(last_at).as_secs_f64().max(0.1);
        last_at = now;
        // Some drivers' counters glitch (read low for a moment, or are
        // rebuilt from hardware registers); a byte delta beyond what the
        // link can carry is such a glitch, not traffic.
        let max_bytes = read_speed_mbps(&iface).map(|m| m as f64 * 1e6 / 8.0 * secs * 2.0);
        for name in RATE_COUNTERS {
            let Some(total) = read_counter(&iface, name) else {
                continue;
            };
            let limit = if name.ends_with("_bytes") {
                max_bytes
            } else {
                None
            };
            let delta = rate_delta(last.get(name).copied(), total, limit);
            last.insert(name, total);
            let rate = delta as f64 / secs;
            match name {
                "rx_bytes" => health.rx_bps = rate * 8.0,
                "tx_bytes" => health.tx_bps = rate * 8.0,
                "rx_packets" => health.rx_pps = rate.round() as u64,
                "tx_packets" => health.tx_pps = rate.round() as u64,
                _ => {}
            }
        }
        for name in ERROR_COUNTERS {
            let Some(total) = read_counter(&iface, name) else {
                continue; // not exposed by this driver
            };
            let delta = total.saturating_sub(*last.get(name).unwrap_or(&total));
            last.insert(name, total);
            if total > 0 {
                health.problems.push(CounterStat {
                    name: name.to_string(),
                    total,
                    delta,
                });
            }
            // The first sample only establishes the baseline; pre-existing
            // totals are shown but not alarmed.
            if !first && delta > 0 {
                let now = Instant::now();
                let due = last_alarm
                    .get(name)
                    .is_none_or(|t| now.duration_since(*t).as_secs() >= 30);
                if due {
                    last_alarm.insert(name, now);
                    let hint = if name.contains("crc") || name.contains("frame") {
                        " — check cabling/connectors"
                    } else if name.contains("missed") || name.contains("fifo") {
                        " — receiver overload"
                    } else {
                        ""
                    };
                    state.lock().unwrap().log(format!(
                        "ALARM: {iface} {name} +{delta} (total {total}){hint}"
                    ));
                }
            }
        }
        health
            .problems
            .sort_by_key(|c| (std::cmp::Reverse(c.delta), std::cmp::Reverse(c.total)));
        state.lock().unwrap().link = health;
        first = false;

        for _ in 0..10 {
            if quit.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn read_counter(iface: &str, name: &str) -> Option<u64> {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{name}"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Link speed in Mb/s; None when down or unknown (-1).
fn read_speed_mbps(iface: &str) -> Option<u64> {
    let v: i64 = std::fs::read_to_string(format!("/sys/class/net/{iface}/speed"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (v > 0).then_some(v as u64)
}

/// Counter increase since the previous sample. A first sample, a counter
/// that went backwards (reset or glitch), or an increase beyond `limit`
/// counts as 0: the new value only becomes the baseline.
fn rate_delta(prev: Option<u64>, total: u64, limit: Option<f64>) -> u64 {
    let Some(prev) = prev else { return 0 };
    let delta = total.saturating_sub(prev);
    match limit {
        Some(l) if delta as f64 > l => 0,
        _ => delta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_glitch_is_not_traffic() {
        assert_eq!(rate_delta(None, 1000, None), 0);
        assert_eq!(rate_delta(Some(1000), 1500, Some(1e6)), 500);
        // Counter read low once, then back: neither sample is a rate.
        assert_eq!(rate_delta(Some(60_000_000_000), 5, Some(1e6)), 0);
        assert_eq!(rate_delta(Some(5), 60_000_000_100, Some(1e6)), 0);
        // Unknown speed: only the backwards step is caught.
        assert_eq!(rate_delta(Some(10), 5, None), 0);
    }
}
