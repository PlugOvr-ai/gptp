//! Command-line configuration.

use clap::{Parser, ValueEnum};

/// How the slave monitor keeps the Linux system time in sync.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeSync {
    /// gptp-gm keeps the clocks in sync itself — the receiving NIC's
    /// hardware clock (with hardware timestamps), the system clock, and
    /// --sync-phc NICs; replaces ptp4l + phc2sys ("servo" also accepted)
    #[value(alias = "servo")]
    Builtin,
    /// linuxptp (ptp4l + phc2sys, set up with --setup-time-sync) keeps the
    /// NIC hardware clocks and the system clock in sync; gptp-gm monitors
    /// listen-only beside it
    Linuxptp,
}

/// Transport of the IEEE 1588 PTP served beside gPTP (`--ptp`).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtpTransport {
    /// UDP/IPv4 multicast 224.0.1.129, ports 319/320 (most devices)
    Udp,
    /// Ethernet, EtherType 0x88F7 to 01:1B:19:00:00:00
    L2,
}

/// Where the grandmaster's time comes from.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GmClock {
    /// phc-disciplined with hardware timestamps, system otherwise
    Auto,
    /// The system clock: each hardware stamp is converted with a fresh
    /// NIC/system clock reading (its noise and NTP adjustments reach the
    /// network directly)
    System,
    /// The NIC hardware clock alone: set once from the system clock, then
    /// free-running — smoothest, but drifts from UTC with the NIC's crystal
    Phc,
    /// The NIC hardware clock, steered slowly toward the system clock (like
    /// phc2sys): smooth and on UTC
    PhcDisciplined,
}

/// gPTP (IEEE 802.1AS) grandmaster with timed handover, and slave monitor.
///
/// Sends Announce + Sync/Follow_Up as forced grandmaster on one interface and
/// answers peer-delay requests. With --handover, announcing stops after the
/// given number of minutes so the automotive Ethernet switch takes over as GM.
/// With --monitor, acts as a passive slave instead and reports jumps and
/// instability in the master's time; --time-sync keeps the system time in
/// sync with the master.
///
/// --setup-time-sync / --remove-time-sync configure the system for that
/// (one-shot, as root) and exit.
#[derive(Parser, Clone, Debug)]
#[command(version, about)]
pub struct Config {
    /// Network interface to serve gPTP on (e.g. eth0)
    #[arg(required_unless_present = "remove_time_sync")]
    pub interface: Option<String>,

    /// Serve time for this many minutes, then hand over to the switch.
    /// Omit to act as grandmaster indefinitely. Fractions allowed (0.5 = 30 s).
    #[arg(long, value_name = "MINUTES", conflicts_with = "monitor")]
    pub handover: Option<f64>,

    /// Slave monitor mode: never announce; follow the best master on the
    /// link and report time steps, spikes, frequency changes, jitter, Sync
    /// loss and GM changes. No clock is adjusted unless --time-sync builtin.
    #[arg(long)]
    pub monitor: bool,

