//! `--seed-from-gnss`: before announcing, take the time and frequency from
//! a GNSS-locked master that is already on the network, then take over as
//! grandmaster with our own (better) priority. The network keeps GNSS time
//! across the takeover instead of jumping to whatever our clock says.
//!
//! Only the current master's Sync reaches us (a bridge forwards the best
//! master's Announce and regenerates its Sync), so the samples are taken
//! from the port whose Announce names a GNSS-locked grandmaster.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::monitor::{fit_line, fmt_ns};
use crate::ptp::{self, AnnounceData, Msg, Timestamp};

/// Sync measured before seeding: long enough for the frequency.
const WINDOW: Duration = Duration::from_secs(8);
const MIN_SAMPLES: usize = 16;
/// A measured frequency error beyond this is not a crystal's: the time
/// is still taken over, the frequency is not.
const MAX_RATE_PPB: f64 = 200_000.0;

/// clockClass 6: synchronized to a primary reference time source (GNSS);
/// 7: was, and is in holdover within specification. Degraded classes
/// (52, 187, 248, ...) do not qualify — their time may be seconds off.
pub fn gnss_locked(d: &AnnounceData) -> bool {
    matches!(d.clock_class, 6 | 7)
}

/// The served clock relative to the GNSS master.
pub struct Seed {
    pub gm: AnnounceData,
    /// Served time − master time at `t0`, ns.
    a: f64,
    /// Our clock's rate error against the master, ppb (positive: fast).
    pub rate_ppb: f64,
    /// Served time of the first sample, ns.
    t0: i128,
    pub sigma_ns: f64,
    pub samples: usize,
}

impl Seed {
    /// Served − master at served time `t`, ns.
    pub fn offset_at(&self, t: i128) -> f64 {
        self.a + self.rate_ppb * (t - self.t0) as f64 / 1e9
    }

    /// The frequency correction to apply, if plausible.
    pub fn rate_usable(&self) -> bool {
        self.rate_ppb.abs() <= MAX_RATE_PPB
    }
}

pub struct Seeder {
    pub deadline: Instant,
    /// Port whose Announce names a GNSS-locked grandmaster, and that GM.
    source: Option<([u8; 10], AnnounceData)>,
    /// Two-step Sync awaiting its Follow_Up: (seq, receive time, correction).
    pending: Option<(u16, Timestamp, i64)>,
    /// (served time − t0 in s, served − master in ns)
    samples: VecDeque<(f64, f64)>,
    t0: Option<i128>,
    first_at: Option<Instant>,
}

impl Seeder {
    pub fn new(deadline: Instant) -> Self {
        Seeder {
            deadline,
            source: None,
            pending: None,
            samples: VecDeque::new(),
            t0: None,
            first_at: None,
        }
    }

    fn restart(&mut self) {
        self.pending = None;
        self.samples.clear();
        self.t0 = None;
        self.first_at = None;
    }

    /// Feed a received message (`t_rx`: its receive time on the served
    /// clock). Returns log lines.
    pub fn handle(
        &mut self,
        msg: &Msg,
        t_rx: Timestamp,
        link_delay_ns: Option<f64>,
        now: Instant,
    ) -> Vec<String> {
        let mut logs = Vec::new();
        let from_source = |s: &[u8; 10], src: &Option<([u8; 10], AnnounceData)>| {
            src.as_ref().is_some_and(|(p, _)| p == s)
        };
        match *msg {
            Msg::Announce { source, data, .. } => {
                let ours = from_source(&source, &self.source);
                if gnss_locked(&data) {
                    let same_gm = self
                        .source
                        .as_ref()
                        .is_some_and(|(_, gm)| gm.gm_identity == data.gm_identity);
                    if !ours || !same_gm {
                        logs.push(format!(
                            "Seeding: {} is GNSS-locked (clockClass {}, time source 0x{:02X}) — \
                             measuring its time and frequency",
                            ptp::fmt_clock_identity(&data.gm_identity),
                            data.clock_class,
                            data.time_source
                        ));
                        self.restart();
                    }
                    self.source = Some((source, data));
                } else if ours {
                    logs.push(format!(
                        "Seeding: {} is no longer GNSS-locked (clockClass {}) — waiting",
                        ptp::fmt_clock_identity(&data.gm_identity),
                        data.clock_class
                    ));
                    self.source = None;
                    self.restart();
                }
            }
            Msg::Sync {
                source,
                seq,
                two_step,
                origin,
                correction_ns,
                ..
            } if from_source(&source, &self.source) => {
                if two_step {
                    self.pending = Some((seq, t_rx, correction_ns));
                } else {
                    self.sample(origin, correction_ns, t_rx, link_delay_ns, now);
                }
            }
            Msg::FollowUp {
                source,
                seq,
                precise_origin,
                correction_ns,
                ..
            } if from_source(&source, &self.source) => {
                if let Some((s, t_rx, sync_corr)) = self.pending {
                    if s == seq {
                        self.pending = None;
                        self.sample(
                            precise_origin,
                            sync_corr + correction_ns,
                            t_rx,
                            link_delay_ns,
                            now,
                        );
                    }
                }
            }
            _ => {}
        }
        logs
    }

