---
description: "Detect a shallow repository and fail its scan with a message naming the repository, instead of silently dating archived tasks from the shallow boundary commit."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T07:44:40Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Fail a shallow repository's scan instead of guessing ship days

## Goal

A repository whose git history is shallow fails its scan with a fixed message naming the repository and telling the user to unshallow it. It keeps its prior rows and never stores a wrong ship day from a shallow scan.

## Context

The archive walk (`archived_leave_lookup`, whose flags `archive_log_args` builds) runs `git log --all` (`task-viewer/src/harvest.rs`), which sees only the commits a shallow clone fetched. A depth-1 clone grafts its shallow boundary commit as a root, and `log.showRoot=true` makes the archive walk list every path in that commit's tree, so an archived task that still exists at the boundary is dated from the boundary commit's day, not from its real archive commit. Only a task absent at the boundary finds no record, returns `None`, and falls back to the frontmatter `updated` day through `dates_for`.

Nothing in `harvest.rs` probes `git rev-parse --is-shallow-repository`, and the `updated` fallback is not counted or logged for a datable path.

The `HarvestError::Git` comment states the consequence the fallback exists to avoid: charting frontmatter dates instead would pile every task onto its last bulk `updated` day. Failing every shallow repository also stops a depth-1 clone that holds only live tasks from charting until it is unshallowed, even though live rows carry no ship day and chart correctly today; that is the accepted cost of refusing to chart a possibly wrong ship day.

This refusal is compatible with `CHARTER.md`'s fallback rule, whose invariant reads "A shipped task is an archived task whose status is `finished`. Its ship day is the commit that first archived it as `finished` ... or the frontmatter `updated` day when git has no such commit or when the task's path cannot be named in a git command". The fallback covers a repository whose history genuinely holds no archive commit for a task; a shallow repository is known to hold an incomplete history, so the boundary commit is not evidence that no archive commit exists. Refusing the scan reports the missing history instead of turning the boundary commit into a guessed ship day.

## Approach

1. In the `Some((root, dir))` arm of the existing `match git.repository()` in `harvest`, once per repository after `GitContext::resolve`, run the bounded `git rev-parse --is-shallow-repository` through the existing read-only runner. Place the probe outside the existing `if git.dir.is_some()` guard, so a folder without its own `.git` still detects the enclosing repository's shallow history. A folder outside git takes the `None` arm and keeps its file-only path, so the probe never runs where there is no git dir.
2. When the trimmed probe output equals `true`, return a new `HarvestError::Shallow` variant whose `Display` is a fixed sentence that puts the fix (`git fetch --unshallow`) first and names the configured folder by its basename, so the enclosing-subfolder case names the subfolder and the 200-character cut in `git_scan_message` (`task-viewer/src/web.rs`) cannot drop the fix from the stored message. In `store_outcome`, handle the variant like the `Git` arm, storing that sentence as the scan error and keeping the prior rows. A probe that cannot run also fails the scan, as the adjacent `rev-parse --git-dir` probe does, so a failed probe is never read as "not shallow".
3. On the `Shallow` arm, also clear the repository's stored `history_key` through a small `db.rs` helper while leaving its rows and the scan error in place, so a leave day stored by an earlier build from a shallow boundary is not reused once the repository is unshallowed; the next full-history scan re-walks and stores the real archive days. The `Git` arm clears the key too, so a failed probe, which maps to `Git`, cannot leave a cached day from a history the scan could not verify.
4. Keep a full-history repository and a folder outside git on their current paths.
5. Add harvest tests that clone a local repository with `--depth 1` (a `file://` clone, offline) and assert the error names the repository and begins with `git fetch --unshallow`; add a full clone beside it that harvests normally. Add a harvest test that configures a `tasks/`-holding subfolder with no `.git` inside a `--depth 1` clone and asserts the same `Shallow` failure names the subfolder, proving the probe sits outside the `if git.dir.is_some()` guard. Add a `web.rs` test that drives a `HarvestError::Shallow` outcome through `store_outcome` over seeded prior rows and asserts the rows stay, the stored `last_error` carries the message, and the stored `history_key` is cleared; then a rescan after `git fetch --unshallow` charts the real archive day. Add a `web.rs` `git_scan_message` case with a long basename that proves the 200-character cut keeps `git fetch --unshallow`.
6. Record the rule in `ARCHITECTURE.md`'s `## Extraction Rules` section, `TESTING.md`'s `Harvest:` bullet, and `FEATURES.md`'s bullet that begins "Update scans every configured repository", naming a shallow repository's failed scan and its kept rows.

**Out of scope:** harvesting the shallow boundary's partial history. **Out of scope:** warning instead of failing, because a warning would still chart wrong ship days, which is the failure this task removes.

## Acceptance

- A shallow repository fails its scan with a message naming the repository and the unshallow command, and a configured subfolder with no `.git` inside a shallow clone fails the same way naming the subfolder.
- A probe that cannot run fails the scan instead of reading as "not shallow".
- Its previously stored rows stay, the scan error is stored and shown, and the shallow failure clears the stored `history_key`; after `git fetch --unshallow` a rescan charts the task's real archive day instead of the boundary day.
- A full-history repository and a folder outside git behave as before.
- `ARCHITECTURE.md`'s extraction rules, `TESTING.md`'s `Harvest:` bullet, and `FEATURES.md`'s bullet that begins "Update scans every configured repository" each record a shallow repository's failed scan and its kept rows.
- The new harvest and web tests pass.
