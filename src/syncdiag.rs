//! Monitor diagnostics: context for *why* the master's time got noisy.
//!
//! The analyzer in `monitor.rs` says *that* the residual jittered or jumped.
//! This module watches everything else a Sync tells us — bridge residence
//! time (correctionField), Sync arrival and send-interval regularity, peer
//! link delay, Follow_Up latency, the GM's rate ratio, link load and error
//! counters — and
//!
//! - logs unusual values of those factors on their own (rate-limited),
//! - detects **jitter bursts** (1-s jitter level of the residual — robust to
//!   single spikes — far above its
//!   long-term baseline) and logs which factors coincided with the burst,
//! - optionally (`--capture-dir`) writes a per-Sync CSV covering ~30 s
//!   before to ~10 s after any alarm, for offline analysis.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::monitor::{fmt_ns, Thresholds};
use crate::ptp::AnnounceData;

/// Residuals in the short burst window (1 s at 8 Sync/s).
const SHORT: usize = 8;
/// Samples before baselines are trusted.
const WARMUP: u64 = 64;
/// A burst starts when the short RMS exceeds this multiple of the baseline...
const BURST_FACTOR: f64 = 3.0;
/// ...and ends after SHORT samples below this multiple.
const CALM_FACTOR: f64 = 1.5;
const BASELINE_ALPHA: f64 = 0.01;
const STAT_ALPHA: f64 = 0.02;
const PRE_SECS: f64 = 30.0;
const POST_SECS: f64 = 10.0;
/// A capture extended by further alarms ends at the latest this long after
/// its first trigger.
const MAX_CAPTURE_SECS: f64 = 60.0;
const RING_SECS: f64 = PRE_SECS + MAX_CAPTURE_SECS + 5.0;
const LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Follow_Up later than this after its Sync is reported.
const FUP_LATE_US: f64 = 10_000.0;

/// One complete Sync (+ Follow_Up) measurement with its context.
#[derive(Clone, Default)]
pub struct SampleRec {
    /// Unix time, s.
    pub wall: f64,
    pub seq: u16,
    /// Our local receive time (free-running timebase), ns.
    pub local_ns: i128,
    /// GM origin timestamp, ns.
    pub origin_ns: i128,
    /// Deviation from the drift model, whatever the classification, ns.
    pub error_ns: Option<f64>,
    pub correction_ns: i64,
    pub link_delay_ns: Option<f64>,
    /// Local delay between Sync and its Follow_Up, µs (two-step only).
    pub fup_delay_us: Option<f64>,
    /// GM frequency vs. the upstream bridge, from the Follow_Up TLV, ppb.
    pub rate_offset_ppb: Option<f64>,
    pub rx_bps: f64,
    /// Link error counter increase during the current second.
    pub link_errors: u64,
    /// Anomalies at this sample (';'-separated).
    pub note: String,
    // Filled in by Diag:
    /// Arrival interval minus the expected one, ns (none after reordering
    /// or a long outage).
    pub arrival_dev_ns: Option<f64>,
    /// GM send interval (origin timestamps) minus nominal, ns.
    pub send_dev_ns: Option<f64>,
    /// Syncs missing before this one.
    pub seq_gap: u16,
}

/// EWMA of a value's mean and mean absolute deviation.
#[derive(Default, Clone, Copy)]
struct Stat {
    mean: f64,
    dev: f64,
    n: u64,
}

impl Stat {
    fn update(&mut self, v: f64) {
        if self.n == 0 {
            self.mean = v;
        } else {
            self.dev += STAT_ALPHA * ((v - self.mean).abs() - self.dev);
            self.mean += STAT_ALPHA * (v - self.mean);
        }
        self.n += 1;
    }

    fn warm(&self) -> bool {
        self.n >= WARMUP
    }

    /// Distance from the mean beyond k typical deviations (and `floor`).
    fn unusual(&self, v: f64, k: f64, floor: f64) -> bool {
        self.warm() && (v - self.mean).abs() > (k * self.dev).max(floor)
    }
}

