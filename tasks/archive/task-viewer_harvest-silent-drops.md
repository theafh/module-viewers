---
description: "Strip trailing # comments from frontmatter values, close frontmatter only on a bare --- line, log every task file dropped for bad frontmatter, and fail the scan on an unreadable tasks directory."
scope: task-viewer
created: 2026-11-08T14:13:24
updated: 2026-11-08T21:56:12
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Stop task files from misparsing or vanishing silently

## Goal

A task file whose frontmatter carries a trailing comment, such as `status: finished  # shipped early`, charts like its plain spelling. A frontmatter value line that happens to begin with `---` no longer ends the block early. A task file that `harvest_file` drops leaves a log line naming it and the reason, and a directory under `tasks/` that cannot be read fails that repository's scan, which keeps its earlier rows and shows the error, instead of silently charting fewer tasks. The parser stays the flat key-value reader it is today.

## Context

`frontmatter` in `task-viewer/src/harvest.rs` strips a BOM and leading blank lines, requires an opening `---` line, ends the block at `rest.find("\n---")`, splits each unindented line at its first `:`, and stores `unquote_frontmatter_value(value.trim())`. Two inputs go wrong:

- A trailing comment stays in the value. `status: finished  # done` stores `finished  # done`, so `classify` files an archived task as `Deferred`, and it never ships. `created: 2026-01-01\t# note` reaches `parse_iso_date` as `2026-01-01\t# note`, which splits only on `T` or a space and fails, so the row is dropped.
- Any line that starts with `---`, such as a value line `---draft`, ends the block there and drops every later key.

`has_archived_status` reads the archived status through the same `frontmatter`, while `first_status_commit_date` finds status commits by raw text (`-S "status: X"` plus quoted forms, and a `-G` pattern anchored on `^status:`) that already match a line carrying a trailing comment. Stripping the comment therefore brings classification in line with the leave-day lookups.

`harvest_file` returns early, with no log line, when a file has no frontmatter, when `created` is missing or does not parse, or when `status` is missing. `files_on_disk` walks `tasks/` with `WalkDir` and drops every walk error through `.filter_map(|entry| entry.ok())`, so an unreadable subdirectory, or an unreadable `tasks/` itself, yields fewer files or none; the following store then replaces the repository's rows with that smaller set. It serves a folder outside git and the working-tree candidates of a git repository alike. `HarvestError::Io` already reports an I/O failure as "The scan could not read the repository: …", and `ARCHITECTURE.md` keeps a repository's rows when its scan fails ("A missing directory keeps the previous rows and records `last_error`"). The `tracing::warn!` that names a lossy path and its fallback day in `HarvestState::finish` is the precedent for a harvest log line that names a file.

The archived `task-viewer_harvest-input-robustness` task settled that the flat key-value frontmatter stays, and the archived `task-viewer_status-regex-escape` task settled that the classification rule stays; this task changes how a value is read, not which statuses classify how.

## Approach

1. Before `unquote_frontmatter_value`, strip a trailing comment from each value. A value that opens with `"` or `'` keeps everything up to its matching closing quote, and a `#` after that quote that follows whitespace starts a comment. An unquoted value ends at the first `#` that follows a space or a tab. Trim the result.
2. End the frontmatter block at the first line, after the opening delimiter, whose content without a trailing `\r` and trailing whitespace is exactly `---`.
3. In `harvest_file`, log `tracing::warn!` naming the path and the missing or unparseable field when `status` is missing, or `created` is missing or does not parse. When a file has no frontmatter, log `tracing::warn!` naming the path and the absent frontmatter if its file name carries the task naming shape — a `.md` name with an `_` in it, the shape `harvest_file` already splits for `scope` — and `tracing::debug!` naming the path otherwise, which is how a stray `README.md` under `tasks/` reads. The returned rows stay unchanged.
4. In `files_on_disk`, return `HarvestError::Io` built from the first walk error, so the repository's scan fails, its rows stay, and the error names the unreadable path. A file that opens but cannot be read as text keeps today's skip.
5. Version the content hash so no stored row survives this reader change: keep the FNV-1a digest in its own helper and return it from `content_hash` behind a revision constant, such as `format!("r2:{digest}")`, which the next frontmatter change bumps again. Every archived row the database holds then mismatches once, re-walks its leave-day history on the next Update, and stores a hash that matches again. Refresh the `content_hash` doc comment to say the digest covers the reader revision as well as the bytes.
6. Extend the **3. Repositories** sentence in `FEATURES.md` that lists a BOM, leading blank lines, quoted values, and a space-separated timestamp with a trailing `#` comment, and extend its scan-error sentence with an unreadable directory under `tasks/`. Extend the `ARCHITECTURE.md` **Data Layout** sentence about a missing directory to cover an unreadable one, and add the comment and walk-error cases to the Harvest bullet of `TESTING.md`.

## Acceptance

- A harvest test archives a task with `status: finished  # shipped early` and charts it as finished with its archive commit's ship day.
- A harvest test harvests a repository whose archived task carries `status: finished  # shipped early` through `harvest_repository_with_prior` with a prior row for that path whose `content_hash` is the bare FNV digest of those same bytes and whose leave day is an earlier commit's day, and finds the returned ship day is that task's archive commit day.
- A harvest test charts a live task whose `created: 2026-01-01\t# note` value carries a tab-separated comment with created day 2026-01-01.
- A unit test of `frontmatter` reads `status: "fin#ished" # note` as `fin#ished`, and reads a block carrying `title: draft`, an unindented `---draft` line, then `status: open`, and finds `title` and `status` both present.
- A harvest test under a thread-scoped `tracing` subscriber, like the existing lossy-path warning test, finds one warning naming a task file whose `created` is `not-a-date`, one warning naming a task-named file with no frontmatter, and no warning for a `README.md` without frontmatter in the same `tasks/` tree.
- A harvest test under the same subscriber finds one warning naming a task file whose frontmatter has no `status` key, and one naming a file with no `created` key.
- A harvest test on a folder outside git whose `tasks/archive/` directory has its read permission removed returns `HarvestError::Io` naming that directory, and a scan test finds the repository's earlier rows intact with the error recorded. The test restores the permission before its temp directory is removed, and returns early when the directory stays readable after the mode change, as it does for root.
- The **3. Repositories** section of `FEATURES.md` names the trailing comment and the unreadable directory, the **Data Layout** section of `ARCHITECTURE.md` covers an unreadable directory, the **Harvest** bullet of `TESTING.md` names the trailing-comment and walk-error cases, and each rule appears once with no superseded wording beside it.
