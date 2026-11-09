---
description: "Read every task timestamp as an instant, chart it on its UTC day, assume Europe/Berlin for a naive time, and run the viewer's clock in UTC."
scope: task-viewer
created: 2026-11-09T10:21:33Z
updated: 2026-11-09T10:58:59Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Chart task dates on their UTC day

## Goal

Every task date the dashboard charts is a UTC calendar day. A frontmatter `created` or `updated` value and a git commit timestamp are read as instants: a value that states `Z` or a numeric offset converts directly, and a value with a time but no zone is read as `Europe/Berlin`, the zone the legacy task system wrote. A date-only value keeps the calendar day it names. The viewer's own clock also runs on UTC, so the dashboard's `as_of` day, the horizon its series end at, and the `created_at` / `scanned_at` stamps on the Repositories tab share the task system's canonical time base.

## Context

`task-viewer/src/dates.rs` holds `parse_iso_date`, which splits at `T` or a space and parses only the date prefix, so the time and any offset are discarded, and `today`, which returns `Local::now().date_naive()`. A commit at `2026-11-09T00:30:00+01:00` therefore charts on 2026-11-09 today, while its UTC day is 2026-11-08.

`task-viewer/src/harvest.rs` parses frontmatter `created` and `updated` through `parse_iso_date` in `harvest_files`, and parses git `%cI` output through the same function in `parse_archive_log` and `scan_log_dates`. One parser change covers both input families. The existing skip and fallback paths stay: an unparseable `created` skips the file, and an unparseable `updated` falls back to `created`.

`task-viewer/src/db.rs` binds `now_stamp`, which formats `Local::now()` as `%Y-%m-%d %H:%M`, to `created_at` and `scanned_at`, and `task-viewer/templates/repositories.html` renders both.

The task system stamps a fresh `created` and every `updated` with `date -u +%Y-%m-%dT%H:%M:%SZ`, so a current task carries a UTC instant with `Z`. Legacy task files and every commit's `%cI` carry a Berlin wall time. `%cI` also carries its numeric offset, so a git timestamp states its own instant; only a naive frontmatter timestamp needs the `Europe/Berlin` default.

`ARCHITECTURE.md` under **Extraction Rules** records the current gap: "Calendar days use the date prefix as written (local Berlin wall time until timezone conversion exists)." That sentence is the passage this task supersedes.

[task-viewer_harvest-input-robustness.md](archive/task-viewer_harvest-input-robustness.md) defines the frontmatter spellings the parser must keep accepting: a UTF-8 BOM, leading blank lines, quoted values, and a space-separated timestamp.

`chrono-tz` 0.9 already resolves through `tera` in `Cargo.lock`, so a direct dependency adds no new crate version.

## Approach

1. Rewrite `parse_iso_date` in `task-viewer/src/dates.rs` to return the UTC day of a full timestamp. Keep the trim and the one matching quote pair. A bare `YYYY-MM-DD` value parses and returns unchanged. A value with a time normalises a space separator to `T`, parses an explicit `Z`, `±HH:MM`, or `±HHMM` offset and converts it to UTC, and parses a value with no offset as `Europe/Berlin` and converts that instant to UTC. Accept an optional seconds part and an optional fractional-second part, so `2026-11-09T00:30` reads as a Berlin wall time and converts to 2026-11-08. An ambiguous Berlin wall time resolves to the earlier instant, and a spring-forward wall time that does not exist resolves with the offset in force before the gap; both choices map to the same UTC day. A value whose date prefix parses but whose time or offset does not returns that date prefix unchanged, and a value whose date prefix does not parse returns `None`, so the `None` set stays exactly what the old parser returned and every harvest skip, warning, and fallback keeps its current trigger.
2. Add `chrono-tz = "0.9"` to the `[dependencies]` block in `task-viewer/Cargo.toml`, the version `Cargo.lock` already resolves through `tera`.
3. Bump `CONTENT_HASH_REVISION` in `task-viewer/src/harvest.rs` from `r2` to `r3` and rewrite its doc comment, so a row stored under the old date reader re-walks its leave day once and stores the UTC day.
4. Change `today` in `task-viewer/src/dates.rs` to `Utc::now().date_naive()` and change `now_stamp` in `task-viewer/src/db.rs` to `Utc::now().format("%Y-%m-%dT%H:%M:%SZ")`, so every displayed repository stamp carries its zone designator. A stored `scanned_at` may still carry the old `%Y-%m-%d %H:%M` form, so `scan_label` in `task-viewer/src/web.rs` compares stamps as instants — a naive stamp as `Europe/Berlin`, a `Z` stamp as UTC — falls back to its current string maximum when no stamp parses, and keeps displaying the stored text, so the label names the latest scan.
5. Rewrite the **Extraction Rules** passages in `ARCHITECTURE.md` that call a stored date a day from a timestamp so they name the UTC day, and replace the "local Berlin wall time" sentence with the conversion rule. Add one sentence to `FEATURES.md` **4. Charts** recording that each chart day is a UTC calendar day and a naive timestamp is read as `Europe/Berlin`, and one to **3. Repositories** that the Added and scanned stamps are UTC. Extend the date-parsing bullet under **What Must Be Covered** in `TESTING.md` with the UTC conversion and the Berlin default.
6. Add the tests the Acceptance names: the `dates` conversion table, the two DST boundary wall times, a git fixture whose commit crosses midnight in Berlin, a folder outside git with one naive and one `Z` `created` value, the `r2` prior-row re-walk, the preserved skip and fallback, and the source-shape check on the viewer's clock, and the `scan_label` mixed-format comparison.

