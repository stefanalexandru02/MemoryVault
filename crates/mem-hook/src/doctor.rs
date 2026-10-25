//! `mem-hook doctor [--json] [--server]`: why is nothing being remembered?
//!
//! One check per link of the chain (config, server, token, binary, plugin, spool, session cache, last
//! delivery), each with the command that fixes it. Exit code 1 when a check fails; warnings do not
//! count. `--server` appends the server's own checks (`GET /v1/admin/doctor`, admin token). Unlike the
//! hooks, this command prints to the terminal and reports failure through its exit code.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::api::Api;
use crate::config::{self, Config};
use crate::{store, util};

const TIMEOUT: Duration = Duration::from_secs(3);
/// A spooled event this old means delivery has been failing for a day.
const SPOOL_STALE_SECONDS: i64 = 86_400;
const USAGE: &str = "usage: mem-hook doctor [--json] [--server]";
const CONNECT_FIX: &str = "make connect-local (or make connect-remote URL=... TOKEN=...)";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Warn,
    Fail,
}

/// Same shape as the server's `DoctorCheck`, so `--json` output and the viewer read alike.
#[derive(Debug, Serialize)]
struct Check {
    name: String,
    status: Status,
    detail: String,
    fix: Option<String>,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>, fix: Option<String>) -> Check {
        Check { name: name.into(), status, detail: detail.into(), fix }
    }

    fn ok(name: &str, detail: impl Into<String>) -> Check {
        Check::new(name, Status::Ok, detail, None)
    }

    fn warn(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check::new(name, Status::Warn, detail, Some(fix.into()))
    }

    fn fail(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
        Check::new(name, Status::Fail, detail, Some(fix.into()))
    }
}

/// Runs the command and returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let (mut json, mut server) = (false, false);
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            "--server" => server = true,
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    let checks = collect(server);
    if json {
        println!("{}", serde_json::to_string_pretty(&json!({ "checks": checks })).unwrap_or_default());
    } else {
        for c in &checks {
            let mark = match c.status {
                Status::Ok => "[ ok ]",
                Status::Warn => "[warn]",
                Status::Fail => "[FAIL]",
            };
            println!("{mark} {:<20} {}", c.name, c.detail);
            if let Some(fix) = &c.fix {
                println!("       fix: {fix}");
            }
        }
    }
    i32::from(checks.iter().any(|c| c.status == Status::Fail))
}

fn collect(server: bool) -> Vec<Check> {
    let home = config::memory_home();
    let (config_check, cfg) = check_config(&home);
    let mut checks = vec![config_check];
    match &cfg {
        Some(cfg) => {
            let api = Api::new(cfg);
            let (health, reachable) = check_server(&api, cfg);
            checks.push(health);
            checks.push(check_token(&api, reachable));
        }
        None => {
            checks.push(Check::warn("server", "not checked: no usable config", CONNECT_FIX));
            checks.push(Check::warn("token", "not checked: no usable config", CONNECT_FIX));
        }
    }
    checks.push(check_environment());
    checks.push(check_binary());
    checks.push(check_plugin());
    checks.push(check_spool(&home));
    checks.push(check_session_cache(&home));
    checks.push(check_last_delivery(&home));
    if server {
        checks.extend(server_checks(&home, cfg.as_ref()));
    }
    checks
}

/// The first line of `text` that is neither blank, a comment nor `KEY=value`, with its number.
fn bad_config_line(text: &str) -> Option<(usize, &str)> {
    text.lines().enumerate().find_map(|(i, raw)| {
        let line = raw.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        (!line.is_empty() && !line.starts_with('#') && !line.contains('=')).then_some((i + 1, raw))
    })
}

fn check_config(home: &Path) -> (Check, Option<Config>) {
    const NAME: &str = "config";
    let path = home.join("config");
    let cfg = Config::load(home);
    let check = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match cfg {
            Some(_) => Check::warn(
                NAME,
                format!("{} does not exist; MEMORY_URL and MEMORY_TOKEN come from the environment", path.display()),
                CONNECT_FIX,
            ),
            None => Check::fail(NAME, format!("{} does not exist", path.display()), CONNECT_FIX),
        },
        Err(e) => {
            Check::fail(NAME, format!("cannot read {}: {e}", path.display()), format!("chmod u+r {}", path.display()))
        }
        Ok(text) => {
            if let Some((n, line)) = bad_config_line(&text) {
                Check::fail(
                    NAME,
                    format!("{} line {n} is not KEY=value: {}", path.display(), line.trim()),
                    format!("edit {}", path.display()),
                )
            } else if cfg.is_none() {
                let file = config::parse(&text);
                let missing: Vec<&str> =
                    ["MEMORY_URL", "MEMORY_TOKEN"].into_iter().filter(|k| config::lookup(&file, k).is_none()).collect();
                Check::fail(NAME, format!("{} is missing {}", path.display(), missing.join(" and ")), CONNECT_FIX)
            } else {
                Check::ok(NAME, path.display().to_string())
            }
        }
    };
    (check, cfg)
}

