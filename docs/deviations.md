# Deviations from the spec

This file lists every place where the build differs from [implementation-spec.md](implementation-spec.md),
with the reason. The spec text stays as written. Section numbers refer to the spec. Items are grouped by area
and repeated items are merged. The last section lists the gaps that are still open.

## 1. Scope (owner decisions)

- **No agentic OS.** The Python client, the assistant observer mode, the agentic OS in-process packet runner, and
  anything whose only consumer is the agentic OS are not built. Reason: the owner decided the server and clients
  are Rust, and the agentic OS is out of scope. This also removes the `agent` token kind.
- **Hooks are one Rust binary.** The bash scripts of spec section 8 are replaced by the `mem-hook` crate, with the
  same behaviour and one subcommand per hook event. `mem-hook observe --all` is the nightly runner (spec 8, 9).
  Reason: no `bash`, `curl`, `jq` or `flock` on client machines, and a faster hook start.
- **Phase 2 and 4 items out of scope.** The Python client and agentic OS parts of Phase 2 were dropped. Phase 4
  items are listed as open in section 14.

## 2. Dependencies and toolchain

- **sqlx 0.8.6, not 0.9.** The spec names 0.8. sqlx 0.9 changes `SqlSafeStr`, which would make the
  `format!`-built queries in the modules harder to write. Module code uses bound runtime queries only.
- **Other crate picks.** axum 0.8.9, tower-http 0.7.1, rmcp 3.7 (features `server`, `macros`,
  `transport-streamable-http-server`), minijinja 3, jsonschema 0.60 (no default features), reqwest 0.13 (rustls),
  rand 0.10, hmac 0.13, sha2 0.11, metrics-exporter-prometheus 0.18 (no default features).
- **rusqlite 0.32 (bundled), for the eval comparison.** The lock file already has `libsqlite3-sys` 0.30 through
  sqlx, and `links = "sqlite3"` allows one copy only. Pinning 0.32 is the version that fits. `regex` 1 is a direct
  dependency, not a workspace entry.
- **`libc` for detaching the observer.** `mem-hook` uses a direct `libc` dependency for `setsid`, not a workspace
  entry. The observer runs `claude` in its own process group, so a timeout kills the whole group.
- **Rust 1.89.** The workspace `rust-version` is 1.89 (was 1.85). Reason: `mem-hook` uses the standard library's
  `File::try_lock` for cross-platform locking, in place of `flock`.
- **Extra workspace settings.** `rustfmt.toml` (max width 120) and the `serde_urlencoded` test dependency were
  added to the workspace.
- **Build profiles.** `[profile.release]` uses `lto = "fat"`, `codegen-units = 1`, `strip = true`, and keeps
  unwinding, so the server survives a handler panic. A `release-small` profile (opt-level `s`, `panic = "abort"`)
  builds `mem-hook`, at 1.87 MB against 3.07 MB. A per-package opt-level override gave 2.77 MB and was not used,
  because it does not reach dependencies. `[profile.dev]` uses line tables only for workspace crates. The shared
  `target/` directory had grown to 17 GB, and the disk filled up. It is now about 6 GB.

## 3. Crate boundaries and signatures

- **Module routers have no `/v1` prefix.** Each module declares paths without `/v1`. `memvault_core::http::api_router`
  nests them under `/v1`. The server and `test_support::app` both use it.
- **`close_idle_windows` takes `&Settings`.** It has the signature `(&PgPool, &Settings) -> anyhow::Result<u64>`.
  The extra argument carries the idle threshold. It has no prompt registry, so packets it creates store the bare
  prompt name (for example `observer.code`). The lease pins that name to the newest loaded version. Packets the
  API creates are pinned when created.
- **`render_context` returns `Result<String, ApiError>`.** The spec's `String` cannot report a failure.
- **`enqueue_embed` lives in core.** It is implemented in `memvault_core::jobs`, and `memvault_worker::embed`
  re-exports it. Ingest needs it, and ingest cannot depend on the worker without a cycle.
- **`expire_leases` takes `&PgPool`.** It runs one transaction per packet. The old form took `&mut PgConnection`.
- **Stub entry points.** The server needs these, and they were not in the task list:
  `memvault_recall::cache::spawn_listener` (a no-op task),
  `memvault_consolidate::planner::run_planner`,
  `memvault_consolidate::admin_cli::run(AppState, Vec<String>)`,
  `memvault_worker::run_worker(AppState, CancellationToken)`,
  and `memvault_evals::cli::run(AppState, Vec<String>)`.
  The CLI and worker entry points return an `anyhow` "not implemented" error where they are not built, so the
  binary never panics. `mcp::service(AppState) -> Router` answered 501 until the MCP server existed.
- **Transaction helpers.** `memvault_core::user_tx` and `system_tx` made futures that are not `Send`, because they
  used `sqlx::raw_sql(&format!(..))`. Axum handlers could not await them. Core's versions now use bound statements,
  so they are `Send`. The private per-crate copies in ingest, recall, memories and consolidate were deleted.
  This item is resolved.
- **`evals replay` creates its own database.** The library function `replay()` works on any `AppState` that has the
  ingest appliers (`state_over`). The tests use it with a fresh user and private space.
- **Engine helpers are public.** `engine::ef_search_for` and `candidates::VECTOR_PART_FANOUT` are `pub` so a test
  can check them. `CacheEntry` gained a required `rendered_at` field. `render_context` takes it from
  `cache::render_start` (`clock_timestamp()`) before `build_pack`.
- **`memvault-ingest` is a dev-dependency of `memvault-server`.** It is used by one end-to-end test (lease, result,
  revert over HTTP). It is not a runtime edge.

## 4. Schema, migrations and data model

- **Migration runner.** Migrations run through `sqlx::migrate` (`_sqlx_migrations`), not Alembic, from the API process at start-up (see the deployment section). The grant step is
  `0099_grants.sql`, which uses `ALTER DEFAULT PRIVILEGES`.
- **Token hash.** The token hash is `HMAC-SHA256(TOKEN_PEPPER, secret)`, not `sha256(secret)`. `api_tokens.kind`
  is added. The `agent` kind is dropped. Reason: the pepper makes a leaked table useless, and the foundation kept
  this design.
