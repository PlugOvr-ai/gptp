//! `--set-system-clock tai|utc`: step the system clock between UTC and TAI,
//! for hosts whose clock should carry the PTP timescale itself.
//!
//! The applied step is recorded in /run, so the clock is stepped at most
//! once per boot (the RTC and NTP give UTC) and can be undone exactly.
//! After NTP has set the clock to UTC again, `--clock-just-set` forgets the
//! earlier step.

use std::fs;
use std::io;

use anyhow::{bail, Context, Result};

use crate::clocksync::{Adjtimex, SystemClock};
use crate::config::SystemClockScale;

/// Holds the seconds added to the clock while it is on TAI.
const MARKER: &str = "/run/gptp-gm-system-clock-tai";

/// The system clock was stepped to TAI by `set`.
pub fn is_on_tai() -> bool {
    std::path::Path::new(MARKER).exists()
}

/// The step to apply (s) and the offset to record afterwards (None: the
/// clock is on UTC). `applied`: the recorded step; `just_set`: the clock
/// was set to UTC since, e.g. by NTP.
fn plan(
    target: SystemClockScale,
    applied: Option<i64>,
    just_set: bool,
    utc_offset: i64,
) -> (i64, Option<i64>) {
    let applied = if just_set { None } else { applied };
    match (target, applied) {
        (SystemClockScale::Tai, None) => (utc_offset, Some(utc_offset)),
        (SystemClockScale::Tai, Some(a)) => (0, Some(a)),
        (SystemClockScale::Utc, Some(a)) => (-a, None),
        (SystemClockScale::Utc, None) => (0, None),
    }
}

pub fn set(target: SystemClockScale, utc_offset: i16, just_set: bool) -> Result<()> {
    let applied = match fs::read_to_string(MARKER) {
        Ok(s) => Some(
            s.trim()
                .parse::<i64>()
                .with_context(|| format!("{MARKER}: not a number of seconds"))?,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {MARKER}")),
    };
    let (step, record) = plan(target, applied, just_set, utc_offset as i64);
    if step != 0 {
        match Adjtimex.step_ns(step as i128 * 1_000_000_000) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                bail!("stepping the system clock needs root (CAP_SYS_TIME)")
            }
            Err(e) => return Err(e).context("stepping the system clock"),
        }
    }
    match record {
        Some(a) => {
            fs::write(MARKER, format!("{a}\n")).with_context(|| format!("writing {MARKER}"))?
        }
        None if applied.is_some() => {
            fs::remove_file(MARKER).with_context(|| format!("removing {MARKER}"))?
        }
        None => {}
    }
    if record.is_some() {
        // Keeps the kernel from writing TAI into the RTC, which stays UTC.
        Adjtimex::set_unsynced().context("marking the system clock unsynchronized")?;
    }
    let scale = if record.is_some() { "TAI" } else { "UTC" };
    if step == 0 {
        println!("System clock already on {scale}");
    } else {
        println!("System clock stepped {step:+} s to {scale}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use SystemClockScale::{Tai, Utc};

    #[test]
    fn steps_once_and_undoes_exactly() {
        // Boot: UTC from the RTC, stepped once; a second call does nothing.
        assert_eq!(plan(Tai, None, false, 37), (37, Some(37)));
        assert_eq!(plan(Tai, Some(37), false, 37), (0, Some(37)));
        // After an NTP sync the clock is UTC again: step again.
        assert_eq!(plan(Tai, Some(37), true, 37), (37, Some(37)));
        // Back to UTC by the recorded step, even if the offset changed.
        assert_eq!(plan(Utc, Some(37), false, 38), (-37, None));
        assert_eq!(plan(Utc, None, false, 37), (0, None));
    }
}
