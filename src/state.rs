//! Status shared between the grandmaster engine and the UI.

use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::ptp::AnnounceData;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Actively sending Announce + Sync/Follow_Up.
    ActiveGm,
    /// Handed over (or manually stopped): silent except peer-delay responses.
    Passive,
    /// Slave monitor (--monitor): follows the best master and watches its
    /// time for jumps and instability, without adjusting any clock.
    Monitor,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SyncState {
    /// No Announce from any master.
    NoMaster,
    /// Master known, collecting samples for the drift model.
    Learning,
    Locked,
    /// Residual jitter above threshold.
    Unstable,
    /// Master announced but Sync/Follow_Up stopped arriving.
    SyncLost,
}

impl SyncState {
    pub fn label(&self) -> &'static str {
        match self {
            SyncState::NoMaster => "no-master",
            SyncState::Learning => "learning",
            SyncState::Locked => "locked",
            SyncState::Unstable => "unstable",
            SyncState::SyncLost => "sync-lost",
        }
    }
}

/// Slave-monitor view of the selected master's time.
///
/// "Offset" is master time minus local time; the local reference is a
/// free-running timebase (raw PHC or CLOCK_MONOTONIC_RAW), so a constant
/// frequency difference shows up as `drift_ppb` and everything else —
/// steps, spikes, wander — as the residual of a linear fit.
#[derive(Clone)]
pub struct SyncMonitor {
    pub state: SyncState,
    pub master_port: Option<[u8; 10]>,
    pub gm: Option<AnnounceData>,
    pub sync_interval_ms: Option<f64>,
    pub sync_rx: u64,
    pub follow_up_rx: u64,
    /// Syncs lost according to sequence-number gaps.
    pub missing_syncs: u64,
    /// Two-step Syncs whose Follow_Up never arrived.
    pub missing_follow_ups: u64,
    pub sync_timeouts: u64,
    pub gm_changes: u64,
    pub time_base_changes: u64,
    pub steps: u64,
    pub spikes: u64,
    pub freq_changes: u64,
    /// Drift model discarded because the master's time was erratic.
    pub relearns: u64,
    /// Residual of the last sample against the drift model, ns.
    pub residual_ns: Option<f64>,
    /// RMS of the residuals in the fit window, ns.
    pub rms_ns: Option<f64>,
    /// Largest |residual| since start (steps excluded), ns.
    pub peak_ns: f64,
    pub last_step_ns: Option<f64>,
    /// Master frequency relative to the local oscillator, ppb.
    pub drift_ppb: Option<f64>,
    /// Master time minus our system clock (TAI), ns — shows whether the
    /// master's absolute time is plausible.
    pub offset_to_system_ns: Option<f64>,
    /// correctionField of the last Sync+Follow_Up (bridge residence time), ns.
    pub correction_ns: Option<i64>,
    /// Recent |residual| values for the sparkline, ns, newest last.
    pub history: VecDeque<f64>,
}

