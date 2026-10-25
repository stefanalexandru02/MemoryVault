//! mem-hook: the Claude Code hook binary (spec 8). One subcommand per hook event
//! (`session-start`, `prompt`, `tool`, `tool-failure`, `stop`, `subagent-stop`, `session-end`, `file-context`)
//! plus `observe` (the observer runner; `observe --all` is the nightly runner), `doctor` (diagnosis) and
//! `ingest` (send a project's docs to the server).
//!
//! Blocking HTTP with `ureq`, no async runtime, no dependency on memvault-core. A hook never
//! fails Claude Code: whatever goes wrong, the process exits 0. `doctor` and `ingest` are run by hand and report
//! failure through their exit code.

mod api;
mod config;
mod deliver;
mod doctor;
mod hooks;
mod ingest;
mod observe;
mod project;
mod spool;
mod store;
mod util;

fn main() {
    // `doctor` is interactive: it reports through its exit code, so it skips the guards below.
    if std::env::args().nth(1).as_deref() == Some("doctor") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(doctor::run(&args));
    }
    // `ingest` is run by hand too, and its failures are the point.
    if std::env::args().nth(1).as_deref() == Some("ingest") {
        let args: Vec<String> = std::env::args().skip(2).collect();
        std::process::exit(ingest::run(&args));
    }
    // A panic must not become a hook failure.
    std::panic::set_hook(Box::new(|_| std::process::exit(0)));
    // Recursion guard (spec 8.3 step 1): never act inside an observer run.
    if std::env::var_os("MEMORY_OBSERVER").is_some_and(|v| !v.is_empty()) {
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("observe") => observe::run(&args[1..]),
        Some(event) => hooks::run(event),
        None => {
            eprintln!(
                "usage: mem-hook <session-start|prompt|tool|tool-failure|stop|subagent-stop|session-end|file-context|observe|doctor|ingest>"
            )
        }
    }
}
