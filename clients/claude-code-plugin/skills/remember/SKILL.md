---
name: remember
description: Store or forget a durable fact, procedure or note when the user says remember, "from now on", "always" or forget.
---

# Remember and forget

Only store what the user asked you to keep, or a correction they gave. Do not save things on your own initiative.
Never store secrets such as passwords, tokens or keys.

## Remember: `memory_remember`

Pick the `kind`:

- **fact**: one short statement about a person, tool or project, at most 300 characters. It needs a
  `predicate`, a dotted lowercase key such as `prefers.editor` or `deploys.target`, and a `subject` (default:
  the user). A new fact with the same subject and predicate replaces the old value.
- **procedure**: how to do a task. Put the task class in `predicate` and a short name in `subject`. The content
  is the steps.
- **note**: anything else worth keeping, in a sentence or two.

Set `scope` to `project` for things that only apply to the current project, and `global` for the user's
preferences and anything that holds everywhere. After storing, tell the user what you saved in one line.

## Forget: `memory_forget`

1. Find the memory with `memory_search`, then `memory_get` to read it. Do not forget by guess.
2. Show the user what you found and confirm it is the right one. Forgetting is soft (facts are retracted,
   observations archived) but it changes what every session sees.
3. Call `memory_forget` with the `id` and a short `reason`.

To correct a fact, remember the new value; the old one is replaced.
