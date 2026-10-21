//! The grandmaster's time source (`--gm-clock`).
//!
//! By default the served time is the system clock: every hardware stamp is
//! converted with a fresh NIC/system cross-timestamp. Where those can only
//! be read with µs uncertainty (no PTP_SYS_OFFSET_PRECISE), that noise and
//! every NTP adjustment of the system clock reach the network directly.
//!
//! The phc modes make the NIC's hardware clock (PHC) the time: it is set to
//! TAI once from the system clock and its stamps are sent as they are. With
//! `phc-disciplined` a slow PI servo (median of cross-timestamps, once per
//! second) keeps it on the system clock, like phc2sys does — the
//! measurement noise is averaged out instead of passed on.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::clock::TaiClock;
use crate::clocksync::{PhcTarget, Servo, ServoInput, SystemClock};
use crate::config::{Config, GmClock};
use crate::monitor::fmt_ns;
use crate::net::RawSock;
use crate::phc::PhcClock;

/// Interval of the PHC servo.
const DISCIPLINE_INTERVAL: Duration = Duration::from_secs(1);
/// Cross-timestamps per servo update (median).
const PAIRS: usize = 5;
/// How long the PHC's frequency is measured against the system clock
/// before serving, so the servo starts close and pulls in gently.
const FREQ_MEASURE: Duration = Duration::from_secs(2);

/// Keeps the PHC on the system clock (phc-disciplined mode).
pub struct Discipline {
    servo: Servo<PhcTarget>,
    next: Instant,
    clock: TaiClock,
    device: String,
}

/// Set up the configured time source. Returns the clock to serve from,
/// the servo for phc-disciplined, a description for the UI, and log lines.
pub fn setup(
    cfg: &Config,
    sock: &RawSock,
    clock: TaiClock,
) -> (TaiClock, Option<Discipline>, String, Vec<String>) {
    let mut logs = Vec::new();
    let mode = match cfg.gm_clock {
        GmClock::Auto if sock.phc().is_some() => GmClock::PhcDisciplined,
        GmClock::Auto => GmClock::System,
        m => m,
    };
    if mode == GmClock::System {
        return (clock, None, clock.describe().to_string(), logs);
    }
    let Some(phc) = sock.phc() else {
        logs.push(format!(
            "WARNING: --gm-clock {} needs hardware timestamps — serving the system clock",
            if mode == GmClock::Phc {
                "phc"
            } else {
                "phc-disciplined"
            }
        ));
        return (clock, None, clock.describe().to_string(), logs);
    };
    match init_phc(phc.index, clock, mode == GmClock::PhcDisciplined) {
        Ok((target, freq_log)) => {
            logs.extend(freq_log);
            let device = target.phc.device.clone();
            let served = clock.with_phc(phc.clock_id());
            if mode == GmClock::Phc {
                logs.push(format!(
                    "Time source: {device}, set from the system clock once, then free-running"
                ));
                return (
                    served,
                    None,
                    format!("NIC clock {device} (free-running)"),
                    logs,
                );
            }
            let max = target.phc.max_adj_ppb().unwrap_or(500_000.0);
            logs.push(format!(
                "Time source: {device}, steered to the system clock"
            ));
            let servo = Servo::new(device.clone(), target, false, 1.0, max);
            (
                served,
                Some(Discipline {
                    servo,
                    next: Instant::now(),
                    clock,
                    device: device.clone(),
                }),
                format!("NIC clock {device} ← system clock"),
                logs,
            )
        }
        Err(e) => {
            logs.push(format!(
                "WARNING: cannot use the NIC clock as time source ({e:#}) — serving the system clock"
            ));
            (clock, None, clock.describe().to_string(), logs)
        }
    }
}

/// Open the PHC, match its frequency to the system clock (if it will be
/// disciplined) and set it to TAI.
fn init_phc(index: i32, clock: TaiClock, match_freq: bool) -> Result<(PhcTarget, Vec<String>)> {
    let mut logs = Vec::new();
    let phc = PhcClock::open(index)?;
    let mut target = PhcTarget::new(phc).context("reading the NIC clock's frequency")?;
    let offset = |t: &PhcTarget| -> Result<f64> {
        median_offset(t, clock).context("no NIC/system clock cross-timestamps")
    };
    if match_freq {
        let a = offset(&target)?;
        std::thread::sleep(FREQ_MEASURE);
        let b = offset(&target)?;
        // The offset (system − PHC) grows when the PHC is slow.
        let rate = (b - a) / FREQ_MEASURE.as_secs_f64();
        let ppb = target.freq_ppb()? + rate;
        target
            .set_freq_ppb(ppb)
            .context("adjusting the NIC clock")?;
        logs.push(format!(
            "NIC clock frequency matched to the system clock: {ppb:+.0} ppb"
        ));
    }
    let off = offset(&target)?;
    if off.abs() > 1_000.0 {
        target
            .step_ns(off.round() as i128)
            .context("setting the NIC clock")?;
        logs.push(format!(
            "NIC clock set to the system clock (TAI), stepped {}",
            fmt_ns(off)
        ));
    }
    Ok((target, logs))
}

/// System time (as TAI) minus PHC, median of a few cross-timestamps, ns.
fn median_offset(t: &PhcTarget, clock: TaiClock) -> Option<f64> {
    let mut v: Vec<f64> = (0..PAIRS)
        .filter_map(|_| t.phc.sample_offset_pair())
        .map(|(p, rt)| (clock.realtime_ns_to_tai(rt).to_ns() - p) as f64)
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

impl Discipline {
    /// Run the servo when due; returns log lines and the UI description.
    pub fn tick(&mut self, now: Instant) -> Option<(Vec<String>, String)> {
        if now < self.next {
            return None;
        }
        self.next = now + DISCIPLINE_INTERVAL;
        let Some(off) = median_offset(self.servo.clock(), self.clock) else {
            return Some((
                Vec::new(),
                format!(
                    "NIC clock {} ← system clock (no cross-timestamp)",
                    self.device
                ),
            ));
        };
        let rt_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let logs = self.servo.sample(&ServoInput {
            offset_ns: off,
            master_utc_secs: rt_secs,
            utc_offset: self.clock.utc_offset() as i32,
            clean: true,
            hold: false,
            master_stepped: false,
        });
        let desc = format!(
            "{} ← sys clock: {} {} {:+.0}ppb",
            self.device,
            self.servo.state.label(),
            fmt_ns(off),
            self.servo.freq_ppb
        );
        Some((logs, desc))
    }

    pub fn next(&self) -> Instant {
        self.next
    }
}
