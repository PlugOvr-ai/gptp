//! Slave monitor (--monitor): we act as a passive gPTP slave, follow the
//! best master on the link, and watch its time for jumps and instability.
//!
//! No clock is adjusted. Each Sync/Follow_Up pair yields one sample of
//! "master time − local time" against a free-running local timebase (raw
//! PHC, or CLOCK_MONOTONIC_RAW for software stamps). A robust (Theil–Sen)
//! line fit over the last few seconds models the frequency difference, so
//! a share of disturbed samples cannot bend it; anything the model does
//! not predict is what we report:
//!
//! - **step**: three consecutive samples off the prediction by the same
//!   amount (two can agree by chance under heavy path-delay jitter) — the master's time jumped (GM change, GM stepped its clock,
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
use crate::clocksync::{self, Rx, SyncEngine};
use crate::config::{Config, TimeSync};
use crate::gm::{log_to_duration, Cmd};
use crate::inventory::fmt_mac;
use crate::net::{RawSock, RawStamp};
use crate::pdelay::PeerDelay;
use crate::ptp::{self, AnnounceData, Msg, MsgBuilder, Timestamp};
use crate::state::{ForeignMaster, Role, Status, SyncMonitor, SyncState};
use crate::syncdiag::{self, Diag, SampleRec};

/// Samples in the drift-model window (8 s at the default 8 Sync/s).
const WINDOW: usize = 64;
/// Samples needed before the model predicts anything.
const MIN_SAMPLES: usize = 8;
/// Consistent out-of-model samples that make a step.
const STEP_SAMPLES: usize = 3;
/// Learning restarts (inconsistent samples) before the model is learned
/// from noisy data anyway.
const LEARN_RESTARTS: usize = 4;
/// Samples kept while learning, for learning from noisy data.
const LEARN_BUF: usize = 32;
/// With a noisy master, spikes/steps must exceed this many σ of its noise,
/// and a step's samples must agree within NOISE_TIGHT σ.
const NOISE_JUMP: f64 = 6.0;
const NOISE_TIGHT: f64 = 4.0;
/// Re-check the drift for frequency changes every this many samples.
const FREQ_CHECK_EVERY: usize = 8;
const HISTORY: usize = 240;
/// Longest time the model stays frozen during a jitter burst, s.
const HOLD_MAX_SECS: f64 = 30.0;
/// Wait this long for Announces before choosing the first master, so we do
/// not briefly follow whichever master happened to announce first.
const SELECTION_SETTLE: Duration = Duration::from_millis(1500);

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
    Step {
        size_ns: f64,
    },
    Spike {
        residual_ns: f64,
    },
    FreqChange {
        from_ppb: f64,
        to_ppb: f64,
    },
    Unstable {
        rms_ns: f64,
    },
    Stable {
        rms_ns: f64,
    },
    /// Too many consecutive samples off the model; it was discarded.
    /// The master's time is too noisy for the configured thresholds; the
    /// model was learned anyway and the thresholds follow the noise.
    Noisy {
        sigma_ns: f64,
        jump_ns: f64,
    },
    Relearn {
        outliers: usize,
    },
}

#[derive(Clone, Copy)]
pub struct Fit {
    /// Offset at x = 0, ns.
    pub a: f64,
    /// Slope, ns per s = ppb.
    pub b: f64,
    rms: f64,
    /// Robust σ of the residuals (1.4826 × MAD), ns.
    pub sigma: f64,
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
    /// Out-of-model samples held back until it is clear whether they are
    /// spikes or a step: (x, raw relative offset, residual).
    suspects: Vec<(f64, i128, f64)>,
    fit: Option<Fit>,
    ref_drift: Option<f64>,
    /// Samples to wait before the drift reference is (re)taken.
    freq_settle: usize,
    /// Model frozen during a jitter burst, with the burst's jitter level:
    /// measure against the model, do not learn.
    hold: Option<f64>,
    since_freq_check: usize,
    unstable: bool,
    /// Consecutive samples outside the model.
    outliers: usize,
    pub last_residual: Option<f64>,
    /// Deviation of the last sample from the model, however it was
    /// classified (None while learning).
    pub last_error: Option<f64>,
    /// Learning restarts since the last reset.
    learn_restarts: usize,
    /// Recent samples while learning (not cleared by restarts).
    learn_buf: VecDeque<(f64, f64)>,
    /// Robust σ of the master's noise once it has proven too noisy for the
    /// configured thresholds; 0 = not noisy. Survives resets.
    noise: f64,
}

impl Analyzer {
    pub fn new(th: Thresholds) -> Self {
        Analyzer {
            th,
            base: None,
            window: VecDeque::with_capacity(WINDOW + 1),
            suspects: Vec::new(),
            fit: None,
            ref_drift: None,
            freq_settle: 0,
            hold: None,
            since_freq_check: 0,
            unstable: false,
            outliers: 0,
            last_residual: None,
            last_error: None,
            learn_restarts: 0,
            learn_buf: VecDeque::with_capacity(LEARN_BUF + 1),
            noise: 0.0,
        }
    }

    /// Forget everything and start learning from the next sample. A known
    /// noise level is kept: the master (or the path) is likely the same.
    pub fn reset(&mut self) {
        let noise = self.noise;
        *self = Analyzer::new(self.th);
        self.noise = noise;
    }

    /// Spike/step threshold: the configured one, or wider for a noisy master.
    fn jump(&self) -> f64 {
        self.th.jump_ns.max(NOISE_JUMP * self.noise)
    }

    /// How closely a step's samples must agree.
    fn tight(&self) -> f64 {
        self.th.jitter_ns().max(NOISE_TIGHT * self.noise)
    }

    /// Robust σ of the master's noise, if it is too noisy for the
    /// configured thresholds.
    pub fn noise_ns(&self) -> Option<f64> {
        (self.noise > 0.0).then_some(self.noise)
    }

    /// Re-take the drift reference once the whole window has been replaced
    /// — after a disturbance its samples may have biased the slope.
    pub fn rebaseline(&mut self) {
        self.ref_drift = None;
        self.freq_settle = WINDOW;
    }

