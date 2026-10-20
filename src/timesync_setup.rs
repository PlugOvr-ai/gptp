//! One-shot system configuration for the slave's time sync
//! (`--setup-time-sync MODE`, `--remove-time-sync`).
//!
//! - **builtin**: only disables time services that would fight gptp-gm's
//!   own clock servo (systemd-timesyncd, chrony, ntpd, ptp4l/phc2sys).
//! - **linuxptp**: additionally writes a gPTP slave config for ptp4l and
//!   systemd units for ptp4l (NIC clock ← master), phc2sys (system clock ←
//!   NIC clock, UTC via an explicit TAI−UTC offset rather than trusting
//!   what the master announces) and phc2sys per `--sync-phc` NIC.
//!
//! Everything disabled is recorded in the state file and re-enabled by
//! `--remove-time-sync`; repeated setups keep the original record.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::{Config, TimeSync};

const STATE_FILE: &str = "/etc/gptp-gm/time-sync.state";
const UNIT_DIR: &str = "/etc/systemd/system";
const LINUXPTP_DIR: &str = "/etc/linuxptp";

/// Services that set the system clock themselves.
const TIME_SERVICES: &[&str] = &[
    "systemd-timesyncd.service",
    "chrony.service",
    "chronyd.service",
    "ntp.service",
    "ntpd.service",
    "ntpsec.service",
    "openntpd.service",
];

/// What setup will do.
#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub mode: String,
    /// (path, content)
    pub files: Vec<(String, String)>,
    /// Our units to enable and start.
    pub units: Vec<String>,
    /// (service, "enabled" | "active") to disable and stop.
    pub disable: Vec<(String, String)>,
}

/// gPTP (802.1AS) slave settings for ptp4l on one interface. `ro_socket`:
/// the ptp4l version knows uds_ro_address (otherwise several ptp4l
/// instances share the default read-only socket path).
pub fn ptp4l_config(
    iface: &str,
    hardware: bool,
    thresh_ns: u32,
    utc_offset: i16,
    ro_socket: bool,
) -> String {
    let ro = if ro_socket {
        format!("uds_ro_address          {}-ro\n", uds_address(iface))
    } else {
        String::new()
    };
    format!(
        "# gPTP slave on {iface} — written by gptp-gm --setup-time-sync linuxptp
[global]
gmCapable               0
priority1               255
priority2               255
domainNumber            0
logAnnounceInterval     0
logSyncInterval         -3
logMinPdelayReqInterval 0
announceReceiptTimeout  3
syncReceiptTimeout      3
neighborPropDelayThresh {thresh_ns}
min_neighbor_prop_delay -20000000
assume_two_step         1
path_trace_enabled      1
follow_up_info          1
transportSpecific       0x1
ptp_dst_mac             01:80:C2:00:00:0E
network_transport       L2
delay_mechanism         P2P
time_stamping           {ts}
tx_timestamp_timeout    50
# TAI - UTC, used where the master does not provide it
utc_offset              {utc_offset}
# Follow large master time jumps by stepping instead of slewing for minutes
step_threshold          1.0
uds_address             {uds}
{ro}
[{iface}]
",
        ts = if hardware { "hardware" } else { "software" },
        uds = uds_address(iface),
    )
}

/// Does this linuxptp version support uds_ro_address (3.1 and later)?
pub fn supports_ro_socket(version: &str) -> bool {
    let mut parts = version
        .trim()
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (3, 1)
}

pub fn uds_address(iface: &str) -> String {
    format!("/var/run/gptp-ptp4l-{iface}")
}

fn unit(description: &str, after: &str, exec: &str) -> String {
    format!(
        "# written by gptp-gm --setup-time-sync linuxptp
[Unit]
Description={description}
Documentation=https://github.com/PlugOvr-ai/gptp
{after}
[Service]
Type=simple
ExecStart={exec}
Restart=always
RestartSec=2

[Install]
WantedBy=multi-user.target
"
    )
}