/// The server check and whether the server answered at all.
fn check_server(api: &Api, cfg: &Config) -> (Check, bool) {
    const NAME: &str = "server";
    let fix_url = format!(
        "check MEMORY_URL in {} and that the server runs: curl {}/health",
        cfg.home.join("config").display(),
        cfg.url
    );
    match api.get("/health", TIMEOUT) {
        Err(e) => (Check::fail(NAME, format!("cannot reach {}: {e}", cfg.url), fix_url), false),
        Ok(resp) if resp.ok() => {
            let status = resp.json().and_then(|j| j["status"].as_str().map(str::to_owned)).unwrap_or_default();
            if status == "degraded" {
                let fix = "run `make doctor` (or `memvault admin doctor` on the server) for the failing part";
                (
                    Check::warn(
                        NAME,
                        format!("{} answers but reports degraded (embedding service down?)", cfg.url),
                        fix,
                    ),
                    true,
                )
            } else {
                (Check::ok(NAME, format!("{} is up", cfg.url)), true)
            }
        }
        Ok(resp) => (
            Check::fail(
                NAME,
                format!("{} answered HTTP {}", cfg.url, resp.status),
                "run `memvault admin doctor` on the server host (the database may be down)",
            ),
            true,
        ),
    }
}

fn check_token(api: &Api, reachable: bool) -> Check {
    const NAME: &str = "token";
    if !reachable {
        return Check::warn(NAME, "not checked: the server is unreachable", "fix the server check first");
    }
    let new_token = "mint a new token on the server (make token CLIENT=<this machine>) and set MEMORY_TOKEN, or re-run make connect-local";
    match api.get("/v1/me", TIMEOUT) {
        Err(e) => Check::fail(NAME, format!("request failed: {e}"), "fix the server check first"),
        Ok(resp) if resp.ok() => {
            let me = resp.json().unwrap_or(Value::Null);
            let scopes: Vec<&str> = me["scopes"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let detail = format!(
                "accepted: {} token for {}, scopes {}",
                me["token_kind"].as_str().unwrap_or("?"),
                me["device_id"].as_str().unwrap_or("?"),
                scopes.join(",")
            );
            if scopes.contains(&"events:write") {
                Check::ok(NAME, detail)
            } else {
                Check::warn(
                    NAME,
                    format!("{detail}; it cannot record events (a runner token?)"),
                    "use a device token for capture: make token KIND=device",
                )
            }
        }
        Ok(resp) if matches!(resp.status, 401 | 403) => {
            let code = resp.json().and_then(|j| j["error"]["code"].as_str().map(str::to_owned)).unwrap_or_default();
            Check::fail(NAME, format!("rejected (HTTP {} {code})", resp.status), new_token)
        }
        Ok(resp) => Check::fail(
            NAME,
            format!("HTTP {} from /v1/me", resp.status),
            "run `memvault admin doctor` on the server host",
        ),
    }
}

/// `MEMORY_OBSERVER` makes every hook return at once (the recursion guard); set in the shell that starts
/// Claude Code, it silently turns capture off.
fn check_environment() -> Check {
    const NAME: &str = "environment";
    if std::env::var_os("MEMORY_OBSERVER").is_some_and(|v| !v.is_empty()) {
        Check::fail(
            NAME,
            "MEMORY_OBSERVER is set: every hook exits immediately",
            "unset MEMORY_OBSERVER (it belongs only inside observer runs)",
        )
    } else {
        Check::ok(NAME, "MEMORY_OBSERVER is not set")
    }
}

fn check_binary() -> Check {
    match std::env::current_exe() {
        Ok(path) => Check::ok("hook binary", format!("{} (version {})", path.display(), env!("CARGO_PKG_VERSION"))),
        Err(e) => Check::warn("hook binary", format!("cannot tell where it runs from: {e}"), "run it by its full path"),
    }
}

/// `$CLAUDE_CONFIG_DIR`, default `~/.claude`.
fn claude_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => util::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".claude"),
    }
}

/// What Claude Code settings say about the memory plugin.
#[derive(Debug, PartialEq, Eq)]
enum PluginState {
    Enabled(String),
    Disabled(String),
    Absent,
}

