//! `~/.memory/config` (or `$MEMORY_HOME/config`): shell-style `KEY=value` lines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Config {
    pub home: PathBuf,
    pub url: String,
    pub token: String,
    pub device: String,
    pub claude_bin: String,
}

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
    for var in ["HOSTNAME", "HOST"] {
        if let Ok(h) = std::env::var(var) {
            if !h.trim().is_empty() {
                return h.trim().to_string();
            }
        }
    }
    for path in ["/etc/hostname", "/proc/sys/kernel/hostname"] {
        if let Ok(h) = std::fs::read_to_string(path) {
            if !h.trim().is_empty() {
                return h.trim().to_string();
            }
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

impl Config {
    /// None when `MEMORY_URL` or `MEMORY_TOKEN` is missing. Values in the file win over the
    /// environment, like sourcing the file would.
    pub fn load(home: &Path) -> Option<Config> {
        let file = std::fs::read_to_string(home.join("config")).map(|t| parse(&t)).unwrap_or_default();
        let get = |key: &str| {
            file.get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let url = get("MEMORY_URL")?.trim_end_matches('/').to_string();
        let token = get("MEMORY_TOKEN")?;
        Some(Config {
            home: home.to_path_buf(),
            url,
            token,
            device: get("DEVICE_ID").unwrap_or_else(hostname),
            claude_bin: get("CLAUDE_BIN").unwrap_or_else(|| "claude".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