    fn sample(
        &mut self,
        origin: Timestamp,
        correction_ns: i64,
        t_rx: Timestamp,
        link_delay_ns: Option<f64>,
        now: Instant,
    ) {
        // Without the link delay the offset would be off by it.
        let Some(delay) = link_delay_ns else { return };
        let rx = t_rx.to_ns();
        let master = origin.to_ns() + correction_ns as i128;
        let t0 = *self.t0.get_or_insert(rx);
        self.first_at.get_or_insert(now);
        self.samples
            .push_back(((rx - t0) as f64 / 1e9, (rx - master) as f64 - delay));
    }

    /// The seed, once enough Sync was measured.
    pub fn ready(&self, now: Instant) -> Option<Seed> {
        let (_, gm) = self.source.as_ref()?;
        let first = self.first_at?;
        if now.duration_since(first) < WINDOW || self.samples.len() < MIN_SAMPLES {
            return None;
        }
        let fit = fit_line(&self.samples);
        Some(Seed {
            gm: *gm,
            a: fit.a,
            rate_ppb: fit.b,
            t0: self.t0?,
            sigma_ns: fit.sigma,
            samples: self.samples.len(),
        })
    }

    /// What is being waited for, for the give-up message.
    pub fn waiting_for(&self) -> String {
        match &self.source {
            None => "no GNSS-locked master (clockClass 6/7) seen".into(),
            Some((_, gm)) => format!(
                "{} announced, but only {} usable Sync{} received",
                ptp::fmt_clock_identity(&gm.gm_identity),
                self.samples.len(),
                if self.samples.len() == 1 { "" } else { "s" }
            ),
        }
    }
}

