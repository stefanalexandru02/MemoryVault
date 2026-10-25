# Clients

MemoryVault has two halves on a client: the MCP server (recall and the write tools, works in any MCP client) and
capture (hooks that record the session and inject context at its start). Capture is done by the one Rust binary
`mem-hook`.

| Client | MCP server | Capture | Context at session start | Set up with |
|---|---|---|---|---|
| Claude Code | yes (plugin) | all hook events, file context on `Read` | yes | `make connect-local` ([plugin README](claude-code-plugin/README.md)) |
| Codex CLI | yes | session start/end, prompts, tool results, final answers | yes, once its hooks are approved | `make connect-local AGENT=codex` |
| Gemini CLI | yes | session start/end, prompts, tool results, final answers | yes | `make connect-local AGENT=gemini` |
| Any other agent CLI that can run a command on events | by its own MCP config | through `mem-hook event` | no | by hand, below |

What Codex CLI and Gemini CLI do not get: failed tool calls as their own event (the error stays inside the tool
response), subagent events, and the "earlier work on this file" hint. Both are recorded as sessions with the tool
name `codex` or `gemini` (shown as "tool" in the viewer), and the extraction and summaries that make memories out of
sessions run exactly as for Claude Code.

## The generic adapter: `mem-hook event --client <name>`

```bash
echo '{"session_id":"s1","cwd":"/work/proj","event":"prompt","text":"fix the flaky test"}' \
  | mem-hook event --client myagent
```

One JSON object on stdin, one event per call, through the same delivery, spool and exclude list as the Claude Code
hooks. It always exits 0 and prints nothing except the context block of a `start` event; a call it cannot use (bad
JSON, no `session_id`, unknown event, nothing to record) is dropped.

`--client` is the name the session is recorded under (the server's `harness` field): lowercase letters, digits, `-`
and `_`, at most 32 characters. Session ids are namespaced by it (`myagent:s1`), so ids of different clients never
meet; `claude-code` uses the plain id, like the Claude Code hooks.

| Field | Meaning |
|---|---|
| `session_id` | required; the client's own session id |
| `cwd` | the project directory (default: the working directory of the call); decides the project key |
| `event` | `start`, `prompt`, `tool`, `assistant` or `end`; when absent, `hook_event_name` is used |
| `prompt` or `text` | the user prompt (`prompt` event) |
| `last_assistant_message`, `prompt_response` or `text` | the final answer of a turn (`assistant` event) |
| `tool_name`, `tool_input`, `tool_response` | a finished tool call (`tool` event; `tool_name` required); `tool_use_id` and `duration_ms` are kept if present |
| `reason` | why the session ended (`end` event) |
| `source`, `model` | `start` event: `startup`, `resume`, `clear` or `compact` (anything else counts as `startup`) and the model name |

`hook_event_name` values understood without an `event` field: `SessionStart`, `UserPromptSubmit` and `BeforeAgent`,
`PostToolUse` and `AfterTool`, `Stop` and `AfterAgent`, `SessionEnd`. This is what lets the Codex and Gemini hook
inputs be piped in unchanged. An `assistant` event with `stop_hook_active: true` is ignored, as for Claude Code.

A `start` event prints `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"..."}}` with the
recalled context, the shape Codex and Gemini both read; `source: "resume"` returns none. The server-side observer
(`mem-hook observe`) runs `claude -p`, so a machine without the `claude` CLI does not start it and leaves the work
packets for a machine that has one (the nightly runner, see the plugin README).

## Codex CLI

```bash
make connect-local AGENT=codex          # or: clients/claude-code-plugin/connect.sh connect --client codex --url ... --token ...
make disconnect AGENT=codex
```

Adds one block to `~/.codex/config.toml` (`$CODEX_HOME/config.toml`), between the markers
`# >>> memoryvault ... >>>` and `# <<< memoryvault <<<`, at the end of the file:

- `[mcp_servers.memory]` with `url = "<server>/mcp"` and the token in `http_headers`. The token is stored in that
  file in clear text, like any other credential Codex keeps there; mind its permissions.
- `[[hooks.SessionStart]]`, `UserPromptSubmit`, `PostToolUse`, `Stop` and `SessionEnd`, each running
  `mem-hook event --client codex`. Timeouts are in seconds; tool and stop events are asynchronous.

Codex only runs a hook after you have approved it: its hooks carry a trust state and a new one stays untrusted until
a `trusted_hash` is recorded for it. `connect.sh` does not write that hash (it is computed by Codex from its own
normalised form of the hook), so until you approve the hooks in Codex only the MCP server is active. The legacy
`notify` setting is not used.

Safety rules of the script: the file is backed up once to `config.toml.memoryvault.bak` before the first change;
nothing outside the block is touched; the block is replaced, not duplicated, on a second connect; if the file is
not valid TOML, or the result would not be (checked when `python3` has `tomllib`), nothing is written. If the file
already defines `[mcp_servers.memory]`, or defines hook events in a form a second `[[hooks.X]]` table cannot extend
(inline `hooks = {...}`, `Stop = [...]` under `[hooks]`), that part is skipped with a warning and the rest is still
registered. `disconnect` removes the block, and when what remains equals the backup it restores the backup byte for
byte. The shared `~/.memory/config` is left alone, since other clients on the machine use it.

## Gemini CLI

```bash
make connect-local AGENT=gemini
make disconnect AGENT=gemini
```

Edits `~/.gemini/settings.json` (`$GEMINI_CLI_HOME/.gemini/settings.json`); needs `python3`:

- `mcpServers.memory` with `httpUrl = "<server>/mcp"` and the token in `headers.Authorization` (stored in that
  file in clear text).
- One hook named `memoryvault` under each of `SessionStart`, `BeforeAgent`, `AfterTool`, `AfterAgent` and
  `SessionEnd`, running `mem-hook event --client gemini`. Timeouts are in milliseconds.

Gemini waits for a hook to finish before it goes on, so with a server that is reachable but slow a hook can delay
the agent by its budget (3 s per event at most; a refused connection costs nothing and the event is spooled and
sent later, a host that does not answer costs the whole budget). Gemini wants nothing but JSON on a hook's stdout; `mem-hook event` prints only the context block of a
`SessionStart`.

Safety rules: the file is backed up once to `settings.json.memoryvault.bak` before the first change; only
`mcpServers.memory` and the hooks named `memoryvault` are touched; a file that is not a plain JSON object (comments,
for instance) is left as it is and the script fails; a `memory` server that is not ours is left alone with a
warning. `disconnect` removes exactly those entries, deletes empty containers it emptied, and restores the backup
byte for byte when the result equals it. The file is rewritten with two-space indentation when it is changed.

## Other CLIs

Register `mem-hook event --client <name>` as the command of the client's events (it must pass the JSON above on
stdin) and add `<server>/mcp` as a streamable-HTTP MCP server with the header `Authorization: Bearer <token>`.
`~/.memory/config` must hold `MEMORY_URL` and `MEMORY_TOKEN` (`install.sh` creates it).