    /// Grandmaster time source (needs hardware timestamps for phc modes)
    #[arg(
        long,
        value_enum,
        value_name = "SOURCE",
        default_value = "auto",
        conflicts_with = "monitor"
    )]
    pub gm_clock: GmClock,

    /// Before announcing, take the time and frequency from a GNSS-locked
    /// master already on the network (clockClass 6 or 7, usually behind the
    /// switch), then take over with our priority — the network stays on
    /// GNSS time instead of jumping to ours. Sets the system clock (needs
    /// CAP_SYS_TIME). Gives up after --seed-wait and announces unseeded.
    #[arg(long, conflicts_with = "monitor")]
    pub seed_from_gnss: bool,

    /// Seconds to wait for a GNSS-locked master with --seed-from-gnss
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 60.0,
        requires = "seed_from_gnss"
    )]
    pub seed_wait: f64,

    /// Also serve IEEE 1588 PTP (default profile: two-step, end-to-end
    /// delay) for devices that do not speak gPTP — the same time from the
    /// same clock, beside gPTP, while we are grandmaster
    #[arg(long, value_enum, value_name = "TRANSPORT", conflicts_with = "monitor")]
    pub ptp: Option<PtpTransport>,

    /// 1588 domainNumber (--ptp); the devices' domain
    #[arg(long, value_name = "N", default_value_t = 0, requires = "ptp")]
    pub ptp_domain: u8,

    /// 1588 log2 Sync interval in seconds (--ptp; 0 = 1 per second)
    #[arg(
        long,
        value_name = "LOG2",
        default_value_t = 0,
        requires = "ptp",
        allow_negative_numbers = true
    )]
    pub ptp_log_sync_interval: i8,

    /// 1588 log2 Announce interval in seconds (--ptp; 1 = every 2 s)
    #[arg(
        long,
        value_name = "LOG2",
        default_value_t = 1,
        requires = "ptp",
        allow_negative_numbers = true
    )]
    pub ptp_log_announce_interval: i8,

    /// Monitor: a deviation from the drift model beyond this is a spike
    /// (one sample) or a step (persistent). Default 1000 ns with hardware
    /// timestamps, 250000 ns with software timestamps.
    #[arg(long, value_name = "NS", requires = "monitor")]
    pub jump_threshold: Option<f64>,

    /// Monitor: a change of the master's frequency beyond this is reported.
    /// Default 100 ppb with hardware timestamps, 5000 ppb with software.
    #[arg(long, value_name = "PPB", requires = "monitor")]
    pub freq_threshold: Option<f64>,

    /// Monitor: on every alarm (step, jitter burst, GM change, Sync loss,
    /// ...) write a per-Sync CSV from ~30 s before to ~10 s after it into
    /// this directory, for offline analysis
    #[arg(long, value_name = "DIR", requires = "monitor")]
    pub capture_dir: Option<String>,

    /// Monitor: send nothing (no peer delay), only listen. Needed when
    /// another gPTP stack such as ptp4l serves the same port — two
    /// responders on one link make the bridge stop sending Sync. Enabled
    /// automatically when ptp4l is running or with --time-sync linuxptp.
    #[arg(long, requires = "monitor")]
    pub listen_only: bool,

    /// Monitor: keep the Linux system time in sync with the master
    #[arg(long, value_name = "MODE", requires = "monitor")]
    pub time_sync: Option<TimeSync>,

    /// Configure this system for --time-sync MODE and exit (as root):
    /// disables conflicting time services (recorded, restored by
    /// --remove-time-sync); for linuxptp also writes the gPTP ptp4l config
    /// and ptp4l/phc2sys systemd units for this interface
    #[arg(long, value_name = "MODE", conflicts_with = "remove_time_sync")]
    pub setup_time_sync: Option<TimeSync>,

    /// Also keep this NIC's hardware clock in sync, e.g. a second port used
    /// for sensor timestamps. Repeatable. With --time-sync builtin at runtime,
    /// with --setup-time-sync linuxptp via phc2sys.
    #[arg(long, value_name = "IFACE")]
    pub sync_phc: Vec<String>,

    /// With --setup-time-sync linuxptp: ptp4l neighborPropDelayThresh in ns
    /// (the link is not used above it; 800 is the 802.1AS default)
    #[arg(long, value_name = "NS", default_value_t = 800)]
    pub neighbor_prop_delay_thresh: u32,

    /// Undo --setup-time-sync: stop and remove the units and config it
    /// created and re-enable the time services it disabled; then exit
    #[arg(long)]
    pub remove_time_sync: bool,

    /// With --setup-time-sync / --remove-time-sync: print what would be
    /// done (files, systemctl commands) without changing anything
    #[arg(long)]
    pub dry_run: bool,

    /// grandmasterPriority1 — lower wins the BMCA; 1 outranks any default
    /// device (switches/endpoints typically use 246-255)
    #[arg(long, default_value_t = 1)]
    pub priority1: u8,

    /// grandmasterPriority2 (tie-breaker after clock quality)
    #[arg(long, default_value_t = 1)]
    pub priority2: u8,

    /// clockClass advertised in Announce (248 = default, free-running)
    #[arg(long, default_value_t = 248)]
    pub clock_class: u8,

    /// clockAccuracy advertised in Announce (0xFE = unknown)
    #[arg(long, default_value_t = 0xFE)]
    pub clock_accuracy: u8,

    /// offsetScaledLogVariance advertised in Announce
    #[arg(long, default_value_t = 0x4E5D)]
    pub variance: u16,

    /// timeSource advertised in Announce (0xA0 = internal oscillator,
    /// 0x20 = GPS, 0x50 = NTP)
    #[arg(long, default_value_t = 0xA0)]
    pub time_source: u8,

    /// gPTP domain number
    #[arg(long, default_value_t = 0)]
    pub domain: u8,

    /// log2 of the Sync interval in seconds (-3 = 8 per second, gPTP default)
    #[arg(long, default_value_t = -3)]
    pub log_sync_interval: i8,

    /// log2 of the Announce interval in seconds (0 = 1 per second, gPTP default)
    #[arg(long, default_value_t = 0)]
    pub log_announce_interval: i8,

    /// UTC-to-TAI offset in seconds, used when the kernel TAI offset is unset
    #[arg(long, default_value_t = 37)]
    pub utc_offset: i16,

    /// Do not use NIC hardware timestamping even if available
    #[arg(long)]
    pub force_sw_timestamps: bool,

    /// Fault injection for tests: serve time this many seconds off
    /// (fractions allowed, negative = behind)
    #[arg(long, value_name = "SECONDS", default_value_t = 0.0, hide = true)]
    pub time_error: f64,

    /// Actively ARP-sweep this subnet (e.g. 10.0.0.0/24, max /22) to find
    /// nodes that never talk on their own
    #[arg(long, value_name = "CIDR")]
    pub scan_subnet: Option<String>,

    /// Disable the node inventory (passive discovery + ICMP/ARP latency probes)
    #[arg(long)]
    pub no_inventory: bool,

    /// Names file for the inventory: one "<mac|ip> = <display name>" per
    /// line. Curated names always win over discovered ones.
    #[arg(long, value_name = "FILE")]
    pub names: Option<String>,

    /// Append a JSONL metrics snapshot (gPTP counters, link errors, nodes,
    /// streams) to this file every 5 s, for offline fault correlation
    #[arg(long, value_name = "FILE")]
    pub log_metrics: Option<String>,

    /// Run without the TUI and log status lines to stdout instead
    #[arg(long)]
    pub no_tui: bool,
}

impl Config {
    /// The network interface; present unless only --remove-time-sync runs.
    pub fn iface(&self) -> &str {
        self.interface.as_deref().unwrap_or("")
    }
}
