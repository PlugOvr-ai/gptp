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
        if source != NameSource::Static
            && matches!(self.name, Some((_, NameSource::Static)))
        {
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
    /// True if this master would beat us in the BMCA while we are active.
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
