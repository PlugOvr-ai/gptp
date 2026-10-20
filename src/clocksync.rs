//! Built-in time sync (`--monitor --time-sync builtin`): what ptp4l + phc2sys
//! do, in one process.
//!
//! With hardware timestamps the receiving NIC's hardware clock (PHC) is
//! disciplined to the master directly — master time and receive stamps are
//! both in its timebase, the most accurate path. The system clock and any
//! further NIC clocks (`--sync-phc`) then follow that PHC via kernel
//! cross-timestamps, like phc2sys. With software timestamps the system
//! clock is disciplined directly.
//!
//! PHCs carry PTP time (TAI); the system clock gets UTC, converted with the
//! offset the master announces (if marked valid and plausible, else
//! `--utc-offset`). The kernel learns the TAI offset and that the clock is
//! synchronized, so `timedatectl`, `adjtimex` and CLOCK_TAI users see a
//! correctly configured system.
//!
//! Each clock has a PI servo with linuxptp's default gains, scaled to the
//! Sync interval. Steps: at start, when the monitor classifies a master
//! time step, and for offsets beyond 128 ms. Samples the monitor rejects
//! (spikes, step candidates) and jitter bursts are not used — the clocks
//! coast on their last frequency (holdover). A master time before 2020 or
//! after 2100 is never followed.
//!
//! The monitor measures the master against the raw PHC. Disciplining that
//! PHC would make it measure a clock that is steered toward the master, so
//! every adjustment is kept in a ledger (steps, and frequency changes timed
//! in the PHC's own timebase) and the monitor gets the reconstructed
//! free-running time instead.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crate::monitor::fmt_ns;
use crate::phc::PhcClock;

/// The kernel's limit for the system clock, ppb.
const SYSTEM_MAX_FREQ_PPB: f64 = 500_000.0;
/// Offsets beyond this are stepped, not slewed.
const PANIC_STEP_NS: f64 = 128e6;
/// Samples stamped before a step are skipped.
const SKIP_AFTER_STEP: u32 = 2;
/// Plausible master UTC range: 2020-01-01 .. 2100-01-01.
const PLAUSIBLE_UTC: std::ops::Range<i64> = 1_577_836_800..4_102_444_800;
const IMPLAUSIBLE_LOG_INTERVAL: Duration = Duration::from_secs(60);
/// Frequency changes remembered for reconstructing free-running time.
const LEDGER_LEN: usize = 64;

/// A clock the servo can steer; real or simulated in tests.
pub trait SystemClock {
    fn step_ns(&mut self, ns: i128) -> io::Result<()>;
    fn freq_ppb(&mut self) -> io::Result<f64>;
    fn set_freq_ppb(&mut self, ppb: f64) -> io::Result<()>;
    /// Mark the clock synchronized with these error bounds (system clock).
    fn mark_synced(&mut self, _max_error_ns: f64, _est_error_ns: f64) -> io::Result<()> {
        Ok(())
    }
    /// Tell the kernel TAI − UTC (system clock).
    fn set_tai_offset(&mut self, _secs: i32) -> io::Result<()> {
        Ok(())
    }
}

/// The system clock via adjtimex (needs CAP_SYS_TIME to write).
pub struct Adjtimex;

impl Adjtimex {
    fn call(tx: &mut libc::timex) -> io::Result<libc::c_int> {
        let rc = unsafe { libc::adjtimex(tx) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc)
        }
    }

    fn read() -> io::Result<libc::timex> {
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        Self::call(&mut tx)?;
        Ok(tx)
    }
}

fn step_timex(ns: i128) -> libc::timex {
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    tx.modes = libc::ADJ_SETOFFSET | libc::ADJ_NANO;
    // A normalized timespec: whole seconds rounded down, ns in 0..1e9.
    tx.time.tv_sec = ns.div_euclid(1_000_000_000) as _;
    tx.time.tv_usec = ns.rem_euclid(1_000_000_000) as _;
    tx
}

fn freq_timex(ppb: f64) -> libc::timex {
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    tx.modes = libc::ADJ_FREQUENCY;
    // ppm with a 16-bit fraction
    tx.freq = (ppb * 65.536).round() as _;
    tx
}

impl SystemClock for Adjtimex {
    fn step_ns(&mut self, ns: i128) -> io::Result<()> {
        Self::call(&mut step_timex(ns)).map(|_| ())
    }

