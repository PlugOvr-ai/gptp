//! Periodic metrics logging: one JSON object per line (JSONL), appended
//! every 5 s. Vehicle faults are intermittent — this is what lets you
//! correlate "sensor X desynced at 14:32:05" with the network state at that
//! second after the drive.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::inventory::fmt_mac;
use crate::state::{Role, Status};

const INTERVAL: Duration = Duration::from_secs(5);
/// When the file grows past this, it is rotated once to <file>.old.
const ROTATE_BYTES: u64 = 50_000_000;

pub fn run(path: String, state: Arc<Mutex<Status>>, quit: Arc<AtomicBool>) {
    while !quit.load(Ordering::SeqCst) {
        let line = snapshot_json(&state.lock().unwrap());
        if let Err(e) = append(&path, &line) {
            state
                .lock()
                .unwrap()
                .log(format!("Metrics log disabled: {e:#}"));
            return;
        }
        for _ in 0..50 {
            if quit.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(INTERVAL / 50);
        }
    }
}

fn append(path: &str, line: &str) -> anyhow::Result<()> {
    if std::fs::metadata(path).map_or(0, |m| m.len()) > ROTATE_BYTES {
        let _ = std::fs::rename(path, format!("{path}.old"));
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

fn snapshot_json(st: &Status) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut s = String::with_capacity(1024);
    s.push('{');
    push_kv(&mut s, "ts", &format!("{ts:.3}"));
    push_kv_str(
        &mut s,
        "role",
        match st.role {
            Role::ActiveGm => "active-gm",
            Role::Seeding => "seeding",
            Role::Passive => "passive",
            Role::Monitor => "monitor",
        },
    );
    push_kv(&mut s, "hw_timestamps", &st.ts_hardware.to_string());
    push_kv(
        &mut s,
        "link_delay_ns",
        &st.link_delay_ns
            .map_or("null".into(), |d| format!("{d:.0}")),
    );
    let c = &st.counters;
    s.push_str(&format!(
        "\"gptp\":{{\"announce_tx\":{},\"sync_tx\":{},\"pdelay_req_rx\":{},\"announce_rx\":{},\"foreign_masters\":{}}},",
        c.announce_tx,
        c.sync_tx,
        c.pdelay_req_rx,
        c.announce_rx,
        st.foreign_masters.len()
    ));
    if let Some(sm) = &st.sync {
        let num = |v: Option<f64>| v.map_or("null".into(), |x| format!("{x:.0}"));
        s.push_str(&format!(
            "\"sync\":{{\"state\":\"{}\",\"gm\":{},\"offset_to_system_ns\":{},\"residual_ns\":{},\"rms_ns\":{},\"peak_ns\":{:.0},\"drift_ppb\":{},\"correction_ns\":{},\"steps\":{},\"last_step_ns\":{},\"spikes\":{},\"freq_changes\":{},\"relearns\":{},\"bursts\":{},\"in_burst\":{},\"captures\":{},\"gm_changes\":{},\"time_base_changes\":{},\"sync_timeouts\":{},\"sync_rx\":{},\"missing_syncs\":{},\"missing_follow_ups\":{}}},",
            sm.state.label(),
            sm.gm.map_or("null".into(), |g| format!(
                "\"{}\"",
                crate::ptp::fmt_clock_identity(&g.gm_identity)
            )),
            num(sm.offset_to_system_ns),
            num(sm.residual_ns),
            num(sm.rms_ns),
            sm.peak_ns,
            num(sm.drift_ppb),
            sm.correction_ns.map_or("null".into(), |c| c.to_string()),
            sm.steps,
            num(sm.last_step_ns),
            sm.spikes,
            sm.freq_changes,
            sm.relearns,
            sm.bursts,
            sm.in_burst,
            sm.captures,
            sm.gm_changes,
            sm.time_base_changes,
            sm.sync_timeouts,
            sm.sync_rx,
            sm.missing_syncs,
            sm.missing_follow_ups,
        ));
        if !sm.clocks.is_empty() || sm.linuxptp_status.is_some() {
            s.push_str("\"time_sync\":{\"clocks\":[");
            for (i, c) in sm.clocks.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    "{{\"name\":\"{}\",\"state\":\"{}\",\"offset_ns\":{},\"freq_ppb\":{:.0},\"steps\":{}}}",
                    esc(&c.name),
                    c.state,
                    c.offset_ns.map_or("null".into(), |o| format!("{o:.0}")),
                    c.freq_ppb,
                    c.steps
                ));
            }
            s.push_str(&format!(
                "],\"linuxptp\":{}}},",
                sm.linuxptp_status
                    .as_ref()
                    .map_or("null".into(), |l| format!("\"{}\"", esc(l)))
            ));
        }
    }
    let l = &st.link;
    s.push_str(&format!(
        "\"link\":{{\"rx_bps\":{:.0},\"tx_bps\":{:.0},\"rx_pps\":{},\"tx_pps\":{},\"errors\":[",
        l.rx_bps, l.tx_bps, l.rx_pps, l.tx_pps
    ));
    for (i, p) in l.problems.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"name\":\"{}\",\"total\":{},\"delta\":{}}}",
            esc(&p.name),
            p.total,
            p.delta
        ));
    }
    s.push_str("]},\"nodes\":[");
    for (i, n) in st.nodes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"mac\":\"{}\",\"ip\":{},\"name\":{},\"rtt_us\":{},\"jitter_us\":{},\"lost\":{},\"sent\":{},\"rate_bps\":{:.0}}}",
            fmt_mac(&n.mac),
            n.ip.map_or("null".into(), |ip| format!("\"{ip}\"")),
            n.name
                .as_ref()
                .map_or("null".into(), |(name, _)| format!("\"{}\"", esc(name))),
            n.rtt_last_us.map_or("null".into(), |v| format!("{v:.0}")),
            n.jitter_us.map_or("null".into(), |v| format!("{v:.0}")),
            n.probes_lost,
            n.probes_sent,
            n.rate_bps
        ));
    }
    s.push_str("],\"streams\":[");
    for (i, st_) in st.streams.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"kind\":\"{}\",\"src\":\"{}\",\"dst\":\"{}\",\"rate_bps\":{:.0},\"packets\":{},\"lost\":{},\"duplicates\":{},\"out_of_order\":{}}}",
            st_.kind,
            fmt_mac(&st_.src_mac),
            esc(&st_.dst),
            st_.rate_bps,
            st_.packets,
            st_.lost,
            st_.duplicates,
            st_.out_of_order
        ));
    }
    s.push_str("]}");
    s
}

