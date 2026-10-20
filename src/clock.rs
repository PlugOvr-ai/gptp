//! PTP timescale (TAI) time source.
//!
//! If the kernel's TAI offset is configured (e.g. by chrony/ptp4l via
//! adjtimex), it is used directly. Otherwise we fall back to a configured
//! static UTC offset (37 s since 2017). All conversions are anchored to
//! CLOCK_REALTIME so kernel software timestamps (which are realtime-based)
//! can be translated exactly.

use crate::net::{RawSock, RawStamp};
use crate::ptp::Timestamp;

#[derive(Clone, Copy)]
pub struct TaiClock {
    offset_secs: i64,
    kernel_tai: bool,
}

impl TaiClock {
    pub fn new(utc_offset: i16) -> Self {
        // The kernel TAI offset is set iff CLOCK_TAI differs from CLOCK_REALTIME.
        let diff_ns = raw(libc::CLOCK_TAI).to_ns() - raw(libc::CLOCK_REALTIME).to_ns();
        let kernel_offset = ((diff_ns + 500_000_000) / 1_000_000_000) as i64;
        let kernel_tai = kernel_offset != 0;
        TaiClock {
            offset_secs: if kernel_tai {
                kernel_offset
            } else {
                utc_offset as i64
            },
            kernel_tai,
        }
    }

    pub fn now(&self) -> Timestamp {
        self.realtime_to_tai(raw(libc::CLOCK_REALTIME))
    }

    /// Convert a CLOCK_REALTIME-based timestamp (e.g. a kernel software
    /// packet timestamp) to the PTP timescale.
    pub fn realtime_to_tai(&self, mut t: Timestamp) -> Timestamp {
        t.secs = t.secs.wrapping_add_signed(self.offset_secs);
        t
    }

    pub fn realtime_ns_to_tai(&self, ns: i128) -> Timestamp {
        Timestamp::from_ns(ns + self.offset_secs as i128 * 1_000_000_000)
    }

    pub fn utc_offset(&self) -> i16 {
        self.offset_secs as i16
    }

    pub fn describe(&self) -> &'static str {
        if self.kernel_tai {
            "system clock (kernel TAI offset)"
        } else {
            "system clock + static UTC offset"
        }
    }
}

/// Convert a raw packet timestamp to the PTP (TAI) timescale. Hardware
/// stamps are in the PHC timebase and are shifted by a fresh
/// (PHC, CLOCK_REALTIME) cross-timestamp; software stamps are
/// CLOCK_REALTIME-based.
pub fn stamp_to_tai(stamp: Option<RawStamp>, sock: &RawSock, clock: &TaiClock) -> Option<Timestamp> {
    let s = stamp?;
    if s.hardware {
        let (phc_ns, real_ns) = sock.phc()?.sample_offset_pair()?;
        Some(clock.realtime_ns_to_tai(s.ts.to_ns() - phc_ns + real_ns))
    } else {
        Some(clock.realtime_to_tai(s.ts))
    }
}

/// A packet timestamp on a free-running local timebase that is never stepped
/// or slewed: the raw PHC for hardware stamps, CLOCK_MONOTONIC_RAW otherwise
/// (software stamps are shifted from CLOCK_REALTIME by a fresh sample pair).
/// Used to measure a master's stability independently of NTP/chrony acting
/// on the system clock.
pub fn stamp_to_local_ns(stamp: Option<RawStamp>) -> i128 {
    match stamp {
        Some(s) if s.hardware => s.ts.to_ns(),
        Some(s) => {
            let a = raw(libc::CLOCK_REALTIME).to_ns();
            let m = raw(libc::CLOCK_MONOTONIC_RAW).to_ns();
            let b = raw(libc::CLOCK_REALTIME).to_ns();
            s.ts.to_ns() - (a + b) / 2 + m
        }
        None => raw(libc::CLOCK_MONOTONIC_RAW).to_ns(),
    }
}

fn raw(clock: libc::clockid_t) -> Timestamp {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(clock, &mut ts) };
    Timestamp {
        secs: ts.tv_sec as u64,
        nanos: ts.tv_nsec as u32,
    }
}
