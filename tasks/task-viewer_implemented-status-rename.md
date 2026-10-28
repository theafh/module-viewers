---
description: "Date a finished task from the commit that first archived it as implemented, so a later bulk rename of implemented to finished stops becoming every such task's ship day."
scope: task-viewer
created: 2026-10-24T18:26:29
updated: 2026-10-24T18:26:29
status: open
reported-by: Andreas Hoffmann
---

# Treat an archived implemented-to-finished change as a status rename

## Goal

Harvest treats `status: implemented` on an archived task file as an earlier spelling of `status: finished`. A finished task's leave day, and with it its ship day, becomes the committer day of the earliest commit in which the file sits under an `archive` directory with either status. When a repository moved its archived tasks from `implemented` to `finished` in one commit, each task now ships on the day it was first archived as implemented, and the rename commit no longer stacks all of them onto one day. A task that was `implemented` while still live in `tasks/` and archived as `finished` later keeps its current dates, because only the archived spelling counts as the rename. That keeps the ship day equal to the leave day, as `ARCHITECTURE.md` states.

## Context

- `fn first_status_commit_date` in `task-viewer/src/harvest.rs` builds the pickaxe from the task's current status alone (`status: {status}` via `-S`, then `^status:[[:space:]]*{status}` via `-G`). `fn run_git_log` runs `git log --follow --diff-merges=first-parent --no-patch --format=%cI` and keeps the earliest date it parses, and `fn git_pickaxe` chooses between the `--git-dir` route and the `-C` route and turns a git failure into `HarvestError::Git`.
- `fn classify` in `harvest.rs` makes an archived file `Finished` only when its status is `finished`, so the rename question arises only for files whose current status is `finished`.
- In one configured repository, a single commit rewrote archived tasks from `status: implemented` to `status: finished`. Because `-S "status: finished"` finds that commit first, every task it touched charts its ship day on that day. Those tasks' histories carry `status: implemented` from the commit that archived them, often weeks earlier.
- `--follow` walks a task file back across its move from `tasks/` to `tasks/archive/`. So an `implemented` hit from the live phase, under the current lifecycle of `implemented`, then `audited`, then `finished`, also shows up in that walk. The file's path in each commit is what tells the two phases apart.
- `CHARTER.md` under `## Key Invariants` defines the ship day in the bullet that begins "A shipped task is an archived task whose status is `finished`." `ARCHITECTURE.md` defines the leave day and ship day in the bullet that begins "The leave day of a finished or deferred task". This task rewrites both, at the user's request.

## Approach

1. In `harvest.rs`, give a finished task a second candidate status. Its leave day is the earliest commit that introduces `status: finished` or `status: implemented` while the file's path in that commit lies under a directory named `archive`. Read that per-commit path from the same `git log --follow` call, for example with `--name-only`, and keep the parser reading committer dates only, so that patch or path lines never become a date. Deferred and live tasks keep their current lookup.
2. Keep the failure handling as it is. A lookup that `git` cannot run still ends in `HarvestError::Git`, and a lookup that runs and finds no qualifying commit still falls back to the frontmatter `updated` day.
3. Rewrite the `CHARTER.md` shipped-task invariant in place: the ship day is the commit that first archived the task as `finished`, or as `implemented` when `implemented` was the archived spelling that a later commit renamed. Rewrite the `ARCHITECTURE.md` leave-day bullet in place to state the same rule and how the archived path is read per commit.
4. Extend the harvest bullet of `TESTING.md` `## What Must Be Covered` with the two cases that Acceptance proves.

**Out of scope:**

- Counting a file that is still archived with `status: implemented` as finished. `fn classify` and the `CHARTER.md` definition of a shipped task (an archived task whose status is `finished`) stay as they are.
- Any other status alias, such as `done` or `shipped`, because no configured repository's history carries one.

## Acceptance

- [ ] A harvest test builds a repository where a task is committed under `tasks/archive/` with `status: implemented` on one day and rewritten to `status: finished` on a later day. The test asserts that `ship_date` and `leave_date` both equal the earlier day.
- [ ] A harvest test builds a repository where a task is set to `status: implemented` while it is still in `tasks/`, then moved to `tasks/archive/` with `status: finished` on a later day. The test asserts that `ship_date` equals the later day.
- [ ] `rg 'Its ship day is the commit that first introduced that status' CHARTER.md` finds nothing, and the rewritten shipped-task bullet names the archived `implemented` rename.
- [ ] The `ARCHITECTURE.md` leave-day bullet states the archived `implemented` rule once, and no other `ARCHITECTURE.md` passage still dates a finished task by `status: finished` alone.
- [ ] The harvest bullet in `TESTING.md` names both cases.
