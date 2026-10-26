---
name: handoff
description: Use when stopping or switching machines, to leave a hand-off so the next session continues where this one stopped.
disable-model-invocation: true
---

# Hand off to the next session

Shared memory already records what happened. A hand-off adds what only you know right now: the goal, where
things stand and what to do next. It is stored as the project's work state, so it appears in the context block
at the start of the next session on any machine. No file is written to the repository.

1. Work out, from this conversation and the repository, in your own words:
   - `goal`: what the user is trying to achieve, in one or two sentences.
   - `state`: what is done and what is half done, naming the files involved.
   - `tried`: approaches that failed and why, so they are not repeated.
   - `next`: the next concrete steps, in order.
   - `blockers`: anything waiting on the user or on something outside the repository. Leave it out if empty.
2. Call **`work_state_write`** with `list: "handoff"` and those keys as `fields`. Keep each value short and
   self-contained. Later writes win, so writing again updates a key.
3. Tell the user what you stored, and that the next session will see it at start-up.

To close a hand-off once the work is finished, call `work_state_write` again with every key set to `null`.