    fn freq_ppb(&mut self) -> io::Result<f64> {
        Ok(Self::read()?.freq as f64 / 65.536)
    }

    fn set_freq_ppb(&mut self, ppb: f64) -> io::Result<()> {
        Self::call(&mut freq_timex(ppb)).map(|_| ())
    }

    fn mark_synced(&mut self, max_error_ns: f64, est_error_ns: f64) -> io::Result<()> {
        let status = Self::read()?.status;
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        tx.modes = libc::ADJ_STATUS | libc::ADJ_MAXERROR | libc::ADJ_ESTERROR;
        tx.status = status & !libc::STA_UNSYNC;
        // Both in µs. The kernel grows maxerror by 500 ppm by itself and
        // marks the clock unsynchronized once we stop updating it.
        tx.maxerror = (max_error_ns / 1e3).ceil().max(1.0) as _;
        tx.esterror = (est_error_ns / 1e3).ceil().max(1.0) as _;
        Self::call(&mut tx).map(|_| ())
    }

    fn set_tai_offset(&mut self, secs: i32) -> io::Result<()> {
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        tx.modes = libc::ADJ_TAI;
        tx.constant = secs as _;
        Self::call(&mut tx).map(|_| ())
    }
}

/// Adjustments applied to a PHC, to undo them on its readings.
struct Ledger {
    /// Frequency the PHC had before we touched it, ppb (our reference).
    base_freq: f64,
    /// (PHC time just after the change, cumulative steps, cumulative
    /// frequency gain at that time, frequency from then on)
    changes: VecDeque<(i128, i128, f64, f64)>,
}

impl Ledger {
    fn new(now: i128, freq: f64) -> Self {
        Ledger {
            base_freq: freq,
            changes: VecDeque::from([(now, 0, 0.0, freq)]),
        }
    }

    fn last(&self) -> (i128, i128, f64, f64) {
        *self.changes.back().unwrap()
    }

    fn push(&mut self, c: (i128, i128, f64, f64)) {
        self.changes.push_back(c);
        if self.changes.len() > LEDGER_LEN {
            self.changes.pop_front();
        }
    }

    fn gain_until(&self, c: (i128, i128, f64, f64), t: i128) -> f64 {
        c.2 + (c.3 - self.base_freq) * (t - c.0) as f64 / 1e9
    }

    fn record_freq(&mut self, now: i128, freq: f64) {
        let last = self.last();
        let gained = self.gain_until(last, now);
        self.push((now, last.1, gained, freq));
    }

    /// `now_after`: PHC time read right after stepping by `ns`.
    fn record_step(&mut self, now_after: i128, ns: i128) {
        let last = self.last();
        let gained = self.gain_until(last, now_after - ns);
        self.push((now_after, last.1 + ns, gained, last.3));
    }

    /// How far our adjustments moved a PHC reading `t`, ns.
    fn applied(&self, t: i128) -> f64 {
        let c = self
            .changes
            .iter()
            .rev()
            .find(|c| c.0 <= t)
            .or(self.changes.front())
            .copied()
            .unwrap();
        c.1 as f64 + self.gain_until(c, t)
    }
}

/// A NIC hardware clock steered via clock_adjtime.
pub struct PhcTarget {
    pub phc: PhcClock,
    ledger: Ledger,
}

impl PhcTarget {
    pub fn new(phc: PhcClock) -> io::Result<Self> {
        let mut tx: libc::timex = unsafe { std::mem::zeroed() };
        phc.adjtime(&mut tx)?;
        let freq = tx.freq as f64 / 65.536;
        let now = phc.now_ns()?;
        Ok(PhcTarget {
            phc,
            ledger: Ledger::new(now, freq),
        })
    }

    /// The PHC reading `t` with our adjustments undone.
    pub fn free_running_ns(&self, t: i128) -> i128 {
        t - self.ledger.applied(t).round() as i128
    }
}

impl SystemClock for PhcTarget {
    fn step_ns(&mut self, ns: i128) -> io::Result<()> {
        self.phc.adjtime(&mut step_timex(ns))?;
        self.ledger.record_step(self.phc.now_ns()?, ns);
        Ok(())
    }

    fn freq_ppb(&mut self) -> io::Result<f64> {
        Ok(self.ledger.last().3)
    }

