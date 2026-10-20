//! PTP hardware clock (/dev/ptpN) access.
//!
//! NIC hardware timestamps are taken by the PHC, whose timebase is generally
//! unrelated to the system clock. To express them in TAI we cross-timestamp:
//! the kernel gives us a (PHC, CLOCK_REALTIME) sample pair taken back-to-back
//! (PTP_SYS_OFFSET_PRECISE uses a hardware cross-timestamp where supported),
//! and each packet timestamp is shifted by that offset. Sampling per
//! conversion keeps the drift error negligible (< ns for the µs between the
//! packet and the sample).

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use anyhow::{Context, Result};

const PTP_MAX_SAMPLES: usize = 25;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PtpClockTime {
    sec: i64,
    nsec: u32,
    reserved: u32,
}

impl PtpClockTime {
    fn to_ns(self) -> i128 {
        self.sec as i128 * 1_000_000_000 + self.nsec as i128
    }
}

#[repr(C)]
struct PtpSysOffset {
    n_samples: u32,
    rsv: [u32; 3],
    ts: [PtpClockTime; 2 * PTP_MAX_SAMPLES + 1],
}

#[repr(C)]
struct PtpSysOffsetPrecise {
    device: PtpClockTime,
    sys_realtime: PtpClockTime,
    sys_monoraw: PtpClockTime,
    rsv: [u32; 4],
}

/// struct ptp_clock_caps (only max_adj is used).
#[repr(C)]
struct PtpClockCaps {
    max_adj: i32,
    rest: [i32; 19],
}

const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    // Linux _IOC() with type '=' (the PTP ioctl magic).
    ((dir << 30) | ((size as u32) << 16) | (b'=' as u32) << 8 | nr) as libc::c_ulong
}
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const IOC_RW: u32 = 3;
const PTP_CLOCK_GETCAPS: libc::c_ulong = ioc(IOC_READ, 1, std::mem::size_of::<PtpClockCaps>());
const PTP_SYS_OFFSET: libc::c_ulong = ioc(IOC_WRITE, 5, std::mem::size_of::<PtpSysOffset>());
const PTP_SYS_OFFSET_PRECISE: libc::c_ulong =
    ioc(IOC_RW, 8, std::mem::size_of::<PtpSysOffsetPrecise>());

pub struct PhcClock {
    file: std::fs::File,
    pub device: String,
    pub index: i32,
}

impl PhcClock {
    pub fn open(phc_index: i32) -> Result<Self> {
        let device = format!("/dev/ptp{phc_index}");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device)
            .with_context(|| format!("opening {device} (PHC)"))?;
        Ok(PhcClock {
            file,
            device,
            index: phc_index,
        })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// The dynamic POSIX clock id of this PHC (FD_TO_CLOCKID).
    pub fn clock_id(&self) -> libc::clockid_t {
        ((!(self.fd() as libc::clockid_t)) << 3) | 3
    }

    /// Current PHC time, ns.
    pub fn now_ns(&self) -> io::Result<i128> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(self.clock_id(), &mut ts) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128)
    }

    /// clock_adjtime on this PHC (needs write access to the device).
    pub fn adjtime(&self, tx: &mut libc::timex) -> io::Result<()> {
        if unsafe { libc::clock_adjtime(self.clock_id(), tx) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Largest frequency adjustment the NIC supports, ppb.
    pub fn max_adj_ppb(&self) -> Option<f64> {
        let mut caps: PtpClockCaps = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(self.fd(), PTP_CLOCK_GETCAPS as libc::Ioctl, &mut caps) };
        (rc == 0 && caps.max_adj > 0).then_some(caps.max_adj as f64)
    }

    /// One (PHC, CLOCK_REALTIME) sample pair in nanoseconds, taken as close
    /// together as the kernel/NIC allow.
    pub fn sample_offset_pair(&self) -> Option<(i128, i128)> {
        // Preferred: hardware cross-timestamp.
        let mut precise: PtpSysOffsetPrecise = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::ioctl(
                self.fd(),
                PTP_SYS_OFFSET_PRECISE as libc::Ioctl,
                &mut precise,
            )
        } == 0
        {
            return Some((precise.device.to_ns(), precise.sys_realtime.to_ns()));
        }

        // Fallback: sys/phc/sys triplets; take the pair with the smallest window.
        let mut off: PtpSysOffset = unsafe { std::mem::zeroed() };
        off.n_samples = 5;
        if unsafe { libc::ioctl(self.fd(), PTP_SYS_OFFSET as libc::Ioctl, &mut off) } != 0 {
            let _ = io::Error::last_os_error();
            return None;
        }
        let mut best: Option<(i128, i128, i128)> = None; // (window, phc, sys_mid)
        for i in 0..off.n_samples as usize {
            let sys_a = off.ts[2 * i].to_ns();
            let phc = off.ts[2 * i + 1].to_ns();
            let sys_b = off.ts[2 * i + 2].to_ns();
            let window = sys_b - sys_a;
            if best.is_none_or(|(w, _, _)| window < w) {
                best = Some((window, phc, (sys_a + sys_b) / 2));
            }
        }
        best.map(|(_, phc, sys)| (phc, sys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_struct_sizes_match_kernel_abi() {
        assert_eq!(std::mem::size_of::<PtpClockTime>(), 16);
        assert_eq!(std::mem::size_of::<PtpSysOffset>(), 832);
        assert_eq!(std::mem::size_of::<PtpSysOffsetPrecise>(), 64);
        assert_eq!(PTP_SYS_OFFSET, 0x4340_3d05);
        assert_eq!(PTP_SYS_OFFSET_PRECISE, 0xc040_3d08);
        assert_eq!(std::mem::size_of::<PtpClockCaps>(), 80);
        assert_eq!(PTP_CLOCK_GETCAPS, 0x8050_3d01);
    }
}
