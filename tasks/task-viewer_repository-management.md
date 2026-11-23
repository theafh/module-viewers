---
description: "Improve repository management: per-repository row counts, a relocate action, duplicate-clone detection, a filtered-dashboard link, human timestamps, and path wrapping."
scope: task-viewer
created: 2026-11-23T17:31:20Z
updated: 2026-11-23T18:08:45Z
status: ready
reported-by: Andreas Hoffmann
---

# Repository management and readability

## Goal

Make the Repositories page useful with several repositories and recoverable when one moves. Each row shows how many rows it contributes, links to its filtered dashboard, offers a relocate action that keeps its rows, and warns when a second clone of the same repository is added. Timestamps read as human times and long paths wrap instead of widening the page.

## Context

- `task-viewer/templates/repositories.html` renders each row as a flex `li` whose inner `div` stacks the name as plain text, the path span, the Added and scanned stamps, and any error, beside one Remove form.
- `task-viewer/src/web.rs` `add_repository` picks a folder, canonicalizes it, and probes `tasks/` (working tree or ref union) before `store_repository` refuses an exact stored path through `db::path_exists`, names the row with `db::unique_name`, inserts it, and harvests it through `refresh_one`. A second clone of one repository has a different path and is accepted.
- `task-viewer/src/schema.sql` gives `repositories.path` a UNIQUE constraint and `tasks.kind` the values `live`, `finished`, and `deferred`, so a grouped count and a path update need no migration.
- `task-viewer/src/db.rs` `list_repositories` orders by name and carries no path-update statement.
- `task-viewer/src/web.rs` `scan_label` formats a raw stamp with `format!("Last scan {stamp}")`, and `scan_instant` is the one reader that accepts RFC 3339, a `Z` stamp without seconds, a numeric offset, and a legacy Berlin wall time; `repositories.html` prints the stored stamps raw.
- `task-viewer/src/web.rs` `web_scan_label_compares_stamps_as_instants` asserts `Last scan 2026-11-08T23:45Z` and `Last scan unknown`, and `web_repositories_page_renders_the_scan_label_once` asserts `scanned {stamp}`.
- `task-viewer/static/style.css` `.repo-list div` is a flex column with no `min-width: 0`; `min-width: 0` appears only on `.chart-card`, and the path span carries only `muted`.
- `task-viewer/src/harvest.rs` `run_git_command` is the bounded, environment-scrubbed git runner, and `harvest_repository_with_prior` canonicalizes the stored path, so a vanished folder reaches `store_outcome` as `HarvestError::Io` and stores the fixed `IO_ERROR` text.
- `CHARTER.md` (key invariants) and `SECURITY.md` (trust boundaries) both enumerate Update and Add as the only paths that replace stored rows.

## Approach

1. Add per-repository counts by extending the read that feeds the Repositories page with one grouped select over `tasks` (a total and a count where `kind = 'live'`) joined to each repository, and render both numbers beside the stamps on a page-specific row so the dashboard's `project_view` keeps its shape. A repository with no `scanned_at` keeps `not scanned yet` in place of a pair of zeros.
2. Add a relocate route `POST /repositories/{id}/path` that opens the folder picker, canonicalizes the pick, and probes `tasks/` exactly as Add does (working tree or ref union), refuses a path another repository already stores through `db::path_exists`, writes the new path for that id with a new `db` update statement (the UNIQUE column backs the refusal), and re-harvests only that repository through `refresh_one` under the scan lock, so its rows survive and read the new folder.
3. Detect a duplicate clone on Add by identifying each configured repository with its canonical origin URL (`git config --get remote.origin.url`, trimmed, one trailing `.git` and `/` dropped) and, for a repository with no remote, with its git common directory (`git rev-parse --git-common-dir`, canonicalized), which is what two worktrees of one repository share; read both through `harvest.rs`'s `run_git_command` so the existing deadline and environment scrubbing apply. On a match, keep the already-validated canonical path and the existing repository's name in a bounded one-shot slot on `AppState` (the `FlashStore` capacity and eviction, named by a canonical decimal token), redirect to `/repositories` with only that token in the query, and render the warning and a Confirm form from the slot, so no free text rides the query and the browser body never carries a path. The page peeks the slot and the confirm POST takes it under the scan lock, storing the held path the way Add stores a picked one. The exact-path refusal stays.
4. Link each repository name to `/` with `?project=<id>`, which `selected_id` already resolves.
5. Format the stored stamps in the view model that feeds the templates (`project_view` for the rows, `scan_label` for the header), leaving the stored columns untouched: render the instant in the machine's local zone and append `N days ago` when the UTC day difference is at least one. A helper takes the reference instant and the zone as parameters, so a test fixes both instead of depending on the clock and the machine's zone. Parse with `scan_instant`'s rules so a legacy Berlin wall time and a `Z` stamp without seconds still format, and fall back to the raw text when a stamp does not parse. Replace the raw-stamp assertions in the two tests named in Context.
6. Give `.repo-list div` `min-width: 0`, give the path span its own `repo-path` class, and set `overflow-wrap: anywhere` on that class; group the row's action forms so Relocate and Remove sit in one action column without stretching the row.

**Settled decision:** relocate keeps its immediate re-harvest, and the guardrail documents are amended to name it. `CHARTER.md`'s invariant and `SECURITY.md`'s data rule are rewritten so one canonical sentence names every refresh path, covering both the relocate refresh and the per-repository rescan that [task-viewer_scan-feedback.md](task-viewer_scan-feedback.md) adds; whichever task lands second leaves that sentence true. The relocated row carries a fresh scan stamp.

**Out of scope:**

- Renaming a repository's display name.
- Reordering repositories.
- Re-harvesting any repository other than the one being relocated.

## Acceptance

- [ ] Each repository row shows its total and live row counts from one grouped select; a database test seeds live, finished, and deferred rows and asserts both numbers, and a repository with no scan stamp still renders `not scanned yet`.
- [ ] Relocating a repository writes the new path and keeps its rows, and the relocated row carries a fresh scan stamp after the immediate re-harvest; a route test asserts the stored path, that every row survives, and that `scanned_at` advanced.
- [ ] A relocate to a path another repository already stores is refused and leaves both repositories unchanged; a test asserts it.
- [ ] Adding a second clone of a configured repository reports a warning naming the existing repository and stores nothing until the confirm token is posted; a test builds two clones of one repository and asserts the banner, the unchanged repository list before the confirm, and the stored repository after it. A second worktree of a configured repository, which shares its git common directory, is detected the same way.
- [ ] A repository name links to its filtered dashboard; a template test asserts the anchor carries `?project=<id>`.
- [ ] Stamps render as human local times with a relative `N days ago` past one day, not raw ISO strings; a template test fixes the reference instant and zone and asserts the text, and the two raw-stamp assertions named in Context are gone.
- [ ] A long repository path wraps at a narrow width; a style test asserts `min-width: 0` on `.repo-list div` and `overflow-wrap: anywhere` on the path class.
- [ ] `FEATURES.md` and `ARCHITECTURE.md` record the relocate route and the duplicate-clone warning.
- [ ] `make test` and `make lint` pass.
