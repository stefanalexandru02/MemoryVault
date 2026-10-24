//! mem-hook: the Claude Code hook binary (spec 8). One subcommand per hook event
//! (`session-start`, `prompt`, `tool`, `tool-failure`, `stop`, `subagent-stop`, `session-end`)
//! plus `observe` (the observer runner, `observe --all` for the nightly runner). Blocking HTTP
//! with `ureq`, no async runtime, no dependency on memvault-core. Skeleton: the owner fills it in.

fn main() {
    // Recursion guard (spec 8.3 step 1): never act inside an observer run.
    if std::env::var_os("MEMORY_OBSERVER").is_some() {
        return;
    }
    let cmd = std::env::args().nth(1).unwrap_or_default();
    eprintln!("mem-hook {cmd}: not implemented yet");
}
