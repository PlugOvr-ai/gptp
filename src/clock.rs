//! PTP timescale (TAI) time source.
//!
//! If the kernel's TAI offset is configured (e.g. by chrony/ptp4l via
//! adjtimex), CLOCK_TAI is used directly. Otherwise we fall back to
//! CLOCK_REALTIME plus a configured UTC offset (37 s since 2017).

use crate::ptp::Timestamp;

#[derive(Clone, Copy)]
pub struct TaiClock {
    kernel_tai: bool,
    utc_offset: i16,
}

impl TaiClock {
    pub fn new(utc_offset: i16) -> Self {
        // The kernel TAI offset is set iff CLOCK_TAI differs from CLOCK_REALTIME.
        let tai = raw(libc::CLOCK_TAI);
        let real = raw(libc::CLOCK_REALTIME);
        let kernel_tai = tai.secs != real.secs;
        TaiClock {
            kernel_tai,
            utc_offset,
        }
    }

    pub fn now(&self) -> Timestamp {
        if self.kernel_tai {
            raw(libc::CLOCK_TAI)
        } else {
            let mut t = raw(libc::CLOCK_REALTIME);
            t.secs = t.secs.wrapping_add_signed(self.utc_offset as i64);
            t
        }
    }

    pub fn utc_offset(&self) -> i16 {
        self.utc_offset
    }

    pub fn describe(&self) -> &'static str {
        if self.kernel_tai {
            "CLOCK_TAI (kernel TAI offset set)"
        } else {
            "CLOCK_REALTIME + static UTC offset"
        }
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
