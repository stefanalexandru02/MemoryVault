# MemoryVault

MemoryVault is a shared memory service for Claude Code. A small Rust hook binary records each session
(prompts, tool use, replies) and sends it to a server on the tailnet. The server runs extraction through
the user's own `claude -p` login, stores observations, facts, procedures and summaries in Postgres with
pgvector, and returns a budgeted context block at session start. Claude Code reaches the same memory
through MCP tools (`memory_search`, `memory_get`, `memory_remember`, ...). Row-level security keeps
private and shared spaces apart. The build contract is [docs/implementation-spec.md](docs/implementation-spec.md),
and the rationale is in [docs/architecture.md](docs/architecture.md).

## Repository layout

| Path | Content |
|---|---|
| `crates/core` | Settings, database pools and transactions, auth and tokens, schemas, prompts, TEI client, error shapes |
| `crates/ingest` | Event ingest, sessions, windows, work-packet queue, extract and summary appliers |
| `crates/recall` | `/v1/recall`, context block and cache, candidates, scoring, rendering |
| `crates/memories` | MCP server (`/mcp`), explicit memory writes, forget, work state |
| `crates/consolidate` | Planner, consolidation appliers, revert, admin CLI and admin API |
| `crates/worker` | Embedding jobs, sweeps, context re-render, retention, metrics |
| `crates/evals` | Session recorder, redaction, replay, comparison with claude-mem |
| `crates/server` | The `memvault` binary: `serve`, `worker`, `planner`, `admin`, `evals` (and `migrate`, which `serve` already runs at start-up) |
| `crates/mem-hook` | The `mem-hook` binary for Claude Code hooks and the observer |
| `clients/claude-code-plugin` | Plugin manifest, hook wiring, `install.sh`, plugin README |
| `migrations` | SQL migrations, applied in order by `memvault serve` as it starts |
| `prompts`, `schemas` | Observer and consolidation prompts (YAML) and their JSON output schemas |
| `evals` | Recorded sessions, questions, gold facts and the recall baseline |
| `deploy/k8s` | Kustomize base and the `home` overlay |
| `deploy/docker` | Container images for the server and Postgres |
| `docs` | Spec, architecture notes and the list of deviations |

`CONTRACTS.md` records crate ownership, cross-crate interfaces and conventions.

## Run it locally with Docker Compose

`docker-compose.yml` runs the whole server on one machine: Postgres (ParadeDB, which bundles pgvector and
`pg_search`), the two TEI servers, migrations, the API, the worker and a scheduler that runs a consolidation
planning pass once a day. The `Makefile` wraps it; `make` lists every target.

```bash
make up              # creates .env with fresh secrets, builds the image, starts everything (viewer at /viewer)
make connect-local   # connects Claude Code on this machine to it: token, hooks, MCP server, permissions
```

`make connect-local` mints a device token for `$(whoami)` (or reuses the one in `~/.memory/config` while it
works), builds `mem-hook`, writes `~/.memory/config`, installs the hook plugin through a local plugin
marketplace (`.claude-plugin/marketplace.json`), registers the MCP server `memory` in user scope with the URL
and token written in, and allows `mcp__memory__*` in `~/.claude/settings.json`. Start a new Claude Code session
afterwards. It needs the `claude` CLI, `cargo` and `curl`, and `python3` or `jq` for the settings edit.

```bash
make connect-status                                         # what is connected, and does the token work
make disconnect                                             # remove all of it and revoke the local token
make connect-remote URL=https://memory.<tailnet>.ts.net TOKEN=mem_...   # the same, for a server elsewhere
```

`make disconnect` keeps `~/.memory/spool` and `sessions` (add `PURGE=1` to delete them). Tokens of a
remote server stay valid until its admin revokes them. All three targets call
`clients/claude-code-plugin/connect.sh`, which you can also run directly.

The first `make up` downloads the TEI models (about 4.5 GB) into a volume. `make up-lite` skips TEI and
uses the fake deterministic embedder: the plumbing works, recall quality does not. Run `make reindex` after
moving from `up-lite` to `up`.

