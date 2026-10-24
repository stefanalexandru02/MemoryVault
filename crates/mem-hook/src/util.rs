//! Small helpers: UTC timestamps, advisory file locks, detached spawning, file-name sanitising.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Diagnostics for a hook, which must stay quiet: printed only with `MEMORY_DEBUG` set.
pub fn debug(msg: &str) {
    if std::env::var_os("MEMORY_DEBUG").is_some() {
        eprintln!("mem-hook: {msg}");
    }
}

/// Seconds and milliseconds since the Unix epoch.
pub fn unix_now() -> (i64, u32) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.subsec_millis())
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `2026-10-24T09:30:00.123Z`.
pub fn rfc3339(secs: i64, millis: u32) -> String {
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", t / 3600, t % 3600 / 60, t % 60)
}

pub fn now_rfc3339() -> String {
    let (s, ms) = unix_now();
    rfc3339(s, ms)
}

/// `20261024`, the spool file name stem and the daily hint stamp.
pub fn yyyymmdd(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}{m:02}{d:02}")
}

pub fn mtime_rfc3339(path: &std::path::Path) -> Option<String> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    let d = m.duration_since(UNIX_EPOCH).ok()?;
    Some(rfc3339(d.as_secs() as i64, 0))
}

/// Non-blocking exclusive `flock`. The lock lives as long as the `File`.
pub fn try_lock(file: &File) -> bool {
    // SAFETY: plain syscall on a descriptor owned by `file`.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Exclusive `flock`, retrying for up to `wait`. False when the lock stayed busy.
pub fn lock_wait(file: &File, wait: Duration) -> bool {
    let end = Instant::now() + wait;
    loop {
        if try_lock(file) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens (creating, mode 0600) a lock file.
pub fn open_lock(path: &std::path::Path) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path).ok()
}

/// `mkdir -p` with mode 0700.
pub fn mkdir_private(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

/// Starts `exe args...` in its own session with no stdio, and does not wait for it.
pub fn spawn_detached(exe: &str, args: &[&str]) {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and touches no Rust state between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Keeps `[A-Za-z0-9._-]`, replaces everything else with `_`; never yields `.`/`..` or an empty name.
pub fn sanitize_component(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    if out.is_empty() || out.chars().all(|c| c == '.') { "_".into() } else { out }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_known_dates() {
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_782_293_400, 123), "2026-06-24T09:30:00.123Z");
        assert_eq!(rfc3339(951_782_400, 0), "2000-02-29T00:00:00.000Z");
        assert_eq!(yyyymmdd(1_782_293_400), "20260624");
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_component("github.com"), "github.com");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("a b/c"), "a_b_c");
        assert_eq!(sanitize_component(""), "_");
    }
}
