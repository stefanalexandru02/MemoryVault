//! `spool/YYYYMMDD.jsonl`: requests that could not be delivered, replayed in order at session start.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::deliver::{self, App, Entry, Fail};
use crate::util;

fn dir(home: &Path) -> PathBuf {
    home.join("spool")
}

const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Bounds the spool: files older than `max_age` go, then the oldest data goes until at most
/// `max_bytes` remain (whole files first, then leading lines of the last one).
fn prune(dir: &Path, max_age: Duration, max_bytes: u64) {
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    for p in std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()) {
        if p.extension().is_none_or(|x| x != "jsonl") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&p) else { continue };
        let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age > max_age);
        if old {
            eprintln!("memory: discarding spool file older than {}d: {}", max_age.as_secs() / 86400, p.display());
            let _ = std::fs::remove_file(&p);
        } else {
            files.push((p, meta.len()));
        }
    }
    files.sort();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= max_bytes {
        return;
    }
    eprintln!(
        "memory: spool exceeds {} bytes (server unreachable or rejecting?); discarding oldest entries",
        max_bytes
    );
    for (i, (p, len)) in files.iter().enumerate() {
        if total <= max_bytes {
            break;
        }
        if i + 1 < files.len() {
            let _ = std::fs::remove_file(p);
            total -= len;
        } else if let Ok(content) = std::fs::read_to_string(p) {
            let mut excess = (total - max_bytes) as usize;
            let rest: String = content
                .split_inclusive('\n')
                .skip_while(|l| {
                    let skip = excess > 0;
                    excess = excess.saturating_sub(l.len());
                    skip
                })
                .collect();
            let _ = util::write_private(p, rest);
        }
    }
}

/// False when the file's last byte is not a newline (an empty or unreadable file counts as fine).
fn ends_with_newline(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let last = std::fs::File::open(path).and_then(|mut f| {
        f.seek(SeekFrom::End(-1))?;
        let mut b = [0u8];
        f.read_exact(&mut b)?;
        Ok(b[0])
    });
    last.map_or(true, |b| b == b'\n')
}

/// Appends one line to today's spool file. Never fails loudly.
pub fn append(home: &Path, entry: &Entry) {
    let dir = dir(home);
    if util::mkdir_private(&dir).is_err() {
        return;
    }
    let Ok(mut line) = serde_json::to_string(entry) else { return };
    line.push('\n');
    let lock = util::open_lock(&dir.join(".append.lock"));
    if let Some(l) = &lock {
        util::lock_wait(l, Duration::from_millis(500));
    }
    let path = dir.join(format!("{}.jsonl", util::yyyymmdd(util::unix_now().0)));
    if let Ok(mut f) = util::open_append(&path) {
        // A hook killed mid-write leaves an unterminated line: end it so it cannot swallow this one.
        if !ends_with_newline(&path) {
            line.insert(0, '\n');
        }
        let _ = f.write_all(line.as_bytes());
    }
    prune(&dir, MAX_AGE, MAX_BYTES);
}

/// Replays the spool oldest first and stops at the first delivery that must be retried.
/// Delivered (and permanently rejected) lines are removed; the rest stay in place.
pub fn drain(app: &App, budget: Duration) {
    let dir = dir(app.home());
    let Some(drain_lock) = util::open_lock(&dir.join(".drain.lock")).filter(util::try_lock) else { return };
    prune(&dir, MAX_AGE, MAX_BYTES);
    let deadline = Instant::now() + budget;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default();
    files.sort();
    for file in files {
        if !drain_file(app, &file, deadline) {
            break;
        }
    }
    drop(drain_lock);
}

/// A request the server answered 500 this many times (while it accepted other requests) is
/// considered poison and dropped, so it cannot block the spool for good.
const MAX_REPLAYS: u32 = 5;
/// After this many 500s in a row without a single delivery the server itself is assumed broken:
/// the replay stops and the failures are not held against the lines.
const SERVER_ERROR_PATIENCE: usize = 5;
/// An unterminated last line younger than this may still be written by a running hook.
const PARTIAL_LINE_GRACE: Duration = Duration::from_secs(10);

