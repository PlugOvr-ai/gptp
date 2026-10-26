//! Grandmaster engine: timers, transmit path, and receive handling.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::clock::{stamp_to_tai, tx_stamp_missed, TaiClock};
use crate::config::Config;
use crate::gmclock;
use crate::net::{self, RawSock};
use crate::pdelay::PeerDelay;
use crate::ptp::{self, AnnounceData, Msg};
use crate::ptp1588::Port1588;
use crate::seed::{self, Seeder};
use crate::state::{ForeignMaster, Ptp1588Status, Role, Status};

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
    let (clock, mut discipline, time_desc, clock_logs) = gmclock::setup(&cfg, &sock, clock);
    let clock_identity = ptp::clock_identity_from_mac(sock.mac);
    let builder = ptp::MsgBuilder {
        clock_identity,
        port_number: 1,
        domain: cfg.domain,
        sdo: ptp::SDO_GPTP,
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
    // --seed-from-gnss: silent until the time is taken from a GNSS master;
    // the handover countdown starts when announcing does.
    let mut seeder = cfg
        .seed_from_gnss
        .then(|| Seeder::new(start + Duration::from_secs_f64(cfg.seed_wait)));
    let mut gm_active = seeder.is_none();
    let handover_from = |t: Instant| cfg.handover.map(|m| t + Duration::from_secs_f64(m * 60.0));
    let mut handover_at = if gm_active {
        handover_from(start)
    } else {
        None
    };

    {
        let mut st = state.lock().unwrap();
        st.clock_identity = clock_identity;
        st.priority1 = cfg.priority1;
        st.priority2 = cfg.priority2;
        st.clock_class = cfg.clock_class;
        st.domain = cfg.domain;
        st.time_source_desc = time_desc;
        st.ts_desc = sock.ts_desc().to_string();
        st.ts_hardware = sock.is_hardware_ts();
        st.utc_offset = clock.utc_offset();
        st.started = start;
        st.handover_deadline = handover_at;
        st.handover_minutes = cfg.handover;
        st.log(format!(
            "Started as grandmaster on {} (clock {})",
            cfg.iface(),
            ptp::fmt_clock_identity(&clock_identity)
        ));
        if seeder.is_some() {
            st.role = Role::Seeding;
            st.log(format!(
                "Seeding: listening up to {} s for a GNSS-locked master (clockClass 6/7) \
                 to take the time from before announcing",
                cfg.seed_wait
            ));
        }
        for l in clock_logs {
            st.log(l);
        }
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

    // IEEE 1588 for devices without gPTP: served while gPTP is.
    let mut port1588 = cfg.ptp.and_then(|t| {
        let mut st = state.lock().unwrap();
        match Port1588::open(&cfg, t, &sock) {
            Ok((p, desc)) => {
                st.log(format!("PTP (IEEE 1588) beside gPTP: {desc}"));
                st.ptp1588 = Some(Ptp1588Status {
                    desc,
                    ..Default::default()
                });
                Some(p)
            }
            Err(e) => {
                st.log(format!(
                    "WARNING: PTP (IEEE 1588) not served ({e:#}) — gPTP runs without it"
                ));
                st.ptp1588 = Some(Ptp1588Status {
                    error: Some(format!("{e:#}")),
                    ..Default::default()
                });
                None
            }
        }
    });

    let mut seq_announce: u16 = 0;
    let mut seq_sync: u16 = 0;
    let mut next_announce = start;
    let mut next_sync = start;
    let mut pdelay = PeerDelay::new(start);
    // While seeding, the master we listen to is not a handover.
    let mut announced_foreign_takeover = seeder.is_some();
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

        if gm_active && seeder.take().is_some() {
            // Activated by hand while seeding.
            handover_at = handover_at.or_else(|| handover_from(Instant::now()));
            let mut st = state.lock().unwrap();
            st.handover_deadline = handover_at;
            st.log("Seeding skipped: announcing with the current time");
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
                // Software mode falls back to a clock sample taken right
                // before the send; hardware mode never sends such a guess
                // (µs off) — like ptp4l, that Sync gets no Follow_Up.
                let fallback = clock.now();
                let (sync_ok, origin) = match sock.send_stamped(&sync) {
                    Ok(raw) if tx_stamp_missed(&sock, &raw) => {
                        let mut st = state.lock().unwrap();
                        st.counters.tx_stamp_missed += 1;
                        if st.counters.tx_stamp_missed == 1 {
                            st.log(
                                "WARNING: a Sync got no hardware transmit timestamp within \
                                 20 ms — its Follow_Up was not sent; see \"TX hw stamp missed\"",
                            );
                        }
                        (true, None)
                    }
                    Ok(raw) => (
                        true,
                        Some(stamp_to_tai(raw, &sock, &clock).unwrap_or(fallback)),
                    ),
                    Err(_) => (false, None),
                };
                let fup_ok = origin.is_some_and(|o| {
                    sock.send(&builder.follow_up(seq_sync, o, cfg.log_sync_interval))
                        .is_ok()
                });
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

        if let Some(p) = port1588.as_mut() {
            p.set_active(gm_active, now);
            p.tick(now, &clock, &sock, &state);
        }

        pdelay.tick(now, &builder, &sock, &clock, &state);
        // Steered also while passive, so a takeover serves the right time.
        if let Some((logs, desc)) = discipline.as_mut().and_then(|d| d.tick(now)) {
            let mut st = state.lock().unwrap();
            st.time_source_desc = desc;
            for l in logs {
                st.log(l);
            }
        }

        // Sleep until the next timer, waking early for received frames.
        let mut deadline = pdelay.next;
        if let Some(d) = &discipline {
            deadline = deadline.min(d.next());
        }
        if gm_active {
            deadline = deadline.min(next_announce).min(next_sync);
        }
        if let (true, Some(h)) = (gm_active, handover_at) {
            deadline = deadline.min(h);
        }
        if let Some(d) = port1588.as_ref().and_then(|p| p.next_deadline()) {
            deadline = deadline.min(d);
        }
        let timeout = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        let mut fds = vec![sock.fd()];
        if let Some(p) = &port1588 {
            fds.extend(p.fds());
        }
        let ready = net::poll_any(&fds, timeout.as_millis() as i32)?;
        if ready[1..].contains(&true) {
            if let Some(p) = port1588.as_mut() {
                if let Err(e) = p.drain(&clock, &sock, &state) {
                    state
                        .lock()
                        .unwrap()
                        .log(format!("WARNING: PTP (1588) receive failed: {e:#}"));
                }
            }
        }
        if ready[0] {
            while let Some((n, stamp, _)) = sock.recv(&mut rxbuf)? {
                let t_rx = stamp_to_tai(stamp, &sock, &clock).unwrap_or_else(|| clock.now());
                if let Some(msg) = ptp::parse(&rxbuf[..n]) {
                    if !pdelay.handle(&msg, t_rx, &builder, &sock, &clock, &state) {
                        if let Some(s) = seeder.as_mut() {
                            let logs = s.handle(&msg, t_rx, pdelay.link_delay_ns, Instant::now());
                            let mut st = state.lock().unwrap();
                            for l in logs {
                                st.log(l);
                            }
                        }
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

        if let Some(s) = &seeder {
            let now = Instant::now();
            let done = if let Some(seed) = s.ready(now) {
                let off = seed.offset_at(clock.now().to_ns());
                let rate = if seed.rate_usable() {
                    seed.rate_ppb
                } else {
                    0.0
                };
                let mut st = state.lock().unwrap();
                match gmclock::apply_seed(&clock, discipline.as_mut(), &sock, off, rate) {
                    Ok(logs) => {
                        for l in logs {
                            st.log(l);
                        }
                        st.log(seed::describe(&seed, off));
                    }
                    Err(e) => st.log(format!(
                        "WARNING: seeding from {} failed ({e:#}) — announcing with the \
                         current time, {} off it ({:+.0} ppb)",
                        ptp::fmt_clock_identity(&seed.gm.gm_identity),
                        crate::monitor::fmt_ns(off),
                        seed.rate_ppb
                    )),
                }
                true
            } else if now >= s.deadline {
                state.lock().unwrap().log(format!(
                    "WARNING: seeding gave up after {} s ({}) — announcing with the current time",
                    cfg.seed_wait,
                    s.waiting_for()
                ));
                true
            } else {
                false
            };
            if done {
                seeder = None;
                gm_active = true;
                announced_foreign_takeover = false;
                next_announce = now;
                next_sync = now;
                handover_at = handover_from(now);
                let mut st = state.lock().unwrap();
                st.role = Role::ActiveGm;
                st.handover_deadline = handover_at;
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
