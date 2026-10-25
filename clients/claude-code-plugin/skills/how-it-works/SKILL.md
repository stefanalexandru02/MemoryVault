---
name: how-it-works
description: Explain how MemoryVault captures, stores and recalls memory, what leaves this machine, and how to inspect or delete it. Use when the user asks how the memory plugin works, what it records, where the data lives, whether it is private, or how to stop or erase it.
---

# How MemoryVault works

Answer from this description, in plain language, and only as deep as the question goes.

**Capture.** Claude Code hooks run `mem-hook` on each event: session start and end, every prompt, every tool
call and failure, and each turn end. The hook posts the event to the user's MemoryVault server and always exits
successfully, so it cannot slow or break a session. When the server cannot be reached, events wait in
`~/.memory/spool` and are replayed at the next session start.

Documents are the one thing sent on request only: `mem-hook ingest` (the `ingest` skill) uploads the repository
files the user names, which are then searchable as memory of type `document`.

**Extraction.** Raw events are not what gets recalled. After a burst of activity, `mem-hook observe` leases a work
packet from the server and runs it through `claude -p` on this machine, using the user's own Claude login. The
model turns the events into short observations (what was learned or built), session summaries and candidate
facts. The server never calls a model and holds no Claude credentials. Consolidation, which runs nightly and
also soon after a session ends or a project has gathered enough new observations, merges duplicates, resolves
contradictions and lets old entries fade.

**Storage.** Everything lives in the user's own Postgres behind the MemoryVault server. Each user has private
spaces plus any spaces shared with them; access is enforced in the database. Tokens are per device and can be
revoked by the server's admin. Local files: `~/.memory/config` (URL and device name), `spool/`, `sessions/`
and `cache/` (the last context block, shown if the server is down).

**Recall.** At session start the hook injects a short context block: the profile, open work state, a hand-off
from other devices and an index of recent observations (the `full` mode, about 1,000 tokens). A `lean` mode
injects only the profile, open work state and a short rubric on when to call the search tools (300 to 400
tokens): cheaper at every start, but recent work is then seen only if the agent searches for it. It is set with
`MEM_INJECTION=lean` in `~/.memory/config` (`make connect-local INJECTION=lean`) or as the default in the viewer's
header, and `make connect-status` shows the tokens each mode has served. During a session the agent searches with
`memory_search`, `memory_timeline` and `memory_get`, and a hook mentions earlier work on a file the first time
it is read. The `recall`, `brief` and `project-history` skills describe how to search well.

**Control.** The server's web viewer (`/viewer` on the server's URL, signed in with a device token) lets the user browse, search and forget memories and see where each came from. `memory_remember` stores something on request; `memory_forget` retracts it; `memory_feedback` marks one helpful, wrong or outdated, which lowers or raises a fact's confidence; `memory_explain` shows
where an entry came from and `memory_history` how a fact or procedure changed over time. To keep one project out of memory entirely, add its key to `~/.memory/exclude`
(one project key per line, such as `github.com/me/private-repo`). To stop capture on this machine, run
`make disconnect` in the MemoryVault checkout.

**Troubleshooting.** When the user asks why nothing is remembered, have them run `mem-hook doctor` (the binary is
`bin/mem-hook` in the plugin; `make doctor` in the MemoryVault checkout also asks the server). It checks the
config, the server, the token, the plugin, the spool of undelivered events and the last successful delivery, and
prints the command that fixes each failing line.
