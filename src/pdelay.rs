//! Peer-delay mechanism (802.1AS): initiator measuring the link delay to the
//! neighbor, and two-step responder so the neighbor keeps us asCapable.
//! Shared by the grandmaster and the slave monitor.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock::{stamp_to_tai, TaiClock};
use crate::net::RawSock;
use crate::ptp::{Msg, MsgBuilder, Timestamp};
use crate::state::Status;

const INTERVAL: Duration = Duration::from_secs(1);

struct Pending {
    seq: u16,
    t1: Timestamp,
    /// Port that answered first; on a shared segment several may answer,
    /// and mixing their timestamps yields garbage.
    responder: Option<[u8; 10]>,
    t2: Option<Timestamp>,
    t4: Option<Timestamp>,
    resp_correction_ns: i64,
    sent_at: Instant,
}

pub struct PeerDelay {
    seq: u16,
    pub next: Instant,
    pending: Option<Pending>,
    /// Smoothed link delay, ns.
    pub link_delay_ns: Option<f64>,
}

impl PeerDelay {
    pub fn new(start: Instant) -> Self {
        PeerDelay {
            seq: 0,
            next: start + Duration::from_millis(200),
            pending: None,
            link_delay_ns: None,
        }
    }

    /// Send a Pdelay_Req when due and drop stale exchanges.
    pub fn tick(
        &mut self,
        now: Instant,
        builder: &MsgBuilder,
        sock: &RawSock,
        clock: &TaiClock,
        state: &Arc<Mutex<Status>>,
    ) {
        if now >= self.next {
            self.seq = self.seq.wrapping_add(1);
            let msg = builder.pdelay_req(self.seq);
            let fallback = clock.now();
            if let Ok(raw) = sock.send_stamped(&msg) {
                let t1 = stamp_to_tai(raw, sock, clock).unwrap_or(fallback);
                self.pending = Some(Pending {
                    seq: self.seq,
                    t1,
                    responder: None,
                    t2: None,
                    t4: None,
                    resp_correction_ns: 0,
                    sent_at: now,
                });
                state.lock().unwrap().counters.pdelay_req_tx += 1;
            }
            self.next = crate::gm::catch_up(self.next, INTERVAL, now);
        }
        if let Some(p) = &self.pending {
            if now.duration_since(p.sent_at) > Duration::from_millis(900) {
                self.pending = None;
            }
        }
    }

    /// Handle a peer-delay message. Returns false if `msg` is not one.
    #[allow(clippy::too_many_arguments)]
    pub fn handle(
        &mut self,
        msg: &Msg,
        t_rx: Timestamp,
        builder: &MsgBuilder,
        sock: &RawSock,
        clock: &TaiClock,
        state: &Arc<Mutex<Status>>,
    ) -> bool {
        let clock_identity = &builder.clock_identity;
        match *msg {
            Msg::PdelayReq { seq, source } => {
                if source[..8] == clock_identity[..] {
                    return true;
                }
                // Two-step responder: t2 in Pdelay_Resp, t3 in Pdelay_Resp_Follow_Up.
                let resp = builder.pdelay_resp(seq, &source, t_rx);
                let fallback = clock.now();
                let (resp_ok, t3) = match sock.send_stamped(&resp) {
                    Ok(raw) => (true, stamp_to_tai(raw, sock, clock).unwrap_or(fallback)),
                    Err(_) => (false, fallback),
                };
                let fup = builder.pdelay_resp_fup(seq, &source, t3);
                let _ = resp_ok && sock.send(&fup).is_ok();
                let mut st = state.lock().unwrap();
                st.counters.pdelay_req_rx += 1;
                if resp_ok {
                    st.counters.pdelay_resp_tx += 1;
                }
            }
            Msg::PdelayResp {
                source,
                seq,
                req_receipt,
                requesting,
                correction_ns,
            } => {
                if requesting[..8] != clock_identity[..] {
                    return true;
                }
                if let Some(p) = &mut self.pending {
                    if p.seq == seq && p.responder.is_none() {
                        p.responder = Some(source);
                        p.t2 = Some(req_receipt);
                        p.t4 = Some(t_rx);
                        p.resp_correction_ns = correction_ns;
                    }
                }
            }
            Msg::PdelayRespFup {
                source,
                seq,
                resp_origin,
                requesting,
                correction_ns,
            } => {
                if requesting[..8] != clock_identity[..] {
                    return true;
                }
                if self
                    .pending
                    .as_ref()
                    .is_none_or(|p| p.seq != seq || p.responder != Some(source))
                {
                    return true;
                }
                let p = self.pending.take().unwrap();
                if let (Some(t2), Some(t4)) = (p.t2, p.t4) {
                    let turnaround = resp_origin.to_ns() - t2.to_ns();
                    let roundtrip = t4.to_ns() - p.t1.to_ns();
                    let delay = (roundtrip - turnaround) as f64 / 2.0
                        - (p.resp_correction_ns + correction_ns) as f64;
                    let ewma = self.link_delay_ns.map_or(delay, |e| e * 0.8 + delay * 0.2);
                    self.link_delay_ns = Some(ewma);
                }
            }
            _ => return false,
        }
        true
    }
}