## Sources and what is not confirmed

Everything above comes from the official repositories, read on 2026-10-25 through `raw.githubusercontent.com`
(`developers.openai.com` and `geminicli.com` were not reachable from where this was written). Neither CLI was
installed there, so the scripts are tested against throwaway config files and the documented input shapes, not
against a running Codex CLI or Gemini CLI.

Gemini CLI (`google-gemini/gemini-cli`, branch `main`):
- `docs/hooks/index.md` and `docs/hooks/reference.md`: the `hooks` object in `settings.json`, events, command hooks
  (`type`, `command`, `name`, `timeout` in ms), the stdin fields (`session_id`, `cwd`, `hook_event_name`, `prompt`,
  `prompt_response`, `tool_name`, `tool_input`, `tool_response`, `source`, `reason`, `stop_hook_active`), JSON-only
  stdout, and `hookSpecificOutput.additionalContext` for `SessionStart`.
- `docs/tools/mcp-server.md`: `mcpServers`, `httpUrl` (streamable HTTP) and `headers`.
- `docs/reference/configuration.md`: `~/.gemini/settings.json` and `GEMINI_CLI_HOME`.

Codex CLI (`openai/codex`, branch `main`, the `codex-rs` tree; the docs pages in the repository only point to
developers.openai.com):
- `codex-rs/core/config.schema.json`: `[mcp_servers.<name>]` with `url`, `http_headers` and `bearer_token_env_var`;
  `hooks` (events `SessionStart`, `UserPromptSubmit`, `PostToolUse`, `Stop`, `SessionEnd`, ...), hook handlers
  (`type = "command"`, `command`, `timeout`, `async`), `notify`, and the hook trust state (`trusted_hash`).
- `codex-rs/config/src/hooks_tests.rs`: the TOML spelling `[[hooks.<Event>]]` / `[[hooks.<Event>.hooks]]`.
- `codex-rs/hooks/schema/generated/*.command.{input,output}.schema.json`: the stdin fields and the `SessionStart`
  output (`session_id`, `cwd`, `prompt`, `tool_name`, `tool_input`, `tool_response`, `last_assistant_message`,
  `stop_hook_active`, `additionalContext`).
- `codex-rs/hooks/src/engine/discovery.rs`: timeouts in seconds (`SessionEnd` capped at 3), untrusted hooks not run,
  `async` hooks.
- `codex-rs/features/src/lib.rs`: the `hooks` feature is stable and on by default.
- `codex-rs/utils/home-dir`: `CODEX_HOME`, default `~/.codex`.

Not confirmed from a documentation page: Codex's hook and trust behaviour is read from source and its schema, not
from user documentation, so it may change between releases; how Codex presents the approval of a new hook; whether
Gemini expands environment variables in `headers` (the token is therefore written out, not referenced); and any
claim about Windows (`connect.sh` is a bash script).