struct Burst {
    started: f64,
    base_rms: f64,
    peak_rms: f64,
    calm: usize,
    /// Samples since the start; the diagnosis is logged after SHORT of
    /// them, so its window covers the onset and the first second.
    samples: usize,
}

struct Capture {
    first_trigger: f64,
    due: f64,
    reasons: Vec<String>,
}

pub struct Diag {
    jump_ns: f64,
    freq_ppb: f64,
    ring: VecDeque<SampleRec>,
    prev: Option<(i128, i128, u16)>,
    short: VecDeque<f64>,
    /// EWMA of the squared residual outside bursts.
    baseline_ms: Option<f64>,
    baseline_n: u64,
    corr: Stat,
    arrival: Stat,
    send: Stat,
    rx: Stat,
    fup: Stat,
    last_link_delay: Option<f64>,
    last_rate: Option<f64>,
    burst: Option<Burst>,
    /// (key, last logged, suppressed since)
    limits: Vec<(&'static str, Instant, u64)>,
    capture_dir: Option<PathBuf>,
    capture: Option<Capture>,
    pub bursts: u64,
    pub captures: u64,
}

impl Diag {
    pub fn new(th: Thresholds, capture_dir: Option<PathBuf>) -> Self {
        Diag {
            jump_ns: th.jump_ns,
            freq_ppb: th.freq_ppb,
            ring: VecDeque::new(),
            prev: None,
            short: VecDeque::with_capacity(SHORT + 1),
            baseline_ms: None,
            baseline_n: 0,
            corr: Stat::default(),
            arrival: Stat::default(),
            send: Stat::default(),
            rx: Stat::default(),
            fup: Stat::default(),
            last_link_delay: None,
            last_rate: None,
            burst: None,
            limits: Vec::new(),
            capture_dir,
            capture: None,
            bursts: 0,
            captures: 0,
        }
    }

    pub fn in_burst(&self) -> bool {
        self.burst.is_some()
    }

    /// A burst that has lasted long enough to be diagnosed and logged.
    pub fn burst_established(&self) -> bool {
        self.burst.as_ref().is_some_and(|b| b.samples >= SHORT)
    }

    /// Following another master: its sequence numbers and timing are
    /// unrelated to the previous one's.
    pub fn on_master_change(&mut self) {
        self.prev = None;
        self.short.clear();
    }

    /// Peak jitter level of the current burst, ns.
    pub fn burst_level(&self) -> Option<f64> {
        self.burst.as_ref().map(|b| b.peak_rms)
    }

    /// Seconds since the current burst started.
    pub fn burst_age(&self) -> Option<f64> {
        self.burst.as_ref().map(|b| wall_now() - b.started)
    }

    /// Rate limiter for log lines: Some(suppressed count) if `key` may log now.
    pub fn may_log(&mut self, key: &'static str) -> Option<u64> {
        let now = Instant::now();
        match self.limits.iter_mut().find(|l| l.0 == key) {
            Some(l) if now.duration_since(l.1) < LOG_INTERVAL => {
                l.2 += 1;
                None
            }
            Some(l) => {
                let suppressed = l.2;
                l.1 = now;
                l.2 = 0;
                Some(suppressed)
            }
            None => {
                self.limits.push((key, now, 0));
                Some(0)
            }
        }
    }

    /// Start (or extend) a capture around an alarm. No-op without --capture-dir.
    pub fn trigger(&mut self, reason: &str) {
        if self.capture_dir.is_none() {
            return;
        }
        let now = wall_now();
        match &mut self.capture {
            Some(c) => {
                if !c.reasons.iter().any(|r| r == reason) {
                    c.reasons.push(reason.to_string());
                }
                c.due = (now + POST_SECS).min(c.first_trigger + MAX_CAPTURE_SECS);
            }
            None => {
                self.capture = Some(Capture {
                    first_trigger: now,
                    due: now + POST_SECS,
                    reasons: vec![reason.to_string()],
                })
            }
        }
    }

