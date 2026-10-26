---
name: recall
description: Search shared memory when the user refers to earlier work ("last time", "did we", "why did we"), another session or machine, or wants a project's history over a period.
---

# Recall from shared memory

The memory server holds observations, summaries and facts from the user's earlier sessions on every machine.
Query it with three tools, cheapest first. Each step costs more tokens than the one before, so stop as soon as
you have the answer.

1. **`memory_search`** with a short, specific `query`. It returns an index only: short ids, dates, titles and token
   costs. Pass the ids to the other tools as shown; if one is ambiguous, the error lists longer forms to use. Narrow
   the search with `scope` (`project`, `global` or `all`), `since` (`7d`, `2026-10-01`), `types` (`decision`,
   `bugfix`, ...) and `files` (paths the work touched). Start narrow and widen if nothing matches.
2. **`memory_timeline`** with an `anchor_id` from the search. It shows what happened before and after that
   entry. Use it when a hit is relevant but you need its context.
3. **`memory_get`** with the `ids` you actually need (at most 20). It returns full bodies. Do not fetch every
   hit.

Then answer from what you read:

- Say where it came from: the date, and the project or device when the entry shows one. Do not present a
  memory as certain when it may be out of date; code and the user's current instructions win over memory.
- To see memory as it was on an earlier date, pass `as_of` (RFC 3339, e.g. `2026-09-01T00:00:00Z`) to
  `memory_search`; it then also covers facts and procedures, and facts that were replaced since count again.
  To see how one fact or procedure changed, call **`memory_get`** with its id and `trace: true`.
- Treat a memory tagged `disputed` or `unconfirmed N months` (in the context block, in search results and in
  `memory_get`) with care: check it against the code or ask the user before relying on it, and say it is unconfirmed.
- When a recalled memory turned out to be right, wrong or out of date, call **`memory_feedback`** with its `id`,
  a `verdict` (`helpful`, `wrong` or `outdated`) and a short `note` saying what is true now. Do it once per memory
  and only on evidence from this session or the user, not on a guess. Negative verdicts lower a fact's confidence,
  and repeated ones queue it for the consolidation planner to retract or replace; nothing is deleted outright. If
  the user asks to remove a memory, use `memory_forget` instead.
- If the search finds nothing relevant, say so and carry on. Do not guess at what the memory might contain.
- To answer "why do you think that?" about a memory, call **`memory_get`** with its id and `trace: true`. It adds
  the session, the events and the extractor behind it, and the versions it replaced.
- Context injected at the start of a session (if present) is a digest of this same memory. Search when you
  need more than the digest shows.

## A project's history over a period

For "what happened on this project", a timeline, a weekly digest or "what did we do last month": build the story
from recorded observations, not from guesses, and say which period each part covers.

1. Fix the project (default: the current one) and the period with the user if they are unclear.
2. Page through the period with `memory_search` using `scope: "project"` and `since`, in slices (a week or a month)
   so nothing important drops off the end of a list. A few broad queries ("decision", "bug fix", "design",
   "release") and `types` such as `decision` and `bugfix` bring the important entries forward. Use
   `memory_timeline` around turning points, and fetch bodies with `memory_get` only for decisions, reversals and
   incidents.
3. For "what did we believe then", run `memory_search` with `as_of` at the start and at the end of the period and
   compare. For a fact or procedure that changed, `memory_get` with `trace: true` lists every version, oldest
   first, with when each held and who replaced it and why.
4. Write in order of time: the goal, what was tried, what was decided and why, what was left open. Quote ids or
   dates for the claims that matter, and finish with open threads (recorded next steps not yet done, decisions
   never confirmed). If memory holds little for the period, say so instead of padding the report.