- **Token estimate.** `token_estimate = ceil(chars / 4)`, not tiktoken `cl100k_base` (spec 11.5). The foundation
  kept this, and it needs no extra dependency.
- **Schema additions.** `users.default_space_id` has a deferrable foreign key to `spaces`. Extra plain indexes sit on
  hot lookup paths. `search_tsv` is added to summaries, facts and procedures for the tsvector fallback. Generated
  columns use `memsvc_join`.
- **RLS details.** `retrieval_log` and `access_log` writes need any membership. `context_cache` is per user.
  `memberships` has an own-rows SELECT policy for the app role. `context_invalidations` (migration 0202) has forced RLS
  and a `memsvc_app` SELECT policy. The app role still reaches it only through SECURITY DEFINER functions. Reason: the
  core RLS invariant test failed without it.
- **Packet prompt.** `PacketPrompt` carries `user` (the rendered text) next to `system` and `user_template`, because
  the observer runner sends the rendered prompt (8.5).
- **Schema fields.** `observer.v1` observations accept an optional `scope_hint` (10.4 mentions it).
  `consolidate.procedures.v1` accepts `skip` and `skip_reason`. `consolidate.summary.v1` `period` is
  `{grain?, start, end}` with dates. Decision ids in consolidation are strings, as given in the packet inputs, not
  forced to UUID.
- **Migration 0200** adds `client_id` and `project_key` to `context_cache`, for the re-render jobs (11.6).
- **Migration 0201 and 0202** add `context_invalidations`, the cache-invalidation record, and its RLS (see above).
- **Migration 0400 (consolidation).** Adds `observations.decayed_at` and `facts.decayed_at` for incremental decay. It
  adds `memory_audit.seq bigserial` (reverse replay order within a transaction), `memory_audit.reverts_audit_id`
  (a double revert undoes nothing twice), `procedure_hints.consumed_run_id` (revert releases consumed hints) and an
  index on `work_packets((inputs->>'run_id'))`.
- **Migration 0500 (jobs priority).** Adds `jobs.priority`, which the worker range uses. It was added in its own
  file, since `0099` is already applied everywhere.
- **Migration 0900 (hardening).** A new range, 0900 to 0999, for hardening changes. It was added instead of editing
  `0099`.
- **No migration needed** in the ranges 0100 (kept as it was), 0300 to 0399, 0500 to 0599 (beyond `0500`) and
  0600 to 0699. The existing jobs table, unique `idempotency_key` and embeddings schema were enough.
- **Entities.** Entities have no scope columns, so their vectors are stored with
  `scope_kind = 'global'` and `scope_key = ''`.
- **Response additions.** `POST /v1/sessions` returns `next_seq`. `POST /v1/sessions/{id}/end` accepts
  `occurred_at`. `LeaseRequest` gained an optional `runner_kind`. The lease's `runner_kind` label comes from the
  request (mem-hook sends `claude-p`), because the result `meta` is only known after the lease.

## 5. Auth and tokens

- Covered in section 4 for the token hash and kinds.
- Tokens are bearer tokens checked by `authenticate_header`, which answers 401 in the spec error shape.

## 6. Ingest, packets and summaries

- **Summaries are deferred (6.1, 9.1).** `summarize_prompt` and `summarize_session` are created once the packets they
  summarise have settled (done, failed or cancelled). Their fixed inputs can then include those packets' observation
  titles and prompt summaries. The earlier design did this too. `windows::after_packet_settled` and the sweep trigger
  this. A Stop creates the prompt summary when its extract settles. `/end` covers the session summary. If a session
  has no prompt summaries when it ends, no `summarize_session` packet is created.
- **Summary is not owed on idle close.** An idle-closed window owes no prompt summary on its own. The prompt may
  continue in a later window, and an early summary would block the real one through its idempotency key. The final
  prompt is covered when the session ends. A prompt interrupted mid-session is folded into the next prompt's
  summary, because window `prompt_no` is that of the last `user_prompt`.
- **`attempts` counts at lease (6.6), not at fail.** A runner that crashes still uses an attempt. This matches the earlier
  design.
- **Fail backoff.** `quota_exhausted` waits 900 s. `cli_missing` waits 1800 s. `timeout` and `other` wait 60 s. Each
  retry doubles the wait, with a 6 h cap. `quota_exhausted` and `cli_missing` also clear `preferred_client_id`.
- **Result route and RLS.** The result route runs in a `system_tx`, because appliers need it (CONTRACTS). It first
  checks that the packet's space is writable by the caller, and it answers 404 otherwise. Lease, fail and heartbeat
  run in `user_tx`, so RLS applies to them.
- **Unresolvable prompts.** A packet whose `prompt_ref` cannot be resolved or rendered at lease time is marked failed
  with `last_error` `prompt_error: ...`. This matches the earlier design.
- **Same-value test (10.4 step 3).** A fact candidate counts as the same value as the active fact when the jsonb
  values are equal, or when `value_text` is equal ignoring case and surrounding whitespace. The spec also asks for a
  `value_text` embedding cosine above 0.95. That is not done, because `PacketApplier::apply` gets no embedder. A
  near-duplicate becomes a conflict candidate, and consolidation resolves it.
- **Fact confirmation.** Restating an identical active fact confirms it (`confirmations + 1`, confidence kept at 0.95
  or higher, audit `fact.confirm`) and returns the same id. A different value supersedes the active fact. The new fact
  is inserted as a candidate, the old one is superseded, and then the new one is activated. This keeps the one-active-
  fact index satisfied. Audit rows are `fact.supersede` and `fact.create`. A procedure with the same name and scope
  is revised. The old one is retired (audit `proc.retire`), and the new one has `version + 1` and `supersedes_id`
  (audit `proc.revise`).
- **Fact candidates and clusters.** Candidates link to active or candidate rows on the same `(scope, subject,
  predicate)` even without `conflict = true`. Cosine above 0.85 on the stored `value` embedding also links candidate
  to candidate. Candidates with no stored embedding are embedded on the fly through TEI. Active facts the worker has
  not embedded yet are not found. Clusters are capped at 25 rows, candidates first.
- **Subagent tagging in code.** `extract::tag_subagent_observations` adds `subagent:<agent_type>` to an
  observation's entities when its `source_event_seqs` point at packet input events that carry `agent_type`. It does
  not rely on the model following the prompt rule.