    /// Freeze the model while samples are disturbed (jitter burst): they
    /// are measured against it but not learned from, so the model is intact
    /// when the disturbance ends. Steps are still detected, with a stricter
    /// agreement rule that random jitter practically never meets.
    /// `level` is the burst's jitter level: during the hold a step must
    /// clearly exceed it, so samples of a noisy burst that happen to agree
    /// are not taken for one. A smaller real step shows up after the burst.
    pub fn set_hold(&mut self, level: Option<f64>) {
        self.hold = level;
    }

    pub fn holding(&self) -> bool {
        self.hold.is_some()
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
            // A master noisier than the threshold never gives such a run:
            // after a few restarts, learn from the recent samples anyway
            // (the fit is robust) and let the thresholds follow the noise.
            self.learn_buf.push_back((x, y_raw as f64));
            if self.learn_buf.len() > LEARN_BUF {
                self.learn_buf.pop_front();
            }
            let n = self.window.len();
            if n >= 2 {
                let (x1, y1) = self.window[n - 2];
                let (x2, y2) = self.window[n - 1];
                let pred = y2 + (y2 - y1) / (x2 - x1).max(1e-9) * (x - x2);
                if (y_raw as f64 - pred).abs() > self.jump() {
                    self.window.clear();
                    self.learn_restarts += 1;
                }
            }
            if self.learn_restarts >= LEARN_RESTARTS && self.learn_buf.len() >= 2 * MIN_SAMPLES {
                let fit = fit_line(&self.learn_buf);
                // Beyond 100× the threshold it is garbage, not noise.
                if NOISE_JUMP * fit.sigma > self.th.jump_ns / 2.0
                    && fit.sigma < 100.0 * self.th.jump_ns
                {
                    self.window = self.learn_buf.clone();
                    self.noise = fit.sigma;
                    self.fit = Some(fit);
                    out.push(Anomaly::Noisy {
                        sigma_ns: fit.sigma,
                        jump_ns: self.jump(),
                    });
                    self.last_residual = None;
                    self.last_error = None;
                    return out;
                }
            }
            self.push(x, y_raw as f64);
            self.last_residual = None;
            self.last_error = None;
            return out;
        };
        let pred = fit.predict(x);
        let r = y_raw as f64 - pred;
        self.last_error = Some(r);

        if let Some(level) = self.hold {
            if r.abs() <= self.jump().max(4.0 * level) {
                self.suspects.clear();
                self.last_residual = (r.abs() <= self.jump()).then_some(r);
                return out;
            }
            let tight = self.tight();
            if !self.suspects.iter().all(|s| (r - s.2).abs() <= tight) {
                self.suspects.clear();
            }
            self.suspects.push((x, y_raw, r));
            self.last_residual = None;
            if self.suspects.len() >= STEP_SAMPLES {
                out.push(self.apply_step(bx, by, y_raw, pred, r));
            }
            return out;
        }

        if r.abs() <= self.jump() {
            for (_, _, sr) in self.suspects.drain(..) {
                out.push(Anomaly::Spike { residual_ns: sr });
            }
            self.outliers = 0;
            self.push(x, y_raw as f64);
            self.last_residual = Some(r);
        } else if self.suspects.iter().all(|s| (r - s.2).abs() <= self.jump()) {
            if self.suspects.len() + 1 < STEP_SAMPLES {
                // Held back: its residual is reported once classified (as a
                // spike), or dropped if it turns out to be part of a step.
                self.suspects.push((x, y_raw, r));
                self.last_residual = None;
                self.outliers += 1;
                return out;
            }
            // A steady ramp (residuals moving monotonically) is a frequency
            // change, not a step: accept the samples as they are so the fit
            // follows, and let the drift check report it.
            // Smooth: monotonic, roughly equal increments, moving by more
            // than half the instability level, and the model was calm —
            // random path-delay jitter does not qualify. Should a noisy
            // real step ever pass as a ramp, the following samples stay
            // offset by a constant and are reported as the step.
            let r0 = self.suspects[0].2;
            let incs: Vec<f64> = self
                .suspects
                .windows(2)
                .map(|w| w[1].2 - w[0].2)
                .chain([r - self.suspects.last().unwrap().2])
                .collect();
            let monotonic = incs.iter().all(|d| d.signum() == (r - r0).signum());
            let (lo, hi) = incs.iter().fold((f64::MAX, 0.0f64), |(lo, hi), d| {
                (lo.min(d.abs()), hi.max(d.abs()))
            });
            let smooth = monotonic && lo >= hi / 3.0 && fit.rms < self.th.jitter_ns();
            if smooth && (r - r0).abs() > self.th.jitter_ns() / 2.0 {
                for (sx, sy, _) in std::mem::take(&mut self.suspects) {
                    self.push(sx, sy as f64);
                }
                self.push(x, y_raw as f64);
                // Forget the old slope so the fit follows the new rate now
                // instead of lagging behind it for a whole window.
                while self.window.len() > 2 * MIN_SAMPLES {
                    self.window.pop_front();
                }
                self.fit = Some(fit_line(&self.window));
                self.last_residual = Some(r);
                self.outliers = 0;
                return out;
            }
            // A step needs the samples to agree tightly; samples that only
            // roughly agree are path-delay jitter (e.g. three similarly
            // queued Syncs at the onset of congestion).
            let tight = self.tight();
            if self.suspects.iter().all(|s| (r - s.2).abs() <= tight) {
                self.suspects.push((x, y_raw, r));
                out.push(self.apply_step(bx, by, y_raw, pred, r));
            } else {
                self.new_suspect(x, y_raw, r, &mut out);
                return out;
            }
        } else {
            self.new_suspect(x, y_raw, r, &mut out);
            return out;
        }

