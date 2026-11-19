---
description: "Align CHARTER, ARCHITECTURE, FEATURES, TESTING, and README with the code: ref scope, Add's write path, toolchain, layout, worker cap, and drifted comments."
scope: .
created: 2026-11-13T13:25:45Z
updated: 2026-11-19T13:20:00Z
status: ready
reported-by: Andreas Hoffmann
---

# Bring the guardrail documents back in line with the code

## Goal

Every guardrail claim that the code contradicts is rewritten to the behavior the code implements, and every behavior the docs promise but omit is documented. After this task, `CHARTER.md`, `ARCHITECTURE.md`, `FEATURES.md`, `TESTING.md`, and `README.md` agree with the shipped code on ref scope, the write paths, the pinned toolchain, the database layout, the worker cap, and the git-failure rule, and the docs carry the missing-`tasks/` and failed-enclosing-probe outcomes the harvest work establishes.

## Context

- Build order: last in the harvest cluster. Ships after `task-viewer_harvest-skip-visibility.md`, `task-viewer_harvest-git-failure-cases.md`, `task-viewer_harvest-raw-path-gaps.md`, and `task-viewer_launcher-build-truth.md`, each of which establishes behaviour this task documents. It blocks no live task.
- `CHARTER.md` says the viewer charts tasks from "any of its refs", calls Update the only refresh path, dates a deferred task from "the commit that first introduced its archived status", and limits the toolchain to Rust, Make, and `Cargo.lock`. That toolchain line is the only toolchain statement in any guardrail doc.
- `ARCHITECTURE.md` omits `created_at` from the repository layout, contradicts itself about `~/.task-viewer` — the System Overview calls it "The SQLite file `~/.task-viewer`" while the Components section and the Data Layout block present it as a directory holding `repositories` and `tasks` — claims completed scan results never accumulate, documents a git fallback, and names no `TASK_VIEWER_SCAN_WORKERS` range.
- `db::database_path` settles that contradiction: it returns `~/.task-viewer` as the database file itself, and `create_parent_dir` creates only that path's parent. The file reading is the correct one, so the directory reading in the Components section and in the Data Layout block is what changes.
- `FEATURES.md` promises `Z` stamps that legacy rows lack and omits the worker range; `README.md` omits it too. The repositories template prints `project.created_at` and `project.scanned_at` as stored, and `db::now_stamp` writes `Z` only for the rows this build inserts, so a row stored by an older build keeps its own spelling.
- `TESTING.md` has no place for `task-viewer/src/launcher_tests.rs`.
- Three comments in `task-viewer/src/harvest.rs` describe behavior the code no longer has: the doc comment on `type Candidate` ("Every task file the working tree or any ref holds, one row per task"), the comment in `harvest_repository_with_prior` above `GitContext::resolve` ("that the working tree or any ref holds"), and the arm comment in `fn git_pickaxe` ("No `.git` and no enclosing repository: git has no commit to offer"), which also serves a resolved enclosing repository because `git.dir` is `None` there.
- The git-failure rules depend on [task-viewer_harvest-git-failure-cases.md](archive/task-viewer_harvest-git-failure-cases.md), which shipped the enclosing-repository arms that propagate an ordinary failure and the empty worktree walk a resolved repository with no working-tree `tasks/` takes. The shipped tree now returns that failure from `first_archived_commit_date` and from `git_pickaxe`'s `git.dir: None` arm, while `ARCHITECTURE.md`'s git-fallback paragraph still describes the old fallback.
- The failed-enclosing-probe rules depend on [task-viewer_harvest-skip-visibility.md](archive/task-viewer_harvest-skip-visibility.md), which makes an enclosing-repository probe that fails for a reason other than git's not-a-repository answer fail the scan with git's first stderr line. This task lands after that one as well.
- The Data Layout re-walk rule and [task-viewer_harvest-raw-path-gaps.md](archive/task-viewer_harvest-raw-path-gaps.md) no longer disagree: that work amended `ARCHITECTURE.md`'s re-walk rule so a fallback day on a lossy path is reused until the file's bytes change, so this task owes no re-walk amendment.
- The pinned toolchain belongs to [task-viewer_launcher-build-truth.md](archive/task-viewer_launcher-build-truth.md), which pins `channel` in `rust-toolchain.toml` and adds the matching `rust-version` in `Cargo.toml` without touching a guardrail doc, so this task states the pin and lands after it.
- `FEATURES.md` reports a scan error for a git history it cannot read but never names the enclosing-repository probe: a `rev-parse` that fails for a reason other than git's not-a-repository answer fails the scan with git's first stderr line in the message.
- Resolved by the user's standing instruction to follow the audit recommendation for the three charter-bound forks: narrow the ref promise to the listed branches (matching `ARCHITECTURE.md` and the deliberate `refs/heads` + `refs/remotes` read), name `bash` and `python3` as launcher tooling rather than rewriting the scripts, and keep the deferred-date rule as the first archived-path commit. The harvest-core and ops-docs rulings settle the rest: the git-fallback rule lands here for the finished/deferred archive walk and for the pickaxe lookup, the missing-`tasks/` and failed-enclosing-probe rules land here, and the pinned toolchain lands here.

