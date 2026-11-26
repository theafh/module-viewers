---
description: "Detect a shallow repository and fail its scan with a message naming the repository, instead of silently dating archived tasks from the shallow boundary commit."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T19:06:35Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Fail a shallow repository's scan instead of guessing ship days

## Goal

A repository whose git history is shallow fails its scan with a fixed message naming the repository and telling the user to unshallow it. It keeps its prior rows and never charts a wrong ship day.

## Context

The archive walk (`archived_leave_lookup`, whose flags `archive_log_args` builds) runs `git log --all` (`task-viewer/src/harvest.rs`), which sees only the commits a shallow clone fetched. A depth-1 clone grafts its shallow boundary commit as a root, and `log.showRoot=true` makes the archive walk list every path in that commit's tree, so an archived task that still exists at the boundary is dated from the boundary commit's day, not from its real archive commit. Only a task absent at the boundary finds no record, returns `None`, and falls back to the frontmatter `updated` day through `dates_for`.

Nothing in `harvest.rs` probes `git rev-parse --is-shallow-repository`, and the `updated` fallback is not counted or logged for a datable path.

The `HarvestError::Git` comment states the consequence the fallback exists to avoid: charting frontmatter dates instead would pile every task onto its last bulk `updated` day. CI clones with `--depth 1` are the common case. Failing every shallow repository also stops a depth-1 clone that holds only live tasks from charting until it is unshallowed, even though live rows carry no ship day and chart correctly today; that is the accepted cost of refusing to chart a possibly wrong ship day.

This refusal is compatible with `CHARTER.md`'s fallback rule. The fallback covers a repository whose history genuinely holds no archive commit for a task; a shallow repository is known to hold an incomplete history, so the boundary commit is not evidence that no archive commit exists. Refusing the scan reports the missing history instead of turning the boundary commit into a guessed ship day.

## Approach

1. In the `Some((root, dir))` arm of the existing `match git.repository()` in `harvest`, once per repository after `GitContext::resolve`, run the bounded `git rev-parse --is-shallow-repository` through the existing read-only runner. A folder outside git takes the `None` arm and keeps its file-only path, so the probe never runs where there is no git dir.
2. When the probe prints `true`, return a `HarvestError::Git` with a fixed sentence that names the repository and puts the fix (`git fetch --unshallow`) first, so the 200-character cut in `git_scan_message` (`task-viewer/src/web.rs`) cannot drop the fix from the stored message. The scan stores a scan error and keeps the prior rows.
3. Keep a full-history repository and a folder outside git on their current paths.
4. Add a harvest test that clones a local repository with `--depth 1` (a `file://` clone, offline) and asserts the error names the repository and the unshallow command; add a full clone beside it that harvests normally.
5. Record the rule in `ARCHITECTURE.md`'s extraction rules, `TESTING.md`'s harvest bullet, and `FEATURES.md`'s Update bullet, naming a shallow repository's failed scan and its kept rows.

**Out of scope:** harvesting the shallow boundary's partial history, and warning instead of failing. A warning would still chart wrong ship days, which is the failure this task removes.

## Acceptance

- A shallow repository fails its scan with a message naming the repository and the unshallow command.
- Its previously stored rows stay, and the scan error is stored and shown.
- A full-history repository and a folder outside git behave as before.
- The new harvest tests, `make lint`, and `make test` pass.