        self.since_freq_check += 1;
        self.freq_settle = self.freq_settle.saturating_sub(1);
        if let Some(fit) = self.fit {
            if self.window.len() >= WINDOW
                && self.freq_settle == 0
                && self.since_freq_check >= FREQ_CHECK_EVERY
            {
                self.since_freq_check = 0;
                // Residual jitter makes the fitted slope itself uncertain;
                // only a change well beyond that uncertainty is real.
                let n = self.window.len() as f64;
                let span = self.window.back().unwrap().0 - self.window.front().unwrap().0;
                let slope_sigma = fit.rms / (span.max(1e-3) * (n / 12.0).sqrt());
                match self.ref_drift {
                    Some(r) if (fit.b - r).abs() > self.th.freq_ppb.max(4.0 * slope_sigma) => {
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

    /// The held suspects were spikes; this sample starts a new run.
    /// Relearns when out-of-model samples keep coming.
    fn new_suspect(&mut self, x: f64, y_raw: i128, r: f64, out: &mut Vec<Anomaly>) {
        for (_, _, sr) in self.suspects.drain(..) {
            out.push(Anomaly::Spike { residual_ns: sr });
        }
        self.suspects.push((x, y_raw, r));
        self.last_residual = None;
        self.outliers += 1;
        if self.outliers >= MIN_SAMPLES {
            let outliers = self.outliers;
            self.reset();
            out.push(Anomaly::Relearn { outliers });
        }
    }

    /// The held suspects (the current sample last) agree: the master's time
    /// stepped. Shift the offset base by the step (in integer ns, so even a
    /// jump of years keeps full precision afterwards).
    fn apply_step(&mut self, bx: i128, by: i128, y_raw: i128, pred: f64, r: f64) -> Anomaly {
        let step = y_raw - pred.round() as i128;
        self.base = Some((bx, by + step));
        for (sx, sy, _) in std::mem::take(&mut self.suspects) {
            self.push(sx, (sy - step) as f64);
        }
        self.last_residual = Some(r - step as f64);
        self.last_error = self.last_residual;
        self.outliers = 0;
        Anomaly::Step {
            size_ns: step as f64,
        }
    }

    fn push(&mut self, x: f64, y: f64) {
        self.window.push_back((x, y));
        if self.window.len() > WINDOW {
            self.window.pop_front();
        }
        self.fit = (self.window.len() >= MIN_SAMPLES).then(|| fit_line(&self.window));
        // Follow a noisy master's noise level; drop it once the noise fits
        // the configured threshold again.
        if let (true, Some(f)) = (self.noise > 0.0, self.fit) {
            self.noise = if NOISE_JUMP * f.sigma > self.th.jump_ns / 2.0 {
                f.sigma
            } else {
                0.0
            };
        }
    }
}

/// Theil–Sen line fit (median of pairwise slopes, median intercept) with
/// RMS residual. Up to ~29% of disturbed samples — path-delay jitter that
/// stayed inside the jump threshold — cannot bias the slope, so the model
/// recovers within seconds after a disturbance instead of a whole window.
pub fn fit_line(pts: &VecDeque<(f64, f64)>) -> Fit {
    let n = pts.len();
    let mut slopes = Vec::with_capacity(n * (n - 1) / 2);
    for i in 0..n {
        for j in i + 1..n {
            let dx = pts[j].0 - pts[i].0;
            if dx > 0.0 {
                slopes.push((pts[j].1 - pts[i].1) / dx);
            }
        }
    }
    let b = median(&mut slopes).unwrap_or(0.0);
    let mut intercepts: Vec<f64> = pts.iter().map(|p| p.1 - b * p.0).collect();
    let a = median(&mut intercepts).unwrap_or(0.0);
    let rms = (pts
        .iter()
        .map(|p| (p.1 - (a + b * p.0)).powi(2))
        .sum::<f64>()
        / n as f64)
        .sqrt();
    let mut abs: Vec<f64> = pts.iter().map(|p| (p.1 - (a + b * p.0)).abs()).collect();
    let sigma = 1.4826 * median(&mut abs).unwrap_or(0.0);
    Fit { a, b, rms, sigma }
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mid = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, f64::total_cmp);
    Some(*m)
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
    utc_offset: i16,
    utc_offset_valid: bool,
    timeout: Duration,
    last_seen: Instant,
    announces: u64,
}

struct PendingSync {
    seq: u16,
    rx_at: Instant,
    stamp: Option<RawStamp>,
    local_ns: i128,
    sys: Timestamp,
    correction_ns: i64,
    src_mac: [u8; 6],
}

/// Recent samples split into their two parts — the GM's origin timestamp
/// and the bridges' correctionField — so a step can be pinned on one.
#[derive(Default)]
struct StepBlame {
    hist: VecDeque<Part>,
}

/// (local ns, origin − local ns, correction ns, sender MAC)
type Part = (i128, i128, i128, [u8; 6]);

impl StepBlame {
    const KEEP: usize = 16;

    fn push(&mut self, local_ns: i128, origin_ns: i128, correction_ns: i64, mac: [u8; 6]) {
        self.hist
            .push_back((local_ns, origin_ns - local_ns, correction_ns as i128, mac));
        if self.hist.len() > Self::KEEP {
            self.hist.pop_front();
        }
    }

    /// Explain a step of `size_ns` just confirmed on the newest sample. The
    /// last STEP_SAMPLES samples are the step; each part is extrapolated
    /// from the samples before it (a bridge relaying a stale origin makes
    /// both parts ramp, which cancels). Afterwards only the post-step
    /// samples are kept as the next baseline.
    fn explain(&mut self, size_ns: f64) -> String {
        let n = self.hist.len();
        let Some(&(x, o, c, mac)) = self.hist.back() else {
            return String::new();
        };
        let mut s = format!(" — sender {}", fmt_mac(&mac));
        if n > STEP_SAMPLES {
            let pre: Vec<_> = self.hist.iter().take(n - STEP_SAMPLES).collect();
            let (first, last) = (pre[0], pre[pre.len() - 1]);
            let extrapolate = |f: fn(&Part) -> i128| -> f64 {
                let span = (last.0 - first.0) as f64;
                let slope = if span > 0.0 {
                    (f(last) - f(first)) as f64 / span
                } else {
                    0.0
                };
                f(last) as f64 + slope * (x - last.0) as f64
            };
            let d_origin = o as f64 - extrapolate(|h| h.1);
            let d_corr = c as f64 - extrapolate(|h| h.2);
            let tol = (size_ns.abs() / 10.0).max(1_000.0);
            if (d_corr - size_ns).abs() < tol && d_origin.abs() < tol {
                s += "; step is in correctionField (a bridge), origin timestamp unchanged";
            } else if (d_origin - size_ns).abs() < tol && d_corr.abs() < tol {
                s += "; step is in the origin timestamp (GM or a bridge relaying it), \
                      correctionField unchanged";
            } else {
                s += &format!(
                    "; origin timestamp moved {}, correctionField {}",
                    fmt_ns(d_origin),
                    fmt_ns(d_corr)
                );
            }
            if last.3 != mac {
                s += &format!(" (sender changed from {})", fmt_mac(&last.3));
            }
        }
        let drop = n.saturating_sub(STEP_SAMPLES);
        self.hist.drain(..drop);
        s
    }
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
        sdo: ptp::SDO_GPTP,
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
             frequency threshold {:.0} ppb{}",
            cfg.iface(),
            ptp::fmt_clock_identity(&clock_identity),
            fmt_ns(th.jump_ns).trim_start_matches('+'),
            th.freq_ppb,
            match cfg.time_sync {
                None => "; no clock is adjusted",
                Some(TimeSync::Builtin) => "; time sync: built-in servo",
                Some(TimeSync::Linuxptp) => "; time sync: linuxptp",
            }
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

    // Another gPTP stack on this port (ptp4l) answers peer delay already: a
    // second responder makes the bridge treat the link as not asCapable and
    // stop sending Sync, and makes ptp4l report rogue responses.
    let ptp4l = clocksync::running(&["ptp4l"]);
    let listen_only =
        cfg.listen_only || cfg.time_sync == Some(TimeSync::Linuxptp) || !ptp4l.is_empty();
    {
        let mut st = state.lock().unwrap();
        if listen_only {
            let why = if cfg.time_sync == Some(TimeSync::Linuxptp) {
                "linuxptp serves this port".to_string()
            } else if let Some((_, pid)) = ptp4l.first() {
                format!("ptp4l is running (pid {pid}) and serves the port")
            } else {
                "--listen-only".to_string()
            };
            st.log(format!(
                "Listen-only: sending nothing, no peer delay ({why}); the offset to the system \
                 clock excludes the link delay"
            ));
        }
        st.sync.as_mut().unwrap().listen_only = listen_only;
        st.sync.as_mut().unwrap().time_sync = cfg.time_sync;
    }
    let mut engine = if cfg.time_sync == Some(TimeSync::Builtin) {
        match setup_engine(&cfg, &sock) {
            Ok(e) => {
                state.lock().unwrap().log(format!(
                    "Time sync: {}",
                    e.views()
                        .iter()
                        .map(|v| v.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                ));
                Some(e)
            }
            Err(e) => {
                state
                    .lock()
                    .unwrap()
                    .log(format!("ALARM: time sync not started — {e:#}"));
                None
            }
        }
    } else {
        None
    };
    let mut conflicts_checked: Option<Instant> = None;
    let mut linuxptp_checked: Option<Instant> = None;
    let mut utc_logged: Option<(i32, bool)> = None;

    let mut pdelay = PeerDelay::new(start);
    let mut analyzer = Analyzer::new(th);
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut selected: Option<[u8; 10]> = None;
    let mut selected_gm: Option<[u8; 8]> = None;
    let mut pending: Option<PendingSync> = None;
    let mut blame = StepBlame::default();
    let mut last_seq: Option<u16> = None;
    let mut last_sync_at: Option<Instant> = None;
    let mut sync_interval = Duration::from_millis(125);
    let mut sync_lost = false;
    let mut last_tbi: Option<u16> = None;
    let mut diag = Diag::new(th, cfg.capture_dir.clone().map(Into::into));
    let mut rxbuf = [0u8; 1600];

    loop {
        while let Ok(cmd) = cmds.try_recv() {
            if let Cmd::Quit = cmd {
                // The UI is gone by now; report a pending capture directly.
                if let Some(l) = diag.flush() {
                    eprintln!("{l}");
                }
                return Ok(());
            }
        }

        let now = Instant::now();
        if !listen_only {
            pdelay.tick(now, &builder, &sock, &clock, &state);
        }

        // Other time daemons would fight the servo: pause it while any runs.
        if let Some(e) = &mut engine {
            if conflicts_checked.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(30)) {
                conflicts_checked = Some(now);
                let found = clocksync::running(clocksync::CONFLICTING_DAEMONS);
                if !found.is_empty() && !e.disabled() {
                    let names: Vec<String> = found
                        .iter()
                        .map(|(n, p)| format!("{n} (pid {p})"))
                        .collect();
                    e.disable(&format!("{} running", names.join(", ")));
                    state.lock().unwrap().log(format!(
                        "ALARM: time sync paused — {} would fight over the clocks; stop it or \
                         run --setup-time-sync builtin once to disable it",
                        names.join(", ")
                    ));
                } else if found.is_empty() && e.disabled() {
                    e.enable();
                    state
                        .lock()
                        .unwrap()
                        .log("Time sync resumed: no conflicting time daemon running");
                }
            }
        }
        if cfg.time_sync == Some(TimeSync::Linuxptp)
            && linuxptp_checked.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10))
        {
            linuxptp_checked = Some(now);
            let status = crate::timesync_setup::unit_status();
            let mut st = state.lock().unwrap();
            let sm = st.sync.as_mut().unwrap();
            let failed: Vec<&(String, String)> =
                status.iter().filter(|(_, s)| s != "active").collect();
            let line = if status.is_empty() {
                "no units set up — run --setup-time-sync linuxptp".to_string()
            } else {
                status
                    .iter()
                    .map(|(u, s)| format!("{u}: {s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let changed = sm.linuxptp_status.as_deref() != Some(line.as_str());
            sm.linuxptp_status = Some(line.clone());
            if changed && (!failed.is_empty() || status.is_empty()) {
                st.log(format!("ALARM: linuxptp time sync not healthy — {line}"));
            }
        }

        // UTC offset: what the followed master announces, if marked valid
        // and plausible; otherwise the configured value.
        let utc_now = {
            let announced = selected
                .and_then(|s| candidates.iter().find(|c| c.port == s))
                .map(|c| (c.utc_offset as i32, c.utc_offset_valid));
            let use_announced = announced.is_some_and(|(o, v)| v && (30..=50).contains(&o));
            let utc = if use_announced {
                announced.unwrap().0
            } else {
                cfg.utc_offset as i32
            };
            if let Some(a) = announced {
                if utc_logged != Some(a) {
                    utc_logged = Some(a);
                    let mut st = state.lock().unwrap();
                    st.sync.as_mut().unwrap().master_utc_offset = Some(a);
                    st.log(if use_announced {
                        format!("Master announces UTC offset {} s (valid)", a.0)
                    } else {
                        format!(
                            "WARNING: master announces UTC offset {} s ({}) — using {} s \
                             (--utc-offset) to derive UTC",
                            a.0,
                            if a.1 {
                                "implausible"
                            } else {
                                "not marked valid"
                            },
                            utc
                        )
                    });
                }
            }
            utc
        };

        // Master selection: best Announce still within its receipt timeout.
        candidates.retain(|c| now.duration_since(c.last_seen) < c.timeout);
        let best = candidates
            .iter()
            .min_by_key(|c| (c.data.bmca_key(), c.data.steps_removed))
            .map(|c| (c.port, c.data));
        let changed = match best {
            // A bridge relaying a new GM keeps its port: compare both.
            Some((port, data)) => Some(port) != selected || Some(data.gm_identity) != selected_gm,
            None => selected.is_some(),
        };
        if changed && now.duration_since(start) >= SELECTION_SETTLE {
            let mut st = state.lock().unwrap();
            match best {
                Some((port, data)) => {
                    if selected_gm.is_some_and(|g| g != data.gm_identity) {
                        st.sync.as_mut().unwrap().gm_changes += 1;
                        diag.trigger("GM change");
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
                None => {
                    diag.trigger("master lost");
                    st.log("ALARM: master lost — no Announce within its receipt timeout")
                }
            }
            selected = best.map(|b| b.0);
            diag.on_master_change();
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
                diag.trigger("Sync timeout");
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
            while let Some((n, stamp, src_mac)) = sock.recv(&mut rxbuf)? {
                let t_rx = stamp_to_tai(stamp, &sock, &clock).unwrap_or_else(|| clock.now());
                let Some(msg) = ptp::parse(&rxbuf[..n]) else {
                    continue;
                };
                if !listen_only && pdelay.handle(&msg, t_rx, &builder, &sock, &clock, &state) {
                    continue;
                }
                match msg {
                    Msg::Announce {
                        source,
                        log_interval,
                        data,
                        utc_offset,
                        utc_offset_valid,
                    } => {
                        if source[..8] == clock_identity[..] {
                            continue;
                        }
                        let rx_at = Instant::now();
                        match candidates.iter_mut().find(|c| c.port == source) {
                            Some(c) => {
                                if Some(source) == selected
                                    && c.data.gm_identity == data.gm_identity
                                {
                                    let diff = syncdiag::announce_diff(&c.data, &data);
                                    if !diff.is_empty() {
                                        diag.trigger("GM announce change");
                                        state.lock().unwrap().log(format!(
                                            "WARNING: GM announce changed: {}",
                                            diff.join(", ")
                                        ));
                                    }
                                }
                                c.data = data;
                                c.utc_offset = utc_offset;
                                c.utc_offset_valid = utc_offset_valid;
                                c.last_seen = rx_at;
                                c.announces += 1;
                            }
                            None => candidates.push(Candidate {
                                port: source,
                                data,
                                utc_offset,
                                utc_offset_valid,
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
                        sm.nominal_interval_ns = sync_interval.as_nanos() as f64;
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
                            let gap =
                                last_sync_at.map_or(0, |t| rx_at.duration_since(t).as_millis());
                            st.log(format!("Sync resumed after {gap} ms"));
                        }
                        last_sync_at = Some(rx_at);
                        drop(st);

                        let p = PendingSync {
                            seq,
                            rx_at,
                            stamp,
                            // With the NIC clock disciplined, measure against
                            // its reconstructed free-running time.
                            local_ns: match (stamp, &engine) {
                                (Some(s), Some(e)) if s.hardware => e.free_running_ns(s.ts.to_ns()),
                                _ => stamp_to_local_ns(stamp),
                            },
                            sys: t_rx,
                            correction_ns,
                            src_mac,
                        };
                        if two_step {
                            pending = Some(p);
                        } else {
                            sample(
                                p,
                                origin,
                                0,
                                None,
                                None,
                                Ctx {
                                    link_delay_ns: pdelay.link_delay_ns.filter(|_| !listen_only),
                                    analyzer: &mut analyzer,
                                    diag: &mut diag,
                                    engine: engine.as_mut(),
                                    utc_offset: utc_now,
                                    check_system: cfg.time_sync == Some(TimeSync::Linuxptp),
                                    clock: &clock,
                                    state: &state,
                                    blame: &mut blame,
                                    fup_mac: src_mac,
                                },
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
                                    diag.trigger("GM time base change");
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
                            let fup_delay_us = p.rx_at.elapsed().as_secs_f64() * 1e6;
                            // cumulativeScaledRateOffset = (rateRatio - 1) * 2^41
                            let rate_ppb = info.map(|i| {
                                i.cumulative_scaled_rate_offset as f64 / 2f64.powi(41) * 1e9
                            });
                            sample(
                                p,
                                precise_origin,
                                correction_ns,
                                Some(fup_delay_us),
                                rate_ppb,
                                Ctx {
                                    link_delay_ns: pdelay.link_delay_ns.filter(|_| !listen_only),
                                    analyzer: &mut analyzer,
                                    diag: &mut diag,
                                    engine: engine.as_mut(),
                                    utc_offset: utc_now,
                                    check_system: cfg.time_sync == Some(TimeSync::Linuxptp),
                                    clock: &clock,
                                    state: &state,
                                    blame: &mut blame,
                                    fup_mac: src_mac,
                                },
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        // Housekeeping for the UI.
        let now = Instant::now();
        let capture_log = diag.poll();
        let mut st = state.lock().unwrap();
        if let Some(l) = capture_log {
            st.log(l);
        }
        st.tai_now_secs = clock.now().secs;
        st.link_delay_ns = pdelay.link_delay_ns.filter(|_| !listen_only);
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
        sm.clocks = engine.as_ref().map_or_else(Vec::new, |e| e.views());
        sm.bursts = diag.bursts;
        sm.in_burst = diag.in_burst();
        sm.captures = diag.captures;
        sm.rms_ns = analyzer.rms_ns();
        sm.noise_ns = analyzer.noise_ns();
        sm.state = if selected.is_none() {
            SyncState::NoMaster
        } else if sync_lost
            || last_sync_at.is_none_or(|t| now.duration_since(t) > Duration::from_secs(3))
        {
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

/// What `sample` works with besides the measurement itself.
struct Ctx<'a> {
    /// None in listen-only mode (we do not measure it).
    link_delay_ns: Option<f64>,
    analyzer: &'a mut Analyzer,
    diag: &'a mut Diag,
    engine: Option<&'a mut SyncEngine>,
    /// TAI − UTC used to derive UTC, s.
    utc_offset: i32,
    /// Alarm when the system clock is far off the master's UTC (another
    /// tool keeps it in sync: linuxptp).
    check_system: bool,
    clock: &'a TaiClock,
    state: &'a Arc<Mutex<Status>>,
    blame: &'a mut StepBlame,
    /// Sender of the Follow_Up (of the Sync for one-step).
    fup_mac: [u8; 6],
}

/// Process one complete Sync (+ Follow_Up) measurement.
fn sample(
    p: PendingSync,
    origin: Timestamp,
    fup_correction_ns: i64,
    fup_delay_us: Option<f64>,
    rate_offset_ppb: Option<f64>,
    ctx: Ctx,
) {
    let Ctx {
        link_delay_ns,
        analyzer,
        diag,
        engine,
        utc_offset,
        check_system,
        clock,
        state,
        blame,
        fup_mac,
    } = ctx;
    let correction = p.correction_ns + fup_correction_ns;
    // Master time at our receive instant.
    let master_ns =
        origin.to_ns() + correction as i128 + link_delay_ns.unwrap_or(0.0).round() as i128;
    let utc_ns = utc_offset as i128 * 1_000_000_000;
    let system_offset = (master_ns - utc_ns - clock.tai_to_realtime_ns(p.sys)) as f64;
    // Freeze the model during a jitter burst, but not forever: a burst that
    // persists is the new normal and has to be learned.
    let hold = diag.burst_age().is_some_and(|a| a < HOLD_MAX_SECS);
    let hold_expired = analyzer.holding() && !hold && diag.burst_age().is_some();
    if hold_expired {
        analyzer.reset();
    }
    analyzer.set_hold(diag.burst_level().filter(|_| hold));
    let anomalies = analyzer.add(p.local_ns, master_ns - p.local_ns);
    blame.push(p.local_ns, origin.to_ns(), correction, fup_mac);

    let mut st = state.lock().unwrap();
    if hold_expired {
        st.log(format!(
            "WARNING: jitter burst persists > {HOLD_MAX_SECS:.0} s — relearning the drift model \
             on the disturbed data"
        ));
    }
    let gm = st
        .sync
        .as_ref()
        .and_then(|s| s.gm)
        .map(|g| ptp::fmt_clock_identity(&g.gm_identity))
        .unwrap_or_else(|| "?".into());
    let (rx_bps, link_errors) = (
        st.link.rx_bps,
        st.link.problems.iter().map(|c| c.delta).sum::<u64>(),
    );
    let mut logs = Vec::new();
    let mut notes = Vec::new();
    let nominal_ns;
    let during_burst = if diag.burst_established() {
        " — during a jitter burst, may be path delay rather than the master's clock"
    } else {
        ""
    };
    {
        let sm = st.sync.as_mut().unwrap();
        nominal_ns = sm.nominal_interval_ns;
        sm.correction_ns = Some(correction);
        sm.offset_to_system_ns = Some(system_offset);
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
                    notes.push("step");
                    diag.trigger("time step");
                    let mut why = blame.explain(size_ns);
                    if p.src_mac != fup_mac {
                        why += &format!(" (Sync from {})", fmt_mac(&p.src_mac));
                    }
                    logs.push(format!(
                        "ALARM: master time STEP {} (GM {gm}){why}{}",
                        fmt_ns(size_ns),
                        during_burst
                    ));
                }
                Anomaly::Spike { residual_ns } => {
                    sm.spikes += 1;
                    notes.push("spike");
                    if let Some(n) = diag.may_log("spike") {
                        let more = if n > 0 {
                            format!(" (+{n} more since last report)")
                        } else {
                            String::new()
                        };
                        logs.push(format!(
                            "WARNING: time spike {} on a single Sync{more}",
                            fmt_ns(residual_ns)
                        ));
                    }
                }
                Anomaly::FreqChange { from_ppb, to_ppb } => {
                    sm.freq_changes += 1;
                    notes.push("freq-change");
                    diag.trigger("frequency change");
                    logs.push(format!(
                        "ALARM: master frequency changed {from_ppb:+.0} → {to_ppb:+.0} ppb{}",
                        during_burst
                    ));
                }
                Anomaly::Unstable { rms_ns } => {
                    notes.push("unstable");
                    diag.trigger("unstable");
                    logs.push(format!(
                        "ALARM: sync unstable — residual jitter {} RMS",
                        fmt_ns(rms_ns).trim_start_matches('+')
                    ))
                }
                Anomaly::Stable { rms_ns } => {
                    notes.push("stable");
                    logs.push(format!(
                        "Sync stable again — residual jitter {} RMS",
                        fmt_ns(rms_ns).trim_start_matches('+')
                    ))
                }
                Anomaly::Noisy { sigma_ns, jump_ns } => {
                    notes.push("noisy");
                    diag.trigger("noisy master");
                    logs.push(format!(
                        "WARNING: master time is noisy (σ {}) — learned anyway; spikes/steps \
                         now need {} (master time converted from the system clock, or \
                         software timestamps?)",
                        fmt_ns(sigma_ns).trim_start_matches('+'),
                        fmt_ns(jump_ns).trim_start_matches('+')
                    ));
                }
                Anomaly::Relearn { outliers } => {
                    sm.relearns += 1;
                    notes.push("relearn");
                    diag.trigger("erratic");
                    logs.push(format!(
                        "ALARM: master time erratic — {outliers} consecutive Syncs fit \
                         neither the drift model nor each other; relearning"
                    ));
                }
            }
        }
    }
    let was_in_burst = diag.in_burst();
    logs.extend(
        diag.on_sample(
            SampleRec {
                wall: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64()),
                seq: p.seq,
                local_ns: p.local_ns,
                origin_ns: origin.to_ns(),
                error_ns: analyzer.last_error,
                correction_ns: correction,
                link_delay_ns,
                fup_delay_us,
                rate_offset_ppb,
                rx_bps,
                link_errors,
                note: notes.join(";"),
                ..Default::default()
            },
            nominal_ns,
        ),
    );
    if was_in_burst && !diag.in_burst() {
        analyzer.rebaseline();
    }
    if let Some(e) = engine {
        let rx = match p.stamp {
            Some(s) if s.hardware => Rx::Phc(s.ts.to_ns()),
            _ => Rx::Realtime(clock.tai_to_realtime_ns(p.sys)),
        };
        let stepped = notes.contains(&"step");
        logs.extend(e.sample(
            master_ns,
            rx,
            utc_offset,
            analyzer.last_residual.is_some(),
            analyzer.holding(),
            stepped,
        ));
    }
    if check_system && system_offset.abs() > 0.5e9 {
        if let Some(n) = diag.may_log("system-offset") {
            let secs = (system_offset / 1e9).round();
            let hint = if (35.0..=40.0).contains(&secs.abs()) {
                " — a whole TAI−UTC offset: the system clock is set to TAI, or the master \
                 announces a wrong UTC offset"
            } else {
                ""
            };
            logs.push(format!(
                "ALARM: system clock is {} off the master's UTC{hint}{}",
                fmt_ns(system_offset),
                if n > 0 {
                    format!(" (+{n} more)")
                } else {
                    String::new()
                }
            ));
        }
    }
    for l in logs {
        st.log(l);
    }
}

/// The clock chain for --time-sync builtin: the receiving NIC's PHC (with
/// hardware timestamps), the system clock, and --sync-phc NICs.
fn setup_engine(cfg: &Config, sock: &RawSock) -> Result<SyncEngine> {
    use anyhow::Context;
    let primary = sock
        .phc()
        .map(|p| crate::phc::PhcClock::open(p.index).map(|c| (cfg.iface().to_string(), c)))
        .transpose()?;
    let mut extras = Vec::new();
    for iface in &cfg.sync_phc {
        let index = crate::net::phc_index(iface)?
            .with_context(|| format!("{iface} has no PTP hardware clock"))?;
        extras.push((iface.clone(), crate::phc::PhcClock::open(index)?));
    }
    let interval = log_to_duration(cfg.log_sync_interval).as_secs_f64();
    SyncEngine::new(primary, extras, interval).context("opening the clocks for adjustment")
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

    /// Deterministic Gaussian noise, ns.
    fn noise(i: i128, sigma: f64) -> i128 {
        let h = |k: i128| {
            let mut z = (i * 2 + k) as u64 ^ 0x9E37_79B9_7F4A_7C15;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64 + 1e-12
        };
        ((-2.0 * h(0).ln()).sqrt() * (2.0 * std::f64::consts::PI * h(1)).cos() * sigma) as i128
    }

    #[test]
    fn noisy_master_is_learned_and_reported_not_stuck() {
        let mut a = Analyzer::new(TH);
        let out = feed(&mut a, 0, 400, 3_000.0, |i| noise(i, 3_000.0));
        assert!(a.ready(), "stuck learning");
        let noisy: Vec<_> = out
            .iter()
            .filter(|(_, an)| matches!(an, Anomaly::Noisy { .. }))
            .collect();
        assert_eq!(noisy.len(), 1, "{out:?}");
        assert!(noisy[0].0 < 80, "learned only at sample {}", noisy[0].0);
        assert!(out
            .iter()
            .any(|(_, an)| matches!(an, Anomaly::Unstable { .. })));
        assert!(!out
            .iter()
            .any(|(_, an)| matches!(an, Anomaly::Step { .. } | Anomaly::Relearn { .. })));
        let sigma = a.noise_ns().unwrap();
        assert!((2_000.0..4_500.0).contains(&sigma), "σ {sigma}");
    }

    #[test]
    fn noisy_master_step_is_still_detected() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 200, 0.0, |i| noise(i, 3_000.0));
        let out = feed(&mut a, 200, 100, 0.0, |i| 200_000 + noise(i, 3_000.0));
        let steps: Vec<f64> = out
            .iter()
            .filter_map(|(_, an)| match an {
                Anomaly::Step { size_ns } => Some(*size_ns),
                _ => None,
            })
            .collect();
        assert_eq!(steps.len(), 1, "{out:?}");
        assert!((steps[0] - 200_000.0).abs() < 15_000.0, "{steps:?}");
    }

    #[test]
    fn clean_master_clears_the_noise_level() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 200, 0.0, |i| noise(i, 3_000.0));
        assert!(a.noise_ns().is_some());
        feed(&mut a, 200, 200, 0.0, |i| noise(i, 5.0));
        assert!(a.noise_ns().is_none());
        assert!(!a.unstable());
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
            (102, Anomaly::Step { size_ns }) => assert!((size_ns - 50_000.0).abs() < 10.0),
            other => panic!("unexpected {other:?}"),
        }
        assert!(a.last_residual.unwrap().abs() < 10.0);
    }

    #[test]
    fn huge_step_keeps_precision() {
        let mut a = Analyzer::new(TH);
        // GM jumps by ~56 years (e.g. a master that lost its time).
        let years: i128 = 56 * 365 * 86_400 * 1_000_000_000;
        let ev = feed(
            &mut a,
            0,
            300,
            -800.0,
            |i| if i >= 100 { -years } else { 0 },
        );
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
            ev.iter()
                .any(|(_, e)| matches!(e, Anomaly::FreqChange { .. })),
            "{ev:?}"
        );
        // A ramp is not a time step.
        assert!(
            !ev.iter().any(|(_, e)| matches!(e, Anomaly::Step { .. })),
            "{ev:?}"
        );
        assert!(
            (a.drift_ppb().unwrap() - 2_000.0).abs() < 50.0,
            "drift {:?} {ev:?}",
            a.drift_ppb()
        );
    }

    #[test]
    fn jitter_raises_and_clears_unstable() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 100, 0.0, |_| 0);
        // ±600 ns alternating noise: RMS 600 > 250 but each sample within 1 µs.
        let ev = feed(
            &mut a,
            100,
            100,
            0.0,
            |i| if i % 2 == 0 { 600 } else { -600 },
        );
        assert!(
            ev.iter()
                .any(|(_, e)| matches!(e, Anomaly::Unstable { .. })),
            "{ev:?}"
        );
        assert!(a.unstable());
        let ev = feed(&mut a, 200, 100, 0.0, |_| 0);
        assert!(
            ev.iter().any(|(_, e)| matches!(e, Anomaly::Stable { .. })),
            "{ev:?}"
        );
    }

    #[test]
    fn erratic_samples_trigger_relearn_and_recover() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 100, 0.0, |_| 0);
        // Wildly alternating garbage, then a clean master 5 s off.
        let ev = feed(&mut a, 100, 20, 0.0, |i| (i % 3 + 1) * 400_000_000);
        assert!(
            ev.iter().any(|(_, e)| matches!(e, Anomaly::Relearn { .. })),
            "{ev:?}"
        );
        let ev = feed(&mut a, 120, 100, 300.0, |_| 5_000_000_000);
        assert!(ev.is_empty(), "{ev:?}");
        assert!(a.ready() && a.last_residual.unwrap().abs() < 10.0);
    }

    #[test]
    fn rebaseline_defers_frequency_checks() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 100, 0.0, |_| 0);
        a.rebaseline();
        // A rate change (+300 ppb, phase-continuous) right after the
        // rebaseline becomes the new reference instead of being reported
        // against the stale one.
        let ev = feed(&mut a, 100, 100, 0.0, |i| (i - 100) * 38);
        assert!(
            !ev.iter()
                .any(|(_, e)| matches!(e, Anomaly::FreqChange { .. })),
            "{ev:?}"
        );
        assert!(
            (a.ref_drift.unwrap() - 300.0).abs() < 20.0,
            "{:?}",
            a.ref_drift
        );
    }

    #[test]
    fn hold_keeps_model_through_jitter_and_still_sees_steps() {
        let mut a = Analyzer::new(TH);
        feed(&mut a, 0, 100, 500.0, |_| 0);
        a.set_hold(Some(1_500.0));
        // ±2 µs uniform-ish jitter, around a 2 µs extra delay: all outside
        // the 1 µs jump threshold, never three within 250 ns of each other.
        let ev = feed(&mut a, 100, 40, 500.0, |i| -2_000 - (i * 977 % 2_000));
        assert!(ev.is_empty(), "{ev:?}");
        // Disturbed samples that agree but stay within 4x the burst level
        // are not a step.
        let ev = feed(&mut a, 140, 6, 500.0, |_| -5_500);
        assert!(ev.is_empty(), "{ev:?}");
        // A real step during the hold is still found.
        let ev = feed(&mut a, 146, 10, 500.0, |_| 80_000);
        assert!(matches!(ev[..], [(_, Anomaly::Step { .. })]), "{ev:?}");
        a.set_hold(None);
        // Model intact: clean samples (with the step) fit immediately.
        let ev = feed(&mut a, 156, 50, 500.0, |_| 80_000);
        assert!(ev.is_empty(), "{ev:?}");
        assert!(a.last_residual.unwrap().abs() < 10.0);
    }

    fn blame_after(origin_step: i128, corr_step: i128, stale_origin: bool) -> String {
        let mut b = StepBlame::default();
        let (bridge, other) = ([1, 2, 3, 4, 5, 6], [9, 9, 9, 9, 9, 9]);
        for i in 0..10i128 {
            let local = i * 125_000_000;
            // A stale origin: frozen, the bridge's correction covers the gap.
            let (mut origin, mut corr) = if stale_origin {
                (
                    1_000_000_000_000,
                    local + 40_000_000_000_000 - 1_000_000_000_000,
                )
            } else {
                (local + 40_000_000_000_000, 500)
            };
            if i >= 7 {
                origin += origin_step;
                corr += corr_step;
            }
            b.push(
                local,
                origin,
                corr as i64,
                if i >= 7 { other } else { bridge },
            );
        }
        b.explain((origin_step + corr_step) as f64)
    }

    #[test]
    fn step_is_blamed_on_the_part_that_moved() {
        let s = blame_after(0, 22_003_056_000, true);
        assert!(s.contains("correctionField (a bridge)"), "{s}");
        assert!(s.contains("sender 09:09:09:09:09:09"), "{s}");
        assert!(s.contains("sender changed from 01:02:03:04:05:06"), "{s}");
        let s = blame_after(-1_130_206_958_000, 0, false);
        assert!(s.contains("in the origin timestamp"), "{s}");
        let s = blame_after(1_000_000, 2_000_000, false);
        assert!(s.contains("origin timestamp moved"), "{s}");
    }

    #[test]
    fn fmt_ns_units() {
        assert_eq!(fmt_ns(-12.0), "-12 ns");
        assert_eq!(fmt_ns(1_500.0), "+1.50 µs");
        assert_eq!(fmt_ns(2_500_000.0), "+2.500 ms");
        assert_eq!(fmt_ns(-3e9), "-3.000000 s");
    }
}
