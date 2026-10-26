---
name: ingest
description: Use when the user asks to load this repository's docs (ADRs, READMEs, runbooks) into shared memory.
disable-model-invocation: true
---

# Ingest the repository's docs

The docs become searchable memory of type `document`, found with `memory_search` (`types: ["document"]`) and in
recall results with their source. They are never put into the session-start context. Only files that changed
since the last run are sent, so running it again is cheap.

1. Find the directories that hold the docs, such as `docs/`, `doc/`, `adr/` or `runbooks/`, and the top-level
   files (`README.md`, `CONTRIBUTING.md`). Ask the user when it is not obvious. Leave out generated or vendored
   text.
2. Preview with a dry run for each: `mem-hook ingest <dir> --dry-run`, using the plugin's `bin/mem-hook` when
   `mem-hook` is not on the `PATH`. It lists the files that would be sent, skipped (binary, over 400 KiB, empty)
   or removed, and sends nothing. Tell the user what it found.
3. After they agree, run it again without `--dry-run`. Add `--prune` for a directory whose deleted files should
   also leave memory, `--glob "**/*.md"` to pick other files, and `--scope global` only for docs that belong to no
   project. A single file is `mem-hook ingest README.md`. Add `--force` to send files whose content has not changed,
   for instance to bring back a chunk the user forgot.
4. Report the summary line it prints. It exits non-zero when a file failed; show the `error:` lines.

Do not paste the files yourself or call `memory_remember` for them: the command chunks them by heading, keeps
the heading path and source for each chunk, and replaces a file's chunks when it changes.
