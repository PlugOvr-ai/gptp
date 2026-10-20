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

    while !quit.load(Ordering::SeqCst) {
        let mut health = LinkHealth::default();
        for name in RATE_COUNTERS {
            let total = read_counter(&iface, name).unwrap_or(0);
            let delta = total.saturating_sub(*last.get(name).unwrap_or(&total));
            last.insert(name, total);
            match name {
                "rx_bytes" => health.rx_bps = delta as f64 * 8.0,
                "tx_bytes" => health.tx_bps = delta as f64 * 8.0,
                "rx_packets" => health.rx_pps = delta,
                "tx_packets" => health.tx_pps = delta,
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
