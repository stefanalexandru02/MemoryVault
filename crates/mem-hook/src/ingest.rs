//! `mem-hook ingest [<path|dir>] [--scope project|global] [--glob PATTERN]... [--dry-run] [--prune] [--force]`:
//! send a project's docs (ADRs, READMEs, runbooks) to the server as searchable memory.
//!
//! Walks a file or a directory (markdown, text, rst and asciidoc by default; `--glob` picks other files, with
//! `**` for any number of directories and a pattern without `/` matching file names at any depth), skips
//! `.git`, `node_modules`, `target`, symlinks, binaries and files over [`MAX_FILE_BYTES`], hashes what is left
//! and compares it with `GET /v1/sources`, so a second run posts only the files that changed (`--force` posts
//! them all and lets the server restore chunks that were forgotten). `--prune` (for a directory) deletes the
//! sources under it whose file no longer exists, whatever the extension or `--glob`. Unlike the hooks this
//! command talks to the terminal and reports failure through its exit code: 0 when every file was handled, 1
//! when something failed, 2 for a bad command line.
//!
//! A source's uri is the path relative to the git root for the project scope (the root is the target itself
//! outside a repository) and the absolute path for the global scope.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::api::{Api, Resp};
use crate::config::{self, Config};
use crate::project;

const USAGE: &str =
    "usage: mem-hook ingest [<path|dir>] [--scope project|global] [--glob PATTERN]... [--dry-run] [--prune] [--force]";
const TIMEOUT: Duration = Duration::from_secs(30);
/// Files above this are skipped (the server refuses more than 400 KiB per source by default).
const MAX_FILE_BYTES: u64 = 400 * 1024;
const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "target"];
const DEFAULT_EXTENSIONS: [&str; 7] = ["md", "markdown", "txt", "text", "rst", "adoc", "asciidoc"];
/// A rate-limited request is retried this many times, after the server's `Retry-After` (at most a minute).
const RETRIES: u32 = 5;
const MAX_WAIT_SECONDS: u64 = 60;

#[derive(Debug, PartialEq, Eq)]
struct Options {
    path: PathBuf,
    global: bool,
    globs: Vec<String>,
    dry_run: bool,
    prune: bool,
    force: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut opts = Options {
        path: PathBuf::from("."),
        global: false,
        globs: Vec::new(),
        dry_run: false,
        prune: false,
        force: false,
    };
    let mut path = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--scope" => match it.next().map(String::as_str) {
                Some("project") => opts.global = false,
                Some("global") => opts.global = true,
                _ => return Err("--scope takes project or global".into()),
            },
            "--glob" => opts.globs.push(it.next().ok_or("--glob takes a pattern")?.clone()),
            "--dry-run" => opts.dry_run = true,
            "--prune" => opts.prune = true,
            "--force" => opts.force = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ if path.is_some() => return Err("only one path".into()),
            _ => path = Some(PathBuf::from(arg)),
        }
    }
    opts.path = path.unwrap_or(opts.path);
    Ok(opts)
}

/// Runs the command and returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let opts = match parse_args(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("mem-hook ingest: {e}\n{USAGE}");
            return 2;
        }
    };
    match ingest(&opts) {
        Ok(report) => {
            report.print(opts.dry_run);
            i32::from(report.errors > 0)
        }
        Err(e) => {
            eprintln!("mem-hook ingest: {e}");
            1
        }
    }
}

#[derive(Debug, Default)]
struct Report {
    created: Vec<String>,
    changed: Vec<String>,
    unchanged: usize,
    removed: Vec<String>,
    skipped: Vec<(String, &'static str)>,
    errors: usize,
}

impl Report {
    fn error(&mut self, msg: String) {
        eprintln!("error: {msg}");
        self.errors += 1;
    }