Other targets: `make logs SERVICE=api`, `make ps`, `make down` (keeps data), `make psql`,
`make consolidate`, `make admin ARGS="space list"`, `make token HANDLE=alex CLIENT=desktop KIND=runner`,
`make test` (the Rust suite against the compose Postgres) and `make clean` (deletes the volumes, asks first).

The API and Postgres bind to `127.0.0.1` by default. To serve other machines, set `BIND_ADDR` in `.env`
to the host's LAN or Tailscale address and use `MEMORY_URL=http://<that address>:8000` on the clients.
`.env` holds `TOKEN_PEPPER`; tokens stop working if it changes, so back it up.

On a Mac, the stack runs in Docker Desktop's Linux VM on the CPU (no Metal). The Makefile selects TEI's arm64 image
on Apple Silicon. The two models need about 5 GB of RAM together, so give Docker Desktop at least 8 GB
(Settings → Resources).

## Local development

### Toolchain

Install Rust 1.89 or newer with `rustup`. The workspace uses edition 2024. Format with `rustfmt.toml`
(max width 120).

### Test Postgres

The tests need Postgres 16 or newer with pgvector (0.6 or newer). Either run `make test`, which uses the
compose Postgres, or start one on `127.0.0.1:55432` with user `postgres` and trust authentication.
`.cargo/config.toml` sets:

```
DATABASE_URL=postgres://postgres@127.0.0.1:55432/postgres
```

Each `#[sqlx::test]` creates its own database from that URL. BM25 search uses ParadeDB `pg_search` when it
is installed and falls back to `tsvector` with `websearch_to_tsquery` and `ts_rank_cd` when it is not.

### Build, test and lint

```bash
cargo build --workspace
cargo test --workspace                       # or -p <crate> for one crate
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

### Run the server locally

```bash
export DATABASE_URL=postgres://postgres@127.0.0.1:55432/postgres
export DATABASE_SYSTEM_URL=$DATABASE_URL
export TOKEN_PEPPER=$(openssl rand -hex 32)   # required by serve and admin; keep it stable
export FAKE_TEI=1                             # deterministic embeddings, no TEI needed

cargo run -p memvault -- serve --port 8000    # applies pending migrations, then serves the HTTP API and MCP server
cargo run -p memvault -- worker               # embed jobs, sweeps, re-render, retention (waits for the schema)
cargo run -p memvault -- planner              # one consolidation planning pass
cargo run -p memvault -- admin --help         # users, spaces, tokens, consolidation, reindex
cargo run -p memvault -- evals --help         # record, redact, replay, compare
```

`FAKE_TEI=1` replaces the TEI client with `FakeTei`, a deterministic in-process embedder used by the tests and
by the CI eval job. Without it, `TEI_EMBED_URL` and `TEI_RERANK_URL` must point at running TEI servers.

Settings come from environment variables with no prefix. See `memvault_core::Settings` in `crates/core/src/config.rs`
and the Settings section of `CONTRACTS.md` for the full list and defaults.

### Create a first user and token locally

```bash
cargo run -p memvault -- admin user create alex --display-name "Alex"
cargo run -p memvault -- admin token create --user alex --client laptop-x1 --kind device
```

`token create` prints the raw token once. Store it when it appears.

## Web viewer

The server serves a viewer at `/viewer` (`http://127.0.0.1:8000/viewer` for the compose stack; `/` redirects
there). Open it, paste a device token (`make token`, or the one in `~/.memory/config`) and you can:

- browse and search observations, summaries, facts and procedures, filtered by kind, project, type and period,
  with live updates as new memories arrive (`/` focuses the search box, `j`/`k` step through the list);
- open any memory to read it in full, see where it came from (session, device, extractor, the observations a
  fact rests on, the audit history) and what was recorded around it;
- forget a memory, with a reason that goes to the audit log;
- read the open work lists per project;
- with an admin token: list and revoke tokens, and see failed work packets and background jobs, with a retry.

