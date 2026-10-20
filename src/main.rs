mod clock;
mod config;
mod gm;
mod inventory;
mod linkstats;
mod metrics;
mod monitor;
mod names;
mod net;
mod pdelay;
mod phc;
mod ptp;
mod state;
mod streams;
mod tui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use crate::config::Config;
use crate::state::{Role, Status};

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_signal(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn main() -> Result<()> {
    let cfg = Config::parse();

    let scan_subnet = cfg
        .scan_subnet
        .as_deref()
        .map(inventory::parse_cidr)
        .transpose()?;
    let static_names = cfg
        .names
        .as_deref()
        .map(names::load_names_file)
        .transpose()?
        .unwrap_or_default();

    let sock = net::RawSock::open(&cfg.interface, cfg.force_sw_timestamps)?;
    let our_mac = sock.mac;
    let clock = clock::TaiClock::new(cfg.utc_offset);
    let state = Arc::new(Mutex::new(Status::new(cfg.interface.clone())));
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let inv_quit = Arc::new(AtomicBool::new(false));

    let engine = {
        let cfg = cfg.clone();
        let state = Arc::clone(&state);
        if cfg.monitor {
            thread::spawn(move || monitor::run(cfg, sock, clock, state, cmd_rx))
        } else {
            thread::spawn(move || gm::run(cfg, sock, clock, state, cmd_rx))
        }
    };
    let inv = if cfg.no_inventory {
        None
    } else {
        let iface = cfg.interface.clone();
        let state = Arc::clone(&state);
        let quit = Arc::clone(&inv_quit);
        Some(thread::spawn(move || {
            inventory::run(iface, our_mac, scan_subnet, static_names, state, quit)
        }))
    };
    let mut aux = Vec::new();
    {
        let iface = cfg.interface.clone();
        let state = Arc::clone(&state);
        let quit = Arc::clone(&inv_quit);
        aux.push(thread::spawn(move || linkstats::run(iface, state, quit)));
    }
    if let Some(path) = cfg.log_metrics.clone() {
        let state = Arc::clone(&state);
        let quit = Arc::clone(&inv_quit);
        aux.push(thread::spawn(move || metrics::run(path, state, quit)));
    }

    let ui_result = if cfg.no_tui {
        run_headless(&state, &cmd_tx)
    } else {
        tui::run(Arc::clone(&state), cmd_tx)
    };

    inv_quit.store(true, Ordering::SeqCst);
    if let Some(inv) = inv {
        let _ = inv.join();
    }
    for t in aux {
        let _ = t.join();
    }
    ui_result?;
    match engine.join() {
        Ok(res) => res,
        Err(_) => anyhow::bail!("gPTP engine panicked"),
    }
}

fn run_headless(state: &Arc<Mutex<Status>>, cmds: &mpsc::Sender<gm::Cmd>) -> Result<()> {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as usize);
        libc::signal(libc::SIGTERM, on_signal as usize);
    }
    let mut printed_events = 0usize;
    while RUNNING.load(Ordering::SeqCst) {
        {
            let st = state.lock().unwrap();
            // Print new events oldest-first (they are stored newest-first).
            let new = st.events.len().saturating_sub(printed_events);
            for e in st.events.iter().take(new).collect::<Vec<_>>().iter().rev() {
                println!("{e}");
            }
            printed_events = st.events.len().min(200);
            let role = match st.role {
                Role::ActiveGm => "ACTIVE-GM",
                Role::Passive => "PASSIVE",
                Role::Monitor => "MONITOR",
            };
            let ts = if st.ts_hardware {
                "hw"
            } else {
                "SW-ONLY(!)"
            };
            if let Some(sm) = &st.sync {
                let ns = |v: Option<f64>| v.map_or("-".into(), monitor::fmt_ns);
                println!(
                    "sync: {} gm={} offset_to_system={} residual={} rms={} peak={} \
                     drift={} steps={} spikes={} freq_changes={} relearns={} gm_changes={} \
                     sync_timeouts={} missing_syncs={} missing_fups={}",
                    sm.state.label(),
                    sm.gm.map_or("-".into(), |g| ptp::fmt_clock_identity(&g.gm_identity)),
                    ns(sm.offset_to_system_ns),
                    ns(sm.residual_ns),
                    ns(sm.rms_ns),
                    monitor::fmt_ns(sm.peak_ns),
                    sm.drift_ppb.map_or("-".into(), |d| format!("{d:+.0}ppb")),
                    sm.steps,
                    sm.spikes,
                    sm.freq_changes,
                    sm.relearns,
                    sm.gm_changes,
                    sm.sync_timeouts,
                    sm.missing_syncs,
                    sm.missing_follow_ups,
                );
            }
            println!(
                "status: {role} ts={ts} announce_tx={} sync_tx={} pdelay_answered={} \
                 foreign_masters={} nodes={} link_delay={}",
                st.counters.announce_tx,
                st.counters.sync_tx,
                st.counters.pdelay_resp_tx,
                st.foreign_masters.len(),
                st.nodes.len(),
                st.link_delay_ns
                    .map(|d| format!("{d:.0}ns"))
                    .unwrap_or_else(|| "n/a".into()),
            );
            let us = |v: Option<f64>| v.map_or("-".into(), |x| format!("{x:.0}µs"));
            for n in &st.nodes {
                println!(
                    "node: {} name={} mac={} rtt={} avg={} jitter={} loss={}/{} via={}",
                    n.ip.map_or("-".into(), |i| i.to_string()),
                    n.name
                        .as_ref()
                        .map_or("-".into(), |(name, src)| format!(
                            "\"{name}\"({})",
                            src.label()
                        )),
                    inventory::fmt_mac(&n.mac),
                    us(n.rtt_last_us),
                    us(n.rtt_avg_us),
                    us(n.jitter_us),
                    n.probes_lost,
                    n.probes_sent,
                    match n.method {
                        state::ProbeMethod::Icmp => "icmp",
                        state::ProbeMethod::Arp => "arp",
                        state::ProbeMethod::None => "passive",
                    },
                );
            }
        }
        for _ in 0..50 {
            if !RUNNING.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = cmds.send(gm::Cmd::Quit);
    Ok(())
}
