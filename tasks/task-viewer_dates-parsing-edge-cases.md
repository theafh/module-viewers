---
description: "Parse lowercase-t separators, hour-only and space-separated offsets, and non-ASCII offset tails without panicking or keeping the wrong UTC day."
scope: task-viewer/src
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T13:25:45Z
status: open
reported-by: Andreas Hoffmann
---

# Harden `parse_iso_date` against malformed offsets and separators

## Goal

A task timestamp with a lowercase `t` separator, an hour-only offset such as `+01`, a space before its offset, or a multi-byte character in a four-byte offset parses to its correct UTC day instead of panicking the harvest or silently keeping the wrong day. The parser keeps its documented fallback: a value whose date prefix parses but whose time or offset does not keeps that date prefix.

## Context

- `task-viewer/src/dates.rs` holds the parser. `fn parse_iso_date` splits the value with `split_once(['T', ' '])`, `fn split_offset` returns the offset suffix with any leading space still attached, and `fn parse_offset_seconds` calls `rest.split_at(2)` when `rest.len() == 4`.
- The doc comment on `parse_iso_date` states the contract: `±HH:MM` and `±HHMM` offsets are read as instants, a bare date keeps its day, and an unreadable time keeps the date prefix.
- The finished task [task-viewer_utc-timestamps.md](archive/task-viewer_utc-timestamps.md) established the UTC rule. These four are gaps in that promise, confirmed by three independent audits at commit `b899b57`.

## Approach

- Replace `rest.split_at(2)` with `rest.split_at_checked(2)?` so a multi-byte tail returns `None` and keeps the date prefix instead of panicking the blocking harvest.
- Split the date from the time on `['T', 't', ' ']`.
- Accept a two-digit hour-only offset in `parse_offset_seconds`, treating `+01` as `+01:00`.
- Trim the time text in `split_offset` before `parse_wall_time` sees it, so `00:30:00 +0100` parses.
- Add one inline `#[cfg(test)]` case per input, including a non-ASCII offset tail that must return `None` rather than unwind.

## Acceptance

- `make test` passes a new inline test for each of the four inputs.
- `created: 2026-11-09T10:00:00+€0` leaves the harvest running and charts `2026-11-09`; the same value with a valid offset charts its UTC day.
- `created: 2026-11-09t00:30:00+01` charts `2026-11-08`.
- `created: 2026-11-09 00:30:00 +0100` charts `2026-11-08`.
- `make lint` passes.
