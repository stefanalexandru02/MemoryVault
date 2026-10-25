# MemoryVault Claude Code plugin

A Claude Code plugin with four parts:

- **Hooks** that record a session into MemoryVault, inject recalled context when it starts, and mention earlier
  work on a file the first time it is read. All behavior lives in one Rust binary, `bin/mem-hook`, with one
  subcommand per hook event (spec section 8).
- **The MCP server** `memory` (`.mcp.json`), with the tools `memory_search`, `memory_timeline`, `memory_get`,
  `memory_explain`, `memory_history`, `memory_remember`, `memory_forget`, `memory_feedback` and `work_state_write`. It reads its URL and token from the
  plugin's own settings (`userConfig`); the token is kept in the keychain, not in a file you edit.
- **Skills**, invoked as `/memory:<name>` or picked up by Claude from the description:

  | Skill | Use it to |
  |---|---|
  | `recall` | search earlier work (search, then timeline, then get), look at it as of an earlier date, trace where a memory came from, and tell memory when a recalled fact was wrong or outdated |
  | `remember` | store a fact, note or procedure on request, or forget one after confirming it |
  | `brief` | compile a sourced briefing on a topic from facts, procedures and past work |
  | `project-history` | write a narrative of a project over a period from the recorded timeline, using `as_of` and `memory_history` for what changed |
  | `handoff` | leave the goal, state and next steps as work state for the next session, on any machine |
  | `how-it-works` | explain what is captured, where it lives, and how to inspect or delete it |

- **Install and wiring scripts**: `install.sh` builds the binary, `connect.sh` wires a machine up in one step.

