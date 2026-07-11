//! Grandmaster engine: timers, transmit path, and receive handling.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::clock::TaiClock;
use crate::config::Config;
use crate::net::RawSock;
use crate::ptp::{self, AnnounceData, Msg, MsgBuilder, Timestamp};
use crate::state::{ForeignMaster, Role, Status};

pub enum Cmd {
    Quit,
    /// Toggle between active GM and passive.
    ToggleGm,
    /// Restart the handover countdown (and reactivate if passive).
    RestartHandover,
}

struct PendingPdelay {
    seq: u16,
    t1: Timestamp,
    t2: Option<Timestamp>,
    t4: Option<Timestamp>,
    resp_correction_ns: i64,
    sent_at: Instant,
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
    let our_announce = AnnounceData {
        priority1: cfg.priority1,
        clock_class: cfg.clock_class,
        clock_accuracy: cfg.clock_accuracy,
        variance: cfg.variance,
        priority2: cfg.priority2,
        gm_identity: clock_identity,
        steps_removed: 0,
        time_source: cfg.time_source,
    };

    let announce_interval = log_to_duration(cfg.log_announce_interval);
    let sync_interval = log_to_duration(cfg.log_sync_interval);
    let pdelay_interval = Duration::from_secs(1);

    let start = Instant::now();
    let mut gm_active = true;
    let mut handover_at = cfg
        .handover
        .map(|m| start + Duration::from_secs_f64(m * 60.0));

    {
        let mut st = state.lock().unwrap();
        st.clock_identity = clock_identity;
        st.priority1 = cfg.priority1;
        st.priority2 = cfg.priority2;
        st.clock_class = cfg.clock_class;
        st.domain = cfg.domain;
        st.time_source_desc = clock.describe().to_string();
        st.utc_offset = clock.utc_offset();
        st.started = start;
        st.handover_deadline = handover_at;
        st.handover_minutes = cfg.handover;
        st.log(format!(
            "Started as grandmaster on {} (clock {})",
            cfg.interface,
            ptp::fmt_clock_identity(&clock_identity)
        ));
        if let Some(m) = cfg.handover {
            st.log(format!(
                "Handover mode: will stop announcing after {m} min and yield to the switch"
            ));
        }
    }

    let mut seq_announce: u16 = 0;
    let mut seq_sync: u16 = 0;
    let mut seq_pdelay: u16 = 0;
    let mut next_announce = start;
    let mut next_sync = start;
    let mut next_pdelay = start + Duration::from_millis(200);
    let mut pending_pdelay: Option<PendingPdelay> = None;
    let mut link_delay_ewma: Option<f64> = None;
    let mut announced_foreign_takeover = false;
    let mut rxbuf = [0u8; 1600];

