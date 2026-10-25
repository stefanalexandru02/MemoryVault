//! On-disk state under the memory home: session cache, exclude list, context cache.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::util;

/// `sessions/<claude_session_id>`: `{session_id, next_seq, window, prompt_no}`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Cache {
    /// Server session id; None until `POST /v1/sessions` succeeded.
    pub session_id: Option<String>,
    pub next_seq: i64,
    pub window: i64,
    pub prompt_no: i64,
}

fn sessions_dir(home: &Path) -> PathBuf {
    home.join("sessions")
}

/// Read-modify-write of one session's cache under an exclusive lock (hooks run concurrently).
pub fn update_cache<R>(home: &Path, claude_sid: &str, f: impl FnOnce(&mut Cache) -> R) -> R {
    let dir = sessions_dir(home);
    let _ = util::mkdir_private(&dir);
    let name = util::sanitize_component(claude_sid);
    let path = dir.join(&name);
    let lock = util::open_lock(&dir.join(format!("{name}.lock")));
    if let Some(l) = &lock {
        util::lock_wait(l, Duration::from_secs(2));
    }
    let mut cache: Cache =
        std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let out = f(&mut cache);
    if let Ok(text) = serde_json::to_string(&cache) {
        let tmp = dir.join(format!("{name}.tmp"));
        if util::write_private(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
    out
}

/// Takes the next event sequence number (and counts a prompt when asked).
pub fn alloc_seq(home: &Path, claude_sid: &str, new_prompt: bool) -> i64 {
    update_cache(home, claude_sid, |c| {
        let seq = c.next_seq;
        c.next_seq += 1;
        if new_prompt {
            c.prompt_no += 1;
        }
        seq
    })
}

/// Session caches older than 30 days are deleted.
pub fn prune_sessions(home: &Path) {
    let Ok(entries) = std::fs::read_dir(sessions_dir(home)) else { return };
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(30 * 86_400);
    for e in entries.flatten() {
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < cutoff) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `exclude`: one project key per line (blank lines and `#` comments ignored).
pub fn is_excluded(home: &Path, project_key: &str) -> bool {
    std::fs::read_to_string(home.join("exclude"))
        .is_ok_and(|t| t.lines().map(str::trim).any(|l| !l.is_empty() && !l.starts_with('#') && l == project_key))
}

/// `cache/<project_key>.md`, one sanitised path component per key segment.
pub fn context_cache_path(home: &Path, project_key: &str) -> PathBuf {
    let mut path = home.join("cache");
    for seg in project_key.split('/').filter(|s| !s.is_empty()) {
        path.push(util::sanitize_component(seg));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "_".into());
    path.set_file_name(format!("{name}.md"));
    path
}

pub fn save_context(home: &Path, project_key: &str, block: &str) {
    let path = context_cache_path(home, project_key);
    if let Some(dir) = path.parent() {
        let _ = util::mkdir_private(dir);
    }
    let _ = util::write_private(&path, block);
}

/// Once per day, tells the user why memory is off.
pub fn hint_missing_config(home: &Path) {
    let today = util::yyyymmdd(util::unix_now().0);
    let stamp = home.join("hint-day");
    if std::fs::read_to_string(&stamp).is_ok_and(|t| t.trim() == today) {
        return;
    }
    let _ = util::mkdir_private(home);
    let _ = std::fs::write(&stamp, &today);
    eprintln!(
        "memory: MEMORY_URL or MEMORY_TOKEN missing in {}; memory hooks are disabled",
        home.join("config").display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_is_confined() {
        let p = context_cache_path(Path::new("/h"), "github.com/alex/homelab");
        assert_eq!(p, Path::new("/h/cache/github.com/alex/homelab.md"));
        let p = context_cache_path(Path::new("/h"), "../../etc/x");
        assert_eq!(p, Path::new("/h/cache/_/_/etc/x.md"));
    }

    #[test]
    fn seq_is_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(alloc_seq(dir.path(), "s1", false), 0);
        assert_eq!(alloc_seq(dir.path(), "s1", true), 1);
        assert_eq!(update_cache(dir.path(), "s1", |c| (c.next_seq, c.prompt_no)), (2, 1));
        assert_eq!(alloc_seq(dir.path(), "s2", false), 0);
    }

    #[test]
    fn exclude_matches_exact_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("exclude"), "# private\n\nlocal/secret\n").unwrap();
        assert!(is_excluded(dir.path(), "local/secret"));
        assert!(!is_excluded(dir.path(), "local/secret2"));
        assert!(!is_excluded(dir.path(), "# private"));
    }
}