/// True when the whole file was consumed and the next one may be tried.
///
/// Walks the file oldest first. A delivered, rejected or unreadable line goes. A transport error or
/// a retryable status stops the replay, keeping that line and everything after it. An HTTP 500 is
/// kept but skipped, so one poison line does not block the lines behind it; once other lines were
/// counted as a failed replay of the line (unless the server fails everything in a row, which says
/// nothing about the line), and at [`MAX_REPLAYS`] it is dropped.
fn drain_file(app: &App, file: &Path, deadline: Instant) -> bool {
    let Ok(content) = std::fs::read_to_string(file) else { return true };
    let partial_in_progress = std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_none_or(|age| age < PARTIAL_LINE_GRACE);
    // One slot per processed line, in order: None = remove, Some(text) = keep as the file has it.
    let mut kept: Vec<Option<String>> = Vec::new();
    let mut failing: Vec<(usize, Entry)> = Vec::new();
    let (mut delivered, mut stopped, mut server_broken) = (0usize, false, false);
    for line in content.split_inclusive('\n') {
        if !line.ends_with('\n') {
            if partial_in_progress {
                stopped = true; // a line still being written
                break;
            }
            kept.push(None); // a writer died mid-line: nothing will complete it
            continue;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(50) {
            stopped = true;
            break;
        }
        let Ok(entry) = serde_json::from_str::<Entry>(line) else {
            kept.push(None); // garbage: discard
            continue;
        };
        match deliver::send(app, &entry, left) {
            Ok(_) => {
                delivered += 1;
                kept.push(None);
            }
            Err(Fail::Drop(msg)) => {
                eprintln!("memory: dropping spooled request: {msg}");
                kept.push(None);
            }
            Err(Fail::Server(why)) => {
                util::debug(&format!("replay skipped a line: {why}"));
                failing.push((kept.len(), entry));
                kept.push(Some(line.to_string()));
                if delivered == 0 && failing.len() >= SERVER_ERROR_PATIENCE {
                    (stopped, server_broken) = (true, true);
                    break;
                }
            }
            Err(Fail::Retry(why)) => {
                util::debug(&format!("replay stopped: {why}"));
                stopped = true;
                break;
            }
        }
    }
    if !server_broken {
        for (slot, mut entry) in failing {
            entry.replays += 1;
            if entry.replays >= MAX_REPLAYS {
                eprintln!("memory: dropping a spooled request the server keeps failing on");
                kept[slot] = None;
            } else if let Ok(text) = serde_json::to_string(&entry) {
                kept[slot] = Some(format!("{text}\n"));
            }
        }
    }
    if !kept.is_empty() {
        replace_prefix(file, kept.len(), kept.into_iter().flatten().collect());
    }
    !stopped
}

/// Replaces the first `n` lines with `keep`, retaining anything appended meanwhile.
fn replace_prefix(file: &Path, n: usize, keep: Vec<String>) {
    let lock = file.parent().and_then(|d| util::open_lock(&d.join(".append.lock")));
    if let Some(l) = &lock {
        util::lock_wait(l, Duration::from_secs(1));
    }
    let Ok(content) = std::fs::read_to_string(file) else { return };
    let mut rest: String = keep.concat();
    rest.extend(content.split_inclusive('\n').skip(n));
    if rest.is_empty() {
        let _ = std::fs::remove_file(file);
    } else {
        let tmp = file.with_extension("tmp");
        if util::write_private(&tmp, rest).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_drops_oldest_data_past_the_size_cap() {
        let d = tempfile_dir("size");
        std::fs::write(d.join("20250101.jsonl"), "a\n".repeat(50)).unwrap();
        std::fs::write(d.join("20250102.jsonl"), "b\n".repeat(50)).unwrap();
        prune(&d, MAX_AGE, 150);
        assert!(!d.join("20250101.jsonl").exists());
        assert_eq!(std::fs::read_to_string(d.join("20250102.jsonl")).unwrap().len(), 100);
        prune(&d, MAX_AGE, 40);
        let left = std::fs::read_to_string(d.join("20250102.jsonl")).unwrap();
        assert!(left.len() <= 40 && left.ends_with("b\n") && !left.is_empty());
    }

    #[test]
    fn prune_drops_old_files() {
        let d = tempfile_dir("age");
        std::fs::write(d.join("20250101.jsonl"), "a\n").unwrap();
        prune(&d, Duration::ZERO, MAX_BYTES);
        assert!(!d.join("20250101.jsonl").exists());
    }

    fn tempfile_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mem-hook-prune-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