/// Log line for an applied seed.
pub fn describe(seed: &Seed, step_ns: f64) -> String {
    format!(
        "Seeded from GNSS master {}: stepped {}, frequency {} (σ {}, {} Syncs) — \
         taking over as grandmaster",
        ptp::fmt_clock_identity(&seed.gm.gm_identity),
        fmt_ns(-step_ns),
        if seed.rate_usable() {
            format!("{:+.0} ppb", -seed.rate_ppb)
        } else {
            format!(
                "not changed ({:+.0} ppb measured — implausible)",
                -seed.rate_ppb
            )
        },
        fmt_ns(seed.sigma_ns),
        seed.samples
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: [u8; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 0, 1];
    const OTHER: [u8; 10] = [9, 9, 9, 9, 9, 9, 9, 9, 0, 1];

    fn announce(port: [u8; 10], class: u8) -> Msg {
        Msg::Announce {
            source: port,
            log_interval: 0,
            data: AnnounceData {
                priority1: 128,
                clock_class: class,
                clock_accuracy: 0x21,
                variance: 0x4E5D,
                priority2: 128,
                gm_identity: [0xAA; 8],
                steps_removed: 1,
                time_source: 0x20,
            },
            utc_offset: 37,
            utc_offset_valid: true,
        }
    }

    /// Feed `n` two-step Syncs at 8/s from `port`: our clock is `off_ns`
    /// ahead of the master and runs `rate_ppb` fast; link delay 500 ns.
    fn feed(s: &mut Seeder, port: [u8; 10], n: u16, off_ns: f64, rate_ppb: f64) -> Instant {
        let start = Instant::now();
        let master0: i128 = 1_700_000_000_000_000_000;
        let mut now = start;
        for seq in 0..n {
            let dt = seq as i128 * 125_000_000;
            let master_rx = master0 + dt;
            let ours = master_rx + (off_ns + rate_ppb * dt as f64 / 1e9) as i128;
            // The bridge's Sync left 2 µs before (residence in correction).
            let origin = master_rx - 500 - 2_000;
            let t_rx = Timestamp::from_ns(ours);
            now = start + Duration::from_millis(125 * seq as u64);
            let sync = Msg::Sync {
                source: port,
                seq,
                log_interval: -3,
                two_step: true,
                origin: Timestamp::default(),
                correction_ns: 1_500,
            };
            let fup = Msg::FollowUp {
                source: port,
                seq,
                precise_origin: Timestamp::from_ns(origin),
                correction_ns: 500,
                info: None,
            };
            s.handle(&sync, t_rx, Some(500.0), now);
            s.handle(&fup, t_rx, Some(500.0), now);
        }
        now
    }

    #[test]
    fn seeds_time_and_frequency_from_a_gnss_master() {
        let mut s = Seeder::new(Instant::now() + Duration::from_secs(60));
        assert_eq!(
            s.handle(
                &announce(PORT, 6),
                Timestamp::default(),
                None,
                Instant::now()
            )
            .len(),
            1
        );
        let now = feed(&mut s, PORT, 80, 1.384e9, 3_000.0);
        let seed = s.ready(now).expect("10 s of Sync is enough");
        assert!(
            (seed.rate_ppb - 3_000.0).abs() < 1.0,
            "rate {}",
            seed.rate_ppb
        );
        // At the first sample our clock was 1.384 s ahead.
        assert!((seed.offset_at(seed.t0) - 1.384e9).abs() < 10.0);
        assert!(seed.rate_usable());
    }

    #[test]
    fn needs_a_full_window() {
        let mut s = Seeder::new(Instant::now() + Duration::from_secs(60));
        s.handle(
            &announce(PORT, 6),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        let now = feed(&mut s, PORT, 40, 0.0, 0.0); // 5 s
        assert!(s.ready(now).is_none());
    }

    #[test]
    fn ignores_degraded_masters_and_other_ports() {
        let mut s = Seeder::new(Instant::now() + Duration::from_secs(60));
        // Out of holdover (52) and free-running (248): not a seed.
        s.handle(
            &announce(PORT, 52),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        s.handle(
            &announce(OTHER, 248),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        let now = feed(&mut s, PORT, 80, 0.0, 0.0);
        assert!(s.ready(now).is_none());
        assert!(s.waiting_for().starts_with("no GNSS-locked master"));

        // Locked on PORT, but Sync from another port is not its time.
        s.handle(
            &announce(PORT, 6),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        let now = feed(&mut s, OTHER, 80, 0.0, 0.0);
        assert!(s.ready(now).is_none());
    }

    #[test]
    fn losing_the_lock_restarts_the_measurement() {
        let mut s = Seeder::new(Instant::now() + Duration::from_secs(60));
        s.handle(
            &announce(PORT, 6),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        feed(&mut s, PORT, 40, 0.0, 0.0);
        let logs = s.handle(
            &announce(PORT, 52),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        assert!(logs[0].contains("no longer GNSS-locked"));
        assert!(s.samples.is_empty());
    }

    #[test]
    fn no_samples_without_link_delay() {
        let mut s = Seeder::new(Instant::now() + Duration::from_secs(60));
        s.handle(
            &announce(PORT, 7),
            Timestamp::default(),
            None,
            Instant::now(),
        );
        let sync = Msg::Sync {
            source: PORT,
            seq: 1,
            log_interval: -3,
            two_step: false,
            origin: Timestamp::from_ns(1_000),
            correction_ns: 0,
        };
        s.handle(&sync, Timestamp::from_ns(2_000), None, Instant::now());
        assert!(s.samples.is_empty());
    }
}
