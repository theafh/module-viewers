---
description: "Align CHARTER, ARCHITECTURE, FEATURES, and TESTING with the code: ref scope, Add's write path, toolchain, layout, worker cap, and drifted comments."
scope: .
created: 2026-11-13T13:25:45Z
updated: 2026-11-13T17:30:00Z
status: open
reported-by: Andreas Hoffmann
---

# Bring the guardrail documents back in line with the code

## Goal

Every guardrail claim that the code contradicts is rewritten to the behavior the code implements, and every behavior the docs promise but omit is documented. After this task, `CHARTER.md`, `ARCHITECTURE.md`, `FEATURES.md`, `TESTING.md`, and `README.md` agree with the shipped code on ref scope, the write paths, the toolchain, the database layout, the worker cap, and the git-failure rule.

## Context

- `CHARTER.md` says the viewer charts tasks from "any of its refs", calls Update the only refresh path, dates a deferred task from "the commit that first introduced its archived status", and limits the toolchain to Rust, Make, and `Cargo.lock`.
- `ARCHITECTURE.md` omits `created_at` from the repository layout, contradicts itself about `~/.task-viewer` — the System Overview calls it "The SQLite file `~/.task-viewer`" while the Components section and the Data Layout block present it as a directory holding `repositories` and `tasks` — claims completed scan results never accumulate, documents a git fallback, and names no `TASK_VIEWER_SCAN_WORKERS` range.
- `db::database_path` settles that contradiction: it returns `~/.task-viewer` as the database file itself, and `create_parent_dir` creates only that path's parent. The file reading is the correct one, so the directory reading in the Components section and in the Data Layout block is what changes.
- `FEATURES.md` promises `Z` stamps that legacy rows lack and omits the worker range; `README.md` omits it too. The repositories template prints `project.created_at` and `project.scanned_at` as stored, and `db::now_stamp` writes `Z` only for the rows this build inserts, so a row stored by an older build keeps its own spelling.
- `TESTING.md` has no place for `task-viewer/src/launcher_tests.rs`.
- Three comments in `task-viewer/src/harvest.rs` describe behavior the code no longer has: the doc comment on `type Candidate` ("Every task file the working tree or any ref holds, one row per task"), the comment in `harvest_repository_with_prior` above `GitContext::resolve` ("that the working tree or any ref holds"), and the arm comment in `fn git_pickaxe` ("No `.git` and no enclosing repository: git has no commit to offer"), which also serves a resolved enclosing repository because `git.dir` is `None` there.
- The git-failure sentence depends on [task-viewer_harvest-git-failure-cases.md](task-viewer_harvest-git-failure-cases.md), which changes the enclosing-repository arms to propagate an ordinary failure. In the current tree an ordinary leave-day failure inside a resolved enclosing repository still returns the `updated` fallback — `first_archived_commit_date` keeps `Err(_) => Ok(None)`, and `git_pickaxe` keeps `LogLookup::Failed(_) => Ok(None)` for `git.dir: None` — while only `GitFailure::Timeout` propagates. This task therefore lands after that one instead of describing behaviour the working tree lacks.
- Resolved by the user's standing instruction to follow the audit recommendation for the three charter-bound forks: narrow the ref promise to the listed branches (matching `ARCHITECTURE.md` and the deliberate `refs/heads` + `refs/remotes` read), name `bash` and `python3` as launcher tooling rather than rewriting the scripts, and keep the deferred-date rule as the first archived-path commit.

## Approach

- In `CHARTER.md`: narrow "any of its refs" to the branches the code lists; add Add to the refresh paths; reword the deferred-date rule to the first archived-path commit; name `bash` and `python3` as launcher tooling.
- In `ARCHITECTURE.md`: add `created_at` to the layout; make `~/.task-viewer` a SQLite file throughout — rewrite the Components sentence so `db.rs` creates the database file's parent directory, and rewrite the Data Layout block so `~/.task-viewer` is the file and `repositories` and `tasks` are its tables; replace "never accumulate" with "bounded at the worker cap"; rewrite the git-fallback paragraph so a failed lookup inside an enclosing repository fails the scan, matching the harvest task this one ships after; document the 1–32 worker range, its default of 4, the clamp above 32, and the warning below 1 or on a non-numeric value.
- In `FEATURES.md` and `README.md`: document the worker range; in `FEATURES.md`, correct the `Z`-stamp claim so it says the `Z` designator applies to the stamps this build writes and that a row stored by an older build keeps the spelling it was stored with.
- In `TESTING.md`: add a place for `src/launcher_tests.rs`.
- Correct the three drifted `harvest.rs` comments named in the Context to the code's actual behavior.
- Leave the ref-scope code unchanged; the charter narrowing makes the tag omission intended behavior.

## Acceptance

- A grep for "any local or remote-tracking ref" in `CHARTER.md` finds the narrowed branch wording, and a grep for "any of its refs" finds nothing.
- `CHARTER.md` names Add among the write paths and names `bash` and `python3` as launcher tooling.
- `CHARTER.md` dates a deferred task's leave day from the first archived-path commit and no longer from "the commit that first introduced its archived status".
- `ARCHITECTURE.md` lists `created_at` and states the worker cap bound, and its Components section and Data Layout block describe `~/.task-viewer` as the SQLite file whose parent directory `db.rs` creates rather than as a directory holding the two tables.
- `ARCHITECTURE.md`'s git-fallback paragraph states that a failed leave-day lookup inside a resolved enclosing repository fails the scan, and that sentence matches the arms in `first_archived_commit_date` and `git_pickaxe` once `task-viewer_harvest-git-failure-cases.md` has landed; this task is not finished before it.
- `ARCHITECTURE.md`, `FEATURES.md`, and `README.md` each state the `TASK_VIEWER_SCAN_WORKERS` range of 1 to 32 and its default of 4.
- `FEATURES.md` states that the `Z` designator applies to the stamps this build writes and that a row stored by an older build keeps its stored spelling, and it no longer claims every Added and scanned stamp carries `Z`.
- `TESTING.md` names `src/launcher_tests.rs`.
- Each of the three `harvest.rs` comments named in the Context matches the code it sits above.
- `make lint` passes.
