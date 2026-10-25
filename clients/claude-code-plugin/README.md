# MemoryVault Claude Code plugin

Hooks that record a Claude Code session into MemoryVault and inject recalled context when a session
starts. All behavior lives in one Rust binary, `bin/mem-hook`, with one subcommand per hook event
(spec section 8). The MCP tools (`memory_search`, `memory_get`, ...) are registered separately with
`claude mcp add`.

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

1. Fill in `~/.memory/config`:

   ```
   MEMORY_URL=https://memory.<tailnet>.ts.net
   MEMORY_TOKEN=mem_...
   # DEVICE_ID=laptop-x1        (default: hostname)
   # CLAUDE_BIN=/usr/local/bin/claude   (default: claude from PATH; useful under cron)
   ```

   Export it from your shell profile so Claude Code can expand the variables in the MCP entry:
   `set -a; . ~/.memory/config; set +a`. Do not name the token `ANTHROPIC_API_KEY` or
   `ANTHROPIC_AUTH_TOKEN`; Claude Code blanks those in MCP headers.

2. Register the MCP server (user scope, token by reference):

   ```bash
   claude mcp add -s user -t http memory '${MEMORY_URL}/mcp' -H 'Authorization: Bearer ${MEMORY_TOKEN}'
   claude mcp list    # warns if a variable is unset
   ```

3. Pre-approve the tools in `~/.claude/settings.json`:

   ```json
   { "permissions": { "allow": ["mcp__memory__*"] } }
   ```

4. Load the plugin: `claude --plugin-dir /path/to/clients/claude-code-plugin`, or install it from a
   marketplace that lists this directory.

Headless runs that should use memory pass `--allowedTools "mcp__memory__*"`.

## What each hook does

| Subcommand | Hook | Behavior |
|---|---|---|
| `session-start` | SessionStart | Replays the spool (oldest first, at most 1.5 s), posts a `system` event, fetches `GET /v1/context` (3 s) and prints it as `additionalContext`. On failure it prints the cached block from `~/.memory/cache/<project_key>.md` prefixed with `> memory: server unreachable, showing cached context from <time>`. On `resume` the event is posted but no context is returned. |
| `prompt` | UserPromptSubmit | Posts `user_prompt`. Returns nothing. |
| `tool` | PostToolUse | Posts `tool_use`. When the server answers `window_closed: true`, starts the observer. |
| `tool-failure` | PostToolUseFailure | Posts `tool_failure`; same observer rule. |
| `stop` | Stop | Posts `assistant_message` and starts the observer. Nothing is posted while `stop_hook_active` is true. |
| `subagent-stop` | SubagentStop | Posts `subagent_stop`. |
| `session-end` | SessionEnd | `POST /v1/sessions/{id}/end` within a 1 s budget, then starts the observer with `--final`. |
| `observe` | (spawned) | The observer runner, below. |

Every invocation exits 0, whatever happens. Each starts with the same checks: nothing happens when
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
| `config` | `MEMORY_URL`, `MEMORY_TOKEN`, optional `DEVICE_ID`, `CLAUDE_BIN` (mode 0600) |
| `exclude` | project keys to ignore |
| `sessions/` | per-session cache: server session id, next sequence number, window, prompt count |
| `spool/` | undelivered requests, one JSON line each |
| `cache/` | last context block per project |
| `observe.lock`, `observe.log` | observer lock and log |

Set `MEMORY_HOME` to use another directory, and `MEMORY_DEBUG=1` to print why a hook spooled or skipped.
The binary talks to the server directly: it ignores `HTTP(S)_PROXY`, since the server is on the tailnet.
