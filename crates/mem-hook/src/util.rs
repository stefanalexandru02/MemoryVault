//! Small helpers: UTC timestamps, advisory file locks, private files, detached spawning, file-name
//! sanitising. The platform-specific parts (permissions, process groups, detaching) are gated with
//! `cfg(unix)` / `cfg(windows)`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

/// (year, month, day) to days since 1970-01-01; the inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * i64::from((m + 9) % 12) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Seconds since the epoch of a timestamp in the shape [`rfc3339`] writes (`2026-10-24T09:30:00.123Z`).
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<u32>().ok();
    let (y, mo, d) = (i64::from(num(0, 4)?), num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + i64::from(h * 3600 + mi * 60 + sec))
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

/// Non-blocking exclusive advisory lock (`flock` on Unix, `LockFileEx` on Windows). The lock lives
/// as long as the `File`.
pub fn try_lock(file: &File) -> bool {
    file.try_lock().is_ok()
}

/// Exclusive lock, retrying for up to `wait`. False when the lock stayed busy.
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

/// `OpenOptions` for a file only the owner may read (mode 0600 when created, on Unix).
fn private_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Narrows a file that already exists with group/world permissions down to the owner.
fn tighten(file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.metadata().is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = file;
}

/// Opens (creating, owner-only) a lock file.
pub fn open_lock(path: &Path) -> Option<File> {
    let file = private_options().create(true).append(true).open(path).ok()?;
    tighten(&file);
    Some(file)
}

/// Opens a file for appending, creating it owner-only.
pub fn open_append(path: &Path) -> std::io::Result<File> {
    let file = private_options().create(true).append(true).open(path)?;
    tighten(&file);
    Ok(file)
}

/// Replaces the content of `path`, owner-only (also for a file that exists with looser permissions).
pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let mut file = private_options().create(true).write(true).truncate(true).open(path)?;
    tighten(&file);
    file.write_all(contents.as_ref())
}

/// `mkdir -p` with mode 0700 on Unix. A leaf directory that already exists with group/world
/// access (a memory home the user created by hand) is narrowed to 0700.
pub fn mkdir_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
        if std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)
}

/// Starts `exe args...` detached from this process (own session on Unix, `DETACHED_PROCESS` on
/// Windows) with no stdio, and does not wait for it.
pub fn spawn_detached(exe: &str, args: &[&str]) {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no Rust state between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let _ = cmd.spawn();
}

/// Puts the child in its own process group (Unix) / process group + no console (Windows), so
/// [`kill_tree`] reaches everything it starts.
pub fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = cmd;
}

/// Kills the child and the processes it started.
pub fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: signals the child's own process group (set with `own_process_group`).
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Keeps `[A-Za-z0-9._-]`, replaces everything else with `_`; never yields `.`/`..` or an empty name.
pub fn sanitize_component(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
    if out.is_empty() || out.chars().all(|c| c == '.') { "_".into() } else { out }
}

/// True when `bin` is an existing file (a path) or one found in a `PATH` directory (a bare name).
pub fn on_path(bin: &str) -> bool {
    if bin.contains(['/', '\\']) {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let plain = dir.join(bin);
            plain.is_file() || (cfg!(windows) && plain.with_extension("exe").is_file())
        })
    })
}

/// `$HOME` (`%USERPROFILE%` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"].iter().find_map(|v| std::env::var_os(v).filter(|h| !h.is_empty())).map(PathBuf::from)
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
    fn rfc3339_round_trips() {
        for secs in [0, 951_782_400, 1_782_293_400, 4_102_444_799] {
            assert_eq!(parse_rfc3339(&rfc3339(secs, 250)), Some(secs));
        }
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
    }

    #[test]
    fn on_path_finds_files_and_bare_names() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("tool");
        std::fs::write(&bin, "").unwrap();
        assert!(on_path(&bin.to_string_lossy()));
        assert!(!on_path(&dir.path().join("nope").to_string_lossy()));
        assert!(!on_path("definitely-not-a-real-binary-name"));
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_component("github.com"), "github.com");
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("a b/c"), "a_b_c");
        assert_eq!(sanitize_component(""), "_");
    }
}
