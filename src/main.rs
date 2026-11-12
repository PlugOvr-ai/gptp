mod clock;
mod clocksync;
mod config;
mod gm;
mod gmclock;
mod inventory;
mod linkstats;
mod metrics;
mod monitor;
mod names;
mod net;
mod pdelay;
mod phc;
mod ptp;
mod ptp1588;
mod seed;
mod state;
mod streams;
mod syncdiag;
mod timesync_setup;
mod tui;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;

use crate::config::{Config, NodeLog};
use crate::state::{Role, Status};

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn on_signal(_: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn main() -> Result<()> {
    let cfg = Config::parse();

    if cfg.remove_time_sync {
        return timesync_setup::remove(&cfg);
    }
    if let Some(mode) = cfg.setup_time_sync {
        return timesync_setup::setup(&cfg, mode);
    }
    if cfg.dry_run {
        anyhow::bail!("--dry-run only applies to --setup-time-sync / --remove-time-sync");
    }
    if !cfg.sync_phc.is_empty() && cfg.time_sync != Some(config::TimeSync::Builtin) {
        anyhow::bail!(
            "--sync-phc needs --time-sync builtin (or --setup-time-sync linuxptp to set up phc2sys)"
        );
    }

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

    let sock = net::RawSock::open(
        cfg.iface(),
        cfg.force_sw_timestamps,
        cfg.ptp == Some(config::PtpTransport::Udp),
    )?;
    let our_mac = sock.mac;
    let clock = clock::TaiClock::new(cfg.utc_offset).with_error(cfg.time_error);
    let state = Arc::new(Mutex::new(Status::new(cfg.iface().to_string())));
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
        let iface = cfg.iface().to_string();
        let state = Arc::clone(&state);
        let quit = Arc::clone(&inv_quit);
        let cfg_probe_interval = cfg.probe_interval;
        Some(thread::spawn(move || {
            inventory::run(
                iface,
                our_mac,
                scan_subnet,
                Duration::from_secs(cfg_probe_interval),
                static_names,
                state,
                quit,
            )
        }))
    };
    let mut aux = Vec::new();
    {
        let iface = cfg.iface().to_string();
        let state = Arc::clone(&state);
        let quit = Arc::clone(&inv_quit);
        aux.push(thread::spawn(move || linkstats::run(iface, state, quit)));
    }
    if let Some(path) = cfg.log_metrics.clone() {
        let state = Arc::clone(&state);
        let quit = Arc::clone(&inv_quit);
        aux.push(thread::spawn(move || metrics::run(path, state, quit)));
    }

    let mut printed_events = 0;
    let ui_result = if cfg.no_tui {
        run_headless(&cfg, &state, &cmd_tx, &mut printed_events)
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
    let res = match engine.join() {
        Ok(res) => res,
        Err(_) => anyhow::bail!("gPTP engine panicked"),
    };
    if cfg.no_tui {
        // Events from the last seconds, e.g. a capture written on shutdown.
        print_new_events(&state.lock().unwrap(), &mut printed_events);
    }
    res
}

fn print_new_events(st: &Status, printed: &mut u64) {
    for e in st.events_since(*printed) {
        println!("{e}");
    }
    *printed = st.events_logged;
}

fn run_headless(
    cfg: &Config,
    state: &Arc<Mutex<Status>>,
    cmds: &mpsc::Sender<gm::Cmd>,
    printed_events: &mut u64,
) -> Result<()> {
    unsafe {
        libc::signal(
            libc::SIGINT,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
        );
    }
    let status_every = Duration::from_secs(cfg.status_interval);
    let node_log = cfg
        .node_log_interval
        .unwrap_or(NodeLog::Every(cfg.status_interval));
    // Passive nodes may send only a frame a minute; probed ones answer
    // every probe, so a lost probe shows first.
    let silent_after = Duration::from_secs((3 * cfg.probe_interval).max(120));
    let mut next_status = Instant::now();
    let mut next_nodes = Instant::now();
    let mut node_conds: HashMap<[u8; 6], NodeCond> = HashMap::new();
    while RUNNING.load(Ordering::SeqCst) {
        let now = Instant::now();
        {
            let st = state.lock().unwrap();
            // Events as they happen; the routine lines on their intervals.
            print_new_events(&st, printed_events);
            if now >= next_status {
                print_status(&st);
                next_status = gm::catch_up(next_status, status_every, now);
            }
            match node_log {
                NodeLog::Every(0) => {}
                NodeLog::Every(secs) => {
                    if now >= next_nodes {
                        for n in &st.nodes {
                            print_node(n, None);
                        }
                        next_nodes = gm::catch_up(next_nodes, Duration::from_secs(secs), now);
                    }
                }
                NodeLog::Changed => {
                    for n in &st.nodes {
                        let old = node_conds.get(&n.mac);
                        let cond = NodeCond::of(n, silent_after, old);
                        let change = match old {
                            None => Some("new".to_string()),
                            Some(old) => cond.changes_from(old),
                        };
                        if let Some(c) = change {
                            print_node(n, Some(&c));
                        }
                        node_conds.insert(n.mac, cond);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = cmds.send(gm::Cmd::Quit);
    Ok(())
}

fn print_status(st: &Status) {
    let role = match st.role {
        Role::ActiveGm => "ACTIVE-GM",
        Role::Seeding => "SEEDING",
        Role::Passive => "PASSIVE",
        Role::Monitor => "MONITOR",
    };
    let ts = if st.ts_hardware { "hw" } else { "SW-ONLY(!)" };
    if let Some(sm) = &st.sync {
        let ns = |v: Option<f64>| v.map_or("-".into(), monitor::fmt_ns);
        println!(
            "sync: {} protocol=\"{}\" gm={} offset_to_system={} residual={} rms={} peak={} \
             drift={} steps={} spikes={} freq_changes={} relearns={} bursts={} gm_changes={} \
             sync_timeouts={} missing_syncs={} missing_fups={}",
            sm.state.label(),
            sm.protocol,
            sm.gm
                .map_or("-".into(), |g| ptp::fmt_clock_identity(&g.gm_identity)),
            ns(sm.offset_to_system_ns),
            ns(sm.residual_ns),
            ns(sm.rms_ns),
            monitor::fmt_ns(sm.peak_ns),
            sm.drift_ppb.map_or("-".into(), |d| format!("{d:+.0}ppb")),
            sm.steps,
            sm.spikes,
            sm.freq_changes,
            sm.relearns,
            sm.bursts,
            sm.gm_changes,
            sm.sync_timeouts,
            sm.missing_syncs,
            sm.missing_follow_ups,
        );
        for c in &sm.clocks {
            println!(
                "clock: \"{}\" state={} offset={} freq={:+.0}ppb steps={}{}",
                c.name,
                c.state,
                ns(c.offset_ns),
                c.freq_ppb,
                c.steps,
                c.note
                    .as_ref()
                    .map_or(String::new(), |n| format!(" note=\"{n}\""))
            );
        }
        if let Some(l) = &sm.linuxptp_status {
            let label = if sm.time_sync == Some(config::TimeSync::Ntp) {
                "ntp"
            } else {
                "linuxptp"
            };
            println!("{label}: {l}");
        }
    }
    if let Some(p) = &st.ptp1588 {
        match &p.error {
            Some(e) => println!("ptp1588: FAILED {e}"),
            None => println!(
                "ptp1588: {} announce_tx={} sync_tx={} follow_up_tx={} delay_req_rx={} \
                 delay_resp_tx={} clients={} foreign_masters={}",
                p.desc,
                p.announce_tx,
                p.sync_tx,
                p.follow_up_tx,
                p.delay_req_rx,
                p.delay_resp_tx,
                p.clients
                    .iter()
                    .map(|c| c.addr.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                p.foreign_masters,
            ),
        }
    }
    println!(
        "status: {role} ts={ts} announce_tx={} sync_tx={} tx_stamp_missed={} pdelay_answered={} \
         foreign_masters={} nodes={} link_delay={}",
        st.counters.announce_tx,
        st.counters.sync_tx,
        st.counters.tx_stamp_missed,
        st.counters.pdelay_resp_tx,
        st.foreign_masters.len(),
        st.nodes.len(),
        st.link_delay_ns
            .map(|d| format!("{d:.0}ns"))
            .unwrap_or_else(|| "n/a".into()),
    );
}

fn print_node(n: &state::NodeInfo, change: Option<&str>) {
    let us = |v: Option<f64>| v.map_or("-".into(), |x| format!("{x:.0}µs"));
    println!(
        "node: {} name={} mac={} rtt={} avg={} jitter={} loss={}/{} via={} seen={}s{}",
        n.ip.map_or("-".into(), |i| i.to_string()),
        n.name.as_ref().map_or("-".into(), |(name, src)| format!(
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
        n.last_seen.elapsed().as_secs(),
        change.map_or(String::new(), |c| format!(" change={c}")),
    );
}

/// What --node-log-interval changed reports on.
#[derive(Clone, PartialEq, Debug)]
struct NodeCond {
    ip: Option<std::net::Ipv4Addr>,
    name: Option<String>,
    silent: bool,
    probe_lost: bool,
    jitter_high: bool,
}

/// Jitter is high above this (as flagged in the TUI) and back to normal
/// below JITTER_OK, so a value near the threshold does not flap.
const JITTER_HIGH_US: f64 = 1000.0;
const JITTER_OK_US: f64 = 800.0;

impl NodeCond {
    fn of(n: &state::NodeInfo, silent_after: Duration, old: Option<&NodeCond>) -> Self {
        let was_high = old.is_some_and(|o| o.jitter_high);
        let limit = if was_high {
            JITTER_OK_US
        } else {
            JITTER_HIGH_US
        };
        NodeCond {
            ip: n.ip,
            name: n.name.as_ref().map(|(name, _)| name.clone()),
            silent: n.last_seen.elapsed() > silent_after,
            probe_lost: n.last_probe_lost,
            jitter_high: n.jitter_us.is_some_and(|j| j > limit),
        }
    }

    /// The changes since `old`, comma-separated, if any.
    fn changes_from(&self, old: &NodeCond) -> Option<String> {
        let mut c = Vec::new();
        if self.ip != old.ip {
            c.push("ip");
        }
        if self.name != old.name {
            c.push("name");
        }
        if self.silent != old.silent {
            c.push(if self.silent { "silent" } else { "heard-again" });
        }
        if self.probe_lost != old.probe_lost {
            c.push(if self.probe_lost {
                "probe-lost"
            } else {
                "probe-answered"
            });
        }
        if self.jitter_high != old.jitter_high {
            c.push(if self.jitter_high {
                "jitter-high"
            } else {
                "jitter-ok"
            });
        }
        (!c.is_empty()).then(|| c.join(","))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(jitter_us: f64, lost: bool) -> state::NodeInfo {
        let mut n = state::NodeInfo::new([2, 0, 0, 0, 0, 1], Instant::now());
        n.ip = Some("10.0.0.5".parse().unwrap());
        n.jitter_us = Some(jitter_us);
        n.last_probe_lost = lost;
        n
    }

    #[test]
    fn node_changes_with_jitter_hysteresis() {
        let after = Duration::from_secs(30);
        let a = NodeCond::of(&node(100.0, false), after, None);
        // Unchanged: nothing to print.
        assert_eq!(
            NodeCond::of(&node(150.0, false), after, Some(&a)).changes_from(&a),
            None
        );
        let b = NodeCond::of(&node(1200.0, true), after, Some(&a));
        assert_eq!(
            b.changes_from(&a).as_deref(),
            Some("probe-lost,jitter-high")
        );
        // Still high between the two thresholds, normal below the lower one.
        let c = NodeCond::of(&node(900.0, true), after, Some(&b));
        assert_eq!(c.changes_from(&b), None);
        let d = NodeCond::of(&node(700.0, false), after, Some(&c));
        assert_eq!(
            d.changes_from(&c).as_deref(),
            Some("probe-answered,jitter-ok")
        );
    }

    #[test]
    fn node_log_interval_values() {
        let parse = |args: &[&str]| {
            Config::try_parse_from([&["gptp-gm", "eth0"], args].concat())
                .map(|c| c.node_log_interval)
        };
        assert_eq!(parse(&[]).unwrap(), None);
        assert_eq!(
            parse(&["--node-log-interval", "0"]).unwrap(),
            Some(NodeLog::Every(0))
        );
        assert_eq!(
            parse(&["--node-log-interval", "changed"]).unwrap(),
            Some(NodeLog::Changed)
        );
        assert!(parse(&["--node-log-interval", "often"]).is_err());
        assert!(Config::try_parse_from(["gptp-gm", "eth0", "--probe-interval", "0"]).is_err());
    }
}
