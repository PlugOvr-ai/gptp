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
    pub utc_offset: i16,
    pub started: Instant,
    pub handover_deadline: Option<Instant>,
    pub handover_minutes: Option<f64>,
    pub counters: Counters,
    /// Smoothed peer-to-peer link delay measured by our own Pdelay_Req, ns.
    pub link_delay_ns: Option<f64>,
    pub foreign_masters: Vec<ForeignMaster>,
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
            utc_offset: 0,
            started: Instant::now(),
            handover_deadline: None,
            handover_minutes: None,
            counters: Counters::default(),
            link_delay_ns: None,
            foreign_masters: Vec::new(),
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
