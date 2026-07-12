mod clock;
mod config;
mod gm;
mod net;
mod phc;
mod ptp;
mod state;
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

    let sock = net::RawSock::open(&cfg.interface, cfg.force_sw_timestamps)?;
    let clock = clock::TaiClock::new(cfg.utc_offset);
    let state = Arc::new(Mutex::new(Status::new(cfg.interface.clone())));
    let (cmd_tx, cmd_rx) = mpsc::channel();

    let engine = {
        let cfg = cfg.clone();
        let state = Arc::clone(&state);
        thread::spawn(move || gm::run(cfg, sock, clock, state, cmd_rx))
    };

    if cfg.no_tui {
        run_headless(&state, &cmd_tx)?;
    } else {
        tui::run(Arc::clone(&state), cmd_tx)?;
    }

    match engine.join() {
        Ok(res) => res,
        Err(_) => anyhow::bail!("grandmaster engine panicked"),
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
            };
            let ts = if st.ts_hardware {
                "hw"
            } else {
                "SW-ONLY(!)"
            };
            println!(
                "status: {role} ts={ts} announce_tx={} sync_tx={} pdelay_answered={} \
                 foreign_masters={} link_delay={}",
                st.counters.announce_tx,
                st.counters.sync_tx,
                st.counters.pdelay_resp_tx,
                st.foreign_masters.len(),
                st.link_delay_ns
                    .map(|d| format!("{d:.0}ns"))
                    .unwrap_or_else(|| "n/a".into()),
            );
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