    loop {
        while let Ok(cmd) = cmds.try_recv() {
            match cmd {
                Cmd::Quit => return Ok(()),
                Cmd::ToggleGm => {
                    gm_active = !gm_active;
                    let mut st = state.lock().unwrap();
                    if gm_active {
                        next_announce = Instant::now();
                        next_sync = Instant::now();
                        announced_foreign_takeover = false;
                        st.role = Role::ActiveGm;
                        st.log("Manually reactivated: announcing as grandmaster again");
                    } else {
                        st.role = Role::Passive;
                        st.log("Manually stopped: no longer announcing (pdelay responder stays on)");
                    }
                }
                Cmd::RestartHandover => {
                    if let Some(m) = cfg.handover {
                        handover_at = Some(Instant::now() + Duration::from_secs_f64(m * 60.0));
                        let mut st = state.lock().unwrap();
                        st.handover_deadline = handover_at;
                        if !gm_active {
                            gm_active = true;
                            next_announce = Instant::now();
                            next_sync = Instant::now();
                            announced_foreign_takeover = false;
                            st.role = Role::ActiveGm;
                        }
                        st.log(format!("Handover countdown restarted ({m} min)"));
                    }
                }
            }
        }

        let now = Instant::now();

        if gm_active {
            if let Some(h) = handover_at {
                if now >= h {
                    gm_active = false;
                    let mut st = state.lock().unwrap();
                    st.role = Role::Passive;
                    st.log(
                        "Handover: stopped Announce/Sync — switch should take over as GM \
                         (press r to restart)",
                    );
                }
            }
        }

        if gm_active {
            if now >= next_announce {
                let msg = builder.announce(
                    seq_announce,
                    cfg.log_announce_interval,
                    clock.utc_offset(),
                    cfg.priority1,
                    cfg.clock_class,
                    cfg.clock_accuracy,
                    cfg.variance,
                    cfg.priority2,
                    cfg.time_source,
                );
                seq_announce = seq_announce.wrapping_add(1);
                if sock.send(&msg).is_ok() {
                    state.lock().unwrap().counters.announce_tx += 1;
                }
                next_announce = catch_up(next_announce, announce_interval, now);
            }
            if now >= next_sync {
                let sync = builder.sync(seq_sync, cfg.log_sync_interval);
                // Software timestamping: sample the clock immediately before the
                // Sync hits the wire and carry it in the two-step Follow_Up.
                let origin = clock.now();
                let sync_ok = sock.send(&sync).is_ok();
                let fup = builder.follow_up(seq_sync, origin, cfg.log_sync_interval);
                let fup_ok = sync_ok && sock.send(&fup).is_ok();
                seq_sync = seq_sync.wrapping_add(1);
                {
                    let mut st = state.lock().unwrap();
                    if sync_ok {
                        st.counters.sync_tx += 1;
                    }
                    if fup_ok {
                        st.counters.follow_up_tx += 1;
                    }
                }
                next_sync = catch_up(next_sync, sync_interval, now);
            }
        }

        if now >= next_pdelay {
            seq_pdelay = seq_pdelay.wrapping_add(1);
            let msg = builder.pdelay_req(seq_pdelay);
            let t1 = clock.now();
            if sock.send(&msg).is_ok() {
                pending_pdelay = Some(PendingPdelay {
                    seq: seq_pdelay,
                    t1,
                    t2: None,
                    t4: None,
                    resp_correction_ns: 0,
                    sent_at: now,
                });
                state.lock().unwrap().counters.pdelay_req_tx += 1;
            }
            next_pdelay = catch_up(next_pdelay, pdelay_interval, now);
        }

        // Drop stale peer-delay exchanges.
        if let Some(p) = &pending_pdelay {
            if now.duration_since(p.sent_at) > Duration::from_millis(900) {
                pending_pdelay = None;
            }
        }

        // Sleep until the next timer, waking early for received frames.
        let mut deadline = next_pdelay;
        if gm_active {
            deadline = deadline.min(next_announce).min(next_sync);
        }
        if let (true, Some(h)) = (gm_active, handover_at) {
            deadline = deadline.min(h);
        }
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        if sock.poll(timeout.as_millis() as i32)? {
            while let Some(n) = sock.recv(&mut rxbuf)? {
                let t_rx = clock.now();
                if let Some(msg) = ptp::parse(&rxbuf[..n]) {
                    handle_msg(
                        msg,
                        t_rx,
                        &builder,
                        &sock,
                        &clock,
                        &clock_identity,
                        &our_announce,
                        gm_active,
                        &mut pending_pdelay,
                        &mut link_delay_ewma,
                        &mut announced_foreign_takeover,
                        &state,
                    );
                }
            }
        }

        // Housekeeping for the UI.
        {
            let mut st = state.lock().unwrap();
            st.tai_now_secs = clock.now().secs;
            st.handover_deadline = handover_at;
            st.link_delay_ns = link_delay_ewma;
            let cutoff = Duration::from_secs(10);
            st.foreign_masters
                .retain(|f| now.duration_since(f.last_seen) < cutoff);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_msg(
    msg: Msg,
    t_rx: Timestamp,
    builder: &MsgBuilder,
    sock: &RawSock,
    clock: &TaiClock,
    clock_identity: &[u8; 8],
    our_announce: &AnnounceData,
    gm_active: bool,
    pending_pdelay: &mut Option<PendingPdelay>,
    link_delay_ewma: &mut Option<f64>,
    announced_foreign_takeover: &mut bool,
    state: &Arc<Mutex<Status>>,
) {
    match msg {
        Msg::PdelayReq { seq, source } => {
            if source[..8] == clock_identity[..] {
                return;
            }
            // Two-step responder: t2 in Pdelay_Resp, t3 in Pdelay_Resp_Follow_Up.
            let resp = builder.pdelay_resp(seq, &source, t_rx);
            let resp_ok = sock.send(&resp).is_ok();
            let t3 = clock.now();
            let fup = builder.pdelay_resp_fup(seq, &source, t3);
            let _ = resp_ok && sock.send(&fup).is_ok();
            let mut st = state.lock().unwrap();
            st.counters.pdelay_req_rx += 1;
            if resp_ok {
                st.counters.pdelay_resp_tx += 1;
            }
        }
        Msg::PdelayResp {
            seq,
            req_receipt,
            requesting,
            correction_ns,
        } => {
            if requesting[..8] != clock_identity[..] {
                return;
            }
            if let Some(p) = pending_pdelay {
                if p.seq == seq {
                    p.t2 = Some(req_receipt);
                    p.t4 = Some(t_rx);
                    p.resp_correction_ns = correction_ns;
                }
            }
        }
        Msg::PdelayRespFup {
            seq,
            resp_origin,
            requesting,
            correction_ns,
        } => {
            if requesting[..8] != clock_identity[..] {
                return;
            }
            let Some(p) = pending_pdelay.take() else {
                return;
            };
            if p.seq != seq {
                return;
            }
            if let (Some(t2), Some(t4)) = (p.t2, p.t4) {
                let turnaround = resp_origin.to_ns() - t2.to_ns();
                let roundtrip = t4.to_ns() - p.t1.to_ns();
                let delay = (roundtrip - turnaround) as f64 / 2.0
                    - (p.resp_correction_ns + correction_ns) as f64;
                let ewma = link_delay_ewma.map_or(delay, |e| e * 0.8 + delay * 0.2);
                *link_delay_ewma = Some(ewma);
            }
        }
        Msg::Announce { source, data } => {
            if data.gm_identity == *clock_identity || source[..8] == clock_identity[..] {
                return;
            }
            let would_win = data.bmca_key() < our_announce.bmca_key();
            let mut st = state.lock().unwrap();
            st.counters.announce_rx += 1;
            let gm_str = ptp::fmt_clock_identity(&data.gm_identity);
            match st
                .foreign_masters
                .iter_mut()
                .find(|f| f.data.gm_identity == data.gm_identity)
            {
                Some(f) => {
                    f.data = data;
                    f.last_seen = Instant::now();
                    f.announces += 1;
                    f.would_win = would_win;
                }
                None => {
                    st.foreign_masters.push(ForeignMaster {
                        data,
                        last_seen: Instant::now(),
                        announces: 1,
                        would_win,
                    });
                    st.log(format!(
                        "Foreign master seen: {gm_str} (prio1 {}, class {})",
                        data.priority1, data.clock_class
                    ));
                    if !gm_active && !*announced_foreign_takeover {
                        *announced_foreign_takeover = true;
                        st.log(format!("Handover confirmed: {gm_str} is announcing as GM"));
                    } else if gm_active && would_win {
                        st.log(format!(
                            "WARNING: {gm_str} outranks us in the BMCA (their prio1 {} vs our {})",
                            data.priority1, our_announce.priority1
                        ));
                    }
                }
            }
        }
        Msg::Sync { source } => {
            if source[..8] != clock_identity[..] {
                state.lock().unwrap().counters.foreign_sync_rx += 1;
            }
        }
    }
}

fn log_to_duration(log: i8) -> Duration {
    Duration::from_secs_f64(2f64.powi(log as i32))
}

/// Advance a periodic deadline past `now` without accumulating drift.
fn catch_up(mut next: Instant, interval: Duration, now: Instant) -> Instant {
    while next <= now {
        next += interval;
    }
    next
}