    fn print(&self, dry_run: bool) {
        let would = if dry_run { "would " } else { "" };
        for uri in &self.created {
            println!("+ {uri}");
        }
        for uri in &self.changed {
            println!("~ {uri}");
        }
        for uri in &self.removed {
            println!("- {uri}");
        }
        for (path, why) in &self.skipped {
            println!("skip {path} ({why})");
        }
        println!(
            "ingest: {would}send {} new and {} changed, {would}remove {}; {} unchanged, {} skipped, {} failed{}",
            self.created.len(),
            self.changed.len(),
            self.removed.len(),
            self.unchanged,
            self.skipped.len(),
            self.errors,
            if dry_run { " (dry run, nothing sent)" } else { "" }
        );
    }
}

fn ingest(opts: &Options) -> Result<Report, String> {
    let home = config::memory_home();
    let cfg = Config::load(&home).ok_or_else(|| {
        format!("no MEMORY_URL and MEMORY_TOKEN in {} (run make connect-local)", home.join("config").display())
    })?;
    let target = fs::canonicalize(&opts.path).map_err(|e| format!("{}: {e}", opts.path.display()))?;
    let is_dir = target.is_dir();
    if opts.prune && !is_dir {
        return Err("--prune applies to a directory".into());
    }
    let dir = if is_dir { target.clone() } else { target.parent().map(Path::to_path_buf).unwrap_or_default() };
    let dir_text = dir.to_string_lossy().into_owned();
    let (scope, root) = if opts.global {
        (json!({"kind": "global", "key": ""}), PathBuf::new())
    } else {
        let root = project::toplevel(&dir_text).unwrap_or_else(|| dir.clone());
        (json!({"kind": "project", "key": project::resolve(&dir_text)}), root)
    };
    let scope_text = match scope["kind"].as_str() {
        Some("project") => format!("project:{}", scope["key"].as_str().unwrap_or_default()),
        _ => "global:".to_string(),
    };

    let api = Api::new(&cfg);
    let known = list_sources(&api, &scope_text)?;
    let files =
        if is_dir { walk(&target).map_err(|e| format!("{}: {e}", target.display()))? } else { vec![target.clone()] };

    let mut report = Report::default();
    for file in files {
        let rel_to_target = relative(&file, if is_dir { &target } else { &dir });
        if is_dir && !selected(&rel_to_target, &opts.globs) {
            continue;
        }
        let uri = source_uri(&file, &root, opts.global);
        let content = match read_text(&file) {
            Ok(text) => text,
            Err(why) => {
                report.skipped.push((uri, why));
                continue;
            }
        };
        let hash = hex::encode(Sha256::digest(content.as_bytes()));
        match known.get(&uri) {
            Some(old) if *old == hash && !opts.force => {
                report.unchanged += 1;
                continue;
            }
            Some(_) => report.changed.push(uri.clone()),
            None => report.created.push(uri.clone()),
        }
        if opts.dry_run {
            continue;
        }
        let content_type = if matches!(extension(&file).as_str(), "md" | "markdown") { "markdown" } else { "text" };
        let body = json!({
            "scope": scope, "uri": uri, "content_type": content_type, "content": content, "content_hash": hash,
        });
        match send_with_retry(|| api.post("/v1/sources", &body, TIMEOUT)) {
            // A forced post of a file the server already has whole.
            Ok(resp) if resp.json().is_some_and(|v| v["status"] == "unchanged") => {
                report.changed.retain(|u| *u != uri);
                report.unchanged += 1;
            }
            Ok(_) => {}
            Err(e) => {
                report.error(format!("{uri}: {e}"));
                // Not sent: it must not be reported as ingested.
                report.created.retain(|u| *u != uri);
                report.changed.retain(|u| *u != uri);
            }
        }
    }

    if opts.prune {
        let prefix = if opts.global {
            format!("{}/", posix(&target))
        } else {
            match target.strip_prefix(&root) {
                Ok(rel) if rel.as_os_str().is_empty() => String::new(),
                Ok(rel) => format!("{}/", posix(rel)),
                Err(_) => String::new(),
            }
        };
        // A file that exists stays, even when this run's extensions or globs leave it out.
        let mut gone: Vec<&String> =
            known.keys().filter(|u| u.starts_with(&prefix) && !root.join(u.as_str()).exists()).collect();
        gone.sort();
        for uri in gone {
            report.removed.push(uri.clone());
            if opts.dry_run {
                continue;
            }
            let path = format!("/v1/sources?uri={}&scope={}", encode(uri), encode(&scope_text));
            match send_with_retry(|| api.delete(&path, TIMEOUT)) {
                Ok(_) => {}
                // Removed since the listing, by someone else: the goal holds.
                Err(e) if e.starts_with("404") => {}
                Err(e) => {
                    report.error(format!("{uri}: {e}"));
                    report.removed.retain(|u| u != uri);
                }
            }
        }
    }
    Ok(report)
}

/// `uri -> content_hash` of the live sources in the scope.
fn list_sources(api: &Api, scope_text: &str) -> Result<HashMap<String, String>, String> {
    let path = format!("/v1/sources?scope={}", encode(scope_text));
    let resp = send_with_retry(|| api.get(&path, TIMEOUT)).map_err(|e| format!("cannot list sources: {e}"))?;
    let body = resp.json().ok_or("cannot list sources: the server answered with something else than JSON")?;
    let rows = body["sources"].as_array().ok_or("cannot list sources: no sources in the answer")?;
    Ok(rows
        .iter()
        .filter_map(|s| Some((s["uri"].as_str()?.to_string(), s["content_hash"].as_str()?.to_string())))
        .collect())
}

/// The response of a successful call, else `"<status> <message>"` (or the transport error). A 429 waits for
/// the server's `Retry-After` and tries again.
fn send_with_retry(mut call: impl FnMut() -> Result<Resp, String>) -> Result<Resp, String> {
    let mut retries = 0;
    loop {
        let resp = call()?;
        if resp.ok() {
            return Ok(resp);
        }
        if resp.status == 429 && retries < RETRIES {
            retries += 1;
            sleep(Duration::from_secs(resp.retry_after.unwrap_or(2).min(MAX_WAIT_SECONDS)));
            continue;
        }
        let message = resp.json().and_then(|v| v["error"]["message"].as_str().map(String::from)).unwrap_or(resp.body);
        return Err(format!("{} {message}", resp.status));
    }
}

/// Every file under `dir`, in path order, without the directories in [`SKIP_DIRS`] and without symlinks.
fn walk(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            if !SKIP_DIRS.iter().any(|d| entry.file_name() == *d) {
                out.extend(walk(&path)?);
            }
        } else if kind.is_file() {
            out.push(path);
        }
    }
    Ok(out)
}

