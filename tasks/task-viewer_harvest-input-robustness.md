---
description: "Accept a UTF-8 BOM, leading blank lines, quoted status values, and space-separated timestamps in task frontmatter."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Accept common frontmatter variations

## Goal

A task file with a UTF-8 BOM, leading blank lines before the frontmatter, a quoted status value, or a space-separated timestamp is still harvested with the same dates and kind.

## Context

`frontmatter` in `task-viewer/src/harvest.rs` requires the file to start with `---\n` or `---\r\n`, and `classify` compares the status value exactly, so `status: "finished"` is treated as deferred. `parse_iso_date` in `task-viewer/src/dates.rs` splits only on `T`, so `created: 2026-05-01 10:00:00` fails and the task is skipped.

## Approach

Strip a leading UTF-8 BOM and any leading blank lines before looking for the opening delimiter. Strip matching single or double quotes from frontmatter values, at least for `status` and consistently for dates, before comparing. Extend `parse_iso_date` to split on `T` or a space and keep the date prefix. Add tests for each variation and for the existing tight `status:finished` form.

**Out of scope:** Full YAML parsing; the flat key-value frontmatter stays.

## Acceptance

- A harvest test with a BOM and leading blank lines charts the task with its frontmatter dates.
- A harvest test with `status: "finished"` under `tasks/archive/` charts it as finished.
- A date test parses `2026-05-01 10:00:00` and `2026-05-01T10:00:00` to 2026-05-01.