    /// Write a due capture. Call regularly — Syncs may have stopped.
    pub fn poll(&mut self) -> Option<String> {
        if self.capture.as_ref().is_none_or(|c| c.due > wall_now()) {
            return None;
        }
        self.write_capture()
    }

    /// Write a pending capture now (on shutdown).
    pub fn flush(&mut self) -> Option<String> {
        self.write_capture()
    }

    fn write_capture(&mut self) -> Option<String> {
        let c = self.capture.take()?;
        let dir = self.capture_dir.as_ref()?;
        let path = dir.join(format!("sync-{}.csv", fmt_utc_compact(c.first_trigger)));
        let rows: Vec<&SampleRec> = self
            .ring
            .iter()
            .filter(|r| r.wall >= c.first_trigger - PRE_SECS)
            .collect();
        let res = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(dir)?;
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path)?);
            writeln!(
                f,
                "# gptp-gm monitor capture — reasons: {}",
                c.reasons.join("; ")
            )?;
            writeln!(
                f,
                "# first trigger at {:.3} (unix s); error = deviation from the drift model",
                c.first_trigger
            )?;
            writeln!(f, "{}", csv_header())?;
            for r in &rows {
                writeln!(f, "{}", csv_row(r))?;
            }
            f.flush()
        })();
        Some(match res {
            Ok(()) => {
                self.captures += 1;
                format!(
                    "Captured {} Syncs around [{}] to {}",
                    rows.len(),
                    c.reasons.join("; "),
                    path.display()
                )
            }
            Err(e) => format!("WARNING: writing capture {} failed: {e}", path.display()),
        })
    }

    /// Process one sample; returns log lines.
    pub fn on_sample(&mut self, mut rec: SampleRec, nominal_ns: f64) -> Vec<String> {
        let mut logs = Vec::new();

        if let Some((pl, po, ps)) = self.prev {
            let gap = rec.seq.wrapping_sub(ps);
            // Across a few missing Syncs the expected interval is a
            // multiple of the nominal one; reordering (gap "negative") and
            // long outages give no interval.
            if (1..=8).contains(&gap) {
                let expected = nominal_ns * gap as f64;
                rec.arrival_dev_ns = Some((rec.local_ns - pl) as f64 - expected);
                rec.send_dev_ns = Some((rec.origin_ns - po) as f64 - expected);
            }
            if gap > 1 && gap < 1000 {
                rec.seq_gap = gap - 1;
            }
        }
        self.prev = Some((rec.local_ns, rec.origin_ns, rec.seq));

        // --- Unusual single factors -------------------------------------
        if let Some(a) = rec.arrival_dev_ns {
            if a.abs() > nominal_ns * 0.5 && rec.seq_gap == 0 {
                if let Some(n) = self.may_log("arrival") {
                    logs.push(format!(
                        "WARNING: irregular Sync arrival — interval {:.1} ms, nominal {:.1} ms{}",
                        (nominal_ns + a) / 1e6,
                        nominal_ns / 1e6,
                        more(n)
                    ));
                }
            }
        }
        let c = rec.correction_ns as f64;
        if self.corr.unusual(c, 6.0, self.jump_ns / 5.0) {
            let typical = format!(
                "{} ± {}",
                fmt_ns(self.corr.mean),
                fmt_ns(self.corr.dev).trim_start_matches('+')
            );
            if let Some(n) = self.may_log("correction") {
                logs.push(format!(
                    "WARNING: bridge residence time (correctionField) {} — typically {typical}{}",
                    fmt_ns(c),
                    more(n)
                ));
            }
            self.trigger("residence time jump");
        }
        if let (Some(prev), Some(ld)) = (self.last_link_delay, rec.link_delay_ns) {
            if (ld - prev).abs() > self.jump_ns / 10.0 {
                if let Some(n) = self.may_log("linkdelay") {
                    logs.push(format!(
                        "WARNING: peer link delay changed {} → {}{}",
                        fmt_ns(prev),
                        fmt_ns(ld),
                        more(n)
                    ));
                }
                self.trigger("link delay change");
            }
        }
        if rec.link_delay_ns.is_some() {
            self.last_link_delay = rec.link_delay_ns;
        }
        if let Some(d) = rec.fup_delay_us {
            if d > FUP_LATE_US {
                if let Some(n) = self.may_log("fup") {
                    logs.push(format!(
                        "WARNING: Follow_Up arrived {:.1} ms after its Sync (GM or bridge busy){}",
                        d / 1e3,
                        more(n)
                    ));
                }
            }
        }
        if let (Some(prev), Some(r)) = (self.last_rate, rec.rate_offset_ppb) {
            if (r - prev).abs() > self.freq_ppb {
                if let Some(n) = self.may_log("rate") {
                    logs.push(format!(
                        "WARNING: GM rate ratio (Follow_Up TLV) changed {prev:+.0} → {r:+.0} ppb{}",
                        more(n)
                    ));
                }
                self.trigger("GM rate ratio change");
            }
        }
        if rec.rate_offset_ppb.is_some() {
            self.last_rate = rec.rate_offset_ppb;
        }

        // --- Jitter bursts -------------------------------------------------
        // A time step is reported as such; its samples are no jitter. Drop
        // them from the short window and cancel a burst they started.
        if rec.note.split(';').any(|n| n == "step") {
            self.short.clear();
            if self.burst.as_ref().is_some_and(|b| b.samples < SHORT) {
                self.burst = None;
                self.bursts -= 1;
                if let Some(c) = &mut self.capture {
                    c.reasons.retain(|r| r != "jitter burst");
                }
            }
        }
        if let Some(e) = rec.error_ns {
            self.short.push_back(e);
            if self.short.len() > SHORT {
                self.short.pop_front();
            }
        }
        let mut burst_started = false;
        let mut report = None;
        if rec.error_ns.is_some() && self.short.len() == SHORT {
            let short_rms = jitter_level(&self.short);
            let base_rms = self.baseline_ms.map(f64::sqrt);
            match (&mut self.burst, base_rms) {
                (None, Some(base))
                    if self.baseline_n >= WARMUP
                        && short_rms > BURST_FACTOR * base
                        && short_rms > self.jump_ns / 20.0 =>
                {
                    self.burst = Some(Burst {
                        started: rec.wall,
                        base_rms: base,
                        peak_rms: short_rms,
                        calm: 0,
                        samples: 0,
                    });
                    self.bursts += 1;
                    burst_started = true;
                }
                (None, _) => {
                    let e2 = short_rms.powi(2);
                    self.baseline_ms = Some(
                        self.baseline_ms
                            .map_or(e2, |b| b + BASELINE_ALPHA * (e2 - b)),
                    );
                    self.baseline_n += 1;
                }
                (Some(b), base) => {
                    b.peak_rms = b.peak_rms.max(short_rms);
                    b.samples += 1;
                    if b.samples == SHORT {
                        report = Some((b.peak_rms, b.base_rms));
                    }
                    // The floor ends bursts even if the baseline itself was
                    // unusually quiet before.
                    let calm_below = (CALM_FACTOR * base.unwrap_or(0.0)).max(self.jump_ns / 20.0);
                    if short_rms < calm_below {
                        b.calm += 1;
                    } else {
                        b.calm = 0;
                    }
                    if b.calm >= SHORT {
                        logs.push(format!(
                            "Jitter burst over after {:.1} s — peak 1-s jitter level {}",
                            rec.wall - b.started,
                            fmt_ns(b.peak_rms).trim_start_matches('+')
                        ));
                        self.burst = None;
                    }
                }
            }
        }

        if burst_started {
            if !rec.note.is_empty() {
                rec.note.push(';');
            }
            rec.note.push_str("burst-start");
        }
        self.ring.push_back(rec);
        while self
            .ring
            .front()
            .is_some_and(|r| r.wall < self.ring.back().unwrap().wall - RING_SECS)
        {
            self.ring.pop_front();
        }
        if burst_started {
            self.trigger("jitter burst");
        }
        if let Some((rms, base)) = report {
            // Logged before a possible "burst over" from the same sample.
            logs.insert(0, self.diagnose(rms, base));
        }

        // Baselines learn only from calm periods.
        if self.burst.is_none() {
            let r = self.ring.back().unwrap();
            self.corr.update(r.correction_ns as f64);
            self.rx.update(r.rx_bps);
            if let Some(a) = r.arrival_dev_ns {
                self.arrival.update(a);
            }
            if let Some(s) = r.send_dev_ns {
                self.send.update(s);
            }
            if let Some(f) = r.fup_delay_us {
                self.fup.update(f);
            }
        }

        logs.extend(self.poll());
        logs
    }

    /// Explain a burst: which context factors were unusual in the window
    /// from one second before its onset until now (one second into it).
    fn diagnose(&self, short_rms: f64, base_rms: f64) -> String {
        let win: Vec<&SampleRec> = self.ring.iter().rev().take(2 * SHORT).collect();
        // A time-like factor must be unusual for its own history *and* large
        // enough to plausibly explain the burst.
        let floor = (self.jump_ns / 20.0).max(0.3 * short_rms);
        let abs = |v: f64| fmt_ns(v).trim_start_matches('+').to_string();
        let max_dev =
            |vals: Vec<f64>, mean: f64| vals.iter().map(|v| (v - mean).abs()).fold(0.0, f64::max);
        let mut found = Vec::new();

        let corrs: Vec<f64> = win.iter().map(|r| r.correction_ns as f64).collect();
        let spread = corrs.iter().cloned().fold(f64::MIN, f64::max)
            - corrs.iter().cloned().fold(f64::MAX, f64::min);
        if self.corr.warm() && spread > (4.0 * self.corr.dev).max(floor) {
            found.push(format!(
                "residence time (correctionField) varied by {} (typically ±{}) — bridge queueing",
                abs(spread),
                abs(self.corr.dev)
            ));
        }
        let arr: Vec<f64> = win.iter().filter_map(|r| r.arrival_dev_ns).collect();
        let d = max_dev(arr, self.arrival.mean);
        if self.arrival.warm() && d > (4.0 * self.arrival.dev).max(floor) {
            found.push(format!(
                "Sync arrival jitter up to {} (typically ±{}) — delay in the path",
                abs(d),
                abs(self.arrival.dev)
            ));
        }
        let snd: Vec<f64> = win.iter().filter_map(|r| r.send_dev_ns).collect();
        let d = max_dev(snd, self.send.mean);
        if self.send.warm() && d > (4.0 * self.send.dev).max(floor) {
            found.push(format!(
                "GM send-interval jitter up to {} (typically ±{}) — if the residual follows it, \
                 the GM's origin timestamps are not taken on the wire",
                abs(d),
                abs(self.send.dev)
            ));
        }
        let rx_max = win.iter().map(|r| r.rx_bps).fold(0.0, f64::max);
        if self.rx.warm()
            && rx_max > (2.0 * self.rx.mean).max(self.rx.mean + 4.0 * self.rx.dev)
            && rx_max - self.rx.mean > 1e6
        {
            found.push(format!(
                "link load {:.1} Mb/s (typically {:.1}) — congestion",
                rx_max / 1e6,
                self.rx.mean / 1e6
            ));
        }
        let errs = win.iter().map(|r| r.link_errors).max().unwrap_or(0);
        if errs > 0 {
            found.push(format!("link error counters rising (+{errs}/s)"));
        }
        let gaps: u64 = win.iter().map(|r| r.seq_gap as u64).sum();
        if gaps > 0 {
            found.push(format!("{gaps} Syncs missing"));
        }
        let lds: Vec<f64> = win.iter().filter_map(|r| r.link_delay_ns).collect();
        if let (Some(lo), Some(hi)) = (
            lds.iter().cloned().reduce(f64::min),
            lds.iter().cloned().reduce(f64::max),
        ) {
            if hi - lo > (self.jump_ns / 10.0).max(floor) {
                found.push(format!("peer link delay moved by {}", abs(hi - lo)));
            }
        }
        let fmax = win
            .iter()
            .filter_map(|r| r.fup_delay_us)
            .fold(0.0, f64::max);
        if self.fup.warm() && fmax > self.fup.mean + (4.0 * self.fup.dev).max(1_000.0) {
            found.push(format!(
                "Follow_Up delay up to {:.1} ms (typically {:.1})",
                fmax / 1e3,
                self.fup.mean / 1e3
            ));
        }

        let cause = if found.is_empty() {
            let mut s = "no correlated network factor — suspect the GM's time base/servo \
                         or local timestamping"
                .to_string();
            if self.corr.warm() && self.corr.dev == 0.0 && spread == 0.0 {
                s.push_str(
                    "; correctionField never changes: a bridge in the path would not be \
                     compensating its residence time",
                );
            }
            s
        } else {
            format!("coincides with: {}", found.join("; "))
        };
        format!(
            "WARNING: jitter burst — 1-s jitter level up to {} vs baseline {}; {cause}",
            abs(short_rms),
            abs(base_rms)
        )
    }
}