fn push_kv(s: &mut String, key: &str, raw_value: &str) {
    s.push_str(&format!("\"{key}\":{raw_value},"));
}

fn push_kv_str(s: &mut String, key: &str, value: &str) {
    s.push_str(&format!("\"{key}\":\"{}\",", esc(value)));
}

fn esc(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CounterStat, NodeInfo, StreamInfo};
    use std::time::Instant;

    #[test]
    fn snapshot_is_wellformed_json() {
        let mut st = Status::new("eth0".into());
        let mut sm = crate::state::SyncMonitor::default();
        sm.clocks.push(crate::clocksync::ClockView {
            name: "NIC clock \"x\"".into(),
            state: "locked",
            offset_ns: Some(-3.0),
            freq_ppb: 12.0,
            steps: 1,
            note: None,
        });
        sm.linuxptp_status = Some("gptp-ptp4l-eth0: active".into());
        st.sync = Some(sm);
        let mut node = NodeInfo::new([2, 0, 0, 0, 0, 9], Instant::now());
        node.ip = Some("10.0.0.2".parse().unwrap());
        node.set_name("na\"me\\x".into(), crate::state::NameSource::Mdns);
        node.rtt_last_us = Some(123.4);
        st.nodes.push(node);
        st.streams.push(StreamInfo {
            kind: "rtp",
            src_mac: [2, 0, 0, 0, 0, 9],
            dst: "239.0.0.5:5004".into(),
            rate_bps: 1e6,
            packets: 100,
            lost: 3,
            duplicates: 0,
            out_of_order: 1,
        });
        st.link.problems.push(CounterStat {
            name: "rx_crc_errors".into(),
            total: 5,
            delta: 1,
        });
        let line = snapshot_json(&st);
        // Minimal structural validation without a JSON dependency: balanced
        // braces/brackets outside strings and correctly escaped quotes.
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        for ch in line.chars() {
            if in_str {
                match (escaped, ch) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => in_str = false,
                    _ => {}
                }
            } else {
                match ch {
                    '"' => in_str = true,
                    '{' | '[' => depth += 1,
                    '}' | ']' => depth -= 1,
                    _ => {}
                }
            }
            assert!(depth >= 0);
        }
        assert_eq!(depth, 0);
        assert!(!in_str);
        assert!(line.contains("\"rx_crc_errors\""));
        assert!(line.contains("\"sync\":{\"state\":\"no-master\""));
        assert!(line.contains("\"time_sync\":{\"clocks\":[{\"name\":\"NIC clock \\\"x\\\"\""));
        assert!(line.contains("na\\\"me\\\\x"));
    }
}
