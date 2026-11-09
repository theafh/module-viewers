---
description: "Accept a UTF-8 BOM, leading blank lines, quoted status values, and space-separated timestamps in task frontmatter."
scope: task-viewer
created: 2026-11-05T06:00:45Z
updated: 2026-11-07T18:43:11Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Accept common frontmatter variations

## Goal

A task file with a UTF-8 BOM, leading blank lines before the frontmatter, a quoted status value, or a space-separated timestamp is still harvested with the same dates and kind.

## Context

`frontmatter` in `task-viewer/src/harvest.rs` requires the file to start with `---\n` or `---\r\n`, and `classify` compares the status value exactly, so `status: "finished"` is treated as deferred. `parse_iso_date` in `task-viewer/src/dates.rs` splits only on `T`, so `created: 2026-05-01 10:00:00` fails and the task is skipped.

## Approach

Strip a leading UTF-8 BOM and any leading blank lines before looking for the opening delimiter. Strip matching single or double quotes from frontmatter values inside `frontmatter`, at least for `status` and consistently for dates, so `classify`, `has_archived_status`, and `leave_dates` all read the unquoted value. Match the quoted form in `first_status_commit_date`, which reads raw commit text rather than frontmatter fields: its status pattern accepts `status: X`, `status: "X"`, and `status: 'X'`, so an archived task with a quoted non-finished status takes its leave day from the earliest commit that introduces the quoted status, the same day the unquoted form yields. Extend `parse_iso_date` to split on `T` or a space and keep the date prefix. Add tests for each variation and for the existing tight `status:finished` form.

**Out of scope:** Full YAML parsing; the flat key-value frontmatter stays.

## Acceptance

- A harvest test with a BOM and leading blank lines charts the task with its frontmatter dates.
- A harvest test with `status: "finished"` under `tasks/archive/` charts it as finished.
- A harvest test with `status: 'finished'` under `tasks/archive/` charts it as finished.
- A harvest test with `status: "finished"` under `tasks/archive/` charts its ship day as the day of the archive commit that introduced the quoted status, not the frontmatter `updated` day.
- A harvest test with `status: 'finished'` under `tasks/archive/` charts its ship day as the day of the archive commit that introduced the quoted status, not the frontmatter `updated` day.
- A test calls `first_status_commit_date` with the unquoted status `open` against a repository whose one archived commit adds a file carrying `status: "open"`, and it returns that commit's day where today it returns `None`.
- A test calls `first_status_commit_date` with the unquoted status `open` against a repository whose one archived commit adds a file carrying `status: 'open'`, and it returns that commit's day where today it returns `None`.
- A date test parses `2026-05-01 10:00:00` and `2026-05-01T10:00:00` to 2026-05-01.