/// Robust 1-s jitter level: square root of the median squared deviation.
/// Up to 3 of the 8 samples may be outliers (spikes, reported on their own)
/// without moving it, so a burst means sustained jitter, and a noisy host's
/// occasional stray sample neither starts a burst nor keeps one alive.
fn jitter_level(window: &VecDeque<f64>) -> f64 {
    let mut sq: Vec<f64> = window.iter().map(|e| e * e).collect();
    sq.sort_by(f64::total_cmp);
    sq[sq.len() / 2].sqrt()
}

/// Changed Announce fields of the followed GM (identity changes are GM
/// changes and reported elsewhere).
pub fn announce_diff(old: &AnnounceData, new: &AnnounceData) -> Vec<String> {
    let mut d = Vec::new();
    if old.clock_class != new.clock_class {
        d.push(format!(
            "clockClass {} → {} ({})",
            old.clock_class,
            new.clock_class,
            clock_class_desc(new.clock_class)
        ));
    }
    let mut num = |name: &str, a: u64, b: u64, hex: bool| {
        if a != b {
            d.push(if hex {
                format!("{name} 0x{a:X} → 0x{b:X}")
            } else {
                format!("{name} {a} → {b}")
            });
        }
    };
    num(
        "clockAccuracy",
        old.clock_accuracy as u64,
        new.clock_accuracy as u64,
        true,
    );
    num("variance", old.variance as u64, new.variance as u64, true);
    num(
        "priority1",
        old.priority1 as u64,
        new.priority1 as u64,
        false,
    );
    num(
        "priority2",
        old.priority2 as u64,
        new.priority2 as u64,
        false,
    );
    num(
        "stepsRemoved",
        old.steps_removed as u64,
        new.steps_removed as u64,
        false,
    );
    num(
        "timeSource",
        old.time_source as u64,
        new.time_source as u64,
        true,
    );
    d
}