- **Writes to unknown ids.** Ids may be full UUIDs or 6-hex short ids, optionally prefixed `fact#`. They must belong
  to the packet's `inputs.fact_ids`. Rejected decisions are counted under `rejected` in the applied counts and logged
  with `tracing::warn`. They write no audit row.
- **Procedure revision.** A procedure packet must carry `inputs.existing_procedure` to be revised. Output with
  `supersedes_id` on a packet that has no existing procedure is rejected.
- **Consolidation templates.** The templates read the packet inputs the planner builds, not the shapes in the spec:
  - facts: `run_id`, `fact_ids`, `facts`, `profile_md`
  - summary: `run_id`, `grain`, `scope{kind,key}`, `period{grain,start,end}`, `summaries`
  - procedures: `run_id`, `scope`, `task_class`, `hints`, `observations`, `existing_procedure`

  If the planner renames an input, strict undefined fails loudly.
- **`observer.code`, `summary.prompt` and `summary.session`** also read the `mode` input from the 9.2 packets.
- **Prompt files.** Each file carries an extra `description` field, which `PromptSpec` allows. Event objects are
  rendered with `tojson`, one JSON line per event, so the templates do not depend on which optional event fields are
  present. Prompt versions are strings like `v1`, not `1`, because `PromptRegistry` requires `v<digits>`.
- **Profile slot.** Profiles are `- <subject>: <predicate> = <value_text> [fact#<short> · <conf> · <devices> ·
  <first_seen>]`, pinned first, then by importance times confidence, up to 400 tokens. `facts_version` is a hash of
  the active facts' rendering inputs.
- **Profile re-render.** Appliers re-render the profile and notify the context-cache invalidation channel for touched
  scopes in the same transaction.
- **Summary planning.** Day packets use `grain = 'session'` summaries only, over a 14-day lookback, never for today.
  The week packet is built on Sundays (UTC). It covers the 7 days before the run, Sunday to Saturday. It uses day
  summaries, else session summaries, and needs at least 2. The applier takes scope, grain and period from the packet
  inputs, not from the model's `period`. It stores `period_end` as the exclusive midnight after the last day.
- **Metric label.** The `runner_kind` label of `packets_leased_total` is what the lease request
  reports (`unknown` otherwise).

## 7. mem-hook client

- **Spool holds logical requests.** Each spool line is a logical request (`{op: event|end, claude_session_id,
  project_key, cwd, seq, kind, occurred_at, payload, reason, metadata}`), not a raw HTTP request. The sequence number
  is allocated locally in the session cache. Events therefore get a stable idempotency key `<claude_session_id>:<seq>`,
  and the server session can be created lazily at replay time.
- **Session counter after a lost cache.** `mem-hook` rebuilds `next_seq` from `POST /v1/sessions`. If the session cache
  is lost while the server is unreachable, a resumed session's offline events restart at the cached value. They can
  collide with stored events until the server answers.
- **Failure classes (8.3).** Network errors, timeouts, 5xx, 408, 425, 429, 401, 403 and 404 are spooled, and replay
  stops at the first one. A 404 also clears the cached server session id, so the session is recreated. Other 4xx are
  dropped, with an `eprintln`. Retrying a request the server judged invalid can never succeed. An HTTP 500 is spooled.
  A replay skips it, counts the failure on the line (`replays`), and drops it after 5 attempts, so a poison request
  cannot block the spool.
- **Observer packet limits.** `--session` without `--final` leases up to 3 packets. `--final` leases up to 10, because
  the last run of a session also clears the windows of the session end (8.5). `--all` leases up to 50. `--max`
  overrides the count. Defaults for `--kinds`: `extract,summarize_prompt,summarize_session` for `--session`, and all six
  kinds for `--all`.
- **Observer lock.** When a run finds `observe.lock` busy, it appends its arguments to `~/.memory/observe.again`, then
  tries the lock once more. The holder replays those requests before it releases the lock, up to 10 passes. Before,
  `flock -n || exit 0` lost the wake-up when Stop's observer started while the window-8 observer was finishing its last
  empty lease. The packets then waited for the next run. A test covers this case.
- **Observer argv.** The argv matches spec 8.5 exactly: `-p --model M --output-format json --disallowedTools '*'
  --no-session-persistence --max-turns 1 --settings '{"disableAllHooks": true}' --system-prompt SYS USER`. A user
  prompt over 100,000 bytes goes to `claude` on stdin instead, because Linux limits one argv string to 128 KiB.
- **Schema retry.** A `422 schema_invalid` retries once. The server's `retry_with` text is appended to the original
  prompt (`'\n\n' + retry_with`), and the run uses `--model opus`. A second 422 posts `/fail` with error `other`.
- **Fail reasons.** The reasons use the server's spellings: `quota_exhausted`, `cli_missing`, `timeout`, `other`.
  `quota_exhausted` and `cli_missing` end the observer loop.
- **Result shape.** The model output is posted as a parsed JSON object when the text parses to one. Otherwise it is
  posted as the raw string, so the server can strip a code fence. Core accepts both.
- **Client-side clamp.** A string over 64k characters keeps the first 32k, then `…[clamped N chars]…`, then the last 8k,
  before it is sent. One event therefore cannot exceed the 4 MB batch limit. The server still applies its own
  per-field clamp.
- **Proxy.** `HTTP(S)_PROXY` is ignored (ureq proxy disabled), because the server is on the tailnet.
- **Context cache path.** The cache is `cache/<project_key>.md`. Each key segment is sanitised, so
  `github.com/alex/homelab` becomes `cache/github.com/alex/homelab.md`, and `..` cannot escape. Session caches older
  than 30 days are pruned at session start. A session cache is not deleted at session end, so a resumed session keeps
  its sequence numbers.
- **SessionStart sources.** A source other than `startup`, `resume`, `clear` or `compact` is treated as `startup`,
  because the server's `ContextSource` accepts only those four. An empty context block prints nothing.
- **Observer log.** The observer writes progress to `~/.memory/observe.log`, truncated at 256 KiB, because it runs
  detached. It starts with `setsid`, and the `claude` child runs in its own process group.
