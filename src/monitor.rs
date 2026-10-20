//! Slave monitor (--monitor): we act as a passive gPTP slave, follow the
//! best master on the link, and watch its time for jumps and instability.
//!
//! No clock is adjusted. Each Sync/Follow_Up pair yields one sample of
//! "master time − local time" against a free-running local timebase (raw
//! PHC, or CLOCK_MONOTONIC_RAW for software stamps). A linear fit over the
//! last few seconds models the frequency difference; anything the model
//! does not predict is what we report:
//!
//! - **step**: two consecutive samples off the prediction by the same
//!   amount — the master's time jumped (GM change, GM stepped its clock,
//!   switch servo reset, ...). The model is re-based and keeps running.
//! - **spike**: a single sample off the prediction — a delayed/corrupted
//!   timestamp, typically queueing in a bridge.
//! - **frequency change**: the fitted drift moves away from its long-term
//!   average — the master's oscillator or servo changed its rate.
//! - **unstable**: RMS residual above a quarter of the jump threshold.
//! - **erratic**: a run of samples that fit neither the model nor each
//!   other; the model is discarded and relearned.
//!
//! On top of that: Sync timeouts, sequence gaps, missing Follow_Ups, GM
//! changes (Announce) and gmTimeBaseIndicator changes (Follow_Up TLV).
//! We keep answering peer delay so the bridge treats our port as asCapable
//! and sends us Sync at all.

use std::collections::VecDeque;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::clock::{stamp_to_local_ns, stamp_to_tai, TaiClock};
use crate::config::Config;
use crate::gm::{log_to_duration, Cmd};
use crate::net::RawSock;
use crate::pdelay::PeerDelay;
use crate::ptp::{self, AnnounceData, Msg, MsgBuilder, Timestamp};
use crate::state::{ForeignMaster, Role, Status, SyncMonitor, SyncState};

/// Samples in the drift-model window (8 s at the default 8 Sync/s).
const WINDOW: usize = 64;
/// Samples needed before the model predicts anything.
const MIN_SAMPLES: usize = 8;
/// Re-check the drift for frequency changes every this many samples.
const FREQ_CHECK_EVERY: usize = 8;
const HISTORY: usize = 240;
/// Wait this long for Announces before choosing the first master, so we do
/// not briefly follow whichever master happened to announce first.
const SELECTION_SETTLE: Duration = Duration::from_millis(1500);
/// Minimum spacing of spike events in the log (they are all counted).
const SPIKE_LOG_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub struct Thresholds {
    /// A residual beyond this is a spike or step, ns.
    pub jump_ns: f64,
    /// A drift change beyond this is a frequency change, ppb.
    pub freq_ppb: f64,
}

impl Thresholds {
    /// Defaults scaled to what the timestamping can resolve.
    pub fn new(cfg: &Config, hardware: bool) -> Self {
        Thresholds {
            jump_ns: cfg
                .jump_threshold
                .unwrap_or(if hardware { 1_000.0 } else { 250_000.0 }),
            freq_ppb: cfg
                .freq_threshold
                .unwrap_or(if hardware { 100.0 } else { 5_000.0 }),
        }
    }