fn clock_class_desc(c: u8) -> &'static str {
    match c {
        6 => "locked to primary reference",
        7 => "holdover, within spec",
        13 | 14 => "application-specific time",
        52 | 58 => "degraded",
        187 | 193 => "holdover, out of spec",
        248 => "default / free-running",
        255 => "slave-only",
        _ => "other",
    }
}

fn more(n: u64) -> String {
    if n > 0 {
        format!(" (+{n} more since last report)")
    } else {
        String::new()
    }
}

fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn csv_header() -> &'static str {
    "unix_s,seq,error_ns,correction_ns,link_delay_ns,arrival_dev_ns,send_dev_ns,\
     seq_gap,fup_delay_us,rate_offset_ppb,rx_bps,link_errors,note"
}

fn csv_row(r: &SampleRec) -> String {
    let o = |v: Option<f64>| v.map_or(String::new(), |x| format!("{x:.0}"));
    format!(
        "{:.6},{},{},{},{},{},{},{},{},{},{:.0},{},{}",
        r.wall,
        r.seq,
        o(r.error_ns),
        r.correction_ns,
        o(r.link_delay_ns),
        o(r.arrival_dev_ns),
        o(r.send_dev_ns),
        r.seq_gap,
        r.fup_delay_us.map_or(String::new(), |x| format!("{x:.1}")),
        r.rate_offset_ppb
            .map_or(String::new(), |x| format!("{x:.1}")),
        r.rx_bps,
        r.link_errors,
        r.note
    )
}