/// Build the plan. `units` lists installed services as (name, unit-file
/// state, active state); `hardware`: the interface has a PHC.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    mode: TimeSync,
    iface: &str,
    sync_phcs: &[String],
    hardware: bool,
    thresh_ns: u32,
    utc_offset: i16,
    ptp4l: &str,
    ptp4l_version: &str,
    phc2sys: &str,
    units: &[(String, String, String)],
) -> Plan {
    let mut p = Plan {
        mode: match mode {
            TimeSync::Builtin => "builtin".into(),
            TimeSync::Linuxptp => "linuxptp".into(),
        },
        ..Default::default()
    };
    for (name, file_state, active) in units {
        let ours = name.starts_with("gptp-");
        let template = name.contains("@.");
        let relevant = TIME_SERVICES.contains(&name.as_str())
            || ["ptp4l", "phc2sys", "timemaster"]
                .iter()
                .any(|k| name.contains(k));
        if ours || template || !relevant || file_state == "masked" {
            continue;
        }
        if file_state.starts_with("enabled") {
            p.disable.push((name.clone(), "enabled".into()));
        } else if active == "active" {
            p.disable.push((name.clone(), "active".into()));
        }
    }
    if mode == TimeSync::Builtin {
        return p;
    }

    let cfg_path = format!("{LINUXPTP_DIR}/gptp-gm-{iface}.cfg");
    p.files.push((
        cfg_path.clone(),
        ptp4l_config(
            iface,
            hardware,
            thresh_ns,
            utc_offset,
            supports_ro_socket(ptp4l_version),
        ),
    ));
    let ptp4l_unit = format!("gptp-ptp4l-{iface}.service");
    p.files.push((
        format!("{UNIT_DIR}/{ptp4l_unit}"),
        unit(
            &format!("gPTP slave (ptp4l) on {iface}"),
            &format!(
                "After=network-online.target sys-subsystem-net-devices-{iface}.device\n\
                 Wants=network-online.target\n"
            ),
            &format!("{ptp4l} -f {cfg_path} -m -q"),
        ),
    ));
    p.units.push(ptp4l_unit.clone());
    let after = format!("After={ptp4l_unit}\nRequires={ptp4l_unit}\n");
    let uds = uds_address(iface);
    if hardware {
        // System clock (UTC) <- NIC clock (TAI): -O is slave - master.
        let name = format!("gptp-phc2sys-{iface}-sysclock.service");
        p.files.push((
            format!("{UNIT_DIR}/{name}"),
            unit(
                &format!("System clock from the {iface} hardware clock (phc2sys)"),
                &after,
                &format!(
                    "{phc2sys} -s {iface} -c CLOCK_REALTIME -O -{utc_offset} -w -z {uds} \
                     -S 1.0 -m -q"
                ),
            ),
        ));
        p.units.push(name);
    }
    for dst in sync_phcs {
        let name = format!("gptp-phc2sys-{iface}-{dst}.service");
        // NIC clock <- NIC clock (both TAI), or <- system clock (UTC) with
        // software timestamping, where ptp4l steers the system clock.
        let exec = if hardware {
            format!("{phc2sys} -s {iface} -c {dst} -O 0 -w -z {uds} -S 1.0 -m -q")
        } else {
            format!("{phc2sys} -s CLOCK_REALTIME -c {dst} -O {utc_offset} -w -z {uds} -S 1.0 -m -q")
        };
        p.files.push((
            format!("{UNIT_DIR}/{name}"),
            unit(
                &format!("{dst} hardware clock from {iface} (phc2sys)"),
                &after,
                &exec,
            ),
        ));
        p.units.push(name);
    }
    p
}

/// Recorded setup: our files/units and what we disabled.
#[derive(Debug, Default, PartialEq)]
pub struct State {
    pub mode: String,
    pub files: Vec<String>,
    pub units: Vec<String>,
    pub disabled: Vec<(String, String)>,
}