- **Config.** Besides `MEMORY_URL` and `MEMORY_TOKEN`, the config accepts `DEVICE_ID` (default: hostname) and
  `CLAUDE_BIN` (default `claude` from `PATH`, since cron has a minimal `PATH`). Values in the file win over the
  environment, as sourcing would. The environment is the fallback. `MEMORY_OBSERVE_TIMEOUT_S` overrides the 180 s
  timeout used by tests. `MEMORY_DEBUG=1` prints why a hook spooled.
- **Install.** `install.sh` also accepts `MEM_HOOK_BIN` to install a prebuilt binary instead of running cargo. The cargo
  path is the default.
- **Windows.** The hook code is portable. It uses `cfg(unix)` and `cfg(windows)` for file modes, locking, detaching
  and process-group kills. It type-checks for `x86_64-pc-windows-gnu`, but it has not been run on Windows.
- **Linux build.** The Linux build is a static (`crt-static`) binary through `install.sh`. A musl build needs
  `musl-tools`.
- **Undefined modes.** The `--final` flag and the "loop up to 3 packets" were undefined for the other modes. The
  decisions are above (session 3, final 10, all 50).

## 8. Recall and context

- **Context budgets are constants.** Profile 300, facts 150 (pinned facts only), procedures 200, observations 700 (at
  most 30 rows), summaries 200, work_state 150. `expand` is 0, and rerank is off.
- **Profile slot.** Profile rows are joined in scope order, then space order, until the budget is used. The first
  scope with a row is not the only one used. With no `profiles` row, the top active facts are rendered instead, and
  those facts are left out of the facts slot.
- **Budget costs.** Facts, procedures, summaries and index rows cost the token estimate of their rendered line.
  Expanded observations cost their stored `token_estimate`.
- **Index.** The index (progressive disclosure) rows are picked by score, then shown newest first.
- **Type boosts (11.4 left these open).** Decision 1.2, `security_alert` 1.2, `bugfix` 1.1. Summaries score with
  importance 0.5.
- **SessionStart extras.** The block adds two things the spec layout lacks. There is a "Recent sessions" section (up
  to 5 session summaries, from architecture 6.2). There is also a live banner when work packets have been pending for
  more than 24 h. There is no TEI health banner.
- **Since-you-were-here (11.7).** `render_context` has no session id in its signature. So `$4` (the current session) is
  this client's newest session on the project, but only if it started within the last 10 minutes. The line shows the
  session summary, or the latest prompt summary while the session is still running. agentic-os clients are labelled
  `assistant`.
- **Cache key (11.6).** The key is sha256 of `(user_id, sorted space_ids, sorted scopes, device_id, source, mode)`. Spec
  11.6 has no `user_id`. It is added because `context_cache` rows are per user (RLS), and `cache_key` is the primary
  key. `mode` is the injection mode (below).
- **Injection modes.** `GET /v1/context` takes `mode=full|lean`; without it the user's default from `user_settings`
  (`GET`/`PUT /v1/me/settings`), else `full`. `mem-hook` sends `MEM_INJECTION` from `~/.memory/config` as `mode`. The
  lean block is the Profile and Open work sections (budgets 120/60/80 for profile, pinned facts and work state) plus a
  fixed rubric of about 140 tokens; it has no since-you-were-here section. A re-render job carries the mode of the
  block it replaces.
- **Context calls are logged on cache hits too.** `retrieval_log` gained `tokens`, `hits` and `injection`. A hit logs
  a row with no candidates, so `GET /v1/stats/recall` (own calls; `all=true` for admin tokens) counts every call. Rows
  from before migration 0203 have no tokens and are left out.
- **ETag.** The ETag is a hash of the full response body, live sections included. It replaces
  `(space, scopes, cache_version)`. `If-None-Match` gets a 304. Cached rows also expire after 1 hour, as a backstop
  against a lost NOTIFY.
- **Listener restart.** The listener drops every cached block when it starts and after each reconnect. Notifications
  sent while it was not listening are lost.
- **Re-render path (11.6).** The API listener calls `memsvc_recall_invalidate`, a SECURITY DEFINER function in
  migration 0200. It deletes matching rows, and queues one `jobs(type='render_context')` per deleted block, with
  `run_after = now + CONTEXT_RERENDER_DEBOUNCE_S`. Jobs are coalesced while one is still queued for that key. The job
  payload names the user, client, project, device and source. `memsvc_recall_invalidate` was rewritten from SQL to
  plpgsql. It records the invalidation and prunes rows older than 2 h.
- **Freshness check.** Cache freshness is checked through the SECURITY DEFINER function `memsvc_context_stale`, not a
  direct table read, because `memsvc_app` has no grants on `context_invalidations`.
- **Debounce in the worker.** The 2 s debounce is not implemented in the worker. `recall`'s `cache::invalidate` already
  sets `run_after` and dedupes queued jobs per `cache_key`. The worker claims due `render_context` jobs and calls
  `memvault_recall::cache::rerender`.
- **Feedback.** Feedback bumps `access_count` and `last_used_at` only for `used_ids` the retrieval actually selected.
  Only the client that made the retrieval may post feedback.
- **Scope validation.** Scope validation errors return 422 `invalid_request`. Both legs filter scope on the owner row,
  not on the scope columns of the embeddings row.
- **Logging.** `context` renders on a cache miss are written to `retrieval_log` with `mode = 'context'`.
- **Cache hits log nothing.** Context-cache hits write no `access_log` row (5.3, 4.6), because the cached block carries
  no row ids to log. Cache misses and every `/v1/recall` log. Index-only observation lines are not logged. Fact values
  (including those rendered in the profile, resolved from `fact#` tags), procedure bodies, summaries and expanded
  observations are.
- **Search window.** `offset + limit` for `GET /v1/search` is capped at 1000.
- **Output caps.** `memory_get` output is capped at about 24k characters, by shortening the longest bodies first.
  Search, timeline and explain are capped at about 8k characters. Search output ends with a hint about
  `memory_timeline` and `memory_get`.

## 9. MCP and memory writes

- **MCP server design.** The MCP server is a hand-written rmcp `ServerHandler`, with `list_tools` and `call_tool` and
  JSON-literal input schemas. It does not use `#[tool]` macros, because the core `mcp.rs` argument types do not derive
  `JsonSchema`. It is mounted through `StreamableHttpService` with `NeverSessionManager`,
  `legacy_session_mode = false`, `json_response = true`, and `disable_allowed_hosts`. Host checking is off so tailnet
  hostnames work. The bearer token guards the endpoint.