impl Default for SyncMonitor {
    fn default() -> Self {
        SyncMonitor {
            state: SyncState::NoMaster,
            master_port: None,
            gm: None,
            sync_interval_ms: None,
            sync_rx: 0,
            follow_up_rx: 0,
            missing_syncs: 0,
            missing_follow_ups: 0,
            sync_timeouts: 0,
            gm_changes: 0,
            time_base_changes: 0,
            steps: 0,
            spikes: 0,
            freq_changes: 0,
            relearns: 0,
            residual_ns: None,
            rms_ns: None,
            peak_ns: 0.0,
            last_step_ns: None,
            drift_ppb: None,
            offset_to_system_ns: None,
            correction_ns: None,
            history: VecDeque::new(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Counters {
    pub announce_tx: u64,
    pub sync_tx: u64,
    pub follow_up_tx: u64,
    pub pdelay_req_rx: u64,
    pub pdelay_resp_tx: u64,
    pub pdelay_req_tx: u64,
    pub announce_rx: u64,
    pub foreign_sync_rx: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProbeMethod {
    None,
    Icmp,
    Arp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NameSource {
    /// From the --names file; never overwritten by discovered names.
    Static,
    Dhcp,
    Mdns,
    Lldp,
}

impl NameSource {
    pub fn label(&self) -> &'static str {
        match self {
            NameSource::Static => "static",
            NameSource::Dhcp => "dhcp",
            NameSource::Mdns => "mdns",
            NameSource::Lldp => "lldp",
        }
    }
}

/// A device seen on the network segment (inventory), with network-level
/// latency stats. RTT/jitter describe path+stack latency, not sync accuracy.
#[derive(Clone)]
pub struct NodeInfo {
    pub mac: [u8; 6],
    pub ip: Option<std::net::Ipv4Addr>,
    pub name: Option<(String, NameSource)>,
    pub last_seen: Instant,
    pub frames: u64,
    pub rtt_last_us: Option<f64>,
    pub rtt_avg_us: Option<f64>,
    pub jitter_us: Option<f64>,
    pub probes_sent: u64,
    pub probes_lost: u64,
    pub method: ProbeMethod,
    /// Observed traffic from this node, bits/s over the last second.
    pub rate_bps: f64,
}

impl NodeInfo {
    pub fn new(mac: [u8; 6], now: Instant) -> Self {
        NodeInfo {
            mac,
            ip: None,
            name: None,
            last_seen: now,
            frames: 0,
            rtt_last_us: None,
            rtt_avg_us: None,
            jitter_us: None,
            probes_sent: 0,
            probes_lost: 0,
            method: ProbeMethod::None,
            rate_bps: 0.0,
        }
    }

    /// Set the display name; static (curated) names always win.
    pub fn set_name(&mut self, name: String, source: NameSource) {
        if source != NameSource::Static && matches!(self.name, Some((_, NameSource::Static))) {
            return;
        }
        self.name = Some((name, source));
    }
}

/// One error/drop counter of the local interface.
#[derive(Clone)]
pub struct CounterStat {
    pub name: String,
    pub total: u64,
    /// Increase during the last sampling second.
    pub delta: u64,
}

/// Local interface health from /sys/class/net/<if>/statistics.
#[derive(Clone, Default)]
pub struct LinkHealth {
    pub rx_bps: f64,
    pub tx_bps: f64,
    pub rx_pps: u64,
    pub tx_pps: u64,
    /// Error counters with a nonzero total, most recently increased first.
    pub problems: Vec<CounterStat>,
}

/// A monitored packet stream with a sequence counter (RTP/AVTP/SOME-IP).
#[derive(Clone)]
pub struct StreamInfo {
    pub kind: &'static str,
    pub src_mac: [u8; 6],
    pub dst: String,
    pub rate_bps: f64,
    pub packets: u64,
    pub lost: u64,
    pub duplicates: u64,
    pub out_of_order: u64,
}

#[derive(Clone)]
pub struct ForeignMaster {
    pub data: AnnounceData,
    pub last_seen: Instant,
    pub announces: u64,
    /// True if this master would beat us in the BMCA while we are active;
    /// in monitor mode, true for the master we follow.
    pub would_win: bool,
}

#[derive(Clone)]
pub struct Status {
    pub role: Role,
    pub iface: String,
    pub clock_identity: [u8; 8],
    pub priority1: u8,
    pub priority2: u8,
    pub clock_class: u8,
    pub domain: u8,
    pub time_source_desc: String,
    /// Active packet-timestamping mode (and why, if degraded).
    pub ts_desc: String,
    pub ts_hardware: bool,
    pub utc_offset: i16,
    pub started: Instant,
    pub handover_deadline: Option<Instant>,
    pub handover_minutes: Option<f64>,
    pub counters: Counters,
    /// Smoothed peer-to-peer link delay measured by our own Pdelay_Req, ns.
    pub link_delay_ns: Option<f64>,
    pub foreign_masters: Vec<ForeignMaster>,
    pub nodes: Vec<NodeInfo>,
    pub streams: Vec<StreamInfo>,
    pub link: LinkHealth,
    pub events: VecDeque<String>,
    pub tai_now_secs: u64,
    /// Present in monitor mode.
    pub sync: Option<SyncMonitor>,
}

impl Status {
    pub fn new(iface: String) -> Self {
        Status {
            role: Role::ActiveGm,
            iface,
            clock_identity: [0; 8],
            priority1: 0,
            priority2: 0,
            clock_class: 0,
            domain: 0,
            time_source_desc: String::new(),
            ts_desc: String::new(),
            ts_hardware: false,
            utc_offset: 0,
            started: Instant::now(),
            handover_deadline: None,
            handover_minutes: None,
            counters: Counters::default(),
            link_delay_ns: None,
            foreign_masters: Vec::new(),
            nodes: Vec::new(),
            streams: Vec::new(),
            link: LinkHealth::default(),
            events: VecDeque::new(),
            tai_now_secs: 0,
            sync: None,
        }
    }

    pub fn log(&mut self, msg: impl Into<String>) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (h, m, s) = (secs / 3600 % 24, secs / 60 % 60, secs % 60);
        self.events
            .push_front(format!("{h:02}:{m:02}:{s:02}Z  {}", msg.into()));
        self.events.truncate(200);
    }
}
