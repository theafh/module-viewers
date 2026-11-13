---
description: "Accept a whitespace-padded opener, trim quoted status values in the archive walk, skip an empty status, and warn on an unparseable updated date."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T13:25:45Z
status: open
reported-by: Andreas Hoffmann
---

# Make frontmatter status and date handling consistent

## Goal

A frontmatter block that opens with `---` plus trailing whitespace parses like a plain `---`. A quoted status padded with spaces (`" finished "`) both classifies as finished and matches the archive walk, so its ship day is the real archived commit instead of the updated fallback. An empty `status:` is skipped rather than charted. An unparseable `updated` date logs a warning the way an unparseable `created` already does.

## Context

- `task-viewer/src/harvest.rs` reads the frontmatter opener, classifies the status, and walks `tasks/archive/` for the archived commit. The opener check is stricter than the closer check, `fn has_archived_status` compares the raw value without trimming, and the empty-status path yields `Some("")`.
- The finished task [task-viewer_harvest-silent-drops.md](archive/task-viewer_harvest-silent-drops.md) made dropped files log a reason. These are the remaining inconsistent paths.
- `ARCHITECTURE.md` names a content-hash revision that must move when parsing changes, so stored rows re-parse.

## Approach

- Trim the first line before comparing it to `---`.
- Trim the value inside `has_archived_status`, and trim it at classification time so both paths see the same string.
- Return `None` for an empty status value so the task is skipped with a log line.
- Warn on an unparseable `updated` with the repository, path, and raw value, mirroring the `created` warning.
- Bump the content-hash revision `ARCHITECTURE.md` names so stored rows re-parse under the new rules.

## Acceptance

- A fixture whose frontmatter opens `---   ` charts its task.
- A fixture with `status: " finished "` inside `tasks/archive/` charts its real archived-commit ship day, not its `updated` day.
- A fixture with an empty `status:` produces no row and one warning.
- A fixture with `updated: not-a-date` produces one warning naming the path and value.
- `make lint` and `make test` pass.