- **Bearer auth on /mcp.** An axum middleware checks the header with `authenticate_header`. It answers 401 in the spec
  error shape for every MCP method, including `initialize`. The identity reaches the tools through request
  extensions. Each tool call runs `require_scope` (`memories:read` or `memories:write`) and the rate limit.
- **Memory feedback.** `POST /v1/memories/{id}/feedback` and the MCP `memory_feedback(id, verdict, note?)` (scope
  `memories:write`, which the spec already lists for "feedback") record `helpful`, `wrong` or `outdated` on a fact,
  observation or procedure in the append-only `memory_feedback` table (user, client, time, note; summaries take
  none). One verdict per user and memory counts once, enforced by a unique index; a repeat answers `applied: false`.
  Effects: a fact's confidence moves +0.05 (helpful, which also sets `last_confirmed_at`), -0.2 (wrong) or -0.1
  (outdated), kept within 0.05 and 0.98, and pinned facts keep theirs; an observation's importance moves by the same
  steps; a procedure counts a success or a failure. Each applied verdict is audited (`fact.feedback`, `obs.feedback`,
  `proc.feedback`), drops the scope's profile row and signals the context cache. `POST /v1/recall/{id}/feedback`
  (per retrieval) is unchanged.
- **Dispute weights.** Net negative feedback is `2 x wrong + outdated - helpful`; at 2 an active, unpinned fact gets
  `disputed_at` (cleared again when helpful verdicts bring it under). The numbers are a choice, not in the spec: a
  single `wrong` queues a fact, a single `outdated` does not. Nothing is retracted by the request itself.
- **Cautions.** A fact is shown as `disputed` when it is queued or its confidence is under 0.4, else as `unconfirmed N
  months` when `last_confirmed_at` is 90 or more days old; a procedure that failed more than it worked is `disputed`.
  The tag is the last part of the citation (`[fact#a91c2e · 0.83 · laptop · 2026-10-02 · unconfirmed 4 months]`),
  appears after the title in index rows (`IndexRow.caution`, `FactFull.caution`) and in rendered profiles. Pinned facts
  carry none. Scoring is unchanged: it already uses confidence, and facts still have no recency term.
- **Tool errors.** Tool failures (bad arguments, 403, 404, scope errors) come back as `CallToolResult` with
  `isError`, and text `error (<code>): <message>`. They are not JSON-RPC errors, so the model can correct the call.
- **Project inference.** The `X-Memory-Project` request header is used when present. Otherwise the project comes from
  `clients.current_project_key`. Claude Code cannot vary MCP headers per project, so this is the device's latest
  project (spec 18). `memory_search` with `scope = project` and no known project falls back to all. `memory_remember`
  with `scope = project` and no known project is a tool error. With no scope, `memory_remember` and `work_state_write`
  use the current project, else global.
- **`memory_remember` kinds.** `kind = fact` needs a predicate, or it returns a tool error. Subject defaults to
  `user`, and `content` becomes `value_text`. `kind = note` becomes an observation. Its title is the first line (at most
  120 characters), its narrative is the content, its type is `discovery`, and its importance is 0.7. `kind = procedure`
  maps `subject` to the name (default: first line) and `predicate` to the `task_class` (default `general`).
- **`memory_search since`** accepts RFC 3339, `YYYY-MM-DD`, or a span (`24h`, `7d`, `2w`).
- **Work state.** `POST` returns the folded list (200). `GET` without a scope uses the caller's default scopes (global,
  current project, device). It folds with `memvault_recall::engine::fold_work_state`.
- **`memory_get` references.** It accepts full ids, `obs#a1b2c3` and 6-hex short ids through
  `memvault_recall::engine::resolve_refs`. If no id resolves to a visible row, it is a tool error.
- **Forget.** Summaries cannot be forgotten through the tool (422). Admin hard delete works on them. Forgetting twice is
  a no-op with no extra audit row. Reader roles get 403 `space_forbidden`. Rows the caller cannot see get 404.
- **Hard delete (admin, REST only).** It removes the row and its embeddings. It nulls references to the row
  (`supersedes_id`, `superseded_by`, `procedure_hints.observation_id`). It removes the row's id from
  `evidence_observation_ids` and `source_observation_ids`. It scrubs the before and after content from that row's
  earlier audit rows. Then it writes a `delete.hard` audit row holding only the scope. It also blanks `work_packets`
  whose `inputs`, `result` or `applied` contain the row's full id. Content that was never tied to the id, such as a
  model's paraphrase in a result, cannot be found.
- **Pinned facts.** Pinned facts are never superseded, retracted, promoted or re-scored by decisions. The spec (12.2)
  says only "pinned facts untouched" for confidence. A merge into a pinned target still adds evidence and
  confirmations, but it keeps its confidence.
- **Promotion.** `promote_scope` supports only `to: global`. Promotion to project, device or agent is rejected,
  because a decision carries no scope key to derive it from.
- **Explain.** `explain` is implemented in the API and the tool. Its output is capped as in section 8.

## 10. Consolidation

- **Revert scope.** Revert cancels every packet of the run, including done ones. It restores any audited table:
  facts, observations, summaries, procedures, sessions, work_state and procedure_hints. Rows with a null `before` are
  deleted along with their embeddings. Embedding scope copies are re-synced.
- **Decay (spec 5.4 half-lives).** Decay is incremental. Each run decays only the time since the later of the last decay
  and, for facts, the last access or creation. Half-lives are 14 days for `change` and `discovery`, 90 days for
  `decision` and `bugfix`, 30 days for other observation types (spec 11.4), and 365 days for facts. Floors are 0.0 for
  observations and 0.1 for facts. Pinned facts, and rows decayed in the last 6 h, are skipped. Observations have no
  access tracking, so their decay runs from creation. The "+0.1 on access" reset is not implemented.