impl State {
    pub fn parse(text: &str) -> Self {
        let mut s = State::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k {
                "mode" => s.mode = v.into(),
                "file" => s.files.push(v.into()),
                "unit" => s.units.push(v.into()),
                "disabled" => {
                    if let Some((name, how)) = v.split_once(':') {
                        s.disabled.push((name.into(), how.into()));
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn render(&self) -> String {
        let mut out = String::from("# gptp-gm time sync setup; used by --remove-time-sync\n");
        out += &format!("mode={}\n", self.mode);
        for f in &self.files {
            out += &format!("file={f}\n");
        }
        for u in &self.units {
            out += &format!("unit={u}\n");
        }
        for (n, h) in &self.disabled {
            out += &format!("disabled={n}:{h}\n");
        }
        out
    }

    pub fn load() -> Option<Self> {
        std::fs::read_to_string(STATE_FILE)
            .ok()
            .map(|t| Self::parse(&t))
    }
}

/// Our linuxptp units and their `systemctl is-active` state (for the
/// monitor's health display).
pub fn unit_status() -> Vec<(String, String)> {
    let Some(state) = State::load() else {
        return Vec::new();
    };
    state
        .units
        .iter()
        .map(|u| {
            let out = Command::new("systemctl").args(["is-active", u]).output();
            let s = out
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".into());
            (u.trim_end_matches(".service").to_string(), s)
        })
        .collect()
}

/// Performs (or with --dry-run prints) the system changes.
struct Sys {
    dry_run: bool,
}

impl Sys {
    fn systemctl(&self, args: &[&str]) -> Result<()> {
        println!("  systemctl {}", args.join(" "));
        if self.dry_run {
            return Ok(());
        }
        let st = Command::new("systemctl")
            .args(args)
            .status()
            .context("running systemctl")?;
        if !st.success() {
            bail!("systemctl {} failed", args.join(" "));
        }
        Ok(())
    }

    fn write(&self, path: &str, content: &str) -> Result<()> {
        println!("  write {path}");
        if self.dry_run {
            for l in content.lines() {
                println!("    | {l}");
            }
            return Ok(());
        }
        if let Some(dir) = Path::new(path).parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, content).with_context(|| format!("writing {path}"))
    }

    fn remove(&self, path: &str) -> Result<()> {
        println!("  remove {path}");
        if self.dry_run {
            return Ok(());
        }
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {path}"))
            }
            _ => Ok(()),
        }
    }
}

/// Installed services: (name, unit-file state, active state).
fn list_units() -> Result<Vec<(String, String, String)>> {
    let run = |args: &[&str]| -> Result<String> {
        let o = Command::new("systemctl")
            .args(args)
            .output()
            .context("running systemctl (is this a systemd system?)")?;
        Ok(String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let files = run(&[
        "list-unit-files",
        "--type=service",
        "--no-legend",
        "--plain",
    ])?;
    let units = run(&[
        "list-units",
        "--type=service",
        "--all",
        "--no-legend",
        "--plain",
    ])?;
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut file_state = std::collections::HashMap::new();
    let mut active = std::collections::HashMap::new();
    for l in files.lines() {
        let mut f = l.split_whitespace();
        if let (Some(n), Some(s)) = (f.next(), f.next()) {
            names.insert(n.into());
            file_state.insert(n.to_string(), s.to_string());
        }
    }
    for l in units.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() >= 3 {
            names.insert(f[0].into());
            active.insert(f[0].to_string(), f[2].to_string());
        }
    }
    Ok(names
        .into_iter()
        .map(|n| {
            let fs = file_state.get(&n).cloned().unwrap_or_default();
            let a = active.get(&n).cloned().unwrap_or_default();
            (n, fs, a)
        })
        .collect())
}

fn which(name: &str) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let found = path
        .split(':')
        .chain(["/usr/sbin", "/sbin", "/usr/local/sbin"])
        .map(|d| format!("{d}/{name}"))
        .find(|p| Path::new(p).is_file());
    found
}

fn require_root(dry_run: bool) -> Result<()> {
    if !dry_run && unsafe { libc::geteuid() } != 0 {
        bail!("must run as root (or try --dry-run to see what would change)");
    }
    Ok(())
}

/// `--setup-time-sync MODE`.
pub fn setup(cfg: &Config, mode: TimeSync) -> Result<()> {
    require_root(cfg.dry_run)?;
    let iface = cfg.iface();
    for x in std::iter::once(iface).chain(cfg.sync_phc.iter().map(|s| s.as_str())) {
        if !Path::new(&format!("/sys/class/net/{x}")).exists() {
            bail!("interface {x} not found");
        }
    }
    let hardware = crate::net::phc_index(iface)?.is_some();
    for x in &cfg.sync_phc {
        if crate::net::phc_index(x)?.is_none() {
            bail!("{x} has no PTP hardware clock to sync");
        }
    }
    let (ptp4l, phc2sys) = if mode == TimeSync::Linuxptp {
        let missing = || "linuxptp is not installed (e.g. sudo apt install linuxptp)";
        (
            which("ptp4l").context(missing())?,
            which("phc2sys").context(missing())?,
        )
    } else {
        (String::new(), String::new())
    };
    let version = if mode == TimeSync::Linuxptp {
        Command::new(&ptp4l)
            .arg("-v")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let plan = plan(
        mode,
        iface,
        &cfg.sync_phc,
        hardware,
        cfg.neighbor_prop_delay_thresh,
        cfg.utc_offset,
        &ptp4l,
        &version,
        &phc2sys,
        &list_units()?,
    );
    let old = State::load().unwrap_or_default();
    let sys = Sys {
        dry_run: cfg.dry_run,
    };
    println!(
        "{}time sync setup ({}) on {iface}{}:",
        if cfg.dry_run { "[dry run] " } else { "" },
        plan.mode,
        if mode == TimeSync::Linuxptp {
            if hardware {
                ", hardware timestamping"
            } else {
                ", software timestamping"
            }
        } else {
            ""
        }
    );

    // Units of a previous setup that this one does not keep.
    for u in old.units.iter().filter(|u| !plan.units.contains(u)) {
        let _ = sys.systemctl(&["disable", "--now", u]);
    }
    for f in old
        .files
        .iter()
        .filter(|f| !plan.files.iter().any(|(p, _)| p == *f))
    {
        sys.remove(f)?;
    }
    for (name, _) in &plan.disable {
        sys.systemctl(&["disable", "--now", name])?;
    }
    for (path, content) in &plan.files {
        sys.write(path, content)?;
    }
    if !plan.units.is_empty() {
        sys.systemctl(&["daemon-reload"])?;
        let mut args = vec!["enable", "--now"];
        args.extend(plan.units.iter().map(|u| u.as_str()));
        sys.systemctl(&args)?;
    }

    // Keep the original record of what we disabled across repeated setups.
    let mut disabled = old.disabled.clone();
    for d in &plan.disable {
        if !disabled.iter().any(|(n, _)| n == &d.0) {
            disabled.push(d.clone());
        }
    }
    let state = State {
        mode: plan.mode.clone(),
        files: plan.files.iter().map(|(p, _)| p.clone()).collect(),
        units: plan.units.clone(),
        disabled,
    };
    sys.write(STATE_FILE, &state.render())?;

    println!();
    if plan.disable.is_empty() {
        println!("No conflicting time services were active.");
    } else {
        println!(
            "Disabled (restored by --remove-time-sync): {}",
            plan.disable
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    match mode {
        TimeSync::Builtin => println!(
            "Run the monitor with --time-sync builtin (needs CAP_SYS_TIME){}.",
            if cfg.sync_phc.is_empty() {
                String::new()
            } else {
                format!(
                    " and {}",
                    cfg.sync_phc
                        .iter()
                        .map(|p| format!("--sync-phc {p}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            }
        ),
        TimeSync::Linuxptp => println!(
            "Check: journalctl -fu gptp-ptp4l-{iface} (port state SLAVE), then run the \
             monitor with --time-sync linuxptp (listen-only beside ptp4l)."
        ),
    }
    Ok(())
}

/// `--remove-time-sync`.
pub fn remove(cfg: &Config) -> Result<()> {
    require_root(cfg.dry_run)?;
    let Some(state) = State::load() else {
        println!("No time sync setup recorded ({STATE_FILE}) — nothing to remove.");
        return Ok(());
    };
    let sys = Sys {
        dry_run: cfg.dry_run,
    };
    println!(
        "{}removing time sync setup ({}):",
        if cfg.dry_run { "[dry run] " } else { "" },
        state.mode
    );
    for u in &state.units {
        let _ = sys.systemctl(&["disable", "--now", u]);
    }
    for f in &state.files {
        sys.remove(f)?;
    }
    if !state.units.is_empty() {
        sys.systemctl(&["daemon-reload"])?;
    }
    for (name, how) in &state.disabled {
        let r = if how == "enabled" {
            sys.systemctl(&["enable", "--now", name])
        } else {
            sys.systemctl(&["start", name])
        };
        if let Err(e) = r {
            println!("  warning: could not restore {name}: {e:#}");
        }
    }
    sys.remove(STATE_FILE)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units() -> Vec<(String, String, String)> {
        [
            ("systemd-timesyncd.service", "enabled", "active"),
            ("chrony.service", "disabled", "inactive"),
            ("ptp4l-mgbe0.service", "enabled", "active"),
            ("nv_ptp4l_slave_mgbe3_0.service", "masked", "inactive"),
            ("phc2sys-mgbe0-sysclock.service", "enabled", "failed"),
            ("ptp4l@.service", "disabled", ""),
            ("ptp4l@eth1.service", "", "active"),
            ("gptp-ptp4l-mgbe0_0.service", "enabled", "active"),
            ("ssh.service", "enabled", "active"),
        ]
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect()
    }

    fn linuxptp(hardware: bool, phcs: &[String]) -> Plan {
        plan(
            TimeSync::Linuxptp,
            "mgbe0_0",
            phcs,
            hardware,
            800,
            37,
            "/usr/sbin/ptp4l",
            "4.0",
            "/usr/sbin/phc2sys",
            &units(),
        )
    }

    #[test]
    fn conflicts_are_found_but_not_ours_masked_templates_or_unrelated() {
        let p = linuxptp(true, &[]);
        let names: Vec<&str> = p.disable.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            [
                "systemd-timesyncd.service",
                "ptp4l-mgbe0.service",
                "phc2sys-mgbe0-sysclock.service",
                "ptp4l@eth1.service"
            ]
        );
        assert_eq!(p.disable[3].1, "active");
    }

    #[test]
    fn linuxptp_hardware_plan_has_ptp4l_and_phc2sys_with_utc_offset() {
        let p = linuxptp(true, &["mgbe3_0".into()]);
        assert_eq!(
            p.units,
            [
                "gptp-ptp4l-mgbe0_0.service",
                "gptp-phc2sys-mgbe0_0-sysclock.service",
                "gptp-phc2sys-mgbe0_0-mgbe3_0.service"
            ]
        );
        let cfg = &p.files[0].1;
        assert!(cfg.contains("time_stamping           hardware"));
        assert!(cfg.contains("transportSpecific       0x1"));
        assert!(cfg.contains("delay_mechanism         P2P"));
        assert!(cfg.contains("utc_offset              37"));
        assert!(cfg.contains("\n[mgbe0_0]\n"));
        assert!(cfg.contains("uds_ro_address          /var/run/gptp-ptp4l-mgbe0_0-ro"));
        let sysclock = &p.files[2].1;
        assert!(sysclock.contains("-c CLOCK_REALTIME -O -37 -w -z /var/run/gptp-ptp4l-mgbe0_0"));
        assert!(sysclock.contains("Requires=gptp-ptp4l-mgbe0_0.service"));
        let phc = &p.files[3].1;
        assert!(phc.contains("-s mgbe0_0 -c mgbe3_0 -O 0"));
    }

    #[test]
    fn software_timestamping_lets_ptp4l_steer_the_system_clock() {
        let p = linuxptp(false, &["mgbe3_0".into()]);
        assert!(p.files[0].1.contains("time_stamping           software"));
        assert!(!p.units.iter().any(|u| u.ends_with("sysclock.service")));
        assert!(p
            .files
            .last()
            .unwrap()
            .1
            .contains("-s CLOCK_REALTIME -c mgbe3_0 -O 37"));
    }

    #[test]
    fn servo_plan_only_disables_conflicts() {
        let p = plan(
            TimeSync::Builtin,
            "eth0",
            &[],
            true,
            800,
            37,
            "",
            "",
            "",
            &units(),
        );
        assert!(p.files.is_empty() && p.units.is_empty());
        assert_eq!(p.disable.len(), 4);
    }

    #[test]
    fn ro_socket_only_where_supported() {
        assert!(supports_ro_socket("4.0"));
        assert!(supports_ro_socket("3.1.1"));
        assert!(!supports_ro_socket("3.0"));
        assert!(!supports_ro_socket("2.0"));
        assert!(!supports_ro_socket(""));
        assert!(!ptp4l_config("eth0", true, 800, 37, false).contains("uds_ro_address"));
    }

    #[test]
    fn state_round_trip() {
        let s = State {
            mode: "linuxptp".into(),
            files: vec!["/etc/linuxptp/gptp-gm-eth0.cfg".into()],
            units: vec!["gptp-ptp4l-eth0.service".into()],
            disabled: vec![
                ("chrony.service".into(), "enabled".into()),
                ("ptp4l@eth1.service".into(), "active".into()),
            ],
        };
        assert_eq!(State::parse(&s.render()), s);
    }
}