/// `enabledPlugins` holds `plugin@marketplace: bool`; the memory plugin is `memory@...`.
fn plugin_state(settings: &Value) -> PluginState {
    let mut disabled = None;
    for (key, value) in settings["enabledPlugins"].as_object().into_iter().flatten() {
        if key.starts_with("memory@") {
            if value.as_bool() == Some(true) {
                return PluginState::Enabled(key.clone());
            }
            disabled = Some(key.clone());
        }
    }
    disabled.map_or(PluginState::Absent, PluginState::Disabled)
}

/// Reads the settings files that can enable the plugin: user settings, then the working directory's
/// project and local settings. Offline, no `claude` process.
fn check_plugin() -> Check {
    const NAME: &str = "plugin";
    let user = claude_dir().join("settings.json");
    let project = std::env::current_dir().unwrap_or_default().join(".claude");
    let files = [user.clone(), project.join("settings.json"), project.join("settings.local.json")];
    let (mut state, mut hooks_off, mut unreadable) = (PluginState::Absent, None, None);
    for path in files.iter().filter(|p| p.exists()) {
        let settings = match std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
        {
            Ok(v) => v,
            Err(e) => {
                unreadable = Some(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if settings["disableAllHooks"].as_bool() == Some(true) {
            hooks_off = Some(path.display().to_string());
        }
        match plugin_state(&settings) {
            PluginState::Enabled(n) => state = PluginState::Enabled(n),
            PluginState::Disabled(n) if state == PluginState::Absent => state = PluginState::Disabled(n),
            _ => {}
        }
    }
    if let Some(path) = hooks_off {
        return Check::fail(
            NAME,
            format!("disableAllHooks is true in {path}: no hook runs"),
            format!("set disableAllHooks to false in {path}"),
        );
    }
    match state {
        PluginState::Enabled(name) => Check::ok(NAME, format!("{name} is enabled")),
        PluginState::Disabled(name) => {
            Check::fail(NAME, format!("{name} is installed but disabled"), format!("claude plugin enable {name}"))
        }
        PluginState::Absent => match unreadable {
            Some(why) => Check::warn(NAME, format!("cannot read Claude Code settings ({why})"), "claude plugin list"),
            None if !user.exists() => Check::warn(
                NAME,
                format!("{} does not exist: cannot tell whether the plugin is installed", user.display()),
                "claude plugin list; if memory is missing: make connect-local",
            ),
            None => Check::fail(
                NAME,
                format!("the memory plugin is not enabled in {}", user.display()),
                "claude plugin install memory@memoryvault (or make connect-local)",
            ),
        },
    }
}

/// Undelivered events: how many lines wait in the spool and when the oldest happened (UTC seconds).
fn scan_spool(dir: &Path) -> (usize, Option<i64>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect())
        .unwrap_or_default();
    files.sort();
    let (mut depth, mut oldest) = (0, None);
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        let mut lines = text.lines().filter(|l| !l.trim().is_empty()).peekable();
        if lines.peek().is_none() {
            continue;
        }
        if oldest.is_none() {
            let occurred = lines
                .peek()
                .and_then(|l| serde_json::from_str::<Value>(l).ok())
                .and_then(|v| util::parse_rfc3339(v["occurred_at"].as_str()?));
            let mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
            oldest = occurred.or_else(|| {
                mtime.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64)
            });
        }
        depth += lines.count();
    }
    (depth, oldest)
}