- **Disputed facts.** Step 2 of the planner (before the clusters) retracts the active, unpinned facts that feedback
  queued (`facts.disputed_at`), in code, with the audit action `fact.retract` under the run id, so `revert_run`
  restores them; the reason names the verdict counts and the feedback ids, because the evidence rule of packet
  retractions (a cited observation) does not fit user feedback. The queue mark is cleared before the audited change,
  so a revert does not queue the fact again. A fact whose key has a candidate waiting stays for the clusters, which
  supersede it. Tally action: `feedback_retract`. Profile lines are tagged like citations, and `facts_version`
  covers the dispute flag and the months since confirmation, so a stale or disputed fact re-renders its profile.
- **Cluster skip.** A cluster is skipped while any of its facts is in a pending or leased `consolidate_facts` packet.
  The idempotency key is `consolidate_facts:<space>:<hash of the sorted ids>`. Revert releases the keys of the packets
  it cancels, so the planner can plan the cluster again.
- **Re-planning cap.** Re-planning a cluster is capped at 3 finished packets (done or failed) per member set
  (`MAX_GENERATIONS`). After that the cluster is left alone, so its candidates are neither re-planned nor
  auto-activated.
- **Consolidation tests.** Consolidate tests load the repository's `prompts/` directory, which is what the deploy ships.
  The crate's own prompt copies were deleted.
- **Not built.** The on-demand consolidation trigger on a candidate backlog above 50 or a conflict older than 24 h
  (architecture 5.4; session-end and observation-threshold triggers are built, below). Marking a procedure's
  `failure_count` from later summaries. Spec 12 requires neither.
- **Admin CLI.** `memvault admin project remap` takes `--from` and `--to`, as the spec says. The positional form is gone.
- **Project remap.** It is audited as `project.remap` with before and after rows. It refuses with 409 when active
  facts would collide on the target key. It is undone through the revert endpoint.
- **Admin REST.** List endpoints return plain JSON arrays. Added: `DELETE /admin/tokens/{id}` (id or 8-char prefix,
  idempotent), `POST /admin/packets/{id}/retry`, `POST /admin/project-remap` and `GET /admin/memberships`.
  `consolidate`, `retry` and `reindex` accept an empty body. `reindex` embed jobs carry an extra `model` key in the
  payload.
- **Reindex.** Reindex skips rows that already have a queued or running embed job, whatever its model.
- **Deleted dead code.** `memvault_worker::other_pass`, the empty `memvault_worker::router()` and the empty
  `memvault_evals::router()` were removed.
- **Replay observers.** `evals replay --observer` is optional. Without it, packet outputs are the recorded ones, or a
  fake observer's when the recording has none. `--prompt` implies the `claude` observer, which runs only `extract`
  packets through `claude -p`. Summaries keep their recorded or fake outputs.
- **Triggers instead of the backlog trigger.** Architecture 5.4 wants an on-demand run when candidates pile up. What is
  built reacts to activity: a session end (once its extract packets settled) and `CONSOLIDATE_OBSERVATION_THRESHOLD`
  new observations in a scope (counted from `scope_planning.last_planned_at`, extract results only) each queue a
  scoped `plan` job. `CONSOLIDATE_OBSERVATION_THRESHOLD=0` turns the second off.
- **One live job per scope.** The unique index `jobs_plan_live` allows one `plan` job per `(space, scope)` while it is
  queued or running, so a trigger that arrives during a run is dropped; the nightly pass or a later trigger picks the
  activity up.
- **Debounce at run time.** The minimum interval is checked when the worker claims the job, not when it is queued
  (ingest has no settings at the point where it queues). A job inside the interval goes back to `queued` with
  `run_after` at the end of it and is not counted as an attempt (`queue::defer`).
- **Nightly skip.** `memvault planner` leaves out scopes whose `scope_planning.last_planned_at` is inside the
  interval. `POST /v1/admin/consolidate`, `memvault admin consolidate now` and `planner::run_planner` are `manual` and
  never skip. Every pass, scoped or not, stamps the scopes it planned.
- **Scope in a pass.** A scoped pass filters its own stages (learned-on refresh, disputed facts, fact candidates,
  decay, archival, summaries, procedures, profiles). A candidate still clusters with a similar fact of another scope
  of the same space, as before.
- **Audit and metrics.** `planner_runs` (one row per run and space, 180-day retention like the other logs) records the
  trigger and scope; `memory_audit` rows keep joining on `run_id`. `memsvc_consolidation_actions_total` gained a
  `trigger` label; new are `memsvc_planner_runs_total{trigger}`, `memsvc_plan_enqueued_total{trigger,outcome}` and
  `memsvc_plan_jobs_total{trigger,outcome}` (planned or deferred).

## 11. Worker and jobs

- **Job statuses.** Statuses used are `queued`, `running`, `done` and `failed`. Failed jobs retry with backoff (5 s,
  doubling, capped at 300 s), for 5 attempts, with a 300 s lock. A payload that cannot be parsed fails at once.
- **Retention.** The sweep queues `jobs(type = 'retention', idempotency_key = 'retention:<UTC date>')` every
  `WORKER_SWEEP_INTERVAL_S` (30 s by default). One replica runs it soon after UTC midnight. Retention also prunes done
  and failed jobs older than 7 days, and `rate_limits` windows older than 1 day. Those are beyond the spec 4.8 list.
- **Event and packet pruning.** Events are trimmed by `received_at`. `excerpt` is the first 500 characters of
  `payload::text`. Work packets are deleted by `coalesce(done_at, created_at)` after `procedure_hints.packet_id` is
  nulled.
- **Embed model.** Embed jobs may carry an optional `payload.model`, which `reindex` sets. A job for a model other than
  `state.tei.model()` fails permanently, with a clear error.
- **Lease expiry.** Packet leases and idle windows are handled inside `memvault_ingest::windows::close_idle_windows`,
  which calls `queue::expire_leases`. The worker's own sweep also expires job-table locks, every 30 s.
- **Batch timeout.** The batch TEI timeout is read in the worker crate from `TEI_EMBED_BATCH_TIMEOUT_S` (default 60 s),
  not from core `Settings`.
- **Metrics.** Beyond spec 15, the worker adds `memsvc_jobs_total{type,outcome}`, `memsvc_embed_parts_total`,
  `memsvc_embed_batch_seconds` and `memsvc_jobs_queued`. The spec metrics set by the worker are
  `memsvc_packets_pending{kind}`, `memsvc_oldest_pending_packet_seconds`, `memsvc_facts_active` and
  `memsvc_facts_conflicts_pending`.
