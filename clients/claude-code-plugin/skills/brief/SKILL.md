---
name: brief
description: Compile a sourced briefing on one topic from everything memory holds about it: facts, procedures and past work. Use when the user asks "what do we know about X", wants to get up to speed on a topic, or asks for the standing rules, preferences or how-tos about something.
---

# Brief me on a topic

Memory keeps several kinds of knowledge. A good briefing draws on all of them, not only past work.

1. Run **`memory_search`** for the topic with `scope: "all"`. Run it again with a narrower `scope: "project"`
   when the topic is project-specific, and with other wordings of the same topic when the first search is thin.
2. Read what matters with **`memory_get`** (at most 20 ids). Separate what you find into:
   - **Facts**: stable statements about the user, tools and projects, with their confidence and date.
   - **Procedures**: how-to steps that were recorded for this kind of task.
   - **Past work**: decisions, fixes and discoveries, newest first.
3. Write the briefing in that order: the standing facts and rules, then the procedures to follow, then the
   history that explains them. Cite each claim with its date and id.
4. Flag conflicts openly: if two entries disagree, show both with dates and say which looks newer. Do not pick
   one silently. Offer to correct the record with `memory_remember` (a new fact replaces the old value) or
   `memory_forget`, after the user confirms.
5. List what is missing: questions the briefing could not answer from memory.