/// `45s`, `12m`, `5h`, `3d`.
fn age(seconds: i64) -> String {
    match seconds.max(0) {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn check_spool(home: &Path) -> Check {
    const NAME: &str = "spool";
    let dir = home.join("spool");
    let (depth, oldest) = scan_spool(&dir);
    let Some(oldest) = oldest else {
        return Check::ok(NAME, "empty: nothing waits for delivery");
    };
    let waiting = util::unix_now().0 - oldest;
    let detail = format!("{depth} undelivered event(s), the oldest {} old ({})", age(waiting), dir.display());
    let fix =
        "fix the server and token checks above, then start a Claude Code session: the spool replays at session start";
    if waiting >= SPOOL_STALE_SECONDS { Check::fail(NAME, detail, fix) } else { Check::warn(NAME, detail, fix) }
}

fn check_session_cache(home: &Path) -> Check {
    const NAME: &str = "session cache";
    let dir = store::sessions_dir(home);
    let probe = dir.join(".doctor-probe");
    let result = util::mkdir_private(&dir)
        .and_then(|()| util::write_private(&probe, "ok"))
        .and_then(|()| std::fs::remove_file(&probe));
    match result {
        Ok(()) => Check::ok(NAME, format!("{} is writable", dir.display())),
        Err(e) => Check::fail(
            NAME,
            format!("cannot write in {}: {e}", dir.display()),
            format!("check the permissions and free space of {}", dir.display()),
        ),
    }
}

fn check_last_delivery(home: &Path) -> Check {
    const NAME: &str = "last delivery";
    let marker = store::delivery_marker(home);
    let at = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    match at {
        Some(at) => Check::ok(NAME, format!("{} ({} ago)", util::rfc3339(at, 0), age(util::unix_now().0 - at))),
        None => Check::warn(
            NAME,
            "no delivery recorded on this machine yet",
            "start a Claude Code session and run mem-hook doctor again; if it stays empty, nothing reaches the server",
        ),
    }
}

/// The server's `GET /v1/admin/doctor`, as `server: <name>` checks. Needs an admin token:
/// `MEMORY_ADMIN_TOKEN` (config file or environment), else the device token when that is one.
fn server_checks(home: &Path, cfg: Option<&Config>) -> Vec<Check> {
    const NAME: &str = "server checks";
    let Some(cfg) = cfg else {
        return vec![Check::warn(NAME, "not run: no usable config", CONNECT_FIX)];
    };
    let admin_token =
        config::lookup(&config::read_file(home), "MEMORY_ADMIN_TOKEN").unwrap_or_else(|| cfg.token.clone());
    let api = Api::new(&Config {
        home: cfg.home.clone(),
        url: cfg.url.clone(),
        token: admin_token,
        device: cfg.device.clone(),
        claude_bin: cfg.claude_bin.clone(),
        injection: None,
    });
    match api.get("/v1/admin/doctor", Duration::from_secs(10)) {
        Err(e) => vec![Check::warn(NAME, format!("not run: {e}"), "fix the server check first")],
        Ok(resp) if resp.ok() => {
            let body = resp.json().unwrap_or(Value::Null);
            let mut out: Vec<Check> = body["checks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| {
                    let status = match c["status"].as_str() {
                        Some("ok") => Status::Ok,
                        Some("warn") => Status::Warn,
                        _ => Status::Fail,
                    };
                    let name = format!("server: {}", c["name"].as_str().unwrap_or("?"));
                    Check::new(
                        &name,
                        status,
                        c["detail"].as_str().unwrap_or_default(),
                        c["fix"].as_str().map(str::to_owned),
                    )
                })
                .collect();
            if out.is_empty() {
                out.push(Check::warn(NAME, "the server returned no checks", "update the server"));
            }
            out
        }
        Ok(resp) if resp.status == 403 => vec![Check::warn(
            NAME,
            "skipped: the token has no admin scope",
            "mint an admin token (make token KIND=admin) and run with MEMORY_ADMIN_TOKEN=mem_... in the environment or ~/.memory/config",
        )],
        Ok(resp) if resp.status == 404 => {
            vec![Check::warn(NAME, "skipped: the server predates the doctor endpoint", "update the server")]
        }
        Ok(resp) => vec![Check::warn(
            NAME,
            format!("skipped: HTTP {}", resp.status),
            "run `memvault admin doctor` on the server host",
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lines_are_checked_like_the_parser_reads_them() {
        assert_eq!(bad_config_line("# c\nMEMORY_URL=x\nexport A=b\n\n"), None);
        assert_eq!(bad_config_line("MEMORY_URL=x\nMEMORY_TOKEN mem_abc\n"), Some((2, "MEMORY_TOKEN mem_abc")));
    }

    #[test]
    fn plugin_state_reads_enabled_plugins() {
        let on = json!({"enabledPlugins": {"other@m": true, "memory@memoryvault": true}});
        assert_eq!(plugin_state(&on), PluginState::Enabled("memory@memoryvault".into()));
        let off = json!({"enabledPlugins": {"memory@memoryvault": false}});
        assert_eq!(plugin_state(&off), PluginState::Disabled("memory@memoryvault".into()));
        assert_eq!(plugin_state(&json!({"enabledPlugins": {"other@m": true}})), PluginState::Absent);
        assert_eq!(plugin_state(&json!({})), PluginState::Absent);
    }

    #[test]
    fn ages_are_short() {
        assert_eq!([30, 600, 7199, 36_000, 400_000].map(age), ["30s", "10m", "119m", "10h", "4d"].map(String::from));
    }

    #[test]
    fn spool_scan_counts_lines_and_dates_the_oldest() {
        let dir = std::env::temp_dir().join(format!("mem-hook-doctor-spool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(scan_spool(&dir), (0, None));
        std::fs::write(dir.join("20260101.jsonl"), "{\"occurred_at\":\"2026-01-01T10:00:00.000Z\"}\n{}\n").unwrap();
        std::fs::write(dir.join("20260102.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x\n").unwrap();
        assert_eq!(scan_spool(&dir), (3, util::parse_rfc3339("2026-01-01T10:00:00Z")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
