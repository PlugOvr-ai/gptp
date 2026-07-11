//! Command-line configuration.

use clap::Parser;

/// gPTP (IEEE 802.1AS) grandmaster with timed handover — NTS Pico3 replacement.
///
/// Sends Announce + Sync/Follow_Up as forced grandmaster on one interface and
/// answers peer-delay requests. With --handover, announcing stops after the
/// given number of minutes so the automotive Ethernet switch takes over as GM.
#[derive(Parser, Clone, Debug)]
#[command(version, about)]
pub struct Config {
    /// Network interface to serve gPTP on (e.g. eth0)
    pub interface: String,

    /// Serve time for this many minutes, then hand over to the switch.
    /// Omit to act as grandmaster indefinitely. Fractions allowed (0.5 = 30 s).
    #[arg(long, value_name = "MINUTES")]
    pub handover: Option<f64>,

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

    /// Run without the TUI and log status lines to stdout instead
    #[arg(long)]
    pub no_tui: bool,
}