## Approach

- In `CHARTER.md`: narrow "any of its refs" to the branches the code lists; add Add to the refresh paths; reword the deferred-date rule to the first archived-path commit; name `bash` and `python3` as launcher tooling; state the pinned toolchain channel and its matching `rust-version`.
- In `ARCHITECTURE.md`: add `created_at` to the layout; make `~/.task-viewer` a SQLite file throughout — rewrite the Components sentence so `db.rs` creates the database file's parent directory, and rewrite the Data Layout block so `~/.task-viewer` is the file and `repositories` and `tasks` are its tables; replace "never accumulate" with "bounded at the worker cap"; document the 1–32 worker range, its default of 4, the clamp above 32, and the warning below 1 or on a non-numeric value; and rewrite the git-fallback paragraph so a failed leave-day lookup inside a resolved enclosing repository fails the scan for the finished/deferred archive walk `first_archived_commit_date` performs and for the pickaxe lookup `git_pickaxe` runs for an archived task whose status is neither `finished` nor `deferred`.
- In `FEATURES.md` and `README.md`: document the worker range; in `FEATURES.md`, correct the `Z`-stamp claim so it says the `Z` designator applies to the stamps this build writes and that a row stored by an older build keeps the spelling it was stored with, and state that a failed enclosing-repository probe fails the scan with git's first stderr line.
- In `TESTING.md`: add a place for `src/launcher_tests.rs`, and correct the "A harvest reads each worktree task file once" sentence, which the harvest cache budget makes false for a tree over the budget, so it states that a tree larger than the budget reads some files twice.
- Correct the three drifted `harvest.rs` comments named in the Context to the code's actual behavior, so the `git_pickaxe` arm comment names both cases it serves.
- Leave the ref-scope code unchanged; the charter narrowing makes the tag omission intended behavior.
- **Out of scope:** changing the `git_pickaxe` `git.dir: None` arm to propagate an ordinary failure; the harvest git-failure task owns that code.

## Acceptance

- A grep for "any local or remote-tracking ref" in `CHARTER.md` finds the narrowed branch wording, and a grep for "any of its refs" finds nothing.
- `CHARTER.md` names Add among the write paths and names `bash` and `python3` as launcher tooling.
- `CHARTER.md` dates a deferred task's leave day from the first archived-path commit and no longer from "the commit that first introduced its archived status".
- `CHARTER.md` names the pinned toolchain channel and the `rust-version` that `task-viewer/Cargo.toml` declares, and both name the same version.
- `ARCHITECTURE.md` lists `created_at` and states the worker cap bound, and its Components section and Data Layout block describe `~/.task-viewer` as the SQLite file whose parent directory `db.rs` creates rather than as a directory holding the two tables.
- `ARCHITECTURE.md`'s git-fallback paragraph states that a failed leave-day lookup inside a resolved enclosing repository fails the scan for the finished/deferred archive walk `first_archived_commit_date` performs and for the pickaxe lookup `git_pickaxe` runs for an archived task whose status is neither `finished` nor `deferred`, and matches the harvest task this one ships after.
- `ARCHITECTURE.md`, `FEATURES.md`, and `README.md` each state the `TASK_VIEWER_SCAN_WORKERS` range of 1 to 32 and its default of 4.
- `FEATURES.md` states that the `Z` designator applies to the stamps this build writes and that a row stored by an older build keeps its stored spelling, and it no longer claims every Added and scanned stamp carries `Z`.
- `FEATURES.md` states that an enclosing-repository probe that fails for a reason other than git's not-a-repository answer fails the scan with git's first stderr line in the message.
- `TESTING.md` names `src/launcher_tests.rs`, and its "A harvest reads each worktree task file once" sentence no longer claims a single read for a tree over the cache budget.
- Each of the three `harvest.rs` comments named in the Context matches the code it sits above.