fn extension(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Whether the file at `rel` (relative to the ingested directory) is one to send.
fn selected(rel: &str, globs: &[String]) -> bool {
    if globs.is_empty() {
        let ext = extension(Path::new(rel));
        return DEFAULT_EXTENSIONS.contains(&ext.as_str());
    }
    globs.iter().any(|g| glob_match(g, rel))
}

/// The text of a file worth sending, or why not.
fn read_text(path: &Path) -> Result<String, &'static str> {
    let len = fs::metadata(path).map_err(|_| "unreadable")?.len();
    if len > MAX_FILE_BYTES {
        return Err("too large");
    }
    let bytes = fs::read(path).map_err(|_| "unreadable")?;
    if bytes.contains(&0) {
        return Err("binary");
    }
    let text = String::from_utf8(bytes).map_err(|_| "binary")?;
    if text.trim().is_empty() { Err("empty") } else { Ok(text) }
}

fn posix(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn relative(file: &Path, base: &Path) -> String {
    posix(file.strip_prefix(base).unwrap_or(file))
}

fn source_uri(file: &Path, root: &Path, global: bool) -> String {
    if global { posix(file) } else { relative(file, root) }
}

/// Percent-encoding of a query value: letters, digits and `-_.~/:` stay.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/:".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Shell-style match of a `/`-separated path: `*` and `?` stay inside a segment, a `**` segment stands for any
/// number of segments (also none). A pattern without `/` is matched against the file name alone.
fn glob_match(pattern: &str, path: &str) -> bool {
    if !pattern.contains('/') {
        return segment_match(pattern.as_bytes(), path.rsplit('/').next().unwrap_or(path).as_bytes());
    }
    let pat: Vec<&str> = pattern.trim_start_matches("./").split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    segments_match(&pat, &segs)
}

fn segments_match(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|skip| segments_match(rest, &segs[skip..])),
        Some((p, rest)) => segs
            .split_first()
            .is_some_and(|(s, tail)| segment_match(p.as_bytes(), s.as_bytes()) && segments_match(rest, tail)),
    }
}