- **Health endpoint.** The worker serves `/metrics` and `/health` on `WORKER_METRICS_ADDR` (default `settings.host`
  and `settings.port`). A failed bind logs an error, and the worker keeps running. This is not an HTTP API in the
  spec's sense.
- **Worker port.** The worker's Service and container port were meant to be removed. See section 14.
- **Plan lane.** Scoped `plan` jobs (triggered consolidation, section 10) have a worker lane of their own, so a long
  reindex cannot hold them up. A job that is not due yet is requeued with `queue::defer`, and the worker now depends on
  `memvault-consolidate`.

## 12. Evals

- **Questions name content.** Questions list expected content, not ids, because ids differ on every replay. They use
  observation title substrings, fact `(subject, predicate)` pairs, or summary text substrings. The header of
  `evals/questions.yaml` documents this.
- **Scratch database.** `replay` creates `memvault_eval_<id>` next to `DATABASE_URL`, runs the migrator, and drops the
  database with `WITH (FORCE)` at the end. `--keep-scratch` keeps it. This needs `CREATEDB`.
- **Sessions run through the real routers.** Sessions go through the real ingest and recall routers in-process, using
  tower `oneshot` over `api_router(ingest + recall)`. Then `memvault_worker::embed_pass` drains the embed jobs. Packet
  outputs are the recorded ones, or a deterministic fake observer when the recording has none, or when `--observer
  fake` is given.
- **Recording format.** Recorded sessions are JSONL with `session`, `event` and `output` lines. `output` lines hold the
  done packets' results, keyed by `window_no` (extract) or `prompt_no` (summarize_prompt). Replay uses idempotency keys
  `<server session id>:<seq>`, since the key is globally unique. Replaying twice into one database would otherwise drop
  events.
- **Metric definitions.** Items from all recall slots are ranked by score.
  - recall@5 is the mean over questions of (expected items found in the top 5 divided by expected items).
  - First-hit rank is the mean 1-based rank of the first matching item, over questions with a hit.
  - Tokens per hit is the mean `token_estimate` sum up to and including that first hit.
  - Fact precision is the share of active and candidate facts equal to a gold fact. Facts not in the gold file count as
    wrong. It is `None` without gold.
  - Contradictions is the number of `(subject, predicate, scope)` groups with more than one distinct `value_text`
    among active and candidate facts.
- **Baseline check.** The check is relative. It fails when `(baseline - recall@5) / baseline > 0.05`. A missing
  `baseline.json` skips the check. `--update-baseline` writes the measured value instead of checking.
- **Comparison with claude-mem.** `compare` matches claude-mem's `sdk_sessions.content_session_id` to this service's
  `sessions.external_session_id`. It prints the observation count, observation tokens (claude-mem: text length / 4,
  ours: stored `token_estimate`), the session summary count, and the facts that cite the session's observations (this
  service only). It does not compute recall for claude-mem.

## 13. Deployment

- **Resource sizing.** API requests are 250m CPU and 128Mi memory, with limits of 1 CPU and 512Mi. The spec's 500m and
  512 MB were the figures for the Python build.
- **Roles.** The database owner is `memsvc_system`. Migrations and grants run as the owner. `memsvc_system` has
  `BYPASSRLS`, which `system_tx` requires. Extensions `vector` and `pg_search` are created in `postInitApplicationSQL`
  as superuser, so migration 0001 is a no-op. This setup is not verified against a live CloudNativePG operator.
- **Role passwords.** CloudNativePG does not generate passwords for managed roles, apart from the owner app secret. The
  `memsvc_app` and `memsvc_system` basic-auth Secrets are templated in `secret-template.yaml` and created out of band.
- **`INITIAL_ADMIN_TOKEN_HASH`** is left out of `memory-secrets`. `core/config.rs` reads no such variable. The template
  carries `TOKEN_PEPPER` only.
- **No migrate Job.** The spec runs migrations in a Job before the rollout. `memvault serve` applies them as it starts,
  as `memsvc_system` over `DATABASE_SYSTEM_URL` (the API already holds that URL for the broker), so the cluster has no
  Job, no Argo CD PreSync hook and no ordering to get right. Replicas that start together take turns on sqlx's advisory
  lock. The worker and the consolidation planner wait up to five minutes for the schema to be current
  (`db::wait_for_schema`). The migrator ignores applied migrations it does not know, so rolling the image back does not
  stop the API from starting. The API pod gets a `startupProbe` (five minutes) so a long migration is not killed by the
  liveness probe. `memvault migrate` stays for running the step by hand. The planner pod keeps the `memvault.io/db-client`
  label for its Postgres egress; the spec named only the API and the worker.
- **Network policy additions.** Beyond spec 14:
  - kube-dns egress, required for any name resolution;
  - Postgres egress to MinIO on 9000, and to the kube API on 443 and 6443, for CNPG backups and the instance manager;
  - a `tei-model-download` egress rule to the internet on 443 for the first model download. Delete it once the PVCs are
    seeded.
- **TEI port.** Both TEI Deployments listen on 8080 (container port and `--port`). The Services keep port 80, so
  `TEI_EMBED_URL` and `TEI_RERANK_URL` are unchanged. A non-root pod with `drop ALL` cannot bind port 80. The
  `tei-clients-egress` and `tei-ingress` network policies moved to 8080.
- **TEI security context.** The pod runs with `runAsNonRoot`, UID, GID and `fsGroup` 1000, and the `RuntimeDefault`
  seccomp profile. The image `USER` could not be verified, because the config blob host was blocked from the build
  environment. The explicit UID does not depend on the image `USER`, but verify before the first apply.
- **TEI image tag.** `cpu-1.9.4` (x86_64 only). arm64 nodes and Apple Silicon need `cpu-arm64-1.9.4`; the compose
  Makefile picks the tag from `uname -m`. Both tags were checked against the ghcr.io registry.
- **Postgres image.** `postgres.Dockerfile` is the fallback. The first choice is the ParadeDB CNPG image. The package
  names assume the PGDG apt repository is in the CNPG base image, and `pg_search` comes from a `.deb` URL build arg.
  Neither was built, because no Docker daemon was available.