    fn set_freq_ppb(&mut self, ppb: f64) -> io::Result<()> {
        self.phc.adjtime(&mut freq_timex(ppb))?;
        self.ledger.record_freq(self.phc.now_ns()?, ppb);
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServoState {
    /// No usable sample yet.
    Waiting,
    Locked,
    /// Disturbed samples (jitter burst, spikes) or implausible master
    /// time: coasting on the last frequency.
    Holdover,
    /// Not allowed to adjust the clock (permissions, conflicting daemon).
    Disabled,
}

impl ServoState {
    pub fn label(&self) -> &'static str {
        match self {
            ServoState::Waiting => "waiting",
            ServoState::Locked => "locked",
            ServoState::Holdover => "holdover",
            ServoState::Disabled => "disabled",
        }
    }
}

/// One measurement handed to a servo.
pub struct ServoInput {
    /// Reference time minus this clock at the same instant, ns.
    pub offset_ns: f64,
    /// Reference time as UTC seconds, for the plausibility check.
    pub master_utc_secs: i64,
    /// TAI − UTC, s (given to the kernel by the system clock).
    pub utc_offset: i32,
    /// The monitor accepted this sample (not a spike / step candidate).
    pub clean: bool,
    /// A jitter burst is in progress.
    pub hold: bool,
    /// The monitor classified a master time step at this sample.
    pub master_stepped: bool,
}

pub struct Servo<C: SystemClock> {
    pub name: String,
    clock: C,
    kp: f64,
    ki: f64,
    max_freq: f64,
    /// Integral term: the clock's frequency error estimate, ppb.
    drift_ppb: f64,
    pub state: ServoState,
    first: bool,
    skip: u32,
    pub steps: u64,
    pub last_offset_ns: Option<f64>,
    pub freq_ppb: f64,
    step_threshold_ns: f64,
    tai_set: Option<i32>,
    implausible_logged: Option<Instant>,
    pub disabled_reason: Option<String>,
}

impl<C: SystemClock> Servo<C> {
    /// `low_noise`: the measurements are hardware timestamps or kernel
    /// cross-timestamps (stiffer loop); `interval_s`: update interval.
    pub fn new(
        name: String,
        mut clock: C,
        low_noise: bool,
        interval_s: f64,
        max_freq: f64,
    ) -> Self {
        // linuxptp's PI defaults, scaled to the update interval.
        let (kp_scale, ki_scale) = if low_noise { (0.7, 0.3) } else { (0.1, 0.001) };
        let t = interval_s.max(1e-3);
        let kp = (kp_scale * t.powf(-0.3)).min(0.7 / t);
        let ki = (ki_scale * t.powf(0.4)).min(0.3 / t);
        let drift_ppb = clock.freq_ppb().unwrap_or(0.0);
        Servo {
            name,
            clock,
            kp,
            ki,
            max_freq,
            drift_ppb,
            state: ServoState::Waiting,
            first: true,
            skip: 0,
            steps: 0,
            last_offset_ns: None,
            freq_ppb: drift_ppb,
            // linuxptp's first_step_threshold for hardware stamps; software
            // stamps are too noisy for 20 µs.
            step_threshold_ns: if low_noise { 20e3 } else { 500e3 },
            tai_set: None,
            implausible_logged: None,
            disabled_reason: None,
        }
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    /// Locked now, or locked before and coasting.
    pub fn has_locked(&self) -> bool {
        matches!(self.state, ServoState::Locked | ServoState::Holdover)
    }

    pub fn disable(&mut self, reason: String) {
        self.state = ServoState::Disabled;
        self.disabled_reason = Some(reason);
    }

    /// Re-enable after a conflict went away; the next sample relocks.
    pub fn enable(&mut self) {
        if self.state == ServoState::Disabled {
            self.state = ServoState::Waiting;
            self.disabled_reason = None;
            self.first = true;
        }
    }

    /// Feed one measurement; returns log lines.
    pub fn sample(&mut self, inp: &ServoInput) -> Vec<String> {
        let mut logs = Vec::new();
        if self.state == ServoState::Disabled {
            return logs;
        }
        if !PLAUSIBLE_UTC.contains(&inp.master_utc_secs) {
            if self
                .implausible_logged
                .is_none_or(|t| t.elapsed() >= IMPLAUSIBLE_LOG_INTERVAL)
            {
                logs.push(format!(
                    "ALARM: master time ({} UTC) is implausible — {} not adjusted",
                    crate::syncdiag::fmt_utc_date(inp.master_utc_secs),
                    self.name
                ));
                self.implausible_logged = Some(Instant::now());
            }
            if self.state == ServoState::Locked {
                self.state = ServoState::Holdover;
            }
            return logs;
        }
        if self.tai_set != Some(inp.utc_offset) {
            if let Err(e) = self.clock.set_tai_offset(inp.utc_offset) {
                return self.fail(e, logs);
            }
            self.tai_set = Some(inp.utc_offset);
        }
        if self.skip > 0 {
            self.skip -= 1;
            return logs;
        }

        let off = inp.offset_ns;
        let step_now = (self.first && off.abs() > self.step_threshold_ns)
            || inp.master_stepped
            || (off.abs() > PANIC_STEP_NS && inp.clean);
        if step_now {
            if let Err(e) = self.clock.step_ns(off.round() as i128) {
                return self.fail(e, logs);
            }
            self.steps += 1;
            self.skip = SKIP_AFTER_STEP;
            self.first = false;
            self.state = ServoState::Locked;
            self.last_offset_ns = Some(off);
            logs.push(format!("{} stepped by {}", self.name, fmt_ns(off)));
            return logs;
        }
        self.first = false;
        if inp.hold || !inp.clean {
            if self.state == ServoState::Locked {
                self.state = ServoState::Holdover;
            }
            return logs;
        }

        // PI: positive offset = clock behind = run it faster.
        self.drift_ppb = (self.drift_ppb + self.ki * off).clamp(-self.max_freq, self.max_freq);
        let ppb = (self.kp * off + self.drift_ppb).clamp(-self.max_freq, self.max_freq);
        if let Err(e) = self.clock.set_freq_ppb(ppb) {
            return self.fail(e, logs);
        }
        if let Err(e) = self.clock.mark_synced(2.0 * off.abs() + 1_000.0, off.abs()) {
            return self.fail(e, logs);
        }
        if self.state != ServoState::Locked {
            logs.push(format!(
                "{} locked (offset {}, frequency {:+.0} ppb)",
                self.name,
                fmt_ns(off),
                ppb
            ));
        }
        self.state = ServoState::Locked;
        self.freq_ppb = ppb;
        self.last_offset_ns = Some(off);
        logs
    }

    fn fail(&mut self, e: io::Error, mut logs: Vec<String>) -> Vec<String> {
        let reason = if e.raw_os_error() == Some(libc::EPERM) {
            "needs CAP_SYS_TIME (run as root)".to_string()
        } else {
            format!("adjusting failed: {e}")
        };
        logs.push(format!(
            "ALARM: time sync of {} disabled — {reason}",
            self.name
        ));
        self.disable(reason);
        logs
    }
}

/// How the Sync that drives the time sync was received.
pub enum Rx {
    /// Hardware stamp, raw PHC time, ns.
    Phc(i128),
    /// Software stamp, CLOCK_REALTIME, ns.
    Realtime(i128),
}

/// Per-clock status for the UI.
#[derive(Clone)]
pub struct ClockView {
    pub name: String,
    pub state: &'static str,
    pub offset_ns: Option<f64>,
    pub freq_ppb: f64,
    pub steps: u64,
    pub note: Option<String>,
}

/// The chain master → (receiving NIC clock →) system clock + further NIC
/// clocks.
pub struct SyncEngine {
    primary: Option<Servo<PhcTarget>>,
    system: Servo<Adjtimex>,
    extras: Vec<Servo<PhcTarget>>,
}

fn phc_servo(name: String, phc: PhcClock, interval_s: f64) -> io::Result<Servo<PhcTarget>> {
    let max = phc.max_adj_ppb().unwrap_or(SYSTEM_MAX_FREQ_PPB);
    Ok(Servo::new(
        name,
        PhcTarget::new(phc)?,
        true,
        interval_s,
        max,
    ))
}

impl SyncEngine {
    /// `primary`: the receiving NIC's PHC when hardware timestamps are used;
    /// `extras`: further NIC clocks to keep in sync.
    pub fn new(
        primary: Option<(String, PhcClock)>,
        extras: Vec<(String, PhcClock)>,
        interval_s: f64,
    ) -> io::Result<Self> {
        let hardware = primary.is_some();
        let primary = primary
            .map(|(iface, phc)| {
                let name = format!("NIC clock {iface} ({})", phc.device);
                phc_servo(name, phc, interval_s)
            })
            .transpose()?;
        let extras = extras
            .into_iter()
            .map(|(iface, phc)| {
                let name = format!("NIC clock {iface} ({})", phc.device);
                phc_servo(name, phc, interval_s)
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(SyncEngine {
            primary,
            // With a primary PHC the system clock follows it via kernel
            // cross-timestamps: low noise.
            system: Servo::new(
                "System clock".into(),
                Adjtimex,
                hardware,
                interval_s,
                SYSTEM_MAX_FREQ_PPB,
            ),
            extras,
        })
    }

    /// A raw PHC reading of the receiving NIC with our adjustments undone.
    pub fn free_running_ns(&self, phc_ns: i128) -> i128 {
        self.primary
            .as_ref()
            .map_or(phc_ns, |p| p.clock().free_running_ns(phc_ns))
    }

    fn servos(&mut self) -> Vec<&mut dyn ServoControl> {
        let mut v: Vec<&mut dyn ServoControl> = Vec::new();
        if let Some(p) = &mut self.primary {
            v.push(p);
        }
        v.push(&mut self.system);
        for x in &mut self.extras {
            v.push(x);
        }
        v
    }

    pub fn disable(&mut self, reason: &str) {
        for s in self.servos() {
            s.disable_with(reason.to_string());
        }
    }

    pub fn enable(&mut self) {
        for s in self.servos() {
            s.enable_again();
        }
    }

    pub fn disabled(&self) -> bool {
        self.system.state == ServoState::Disabled
    }

    /// One Sync measurement of the master (PTP time, ns).
    #[allow(clippy::too_many_arguments)]
    pub fn sample(
        &mut self,
        master_tai_ns: i128,
        rx: Rx,
        utc_offset: i32,
        clean: bool,
        hold: bool,
        stepped: bool,
    ) -> Vec<String> {
        let mut logs = Vec::new();
        let utc_ns = utc_offset as i128 * 1_000_000_000;
        let utc_secs = |tai_ns: i128| ((tai_ns - utc_ns) / 1_000_000_000) as i64;
        match (&mut self.primary, rx) {
            (Some(p), Rx::Phc(t)) => {
                logs.extend(p.sample(&ServoInput {
                    offset_ns: (master_tai_ns - t) as f64,
                    master_utc_secs: utc_secs(master_tai_ns),
                    utc_offset,
                    clean,
                    hold,
                    master_stepped: stepped,
                }));
                if !p.has_locked() {
                    return logs;
                }
                let Some((phc, rt)) = p.clock().phc.sample_offset_pair() else {
                    return logs;
                };
                logs.extend(self.system.sample(&ServoInput {
                    offset_ns: (phc - utc_ns - rt) as f64,
                    master_utc_secs: utc_secs(phc),
                    utc_offset,
                    clean: true,
                    hold: false,
                    master_stepped: false,
                }));
                for x in &mut self.extras {
                    if let Some((px, rtx)) = x.clock().phc.sample_offset_pair() {
                        logs.extend(x.sample(&ServoInput {
                            offset_ns: ((phc - rt) - (px - rtx)) as f64,
                            master_utc_secs: utc_secs(phc),
                            utc_offset,
                            clean: true,
                            hold: false,
                            master_stepped: false,
                        }));
                    }
                }
            }
            (None, Rx::Realtime(rt_rx)) => {
                logs.extend(self.system.sample(&ServoInput {
                    offset_ns: (master_tai_ns - utc_ns - rt_rx) as f64,
                    master_utc_secs: utc_secs(master_tai_ns),
                    utc_offset,
                    clean,
                    hold,
                    master_stepped: stepped,
                }));
                if !self.system.has_locked() {
                    return logs;
                }
                for x in &mut self.extras {
                    if let Some((px, rtx)) = x.clock().phc.sample_offset_pair() {
                        logs.extend(x.sample(&ServoInput {
                            // PHCs carry TAI: system UTC + offset.
                            offset_ns: (rtx + utc_ns - px) as f64,
                            master_utc_secs: utc_secs(master_tai_ns),
                            utc_offset,
                            clean: true,
                            hold: false,
                            master_stepped: false,
                        }));
                    }
                }
            }
            // Timestamp kind changed (e.g. a frame without a hardware
            // stamp): never mix timebases.
            _ => {}
        }
        logs
    }

    pub fn views(&self) -> Vec<ClockView> {
        let mut v = Vec::new();
        if let Some(p) = &self.primary {
            v.push(view(p));
        }
        v.push(view(&self.system));
        v.extend(self.extras.iter().map(view));
        v
    }
}

fn view<C: SystemClock>(s: &Servo<C>) -> ClockView {
    ClockView {
        name: s.name.clone(),
        state: s.state.label(),
        offset_ns: s.last_offset_ns,
        freq_ppb: s.freq_ppb,
        steps: s.steps,
        note: s.disabled_reason.clone(),
    }
}

/// Object-safe enable/disable over servos of different clock types.
trait ServoControl {
    fn disable_with(&mut self, reason: String);
    fn enable_again(&mut self);
}

impl<C: SystemClock> ServoControl for Servo<C> {
    fn disable_with(&mut self, reason: String) {
        self.disable(reason);
    }
    fn enable_again(&mut self) {
        self.enable();
    }
}

/// Processes that discipline clocks themselves and would fight the servo
/// (comm names; systemd-timesyncd is truncated to 15 characters).
pub const CONFLICTING_DAEMONS: &[&str] = &[
    "chronyd",
    "ntpd",
    "ntpsec",
    "openntpd",
    "systemd-timesyn",
    "phc2sys",
    "ptp4l",
    "timemaster",
];

/// Running processes from `names` (by /proc/<pid>/comm), with their pids.
pub fn running(names: &[&str]) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            let comm = comm.trim();
            if names.contains(&comm) {
                // comm is cut to 15 characters
                let name = if comm == "systemd-timesyn" {
                    "systemd-timesyncd"
                } else {
                    comm
                };
                found.push((name.to_string(), pid));
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock with a fixed crystal error, steered by the servo.
    struct SimClock {
        /// clock − true time, ns
        error_ns: f64,
        /// crystal error, ppb (runs fast if positive)
        crystal_ppb: f64,
        adj_ppb: f64,
        steps: Vec<i128>,
        synced: bool,
        tai: Option<i32>,
        deny: bool,
    }

    impl SimClock {
        fn new(error_ns: f64, crystal_ppb: f64) -> Self {
            SimClock {
                error_ns,
                crystal_ppb,
                adj_ppb: 0.0,
                steps: Vec::new(),
                synced: false,
                tai: None,
                deny: false,
            }
        }

        fn advance(&mut self, secs: f64) {
            self.error_ns += (self.crystal_ppb + self.adj_ppb) * secs;
        }

        fn check(&self) -> io::Result<()> {
            if self.deny {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            } else {
                Ok(())
            }
        }
    }

    impl SystemClock for SimClock {
        fn step_ns(&mut self, ns: i128) -> io::Result<()> {
            self.check()?;
            self.error_ns += ns as f64;
            self.steps.push(ns);
            Ok(())
        }
        fn freq_ppb(&mut self) -> io::Result<f64> {
            Ok(self.adj_ppb)
        }
        fn set_freq_ppb(&mut self, ppb: f64) -> io::Result<()> {
            self.check()?;
            self.adj_ppb = ppb;
            Ok(())
        }
        fn mark_synced(&mut self, _: f64, _: f64) -> io::Result<()> {
            self.check()?;
            self.synced = true;
            Ok(())
        }
        fn set_tai_offset(&mut self, secs: i32) -> io::Result<()> {
            self.check()?;
            self.tai = Some(secs);
            Ok(())
        }
    }

    const T: f64 = 0.125;
    const NOW: i64 = 1_792_000_000;

    fn servo(sim: SimClock, low_noise: bool) -> Servo<SimClock> {
        Servo::new("Sim clock".into(), sim, low_noise, T, SYSTEM_MAX_FREQ_PPB)
    }

    fn input(s: &Servo<SimClock>, noise_ns: f64) -> ServoInput {
        ServoInput {
            offset_ns: -s.clock().error_ns + noise_ns,
            master_utc_secs: NOW,
            utc_offset: 37,
            clean: true,
            hold: false,
            master_stepped: false,
        }
    }

    fn tick(s: &mut Servo<SimClock>, noise_ns: f64) -> Vec<String> {
        let inp = input(s, noise_ns);
        let logs = s.sample(&inp);
        s.clock.advance(T);
        logs
    }

    #[test]
    fn steps_once_then_locks_and_tracks_a_drifting_crystal() {
        // 3.2 s off, crystal 40 ppm fast, ±30 ns timestamp noise.
        let mut s = servo(SimClock::new(3.2e9, 40_000.0), true);
        let mut late = 0.0f64;
        for i in 0..800 {
            tick(&mut s, ((i * 37 % 21) as f64 - 10.0) * 3.0);
            if i >= 600 {
                late = late.max(s.clock().error_ns.abs());
            }
        }
        assert_eq!(s.steps, 1);
        assert_eq!(s.state, ServoState::Locked);
        assert_eq!(s.clock().tai, Some(37));
        assert!(s.clock().synced);
        assert!(late < 200.0, "residual error {late} ns");
        assert!((s.clock().adj_ppb + 40_000.0).abs() < 500.0);
    }

    #[test]
    fn software_gains_converge_without_oscillating() {
        // ±20 µs software timestamp noise, 25 ppm crystal.
        let mut s = servo(SimClock::new(0.0, 25_000.0), false);
        let mut late = 0.0f64;
        for i in 0..4000 {
            tick(&mut s, ((i * 37 % 21) as f64 - 10.0) * 2_000.0);
            if i >= 3000 {
                late = late.max(s.clock().error_ns.abs());
            }
        }
        assert!(late < 50_000.0, "residual error {late} ns");
    }

    #[test]
    fn holdover_keeps_frequency_and_a_master_step_is_followed() {
        let mut s = servo(SimClock::new(0.0, 10_000.0), true);
        for _ in 0..400 {
            tick(&mut s, 0.0);
        }
        let freq = s.clock().adj_ppb;
        // Jitter burst: wildly disturbed samples, marked as hold.
        for i in 0..40 {
            let mut inp = input(&s, if i % 2 == 0 { 2e6 } else { -2e6 });
            inp.hold = true;
            s.sample(&inp);
            s.clock.advance(T);
        }
        assert_eq!(s.state, ServoState::Holdover);
        assert_eq!(s.clock().adj_ppb, freq);
        assert!(s.clock().error_ns.abs() < 1_000.0);
        // The master jumps by +1 s: the clock follows with a step.
        s.clock.error_ns -= 1e9;
        let mut inp = input(&s, 0.0);
        inp.master_stepped = true;
        let logs = s.sample(&inp);
        assert!(logs.iter().any(|l| l.contains("stepped")), "{logs:?}");
        assert!(s.clock().error_ns.abs() < 1_000.0);
    }

    #[test]
    fn implausible_master_time_is_not_followed() {
        let mut s = servo(SimClock::new(0.0, 0.0), true);
        let mut inp = input(&s, 5e9);
        inp.master_utc_secs = 86_400; // 1970-01-02
        let logs = s.sample(&inp);
        assert!(logs[0].contains("implausible"), "{logs:?}");
        assert!(s.clock().steps.is_empty());
        assert_eq!(s.clock().tai, None);
    }

    #[test]
    fn missing_permission_disables_with_a_clear_reason() {
        let mut sim = SimClock::new(1e9, 0.0);
        sim.deny = true;
        let mut s = servo(sim, true);
        let logs = tick(&mut s, 0.0);
        assert_eq!(s.state, ServoState::Disabled);
        assert!(logs[0].contains("CAP_SYS_TIME"), "{logs:?}");
    }

    #[test]
    fn ledger_reconstructs_free_running_time() {
        // PHC starts at 1000 s with a pre-existing +5 ppm adjustment.
        let s = 1_000_000_000i128;
        let mut l = Ledger::new(1000 * s, 5_000.0);
        assert_eq!(l.applied(1001 * s), 0.0);
        // At 1010 s we set +105 ppm (i.e. +100 ppm vs. the reference).
        l.record_freq(1010 * s, 105_000.0);
        // 10 s later the PHC gained 1 ms over the reference.
        assert!((l.applied(1020 * s) - 1_000_000.0).abs() < 1.0);
        // A +2 s step at PHC time 1030 s (1028 s before the step).
        l.record_step(1030 * s, 2 * s);
        let gain_at_step = 100_000.0 * 18.0; // 18 s at +100 ppm
        assert!((l.applied(1030 * s) - (2e9 + gain_at_step)).abs() < 1.0);
        // Readings before a change still use the old segment.
        assert!((l.applied(1005 * s)).abs() < 1.0);
    }

    #[test]
    fn adjtimex_can_be_read_without_privileges() {
        // Read-only query; never adjusts the clock running these tests.
        assert!(Adjtimex::read().is_ok());
    }
}