fn segment_match(pat: &[u8], text: &[u8]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_match(rest, &text[skip..])),
        Some((b'?', rest)) => text.split_first().is_some_and(|(_, tail)| segment_match(rest, tail)),
        Some((c, rest)) => text.split_first().is_some_and(|(t, tail)| t == c && segment_match(rest, tail)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn arguments() {
        let o = parse_args(&args(&[
            "docs",
            "--scope",
            "global",
            "--glob",
            "**/*.md",
            "--glob",
            "*.rst",
            "--dry-run",
            "--prune",
            "--force",
        ]))
        .unwrap();
        assert_eq!(
            o,
            Options {
                path: "docs".into(),
                global: true,
                globs: vec!["**/*.md".into(), "*.rst".into()],
                dry_run: true,
                prune: true,
                force: true
            }
        );
        let o = parse_args(&[]).unwrap();
        assert_eq!((o.path, o.global, o.dry_run, o.prune, o.force), (PathBuf::from("."), false, false, false, false));
        for bad in [&["--scope", "device"][..], &["--scope"], &["--glob"], &["a", "b"], &["--nope"]] {
            assert!(parse_args(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn globs() {
        for (pattern, path, want) in [
            ("**/*.md", "README.md", true),
            ("**/*.md", "docs/adr/0001.md", true),
            ("**/*.md", "docs/adr/0001.txt", false),
            ("*.md", "docs/adr/0001.md", true),
            ("docs/*.md", "docs/a.md", true),
            ("docs/*.md", "docs/adr/a.md", false),
            ("docs/**/*.md", "docs/a.md", true),
            ("docs/**/*.md", "docs/adr/a.md", true),
            ("docs/**/*.md", "src/docs/a.md", false),
            ("./docs/**", "docs/a/b.md", true),
            ("ADR-?.md", "ADR-1.md", true),
            ("ADR-?.md", "ADR-10.md", false),
            ("a*c", "abbbc", true),
            ("a*c", "a/c", false),
        ] {
            assert_eq!(glob_match(pattern, path), want, "{pattern} vs {path}");
        }
    }

    #[test]
    fn default_selection_is_by_extension() {
        for (path, want) in [
            ("README.md", true),
            ("docs/Runbook.MD", true),
            ("notes.txt", true),
            ("guide.rst", true),
            ("manual.adoc", true),
            ("main.rs", false),
            ("Makefile", false),
            ("image.png", false),
        ] {
            assert_eq!(selected(path, &[]), want, "{path}");
        }
        assert!(selected("config/app.yaml", &["**/*.yaml".to_string()]));
        assert!(!selected("README.md", &["**/*.yaml".to_string()]));
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(encode("project:github.com/alex/home lab"), "project:github.com/alex/home%20lab");
        assert_eq!(encode("a&b=c#d+é"), "a%26b%3Dc%23d%2B%C3%A9");
    }

    #[test]
    fn uris_are_relative_to_the_root_or_absolute() {
        let root = Path::new("/repo");
        assert_eq!(source_uri(Path::new("/repo/docs/a.md"), root, false), "docs/a.md");
        assert_eq!(source_uri(Path::new("/repo/docs/a.md"), root, true), "/repo/docs/a.md");
    }
}