- **Server image.** `server.Dockerfile` uses `debian:bookworm-slim` with `tini`, not distroless, so `kubectl exec` has a
  shell. Distroless is a one-line change.
- **Image names.** Image names are placeholders (`ghcr.io/REPLACE-OWNER/...`). Set them in `overlays/home`.
- **Ingress.** The ingress keeps the spec's nginx `proxy-body-size` annotation, which the Tailscale proxy ignores. The
  server's 8 MB body limit is what applies.
- **Kustomize and symlinks.** Kustomize refuses paths outside the kustomization root. `prompts/` and `schemas/` are
  reached through relative symlinks inside `deploy/k8s/base`, and the generator lists files explicitly. Adding a prompt
  or schema means adding a line to `kustomization.yaml`. Symlink handling under kustomize is not confirmed.
- **Prompt rule for subagents.** The server-side step was replaced by a prompt rule, because only `prompts/*.yaml` was
  owned at the time. Subagent tagging is now also done in code (section 6).
- **Worker metrics listener.** The worker serves `/metrics` on `WORKER_METRICS_ADDR` (port 8000), so its Deployment keeps a
  container port, a Service, a ServiceMonitor and an ingress policy for Prometheus. Spec 14 did not list a worker listener.
- **API gets the system role.** The API Deployment has `DATABASE_SYSTEM_URL` (`memsvc_system`) as well as `DATABASE_URL`
  (`memsvc_app`). Spec 14 lists only the app role for the API. The API process is the broker (spec 4.7). Result
  application runs in `system_tx`, and every `/v1/admin` route does too. Without this, both fail in production.
  User-facing reads and writes still run in `user_tx` under RLS. Documented in `CONTRACTS.md` and in `api.yaml`.
- **Empty pepper.** Only `serve` and `admin` refuse to start with an empty `TOKEN_PEPPER`, through
  `Settings::require_token_pepper`. `worker` and `planner` never hash tokens, and their manifests do not receive the secret.
- **Secret template.** Empty-string placeholders were removed. Secrets carry no values, and the required keys are listed
  in comments. Workloads reference keys without `optional`, so a missing key blocks pod start. The server refuses an
  empty pepper for `serve` and `admin`.
- **Worker Service still in the manifest.** The deviation list says the `memory-worker` Service, its port and its
  ServiceMonitor were removed. `deploy/k8s/base/worker.yaml` still defines a `memory-worker` Service on port 8000, and
  `containerPort: 8000`. The `ServiceMonitor` comment still names it. Either remove these or record the change again.
- **Stale secret comment.** `secret-template.yaml` still says the server does not refuse an empty pepper. It does for
  `serve` and `admin`.
- **Python wording in a Rust doc comment.** `crates/core/tests/foundation/main.rs` says the tests were ported from the
  Python foundation suite. That is a history note and can be reworded.
- **Unverified deployment pieces.** The CNPG role setup, the TEI image tag and user, the kustomize symlinks, and the
  Postgres image build have not been run against a live cluster.
- **Phase 4 items not built.** OAuth for more users, KEDA scaling and
  export and import per space. The base manifests include no ScaledObject.
- **Embedding cosine check.** The same-value check does not use embeddings (section 6). Near-duplicates become conflict
  candidates until consolidation resolves them.
- **Eval replay and checks not run here.** The acceptance checks in spec section 17 need a live environment. The
  Windows hook has not been run.
- **MCP server bundled in the plugin.** The spec registers the MCP server separately with `claude mcp add`, with
  the token as an environment reference. The plugin now ships `.mcp.json` and takes the URL and token from its own
  `userConfig` (token in the keychain, or `~/.claude/.credentials.json` where there is none), so no shell profile
  is involved and IDE and desktop apps connect too. Tool names become `mcp__plugin_memory_memory__*`. The plugin
  also ships six skills (`recall`, `remember`, `brief`, `project-history`, `handoff`, `how-it-works`), which the
  spec does not list. `make connect-local`, `make connect-remote` and `make disconnect` wrap the setup.
- **File-context hook is informational.** Phase 4's file-context hook is built as a `PreToolUse` hook on `Read`
  (`mem-hook file-context`). It adds earlier observations for the file as context, once per file per session, and
  never blocks or replaces the read. claude-mem's file-read gate can replace a read of a large unchanged file with
  its timeline; this one does not, because a wrong block costs the agent the file.
- **Web viewer.** Phase 4's admin viewer is a static page at `/viewer` that signs in with a bearer token. It lists,
  searches and forgets memories, shows provenance and work state, and for admin tokens lists and revokes tokens and
  shows failed packets and jobs. It adds `GET /v1/me` (who the token is, used to hide admin pages) and a `reason` query
  parameter on `DELETE /v1/memories/{id}` (the audit log already had a reason column). Not built: sessions and raw
  events, editing a memory, undoing a forget, user and space administration, and server-side sign-in (cookies,
  OAuth), so a token is typed or pasted per browser.
- **Point-in-time queries.** `as_of` (RFC 3339) on `GET /v1/search`, `POST /v1/recall` and the MCP `memory_search`
  (which then also searches facts and procedures) replaces the "live" filter with what held at that instant. A fact
  counts from `valid_from` (else `first_seen_at`) until `valid_to`, so a superseded fact is back in force before it was
  replaced; candidates and duplicates merged into another fact (superseded without an end) never count. A procedure
  counts from `created_at` until it was retired, taken from the `proc.retire` audit entry (else when its successor was
  created). Observations and summaries count from `created_at`, observations until `archived_at`. Recall ranks relative to
  `as_of`, rebuilds the profile from the facts valid then instead of the rendered `profiles` row, and leaves work state
  out. `/v1/context`, the timeline and by-file do not take `as_of`; the context cache is not time-aware.
- **History endpoint.** `GET /v1/memories/{id}/history` and the MCP `memory_history` follow `supersedes_id` in both
  directions (at most 50 links each way) under the caller's RLS, so links in spaces the caller cannot read drop out and
  an invisible id is a 404. Only facts and procedures chain; an observation or summary is a chain of one. Other
  `secret` rows in a chain are left out. Procedures have no `superseded_by` or end time columns, so both are derived
  from the chain and the audit log.
