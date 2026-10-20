//! Grandmaster engine: timers, transmit path, and receive handling.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::clock::{stamp_to_tai, TaiClock};
use crate::config::Config;
use crate::net::RawSock;
use crate::pdelay::PeerDelay;
use crate::ptp::{self, AnnounceData, Msg};
use crate::state::{ForeignMaster, Role, Status};

pub enum Cmd {
    Quit,
    /// Toggle between active GM and passive.
    ToggleGm,
    /// Restart the handover countdown (and reactivate if passive).
    RestartHandover,
}

pub fn run(
    cfg: Config,
    sock: RawSock,
    clock: TaiClock,
    state: Arc<Mutex<Status>>,
    cmds: Receiver<Cmd>,
) -> Result<()> {
    let clock_identity = ptp::clock_identity_from_mac(sock.mac);
    let builder = ptp::MsgBuilder {
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
        st.ts_desc = sock.ts_desc().to_string();
        st.ts_hardware = sock.is_hardware_ts();
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
        if sock.is_hardware_ts() {
            st.log(format!("Hardware timestamping active: {}", sock.ts_desc()));
        } else {
            st.log(format!(
                "WARNING: no hardware timestamping ({}) — accuracy limited to tens of µs \
                 and strict gPTP peers may reject the link (neighborPropDelayThresh)",
                sock.ts_desc()
            ));
        }
    }

    let mut seq_announce: u16 = 0;
    let mut seq_sync: u16 = 0;
    let mut next_announce = start;
    let mut next_sync = start;
    let mut pdelay = PeerDelay::new(start);
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
                        st.log(
                            "Manually stopped: no longer announcing (pdelay responder stays on)",
                        );
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
                // Two-step: the Sync's transmit timestamp (hardware if
                // available) becomes the Follow_Up's preciseOriginTimestamp.
                // Fall back to a clock sample taken right before the send.
                let fallback = clock.now();
                let (sync_ok, origin) = match sock.send_stamped(&sync) {
                    Ok(raw) => (true, stamp_to_tai(raw, &sock, &clock).unwrap_or(fallback)),
                    Err(_) => (false, fallback),
                };
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

        pdelay.tick(now, &builder, &sock, &clock, &state);

        // Sleep until the next timer, waking early for received frames.
        let mut deadline = pdelay.next;
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
            while let Some((n, stamp)) = sock.recv(&mut rxbuf)? {
                let t_rx = stamp_to_tai(stamp, &sock, &clock).unwrap_or_else(|| clock.now());
                if let Some(msg) = ptp::parse(&rxbuf[..n]) {
                    if !pdelay.handle(&msg, t_rx, &builder, &sock, &clock, &state) {
                        handle_msg(
                            msg,
                            &clock_identity,
                            &our_announce,
                            gm_active,
                            &mut announced_foreign_takeover,
                            &state,
                        );
                    }
                }
            }
        }

        // Housekeeping for the UI.
        {
            let mut st = state.lock().unwrap();
            st.tai_now_secs = clock.now().secs;
            st.handover_deadline = handover_at;
            st.link_delay_ns = pdelay.link_delay_ns;
            let cutoff = Duration::from_secs(10);
            st.foreign_masters
                .retain(|f| now.duration_since(f.last_seen) < cutoff);
        }
    }
}

fn handle_msg(
    msg: Msg,
    clock_identity: &[u8; 8],
    our_announce: &AnnounceData,
    gm_active: bool,
    announced_foreign_takeover: &mut bool,
    state: &Arc<Mutex<Status>>,
) {
    match msg {
        Msg::Announce { source, data, .. } => {
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
        Msg::Sync { source, .. } if source[..8] != clock_identity[..] => {
            state.lock().unwrap().counters.foreign_sync_rx += 1;
        }
        _ => {}
    }
}

pub fn log_to_duration(log: i8) -> Duration {
    Duration::from_secs_f64(2f64.powi(log as i32))
}

/// Advance a periodic deadline past `now` without accumulating drift.
pub fn catch_up(mut next: Instant, interval: Duration, now: Instant) -> Instant {
    while next <= now {
        next += interval;
    }
    next
}