    fn jitter_ns(&self) -> f64 {
        self.jump_ns / 4.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anomaly {
    Step { size_ns: f64 },
    Spike { residual_ns: f64 },
    FreqChange { from_ppb: f64, to_ppb: f64 },
    Unstable { rms_ns: f64 },
    Stable { rms_ns: f64 },
    /// Too many consecutive samples off the model; it was discarded.
    Relearn { outliers: usize },
}

#[derive(Clone, Copy)]
struct Fit {
    /// Offset at x = 0, ns.
    a: f64,
    /// Slope, ns per s = ppb.
    b: f64,
    rms: f64,
}

impl Fit {
    fn predict(&self, x: f64) -> f64 {
        self.a + self.b * x
    }
}

/// Offset-series analyzer; pure, so it can be tested without a network.
pub struct Analyzer {
    th: Thresholds,
    /// (local ns, offset ns) of the first sample; all math is relative to it
    /// so f64 keeps ns precision. Steps move the offset base.
    base: Option<(i128, i128)>,
    /// (local s, offset ns) relative to `base`.
    window: VecDeque<(f64, f64)>,
    /// First out-of-model sample, held back until the next one tells
    /// whether it was a spike or a step: (x, raw relative offset, residual).
    suspect: Option<(f64, i128, f64)>,
    fit: Option<Fit>,
    ref_drift: Option<f64>,
    since_freq_check: usize,
    unstable: bool,
    /// Consecutive samples outside the model.
    outliers: usize,
    pub last_residual: Option<f64>,
}

impl Analyzer {
    pub fn new(th: Thresholds) -> Self {
        Analyzer {
            th,
            base: None,
            window: VecDeque::with_capacity(WINDOW + 1),
            suspect: None,
            fit: None,
            ref_drift: None,
            since_freq_check: 0,
            unstable: false,
            outliers: 0,
            last_residual: None,
        }
    }

    /// Forget everything and start learning from the next sample.
    pub fn reset(&mut self) {
        *self = Analyzer::new(self.th);
    }

    pub fn ready(&self) -> bool {
        self.fit.is_some()
    }

    pub fn unstable(&self) -> bool {
        self.unstable
    }

    pub fn drift_ppb(&self) -> Option<f64> {
        self.fit.map(|f| f.b)
    }

    pub fn rms_ns(&self) -> Option<f64> {
        self.fit.map(|f| f.rms)
    }

    pub fn add(&mut self, local_ns: i128, offset_ns: i128) -> Vec<Anomaly> {
        let mut out = Vec::new();
        let (bx, by) = *self.base.get_or_insert((local_ns, offset_ns));
        let x = (local_ns - bx) as f64 / 1e9;
        let y_raw = offset_ns - by;

        let Some(fit) = self.fit else {
            // Learning: only a run of mutually consistent samples may form
            // the model — restart if this one does not continue the line
            // through the previous two.
            let n = self.window.len();
            if n >= 2 {
                let (x1, y1) = self.window[n - 2];
                let (x2, y2) = self.window[n - 1];
                let pred = y2 + (y2 - y1) / (x2 - x1).max(1e-9) * (x - x2);
                if (y_raw as f64 - pred).abs() > self.th.jump_ns {
                    self.window.clear();
                }
            }
            self.push(x, y_raw as f64);
            self.last_residual = None;
            return out;
        };
        let pred = fit.predict(x);
        let r = y_raw as f64 - pred;

        if r.abs() <= self.th.jump_ns {
            if let Some((_, _, sr)) = self.suspect.take() {
                out.push(Anomaly::Spike { residual_ns: sr });
            }
            self.outliers = 0;
            self.push(x, y_raw as f64);
            self.last_residual = Some(r);
        } else {
            match self.suspect.take() {
                None => {
                    // Held back: its residual is reported once classified
                    // (as a spike), or dropped if it turns out to be a step.
                    self.suspect = Some((x, y_raw, r));
                    self.last_residual = None;
                    self.outliers += 1;
                    return out;
                }
                Some((sx, sy, sr)) if (r - sr).abs() <= self.th.jump_ns => {
                    // Both samples agree: the master's time stepped. Shift
                    // the offset base by the step (in integer ns, so even a
                    // jump of years keeps full precision afterwards).
                    let step = y_raw - pred.round() as i128;
                    self.base = Some((bx, by + step));
                    self.push(sx, (sy - step) as f64);
                    self.push(x, (y_raw - step) as f64);
                    self.last_residual = Some(r - step as f64);
                    self.outliers = 0;
                    out.push(Anomaly::Step {
                        size_ns: step as f64,
                    });
                }
                Some((_, _, sr)) => {
                    out.push(Anomaly::Spike { residual_ns: sr });
                    self.suspect = Some((x, y_raw, r));
                    self.last_residual = None;
                    self.outliers += 1;
                    if self.outliers >= MIN_SAMPLES {
                        let outliers = self.outliers;
                        self.reset();
                        out.push(Anomaly::Relearn { outliers });
                    }
                    return out;
                }
            }
        }

        self.since_freq_check += 1;
        if let Some(fit) = self.fit {
            if self.window.len() >= WINDOW && self.since_freq_check >= FREQ_CHECK_EVERY {
                self.since_freq_check = 0;
                match self.ref_drift {
                    Some(r) if (fit.b - r).abs() > self.th.freq_ppb => {
                        out.push(Anomaly::FreqChange {
                            from_ppb: r,
                            to_ppb: fit.b,
                        });
                        self.ref_drift = Some(fit.b);
                    }
                    Some(r) => self.ref_drift = Some(r * 0.9 + fit.b * 0.1),
                    None => self.ref_drift = Some(fit.b),
                }
            }
            let jitter = self.th.jitter_ns();
            if !self.unstable && fit.rms > jitter {
                self.unstable = true;
                out.push(Anomaly::Unstable { rms_ns: fit.rms });
            } else if self.unstable && fit.rms < jitter * 0.7 {
                self.unstable = false;
                out.push(Anomaly::Stable { rms_ns: fit.rms });
            }
        }
        out
    }

    fn push(&mut self, x: f64, y: f64) {
        self.window.push_back((x, y));
        if self.window.len() > WINDOW {
            self.window.pop_front();
        }
        self.fit = (self.window.len() >= MIN_SAMPLES).then(|| fit_line(&self.window));
    }
}

/// Least-squares line with RMS residual.
fn fit_line(pts: &VecDeque<(f64, f64)>) -> Fit {
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let b = if sxx > 0.0 { sxy / sxx } else { 0.0 };
    let a = my - b * mx;
    let rms = (pts
        .iter()
        .map(|p| (p.1 - (a + b * p.0)).powi(2))
        .sum::<f64>()
        / n)
        .sqrt();
    Fit { a, b, rms }
}

/// Human-readable signed nanoseconds.
pub fn fmt_ns(ns: f64) -> String {
    let a = ns.abs();
    if a >= 1e9 {
        format!("{:+.6} s", ns / 1e9)
    } else if a >= 1e6 {
        format!("{:+.3} ms", ns / 1e6)
    } else if a >= 1e3 {
        format!("{:+.2} µs", ns / 1e3)
    } else {
        format!("{ns:+.0} ns")
    }
}

struct Candidate {
    port: [u8; 10],
    data: AnnounceData,
    timeout: Duration,
    last_seen: Instant,
    announces: u64,
}

struct PendingSync {
    seq: u16,
    local_ns: i128,
    sys: Timestamp,
    correction_ns: i64,
}

pub fn run(
    cfg: Config,
    sock: RawSock,
    clock: TaiClock,
    state: Arc<Mutex<Status>>,
    cmds: Receiver<Cmd>,
) -> Result<()> {
    let clock_identity = ptp::clock_identity_from_mac(sock.mac);
    let builder = MsgBuilder {
        clock_identity,
        port_number: 1,
        domain: cfg.domain,
    };
    let hw = sock.is_hardware_ts();
    let th = Thresholds::new(&cfg, hw);
    let start = Instant::now();

    {
        let mut st = state.lock().unwrap();
        st.role = Role::Monitor;
        st.sync = Some(SyncMonitor::default());
        st.clock_identity = clock_identity;
        st.domain = cfg.domain;
        st.time_source_desc = clock.describe().to_string();
        st.ts_desc = sock.ts_desc().to_string();
        st.ts_hardware = hw;
        st.utc_offset = clock.utc_offset();
        st.started = start;
        st.log(format!(
            "Started as slave monitor on {} (clock {}) — jump threshold {}, \
             frequency threshold {:.0} ppb; no clock is adjusted",
            cfg.interface,
            ptp::fmt_clock_identity(&clock_identity),
            fmt_ns(th.jump_ns).trim_start_matches('+'),
            th.freq_ppb
        ));
        if hw {
            st.log(format!("Hardware timestamping active: {}", sock.ts_desc()));
        } else {
            st.log(format!(
                "WARNING: no hardware timestamping ({}) — only jumps above ~100 µs \
                 are distinguishable from timestamp noise",
                sock.ts_desc()
            ));
        }
    }

    let mut pdelay = PeerDelay::new(start);
    let mut analyzer = Analyzer::new(th);
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut selected: Option<[u8; 10]> = None;
    let mut selected_gm: Option<[u8; 8]> = None;
    let mut pending: Option<PendingSync> = None;
    let mut last_seq: Option<u16> = None;
    let mut last_sync_at: Option<Instant> = None;
    let mut sync_interval = Duration::from_millis(125);
    let mut sync_lost = false;
    let mut last_tbi: Option<u16> = None;
    let mut last_spike_log: Option<Instant> = None;
    let mut spikes_unlogged = 0u64;
    let mut rxbuf = [0u8; 1600];

    loop {
        while let Ok(cmd) = cmds.try_recv() {
            if let Cmd::Quit = cmd {
                return Ok(());
            }
        }

        let now = Instant::now();
        pdelay.tick(now, &builder, &sock, &clock, &state);

        // Master selection: best Announce still within its receipt timeout.
        candidates.retain(|c| now.duration_since(c.last_seen) < c.timeout);
        let best = candidates
            .iter()
            .min_by_key(|c| (c.data.bmca_key(), c.data.steps_removed))
            .map(|c| (c.port, c.data));
        if best.map(|b| b.0) != selected && now.duration_since(start) >= SELECTION_SETTLE {
            let mut st = state.lock().unwrap();
            match best {
                Some((port, data)) => {
                    if selected_gm.is_some_and(|g| g != data.gm_identity) {
                        st.sync.as_mut().unwrap().gm_changes += 1;
                        st.log(format!(
                            "ALARM: grandmaster changed {} → {}",
                            ptp::fmt_clock_identity(&selected_gm.unwrap()),
                            ptp::fmt_clock_identity(&data.gm_identity)
                        ));
                    }
                    st.log(format!(
                        "Following master port {} — GM {} (prio1 {}, class {}, {} hops)",
                        fmt_port(&port),
                        ptp::fmt_clock_identity(&data.gm_identity),
                        data.priority1,
                        data.clock_class,
                        data.steps_removed
                    ));
                    selected_gm = Some(data.gm_identity);
                }
                None => st.log("ALARM: master lost — no Announce within its receipt timeout"),
            }
            selected = best.map(|b| b.0);
            pending = None;
            last_seq = None;
            // A half-learned model would mix two masters' samples; a
            // complete one is kept so a time jump between them is reported.
            if !analyzer.ready() {
                analyzer.reset();
            }
        }

        if let Some(t) = last_sync_at {
            if !sync_lost && now.duration_since(t) > sync_interval * 3 + Duration::from_millis(50) {
                sync_lost = true;
                let mut st = state.lock().unwrap();
                st.sync.as_mut().unwrap().sync_timeouts += 1;
                st.log(format!(
                    "ALARM: Sync stopped — none for {} ms",
                    now.duration_since(t).as_millis()
                ));
            }
        }

        let deadline = pdelay.next;
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        if sock.poll(timeout.as_millis() as i32)? {
            while let Some((n, stamp)) = sock.recv(&mut rxbuf)? {
                let t_rx = stamp_to_tai(stamp, &sock, &clock).unwrap_or_else(|| clock.now());
                let Some(msg) = ptp::parse(&rxbuf[..n]) else {
                    continue;
                };
                if pdelay.handle(&msg, t_rx, &builder, &sock, &clock, &state) {
                    continue;
                }
                match msg {
                    Msg::Announce {
                        source,
                        log_interval,
                        data,
                    } => {
                        if source[..8] == clock_identity[..] {
                            continue;
                        }
                        let rx_at = Instant::now();
                        match candidates.iter_mut().find(|c| c.port == source) {
                            Some(c) => {
                                c.data = data;
                                c.last_seen = rx_at;
                                c.announces += 1;
                            }
                            None => candidates.push(Candidate {
                                port: source,
                                data,
                                timeout: log_to_duration(log_interval.clamp(-4, 4)) * 3,
                                last_seen: rx_at,
                                announces: 1,
                            }),
                        }
                        state.lock().unwrap().counters.announce_rx += 1;
                    }
                    Msg::Sync {
                        source,
                        seq,
                        log_interval,
                        two_step,
                        origin,
                        correction_ns,
                    } => {
                        if Some(source) != selected {
                            state.lock().unwrap().counters.foreign_sync_rx += 1;
                            continue;
                        }
                        // Never mix timebases: in hardware mode a frame
                        // without a hardware stamp is unusable.
                        if hw && !stamp.is_some_and(|s| s.hardware) {
                            continue;
                        }
                        let rx_at = Instant::now();
                        sync_interval = log_to_duration(log_interval.clamp(-7, 4));
                        let mut st = state.lock().unwrap();
                        let sm = st.sync.as_mut().unwrap();
                        sm.sync_rx += 1;
                        if let Some(last) = last_seq {
                            let gap = seq.wrapping_sub(last).wrapping_sub(1);
                            if gap > 0 && gap < 1000 {
                                sm.missing_syncs += gap as u64;
                            }
                        }
                        if let Some(p) = pending.take() {
                            if p.seq != seq {
                                sm.missing_follow_ups += 1;
                            }
                        }
                        if let Some(t) = last_sync_at {
                            let ms = rx_at.duration_since(t).as_secs_f64() * 1e3;
                            sm.sync_interval_ms =
                                Some(sm.sync_interval_ms.map_or(ms, |e| e * 0.9 + ms * 0.1));
                        }
                        last_seq = Some(seq);
                        if sync_lost {
                            sync_lost = false;
                            let gap = last_sync_at.map_or(0, |t| rx_at.duration_since(t).as_millis());
                            st.log(format!("Sync resumed after {gap} ms"));
                        }
                        last_sync_at = Some(rx_at);
                        drop(st);

                        let p = PendingSync {
                            seq,
                            local_ns: stamp_to_local_ns(stamp),
                            sys: t_rx,
                            correction_ns,
                        };
                        if two_step {
                            pending = Some(p);
                        } else {
                            sample(
                                p, origin, 0, &pdelay, &mut analyzer, &state,
                                &mut last_spike_log, &mut spikes_unlogged,
                            );
                        }
                    }
                    Msg::FollowUp {
                        source,
                        seq,
                        precise_origin,
                        correction_ns,
                        info,
                    } => {
                        if Some(source) != selected {
                            continue;
                        }
                        {
                            let mut st = state.lock().unwrap();
                            st.sync.as_mut().unwrap().follow_up_rx += 1;
                            if let Some(info) = info {
                                let tbi = info.gm_time_base_indicator;
                                if last_tbi.is_some_and(|l| l != tbi) {
                                    st.sync.as_mut().unwrap().time_base_changes += 1;
                                    st.log(format!(
                                        "ALARM: GM time base changed (gmTimeBaseIndicator {} → {tbi}, \
                                         lastGmPhaseChange {})",
                                        last_tbi.unwrap(),
                                        fmt_ns(info.last_gm_phase_change_ns as f64)
                                    ));
                                }
                                last_tbi = Some(tbi);
                            }
                        }
                        if pending.as_ref().is_some_and(|p| p.seq == seq) {
                            let p = pending.take().unwrap();
                            sample(
                                p, precise_origin, correction_ns, &pdelay, &mut analyzer,
                                &state, &mut last_spike_log, &mut spikes_unlogged,
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        // Housekeeping for the UI.
        let now = Instant::now();
        let mut st = state.lock().unwrap();
        st.tai_now_secs = clock.now().secs;
        st.link_delay_ns = pdelay.link_delay_ns;
        st.foreign_masters = candidates
            .iter()
            .map(|c| ForeignMaster {
                data: c.data,
                last_seen: c.last_seen,
                announces: c.announces,
                would_win: Some(c.port) == selected,
            })
            .collect();
        let sm = st.sync.as_mut().unwrap();
        sm.master_port = selected;
        sm.gm = selected.and_then(|s| candidates.iter().find(|c| c.port == s).map(|c| c.data));
        sm.drift_ppb = analyzer.drift_ppb();
        sm.rms_ns = analyzer.rms_ns();
        sm.state = if selected.is_none() {
            SyncState::NoMaster
        } else if sync_lost || last_sync_at.is_none_or(|t| now.duration_since(t) > Duration::from_secs(3)) {
            SyncState::SyncLost
        } else if !analyzer.ready() {
            SyncState::Learning
        } else if analyzer.unstable() {
            SyncState::Unstable
        } else {
            SyncState::Locked
        };
    }
}

/// Process one complete Sync (+ Follow_Up) measurement.
#[allow(clippy::too_many_arguments)]
fn sample(
    p: PendingSync,
    origin: Timestamp,
    fup_correction_ns: i64,
    pdelay: &PeerDelay,
    analyzer: &mut Analyzer,
    state: &Arc<Mutex<Status>>,
    last_spike_log: &mut Option<Instant>,
    spikes_unlogged: &mut u64,
) {
    let correction = p.correction_ns + fup_correction_ns;
    // Master time at our receive instant.
    let master_ns = origin.to_ns()
        + correction as i128
        + pdelay.link_delay_ns.unwrap_or(0.0).round() as i128;
    let anomalies = analyzer.add(p.local_ns, master_ns - p.local_ns);

    let mut st = state.lock().unwrap();
    let gm = st
        .sync
        .as_ref()
        .and_then(|s| s.gm)
        .map(|g| ptp::fmt_clock_identity(&g.gm_identity))
        .unwrap_or_else(|| "?".into());
    let mut logs = Vec::new();
    {
        let sm = st.sync.as_mut().unwrap();
        sm.correction_ns = Some(correction);
        sm.offset_to_system_ns = Some((master_ns - p.sys.to_ns()) as f64);
        sm.residual_ns = analyzer.last_residual;
        let spikes = anomalies.iter().filter_map(|a| match a {
            Anomaly::Spike { residual_ns } => Some(*residual_ns),
            _ => None,
        });
        for r in spikes.chain(analyzer.last_residual) {
            sm.peak_ns = sm.peak_ns.max(r.abs());
            sm.history.push_back(r.abs());
            if sm.history.len() > HISTORY {
                sm.history.pop_front();
            }
        }
        for a in anomalies {
            match a {
                Anomaly::Step { size_ns } => {
                    sm.steps += 1;
                    sm.last_step_ns = Some(size_ns);
                    logs.push(format!("ALARM: master time STEP {} (GM {gm})", fmt_ns(size_ns)));
                }
                Anomaly::Spike { residual_ns } => {
                    sm.spikes += 1;
                    let now = Instant::now();
                    if last_spike_log.is_none_or(|t| now.duration_since(t) >= SPIKE_LOG_INTERVAL) {
                        let more = if *spikes_unlogged > 0 {
                            format!(" (+{} more since last report)", spikes_unlogged)
                        } else {
                            String::new()
                        };
                        logs.push(format!(
                            "WARNING: time spike {} on a single Sync{more}",
                            fmt_ns(residual_ns)
                        ));
                        *last_spike_log = Some(now);
                        *spikes_unlogged = 0;
                    } else {
                        *spikes_unlogged += 1;
                    }
                }
                Anomaly::FreqChange { from_ppb, to_ppb } => {
                    sm.freq_changes += 1;
                    logs.push(format!(
                        "ALARM: master frequency changed {from_ppb:+.0} → {to_ppb:+.0} ppb"
                    ));
                }
                Anomaly::Unstable { rms_ns } => logs.push(format!(
                    "ALARM: sync unstable — residual jitter {} RMS",
                    fmt_ns(rms_ns).trim_start_matches('+')
                )),
                Anomaly::Stable { rms_ns } => logs.push(format!(
                    "Sync stable again — residual jitter {} RMS",
                    fmt_ns(rms_ns).trim_start_matches('+')
                )),
                Anomaly::Relearn { outliers } => {
                    sm.relearns += 1;
                    logs.push(format!(
                        "ALARM: master time erratic — {outliers} consecutive Syncs fit \
                         neither the drift model nor each other; relearning"
                    ));
                }
            }
        }
    }
    for l in logs {
        st.log(l);
    }
}

fn fmt_port(p: &[u8; 10]) -> String {
    let mut id = [0u8; 8];
    id.copy_from_slice(&p[..8]);
    format!(
        "{}-{}",
        ptp::fmt_clock_identity(&id),
        u16::from_be_bytes([p[8], p[9]])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TH: Thresholds = Thresholds {
        jump_ns: 1_000.0,
        freq_ppb: 100.0,
    };
    const PERIOD: i128 = 125_000_000;

    /// Feed `n` samples starting at sample index `from`, with a master that
    /// runs `drift_ppb` fast, plus an offset function.
    fn feed(
        a: &mut Analyzer,
        from: i128,
        n: i128,
        drift_ppb: f64,
        extra: impl Fn(i128) -> i128,
    ) -> Vec<(i128, Anomaly)> {
        let mut out = Vec::new();
        for i in from..from + n {
            let local = 5_000_000_000 + i * PERIOD;
            let off = 1_000_000_000_000 + (local as f64 * drift_ppb / 1e9) as i128 + extra(i);
            for an in a.add(local, off) {
                out.push((i, an));
            }
        }
        out
    }

    #[test]
    fn clean_drift_has_no_anomalies() {
        let mut a = Analyzer::new(TH);
        // Small deterministic jitter (±40 ns).
        let ev = feed(&mut a, 0, 400, 12_345.0, |i| (i * 37 % 81) - 40);
        assert!(ev.is_empty(), "{ev:?}");
        assert!((a.drift_ppb().unwrap() - 12_345.0).abs() < 5.0);
        assert!(a.ready() && !a.unstable());
    }

    #[test]
    fn step_is_detected_once_and_model_recovers() {
        let mut a = Analyzer::new(TH);
        let ev = feed(&mut a, 0, 200, 500.0, |i| if i >= 100 { 50_000 } else { 0 });
        assert_eq!(ev.len(), 1, "{ev:?}");
        match ev[0] {
            (101, Anomaly::Step { size_ns }) => assert!((size_ns - 50_000.0).abs() < 10.0),
            other => panic!("unexpected {other:?}"),
        }
        assert!(a.last_residual.unwrap().abs() < 10.0);
    }

    #[test]
    fn huge_step_keeps_precision() {
        let mut a = Analyzer::new(TH);
        // GM jumps by ~56 years (e.g. a master that lost its time).
        let years: i128 = 56 * 365 * 86_400 * 1_000_000_000;
        let ev = feed(&mut a, 0, 300, -800.0, |i| if i >= 100 { -years } else { 0 });
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert!(matches!(ev[0].1, Anomaly::Step { .. }));
        assert!(a.last_residual.unwrap().abs() < 10.0);
    }

    #[test]
    fn single_outlier_is_a_spike_not_a_step() {
        let mut a = Analyzer::new(TH);
        let ev = feed(&mut a, 0, 200, 0.0, |i| if i == 120 { 30_000 } else { 0 });
        assert_eq!(ev.len(), 1, "{ev:?}");
        match ev[0] {
            (121, Anomaly::Spike { residual_ns }) => {
                assert!((residual_ns - 30_000.0).abs() < 10.0)
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn frequency_change_is_detected() {
        let mut a = Analyzer::new(TH);
        let mut ev = feed(&mut a, 0, 200, 0.0, |_| 0);
        assert!(ev.is_empty());
        // From sample 200 on, the master runs 2 ppm faster (phase continuous):
        // over 125 ms that is 250 ns per sample, below the jump threshold.
        let t0 = 5_000_000_000 + 200 * PERIOD;
        ev = feed(&mut a, 200, 200, 0.0, |i| {
            let local = 5_000_000_000 + i * PERIOD;
            (local - t0) * 2_000 / 1_000_000_000
        });
        assert!(
            ev.iter().any(|(_, e)| matches!(e, Anomaly::FreqChange { .. })),
            "{ev:?}"
        );
        assert!((a.drift_ppb().unwrap() - 2_000.0).abs() < 50.0);
    }

    #[test]
    fn jitter_raises_and_clears_unstable() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 100, 0.0, |_| 0);
        // ±600 ns alternating noise: RMS 600 > 250 but each sample within 1 µs.
        let ev = feed(&mut a, 100, 100, 0.0, |i| if i % 2 == 0 { 600 } else { -600 });
        assert!(ev.iter().any(|(_, e)| matches!(e, Anomaly::Unstable { .. })), "{ev:?}");
        assert!(a.unstable());
        let ev = feed(&mut a, 200, 100, 0.0, |_| 0);
        assert!(ev.iter().any(|(_, e)| matches!(e, Anomaly::Stable { .. })), "{ev:?}");
    }

    #[test]
    fn erratic_samples_trigger_relearn_and_recover() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 100, 0.0, |_| 0);
        // Wildly alternating garbage, then a clean master 5 s off.
        let ev = feed(&mut a, 100, 20, 0.0, |i| (i % 3 + 1) * 400_000_000);
        assert!(ev.iter().any(|(_, e)| matches!(e, Anomaly::Relearn { .. })), "{ev:?}");
        let ev = feed(&mut a, 120, 100, 300.0, |_| 5_000_000_000);
        assert!(ev.is_empty(), "{ev:?}");
        assert!(a.ready() && a.last_residual.unwrap().abs() < 10.0);
    }

    #[test]
    fn fmt_ns_units() {
        assert_eq!(fmt_ns(-12.0), "-12 ns");
        assert_eq!(fmt_ns(1_500.0), "+1.50 µs");
        assert_eq!(fmt_ns(2_500_000.0), "+2.500 ms");
        assert_eq!(fmt_ns(-3e9), "-3.000000 s");
    }
}