The quick way, from the repository root: `make connect-local` (this machine's own stack) or
`make connect-remote URL=... TOKEN=...`; `make disconnect` undoes it. Add `INJECTION=lean` to start sessions with
only the profile, open work and a rubric for the search tools instead of the full block (see "Full and lean
session context" in the main README); `make connect-status` shows the tokens served. The manual steps follow.

Requirements on the machine: the `claude` CLI (the observer runs `claude -p`) and `git` (project
detection). No Node, Python, `curl` or `jq`.

## Install

```bash
./install.sh
```

It runs `cargo build --profile release-small -p mem-hook` (size-optimised, about 1.9 MB), copies the binary to `bin/mem-hook`, and creates
`~/.memory` (mode 0700) with a `config` template (mode 0600). Set `MEM_HOOK_BIN=/path/to/mem-hook` to
install a prebuilt binary instead of building. On Linux the build is statically linked
(`-C target-feature=+crt-static`, so the one binary runs on any distribution); `MEM_HOOK_STATIC=0`
builds a dynamic one. For a musl build instead, install `musl-tools` and run
`CC_x86_64_unknown_linux_musl=musl-gcc cargo build --profile release-small -p mem-hook --target x86_64-unknown-linux-musl`.

Windows: the hook code is portable (`cfg(unix)` / `cfg(windows)` for file modes, locking, detaching and
process-group kills; it type-checks for `x86_64-pc-windows-gnu`, but nobody has run it there yet). Build
`cargo build --profile release-small -p mem-hook --target x86_64-pc-windows-msvc`, copy the result to
`bin/mem-hook.exe`, and keep the config in `%USERPROFILE%\.memory`. `install.sh` itself is a bash script.

Then:

1. Fill in `~/.memory/config`, which the hooks read:

   ```
   MEMORY_URL=https://memory.<tailnet>.ts.net
   MEMORY_TOKEN=mem_...
   # DEVICE_ID=laptop-x1        (default: hostname)
   # CLAUDE_BIN=/usr/local/bin/claude   (default: claude from PATH; useful under cron)
   ```

2. Install the plugin from the local marketplace in this repository, then give it the same URL and token for its
   MCP server:

   ```bash
   claude plugin marketplace add /path/to/MemoryVault
   claude plugin install memory@memoryvault
   printf '{"memory_url":"%s","memory_token":"%s"}' "$MEMORY_URL" "$MEMORY_TOKEN" \
     | claude plugin configure memory@memoryvault --values-stdin
   ```

   Or run `claude plugin install memory@memoryvault` and fill in the two values in `/plugin`.

3. Pre-approve the tools in `~/.claude/settings.json`:

   ```json
   { "permissions": { "allow": ["mcp__plugin_memory_memory__*"] } }
   ```

   Start a new session afterwards. For a one-off test without installing, use
   `claude --plugin-dir /path/to/clients/claude-code-plugin`.

Headless runs that should use memory pass `--allowedTools "mcp__plugin_memory_memory__*"`.

## What each hook does

| Subcommand | Hook | Behavior |
|---|---|---|
| `session-start` | SessionStart | Replays the spool (oldest first, at most 1.5 s), posts a `system` event, fetches `GET /v1/context` (3 s; with `&mode=lean` or `&mode=full` when `MEM_INJECTION` is set) and prints it as `additionalContext`. On failure it prints the cached block from `~/.memory/cache/<project_key>.md` prefixed with `> memory: server unreachable, showing cached context from <time>`. On `resume` the event is posted but no context is returned. |
| `prompt` | UserPromptSubmit | Posts `user_prompt`. Returns nothing. |
| `tool` | PostToolUse | Posts `tool_use`. When the server answers `window_closed: true`, starts the observer. |
| `tool-failure` | PostToolUseFailure | Posts `tool_failure`; same observer rule. |
| `file-context` | PreToolUse (`Read`) | Looks up observations that touched the file (`GET /v1/observations/by-file`, 1.5 s) and, the first time a file is read in a session, prints up to five of them (date, type, title, id) as `additionalContext`. It never blocks or changes the read, and prints nothing when there is nothing to say. |
| `stop` | Stop | Posts `assistant_message` and starts the observer. Nothing is posted while `stop_hook_active` is true. |
| `subagent-stop` | SubagentStop | Posts `subagent_stop`. |
| `session-end` | SessionEnd | `POST /v1/sessions/{id}/end` within a 1 s budget, then starts the observer with `--final`. |
| `observe` | (spawned) | The observer runner, below. |
| `doctor` | (by hand) | Diagnosis, below. Unlike the hooks it prints to the terminal and exits 1 when a check fails. |

Every hook invocation exits 0, whatever happens (`doctor` is the exception). Each starts with the same checks: nothing happens when
`MEMORY_OBSERVER` is set (recursion guard), when `MEMORY_URL`/`MEMORY_TOKEN` are missing (a one-line
stderr hint once a day on `session-start`), or when the project is listed in `~/.memory/exclude`.

### Project key

`git remote get-url origin` with scheme, user, port and `.git` removed, lowercased
(`github.com/alex/homelab`). Without a remote: `local/<repo name>`, where worktrees resolve through
`git rev-parse --git-common-dir` to the main repository. Outside git: `local/<directory name>`.
`~/.memory/exclude` holds one project key per line (`#` comments allowed).

### Failures and the spool

Event sequence numbers are allocated locally (`~/.memory/sessions/<claude_session_id>`), so every event
has a stable idempotency key (`<session>:<seq>`). A request that cannot be delivered (network error,
timeout, 5xx, 429, 401/403/404) is appended to `~/.memory/spool/YYYYMMDD.jsonl` and replayed in order at the
next `session-start`; replay stops at the first failure. A request the server rejects as invalid (other
4xx) is dropped. The request timeout is 2 s.

## Doctor

`mem-hook doctor [--json] [--server]` answers "why is nothing being remembered?". It prints one line per check
with `[ ok ]`, `[warn]` or `[FAIL]` and, unless the check passed, the command that fixes it:

| Check | Looks at |
|---|---|
| `config` | `~/.memory/config` exists, every line is `KEY=value`, `MEMORY_URL` and `MEMORY_TOKEN` are set |
| `server` | `GET /health` (3 s) |
| `token` | `GET /v1/me`: accepted, token kind and scopes (a token without `events:write` cannot record) |
| `environment` | `MEMORY_OBSERVER` is not set (it makes every hook return at once) |
| `hook binary` | the path and version of the running binary |
| `plugin` | `enabledPlugins` and `disableAllHooks` in `~/.claude/settings.json` (`$CLAUDE_CONFIG_DIR`) and the project's `.claude/settings*.json`; read from disk, `claude` is not run |
| `spool` | undelivered events and the age of the oldest (a warning, a failure after a day) |
| `session cache` | `~/.memory/sessions` is writable |
| `last delivery` | when the server last accepted a request from this machine (`~/.memory/last-delivery`, refreshed at most once a minute) |

`--json` prints the same checks as `{"checks": [{name, status, detail, fix}]}`. `--server` appends the server's
own checks from `GET /v1/admin/doctor`, which needs an admin token: set `MEMORY_ADMIN_TOKEN` in the config file
or the environment (the device token is tried otherwise, and the section is skipped with a hint when it is
refused). `make doctor` in the repository runs `mem-hook doctor --server`.

## The observer

`mem-hook observe --session <server-session-id> [--final]` takes the machine-wide lock
`~/.memory/observe.lock` (a second observer exits at once), then leases up to 3 packets (10 with
`--final`) for the session and runs each through:

```
claude -p --model <model_hint> --output-format json --disallowedTools "*" --no-session-persistence \
  --max-turns 1 --settings '{"disableAllHooks": true}' --system-prompt <system> <user>
```

in a fresh temporary directory with `MEMORY_OBSERVER=1`, 180 s timeout (`MEMORY_OBSERVE_TIMEOUT_S`
overrides). A prompt over 100 KB is sent on stdin because Linux limits a single argument to 128 KB.
The model's text is posted to `/v1/work/{id}/result` with `meta` (model, duration, cost,
`runner_kind: "claude-p"`, device). On `422 schema_invalid` the packet is re-run once with the error
appended and `--model opus`, then failed. A failed run is reported to `/v1/work/{id}/fail` as
`quota_exhausted`, `cli_missing`, `timeout` or `other`; quota and missing-CLI errors end the run.
Progress is logged to `~/.memory/observe.log`.

### Nightly runner

```
mem-hook observe --all [--kinds extract,summarize_prompt,summarize_session,consolidate_facts,consolidate_summary,consolidate_procedures] [--max 50]
```

leases packets of any session. Run it from cron on an always-on machine with `claude`, using a
`runner` token in that machine's config. Cron has a minimal `PATH`: set `CLAUDE_BIN` in the config.

## Files under `~/.memory`

| Path | Content |
|---|---|
| `config` | `MEMORY_URL`, `MEMORY_TOKEN`, optional `DEVICE_ID`, `CLAUDE_BIN`, `MEM_INJECTION` (`lean` or `full`), `MEMORY_ADMIN_TOKEN` (mode 0600) |
| `exclude` | project keys to ignore |
| `sessions/` | per-session cache: server session id, next sequence number, window, prompt count |
| `spool/` | undelivered requests, one JSON line each |
| `last-delivery` | modification time = last request the server accepted (for `doctor`) |
| `cache/` | last context block per project |
| `observe.lock`, `observe.log` | observer lock and log |

Set `MEMORY_HOME` to use another directory, and `MEMORY_DEBUG=1` to print why a hook spooled or skipped.
The binary talks to the server directly: it ignores `HTTP(S)_PROXY`, since the server is on the tailnet.
