---
name: project-history
description: Write a narrative of how a project developed, from shared memory. Use when the user asks for a project history, a timeline report, "what happened on this project", a weekly digest, or a summary of a period such as "what did we do last month".
disable-model-invocation: true
---

# Project history from memory

Build the story from recorded observations, not from guesses, and say which period each part covers.

1. Fix the scope with the user if it is unclear: which project (default: the current one) and which period.
2. Page through the period with **`memory_search`** using `scope: "project"` and `since`. Use a few broad
   queries ("decision", "bug fix", "design", "release") and `types` such as `decision` and `bugfix` to bring the
   important entries to the front. Note the ids and dates in the index; do not fetch bodies yet.
3. For periods with many entries, work in slices (one week or one month at a time) so that nothing important
   drops off the end of a result list. Use **`memory_timeline`** around the turning points.
4. Fetch full bodies with **`memory_get`** only for entries that carry the story: decisions, reversals,
   incidents. At most 20 ids per call.
5. For "what did we believe then" and for reversals, ask the facts directly. **`memory_search`** with `as_of`
   (an RFC 3339 time such as `2026-09-01T00:00:00Z`) returns facts, procedures and past work as they stood at that
   instant, with replaced facts back in force. Run it for the start and the end of the period to see what
   changed. For a fact or procedure that changed, **`memory_history`** with its id lists every version, oldest
   first, with when each held and who replaced it and why.
6. Write the report in order of time. For each stretch: what the goal was, what was tried, what was decided and
   why, what was left open. Quote ids or dates for the claims that matter so the user can check them with
   `memory_explain`.
7. Finish with open threads: next steps recorded but not yet done, and decisions that were never confirmed.

If memory holds little for the period, say so instead of padding the report.
