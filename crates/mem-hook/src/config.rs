//! `~/.memory/config` (or `$MEMORY_HOME/config`): shell-style `KEY=value` lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Config {
    pub home: PathBuf,
    pub url: String,
    pub token: String,
    pub device: String,
    pub claude_bin: String,
    /// `MEM_INJECTION`: `lean`, `full` or `smart`, sent as `mode` with the SessionStart request. Unset (or anything
    /// else) leaves the choice to the user's default on the server.
    pub injection: Option<String>,
    /// `MEM_CONTEXT_ROWS` (1 to 30): rows of the observation index of the full block, sent as `rows`. Unset (or
    /// anything else) leaves the choice to the server.
    pub context_rows: Option<u32>,
    /// `MEM_CONTEXT_TYPES`: the observation types the index is limited to, sent as `types`; names the server does
    /// not know are dropped, so a typo cannot make the context request fail.
    pub context_types: Vec<String>,
    /// `MEM_FILE_CONTEXT_BUDGET`: estimated tokens the file-context hook may add to one session; 0 turns it off.
    pub file_context_budget: u32,
}

/// The observation types the server knows (`GET /v1/context?types=`).
const OBSERVATION_TYPES: [&str; 9] = [
    "bugfix",
    "feature",
    "refactor",
    "change",
    "discovery",
    "decision",
    "security_alert",
    "security_note",
    "sensitive",
];

/// Default of `MEM_FILE_CONTEXT_BUDGET`.
pub const DEFAULT_FILE_CONTEXT_BUDGET: u32 = 1200;

/// `$MEMORY_HOME`, default `~/.memory`.
pub fn memory_home() -> PathBuf {
    match std::env::var_os("MEMORY_HOME").filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => crate::util::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".memory"),
    }
}

/// Parses `KEY=value` lines; accepts `export KEY=value`, single or double quotes and `#` comments.
pub fn parse(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        let value = match value.as_bytes() {
            [q @ (b'"' | b'\''), .., last] if q == last => &value[1..value.len() - 1],
            _ => value,
        };
        out.insert(key.trim().to_string(), value.to_string());
    }
    out
}

fn hostname() -> String {
    for var in ["HOSTNAME", "HOST", "COMPUTERNAME"] {
        if let Ok(h) = std::env::var(var)
            && !h.trim().is_empty()
        {
            return h.trim().to_string();
        }
    }
    for path in ["/etc/hostname", "/proc/sys/kernel/hostname"] {
        if let Ok(h) = std::fs::read_to_string(path)
            && !h.trim().is_empty()
        {
            return h.trim().to_string();
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// The settings in `home/config` (empty when the file is missing or unreadable).
pub fn read_file(home: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(home.join("config")).map(|t| parse(&t)).unwrap_or_default()
}

/// `key` from the config file, else the environment; blank counts as unset. The file wins over the
/// environment, like sourcing it would.
pub fn lookup(file: &HashMap<String, String>, key: &str) -> Option<String> {
    file.get(key).cloned().or_else(|| std::env::var(key).ok()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

impl Config {
    /// None when `MEMORY_URL` or `MEMORY_TOKEN` is missing.
    pub fn load(home: &Path) -> Option<Config> {
        let file = read_file(home);
        let get = |key: &str| lookup(&file, key);
        let url = get("MEMORY_URL")?.trim_end_matches('/').to_string();
        let token = get("MEMORY_TOKEN")?;
        Some(Config {
            home: home.to_path_buf(),
            url,
            token,
            device: get("DEVICE_ID").unwrap_or_else(hostname),
            claude_bin: get("CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
            injection: get("MEM_INJECTION").filter(|m| matches!(m.as_str(), "lean" | "full" | "smart")),
            context_rows: get("MEM_CONTEXT_ROWS").and_then(|v| v.parse().ok()).filter(|r| (1..=30).contains(r)),
            context_types: get("MEM_CONTEXT_TYPES").map(|v| known_types(&v)).unwrap_or_default(),
            file_context_budget: get("MEM_FILE_CONTEXT_BUDGET")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_FILE_CONTEXT_BUDGET),
        })
    }
}

/// The known observation types of a comma-separated list, in the order given, once each.
fn known_types(list: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in list.split(',').map(|n| n.trim().to_lowercase()) {
        if OBSERVATION_TYPES.contains(&name.as_str()) && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_known_observation_types_only() {
        assert_eq!(
            known_types("Decision, bugfix,decision,opinion,,security_alert"),
            ["decision", "bugfix", "security_alert"]
        );
        assert!(known_types("everything").is_empty());
    }

    #[test]
    fn parses_shell_style_lines() {
        let m = parse(
            "# c\nMEMORY_URL=https://m.ts.net\nexport MEMORY_TOKEN=\"mem_a_b\"\n\nDEVICE_ID='lap top'\nbad line\n",
        );
        assert_eq!(m["MEMORY_URL"], "https://m.ts.net");
        assert_eq!(m["MEMORY_TOKEN"], "mem_a_b");
        assert_eq!(m["DEVICE_ID"], "lap top");
        assert_eq!(m.len(), 3);
    }
}