**Out of scope:**

- Rewriting existing task files, commits, or stored database rows to UTC. The viewer converts at read time and leaves every source record as it was written.
- A per-viewer timezone setting or browser-local chart days. UTC is the fixed chart time base.
- Sub-day precision in the charts or the database. Rows stay date-granular.

## Acceptance

- A `dates` unit test parses `2026-11-09T00:30:00+01:00`, `2026-11-09T00:30:00+0100`, and `2026-11-09T00:30:00.500+01:00` to 2026-11-08; `2026-11-09T00:30:00Z` to 2026-11-09; `2026-11-09 00:30:00` to 2026-11-08; `2026-07-01T00:30:00` to 2026-06-30; and `2026-11-08` to 2026-11-08, and keeps `2026-05-01 10:00:00` and `2026-07-17T12:00:00+00:00` on their current days.
- A `dates` unit test parses `2026-11-09T00:30` to 2026-11-08 and `2026-11-09T10:00` to 2026-11-09, and parses `2026-11-09Tgarbage` and `2026-11-09T` to 2026-11-09, proving a minute-precision wall time converts and a value whose time does not parse keeps its date prefix.
- A `dates` unit test parses `2026-03-29T02:30:00` to 2026-03-29 and `2026-10-25T02:30:00` to 2026-10-25 without panicking, proving the spring-forward gap and the fall-back ambiguity each resolve to a day.
- The existing `dates_parse_frontmatter_and_commit_timestamps` and `dates_parse_a_space_separated_timestamp` tests pass unchanged.
- A harvest test commits an archived `finished` task with `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` set to `2026-11-09T00:30:00+01:00` and asserts the parsed row's `ship_date` and `leave_date` are 2026-11-08.
- A harvest test harvests a folder outside git holding one task with `created: 2026-11-09T00:30:00` and one with `created: 2026-11-09T00:30:00Z`, and asserts the rows' `created` days are 2026-11-08 and 2026-11-09.
- A harvest test charts a task whose `created` is unparseable as a skipped file and a task whose `updated` is unparseable with `updated` equal to `created`, so the existing skip and fallback paths keep their trigger.
- A harvest test stores a prior row whose `content_hash` is `r2:` plus the file digest, harvests the unchanged archived file, and finds one leave walk and the stored hash prefixed `r3:`, in the shape of `harvest_rewalks_a_prior_row_whose_hash_predates_the_comment_reader`.
- A source-shape test asserts the `today` body in `task-viewer/src/dates.rs` contains `Utc::now().date_naive()` and no `Local::now()`, and the `now_stamp` body in `task-viewer/src/db.rs` contains `Utc::now()` with a format string ending in `Z` and no `Local::now()`.
- A `db` unit test parses `now_stamp()` with `DateTime::parse_from_rfc3339` and finds it ends with `Z`.
- A `web` unit test builds one repository with `scanned_at` `2026-11-09 00:30` and another with `2026-11-08T23:45Z` and asserts `scan_label` names the `2026-11-08T23:45Z` scan, and builds two repositories whose `scanned_at` values are `pending` and `unknown` and asserts `scan_label` names `unknown`, so a naive stamp is read as `Europe/Berlin`, a `Z` stamp as UTC, and the string maximum still covers an unparseable stamp.
- `ARCHITECTURE.md` contains no `local Berlin wall time until timezone conversion exists`, and its `created`, leave-day, and mean-age passages name the UTC day. `FEATURES.md` **4. Charts** and **3. Repositories** and `TESTING.md` **What Must Be Covered** name the UTC day, the UTC stamps, and the `Europe/Berlin` default.
- `task-viewer/Cargo.toml` declares `chrono-tz`, and `cargo metadata` resolves exactly one `chrono-tz` version.