/// UTC "YYYYMMDDTHHMMSSZ" for file names.
fn fmt_utc_compact(unix: f64) -> String {
    let secs = unix as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TH: Thresholds = Thresholds {
        jump_ns: 1_000.0,
        freq_ppb: 100.0,
    };
    const NOMINAL: i128 = 125_000_000;

    /// Feed samples; `f(i)` returns (error, correction, arrival jitter).
    fn feed(
        d: &mut Diag,
        from: i128,
        n: i128,
        f: impl Fn(i128) -> (f64, i64, i128),
    ) -> Vec<String> {
        let mut logs = Vec::new();
        // Recent wall times: captures select rows relative to the real clock.
        let base = wall_now() - 20.0;
        for i in from..from + n {
            let (err, corr, jitter) = f(i);
            let rec = SampleRec {
                wall: base + i as f64 * 0.125,
                seq: i as u16,
                local_ns: i * NOMINAL + jitter,
                origin_ns: 5_000_000_000 + i * NOMINAL,
                error_ns: Some(err),
                correction_ns: corr,
                link_delay_ns: Some(500.0),
                rx_bps: 2e6,
                ..Default::default()
            };
            logs.extend(d.on_sample(rec, NOMINAL as f64));
        }
        logs
    }

    /// Small deterministic noise, ±`amp`.
    fn noise(i: i128, amp: f64) -> f64 {
        ((i * 37 % 21) as f64 - 10.0) / 10.0 * amp
    }

    #[test]
    fn calm_series_logs_nothing() {
        let mut d = Diag::new(TH, None);
        let logs = feed(&mut d, 0, 400, |i| {
            (noise(i, 20.0), 3_000 + (i % 5) as i64, 0)
        });
        assert!(logs.is_empty(), "{logs:?}");
        assert_eq!(d.bursts, 0);
    }

    #[test]
    fn burst_with_residence_variation_is_diagnosed() {
        let mut d = Diag::new(TH, None);
        feed(&mut d, 0, 200, |i| {
            (noise(i, 20.0), 3_000 + (i % 5) as i64, 0)
        });
        // Bridge queueing: residence time swings by µs, compensation is
        // imperfect, so the residual jitters by a few hundred ns.
        let logs = feed(&mut d, 200, 16, |i| {
            (noise(i, 400.0), 3_000 + (i % 4) as i64 * 2_000, 0)
        });
        let burst = logs
            .iter()
            .find(|l| l.contains("jitter burst —"))
            .expect("burst");
        assert!(burst.contains("residence time"), "{burst}");
        assert_eq!(d.bursts, 1);
        let logs = feed(&mut d, 216, 40, |i| {
            (noise(i, 20.0), 3_000 + (i % 5) as i64, 0)
        });
        assert!(
            logs.iter().any(|l| l.starts_with("Jitter burst over")),
            "{logs:?}"
        );
        assert!(!d.in_burst());
    }

    #[test]
    fn burst_without_cause_says_so() {
        let mut d = Diag::new(TH, None);
        feed(&mut d, 0, 200, |i| (noise(i, 20.0), 3_000, 0));
        let logs = feed(&mut d, 200, 20, |i| (noise(i, 500.0) + 600.0, 3_000, 0));
        let burst = logs
            .iter()
            .find(|l| l.contains("jitter burst —"))
            .expect("burst");
        assert!(burst.contains("no correlated network factor"), "{burst}");
        assert!(burst.contains("correctionField never changes"), "{burst}");
    }

    #[test]
    fn arrival_jitter_is_diagnosed_and_irregular_arrival_logged() {
        let mut d = Diag::new(TH, None);
        feed(&mut d, 0, 200, |i| (noise(i, 20.0), 0, (i % 3) * 100));
        let logs = feed(&mut d, 200, 20, |i| {
            (noise(i, 500.0), 0, if i % 2 == 0 { 80_000_000 } else { 0 })
        });
        assert!(
            logs.iter().any(|l| l.contains("irregular Sync arrival")),
            "{logs:?}"
        );
        let burst = logs
            .iter()
            .find(|l| l.contains("jitter burst —"))
            .expect("burst");
        assert!(burst.contains("Sync arrival jitter"), "{burst}");
    }

    #[test]
    fn time_step_cancels_the_burst_it_started() {
        let mut d = Diag::new(TH, None);
        feed(&mut d, 0, 200, |i| (noise(i, 20.0), 0, 0));
        // Two held-back step samples are outliers in the 1-s window, not a
        // burst...
        feed(&mut d, 200, 2, |_| (1e9, 0, 0));
        assert!(!d.in_burst());
        // ...and the third, classified as a step, does not start one either.
        let mut rec = SampleRec {
            wall: wall_now(),
            seq: 202,
            local_ns: 202 * NOMINAL,
            origin_ns: 5_000_000_000 + 202 * NOMINAL,
            error_ns: Some(3.0),
            note: "step".into(),
            ..Default::default()
        };
        rec.link_delay_ns = Some(500.0);
        let logs = d.on_sample(rec, NOMINAL as f64);
        assert!(!d.in_burst(), "{logs:?}");
        assert_eq!(d.bursts, 0);
    }

    #[test]
    fn stray_spikes_neither_start_nor_prolong_a_burst() {
        let mut d = Diag::new(TH, None);
        // Calm series with a stray 5 µs spike every 6th sample.
        let spiky = |i: i128| {
            let e = if i % 6 == 0 { 5_000.0 } else { noise(i, 20.0) };
            (e, 0, 0)
        };
        let logs = feed(&mut d, 0, 300, spiky);
        assert_eq!(d.bursts, 0, "{logs:?}");
        // A real burst on top of the same noisy host still ends.
        feed(&mut d, 300, 16, |i| (noise(i, 2_000.0) + 1_500.0, 0, 0));
        assert!(d.in_burst());
        let logs = feed(&mut d, 316, 40, spiky);
        assert!(
            logs.iter().any(|l| l.starts_with("Jitter burst over")),
            "{logs:?}"
        );
    }

    #[test]
    fn capture_writes_csv_around_trigger() {
        let dir = std::env::temp_dir().join(format!("gptp-diag-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut d = Diag::new(TH, Some(dir.clone()));
        feed(&mut d, 0, 50, |i| (noise(i, 20.0), 0, 0));
        d.trigger("test alarm");
        // Force the capture due now (POST_SECS would otherwise wait 10 s).
        d.capture.as_mut().unwrap().due = 0.0;
        let msg = d.poll().expect("capture written");
        assert!(msg.contains("Captured"), "{msg}");
        let file = std::fs::read_dir(&dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("test alarm"));
        assert!(text.contains(csv_header()));
        // 2 comment lines + header + all 50 samples (they span 6 s < PRE_SECS).
        assert_eq!(text.lines().count(), 3 + 50);
        assert_eq!(d.captures, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn announce_diff_lists_changes() {
        let a = AnnounceData {
            priority1: 246,
            clock_class: 6,
            clock_accuracy: 0x21,
            variance: 0x4E5D,
            priority2: 248,
            gm_identity: [1; 8],
            steps_removed: 1,
            time_source: 0x20,
        };
        let mut b = a;
        b.clock_class = 7;
        b.steps_removed = 2;
        let d = announce_diff(&a, &b);
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d[0].contains("clockClass 6 → 7 (holdover"));
        assert!(announce_diff(&a, &a).is_empty());
    }

    #[test]
    fn utc_file_stamp() {
        assert_eq!(fmt_utc_compact(0.0), "19700101T000000Z");
        assert_eq!(fmt_utc_compact(1_792_498_263.4), "20261020T121103Z");
    }
}