The viewer is three static files compiled into the server (`crates/server/assets/viewer`). It calls the same
`/v1` API as the clients, so it shows exactly what your token may see and nothing more. The token stays in
`sessionStorage` unless you tick "Remember on this device". The page sets a strict content security policy (no
inline script or style, same-origin requests only) and renders memory text as text, never as markup, because
that text comes from tool output.

## Installing the Claude Code plugin

`make connect-local` and `make connect-remote` (above) do all of this in one step. The manual route is below.

The plugin is in `clients/claude-code-plugin`. It needs the `claude` CLI and `git`. It does not need Node,
Python, `curl` or `jq`.

1. Build and install the hook binary:

   ```bash
   cd clients/claude-code-plugin
   ./install.sh
   ```

   The script runs `cargo build --profile release-small -p mem-hook` and copies the binary to `bin/mem-hook`.
   It creates `~/.memory` (mode 0700) and a `config` template (mode 0600). Set `MEM_HOOK_BIN` to install a
   prebuilt binary instead. On Linux the build is static. A musl build is described in the plugin README.

2. Fill in `~/.memory/config`, which the hooks read:

   ```
   MEMORY_URL=https://memory.<tailnet>.ts.net
   MEMORY_TOKEN=mem_...
   # DEVICE_ID=laptop-x1
   # CLAUDE_BIN=/usr/local/bin/claude
   ```

   Export it in the shell where you run the commands below: `set -a; . ~/.memory/config; set +a`.

3. Install the plugin from the local marketplace and give its MCP server the URL and token. The token goes to
   the keychain, not into a file:

   ```bash
   claude plugin marketplace add "$PWD"
   claude plugin install memory@memoryvault
   printf '{"memory_url":"%s","memory_token":"%s"}' "$MEMORY_URL" "$MEMORY_TOKEN" \
     | claude plugin configure memory@memoryvault --values-stdin
   ```

4. Allow the MCP tools in `~/.claude/settings.json` so they run without prompts:

   ```json
   { "permissions": { "allow": ["mcp__plugin_memory_memory__*"] } }
   ```

   Start a new session. `claude --plugin-dir /path/to/clients/claude-code-plugin` loads the plugin for one
   session without installing it.

The plugin brings six skills (`recall`, `remember`, `brief`, `project-history`, `handoff`, `how-it-works`),
the MCP tools and the hooks. The plugin README lists them.

Each hook event runs one `mem-hook` subcommand: `session-start`, `prompt`, `tool`, `tool-failure`, `stop`,
`subagent-stop`, `session-end` and `file-context`. Hooks exit 0 in every case. Undelivered requests go to
`~/.memory/spool` and are replayed at the next session start. The plugin README has the full table.

### Nightly consolidation runner

On an always-on machine with `claude` installed, use a runner token in that machine's config and add a cron
entry. Cron has a minimal `PATH`, so set `CLAUDE_BIN` in the config:

```
0 3 * * *  /path/to/clients/claude-code-plugin/bin/mem-hook observe --all --max 50
```

## Deploying to Kubernetes

The base is in `deploy/k8s/base` and runs in namespace `memory`. The `home` overlay sets the image names,
the backup endpoint and the Postgres image. Apply it with:

```bash
kubectl apply -k deploy/k8s/overlays/home
```

Before the first apply, change the placeholder values in the overlay: `ghcr.io/REPLACE-OWNER/...` for the images, and
the MinIO endpoint and bucket for backups. Build and push the images from `deploy/docker/server.Dockerfile`
and, if you use the fallback, `deploy/docker/postgres.Dockerfile`. The TEI manifests use the x86_64 image `cpu-1.9.4`; on arm64 nodes,
switch to `cpu-arm64-1.9.4`.

### Secrets

Secrets are not in the kustomization. Create them before the first apply, using the shapes in
`deploy/k8s/base/secret-template.yaml`:

```bash
kubectl -n memory create secret generic memory-secrets --from-literal=TOKEN_PEPPER="$(openssl rand -hex 32)"
kubectl -n memory create secret generic memory-pg-memsvc-app --type kubernetes.io/basic-auth \
  --from-literal=username=memsvc_app --from-literal=password="$(openssl rand -hex 24)"
kubectl -n memory create secret generic memory-pg-memsvc-system --type kubernetes.io/basic-auth \
  --from-literal=username=memsvc_system --from-literal=password="$(openssl rand -hex 24)"
kubectl -n memory create secret generic memory-backup-s3 \
  --from-literal=ACCESS_KEY_ID=... --from-literal=SECRET_ACCESS_KEY=...
```

`TOKEN_PEPPER` must stay the same for the life of the deployment. Changing it invalidates every token. The
server refuses to start `serve` or `admin` with an empty pepper. The workloads reference these keys without
`optional`, so a missing key keeps the pod from starting. No Claude or Anthropic credential belongs in this
namespace. Extraction uses each client's own `claude` login.

### First admin token

When the API pod is up, create the first user and an admin token from inside it:

```bash
kubectl -n memory exec deploy/memory-api -- memvault admin user create alex --display-name "Alex"
kubectl -n memory exec deploy/memory-api -- memvault admin token create --user alex --client admin-cli --kind admin
```

The raw token prints once. Store it in a password manager. Use it for `memvault admin` calls through the API
and for later device tokens. Create device tokens with `--kind device` and a client id such as `laptop-x1`.

### What runs

| Object | Kind | Role |
|---|---|---|
| `memory-api` | Deployment, Service, HPA | HTTP API and MCP server; applies pending migrations as it starts |
| `memory-worker` | Deployment | Embedding, sweeps, re-render, retention. Metrics and health on `WORKER_METRICS_ADDR` |
| `memory-consolidate` | CronJob (03:00 daily) | Consolidation planner |
| `tei-embed`, `tei-rerank` | Deployment, Service | TEI servers on port 8080 behind services on port 80 |
| `memory-pg` | CloudNativePG Cluster | Postgres with pgvector and pg_search, scheduled backups |

The base also has the ingress, network policies, a ServiceMonitor and Prometheus alert rules.

## Status by milestone

Status reflects what the repository contains. The acceptance checks in spec section 17 were not run as part of
this write-up. The live cluster, the laptop and desktop checks, and the Windows hook are not verified.

**Phase 1: capture and pull-mode recall.** Built: sessions and events with idempotent ingest, the
hook binary with spool and replay, the observer through work packets, `extract` and `summarize_*` packets, TEI
embedding, the context block with index and since-you-were-here, MCP search, timeline and get, and the eval
recorder. Open: the acceptance checks (window to observation within 2 min, cross-machine `next_steps`, zero-duplicate
replay) need a live run.

**Phase 2: semantic tier and push mode.** Built: entities, facts and profiles, fact candidates in extraction,
`/v1/recall` with budgets, retrieval log and feedback, `memory_remember`, `memory_forget` and `work_state_write`.
Dropped by owner decision: the Python client and the agentic OS packet runner, so the "agentic OS injects a
slot under 300 ms p95" check does not apply. Open: the fact-confirmation check across two sessions needs a
live run.

**Phase 3: consolidation and explain.** Built: the planner, `consolidate_facts`, `consolidate_summary` and
`consolidate_procedures` packets, apply rules with audit rows, revert, the nightly runner
(`mem-hook observe --all`), the `explain` endpoint and tool, incremental decay, day and week summaries, and
eval replay in CI against `evals/baseline.json`. Open: the planted-contradiction check (resolved in one nightly
run, then reverted) needs a live run. The on-demand consolidation trigger from architecture 5.4 is not built.

**Phase 4: polish.** Built: procedure mining (through `consolidate_procedures`) and the Rust hook binary for
Linux. The hook code type-checks for Windows but has not been run there. The RLS test suite exists in the crate
tests. Not built: the file-context hook, the admin viewer UI, OAuth for more users, KEDA scaling (no
ScaledObject in the base), and export and import per space.

## More

- [docs/deviations.md](docs/deviations.md): every deviation from the spec, with its reason.
- [CONTRACTS.md](CONTRACTS.md): ownership, interfaces and conventions.
- [clients/claude-code-plugin/README.md](clients/claude-code-plugin/README.md): hook behaviour, the observer, files under `~/.memory`.
