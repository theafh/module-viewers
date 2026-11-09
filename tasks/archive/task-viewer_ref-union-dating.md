---
description: "Date merge-produced task blobs in the ref union so the newest commit decides, and list each distinct ref commit and blob id once."
scope: task-viewer
created: 2026-11-08T13:13:24Z
updated: 2026-11-08T21:39:31Z
status: finished
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: true
---

# Date merge-produced blobs and list each ref commit once

## Goal

The ref union dates every committed task version from the newest commit that introduced it on any ref, including a version that only a merge commit produced, so the rule "the newest commit decides" in `ARCHITECTURE.md` holds for task files that two branches edited and a merge combined. The union also runs one `ls-tree` per distinct ref commit and keeps one copy of each blob id, so a repository whose refs share commits spends no process and no memory on duplicates. Charted rows stay identical apart from the corrected tie-break.

## Context

`union_files` in `task-viewer/src/harvest.rs` builds the committed half of the ref union. It lists refs with `for-each-ref --format=%(refname) refs/heads refs/remotes`, runs one `ls-tree -r -z <ref> -- <prefix>tasks` per ref, pushes every listed blob id into `all_blobs` once per ref, and hands that vector to `blob_days`, which collapses it into a `HashSet` and streams `git log` through `stream_blob_days` to stamp each blob with the committer time of its newest touch. A candidate whose blob is missing from that stream takes stamp 0 (`times.get(&sha).copied().unwrap_or(0)`).

`blob_days_log_args` builds `--all --format=%ct --raw --no-renames --no-abbrev -- <pathspec>`. Git prints no diff for a merge commit unless asked, so a blob that first exists in a merge commit never reaches the stream. Two everyday cases produce such a blob: a file both branches edited that git auto-merged without a conflict, and a resolved conflict. A throwaway fixture confirmed it: the auto-merged blob matched 0 raw lines with the current arguments and 1 line once `--diff-merges=first-parent` was added. The archive walk (`archive_log_args`) and the pickaxe (`pickaxe_log_args`) already pass that flag, and the **Extraction Rules** in `ARCHITECTURE.md` already say "Merge commits are diffed against their first parent (`--diff-merges=first-parent`), so a task archived while resolving a merge still matches", so this task brings the blob-dating pass in line with the two walks beside it.

The consequence shows on a branch that does not hold the file in its working tree. The ref union then picks among committed versions by archive rank and then by stamp, so an older version on a stale branch (stamp of its own commit) outranks the merged version on `main` (stamp 0). For example, a stale branch's archived `deferred` copy beats the merge's archived `finished` copy, and the task charts as deferred.

Three smaller costs sit on the same path. `refs/remotes/origin/HEAD` resolves to the same commit as its target branch, and a local branch usually sits at the same commit as its upstream, yet each gets its own `ls-tree` and its own copy of every blob id in `all_blobs`. `stream_blob_days` collects `meta.split_whitespace()` into a `Vec<&str>` for every raw line of the whole-history log only to read field 3.

The listing pathspec `{prefix}tasks` reaches `ls-tree` and `git log` raw, while the per-file lookups wrap their path in `literal_pathspec`. A probe with folders named `my[repo]` and `st*ar` beside decoys `myr` and `stXar` showed both commands listing exactly the named folder's files, because git matches that raw prefix literally, so the raw form is correct and only lacks the comment that says so.

## Approach

1. Add `--diff-merges=first-parent` to `blob_days_log_args`, beside `--raw`. A merge commit then reports every blob its first-parent diff introduces, so a merged result gets the merge's committer time, and a side-branch blob that the merge brought onto the first-parent line takes the merge time as its newest touch. Both follow the stamp's existing meaning, "the newest touch", which `blob_days` records with `or_insert` over a newest-first stream. Update the `blob_days_log_args` entry in the `sites` array of `harvest_log_call_sites_pin_output_options_ahead_of_log` to the new argument list.
2. Change the ref listing format to `%(objectname) %(refname)` and skip a ref whose commit id was already listed before running its `ls-tree`. Two refs at one commit list identical `(path, blob, lossy)` triples, so dropping the repeat leaves the candidate set unchanged.
3. Collect the blob ids into a `HashSet<String>` while listing and pass that set to `blob_days`, so the vector of per-ref duplicates is never built.
4. Read the blob id in `stream_blob_days` with `meta.split_whitespace().nth(3)`, and make the same change in the test-only `whole_buffer_blob_days` helper that `stream_blob_days_matches_the_whole_buffer_parser` compares against, so the file-wide `fields.get(3)` grep returns nothing.
5. Put one comment on the `pathspec` binding in `union_files` stating that the raw prefix is matched literally by `ls-tree` and `git log`, so it needs no `:(literal)` magic, unlike the per-file lookups.
6. Rewrite the first **Extraction Rules** bullet in `ARCHITECTURE.md` so its tie-break sentence says the newest commit is read with merges diffed against their first parent, which dates a merged task version from its merge commit. Rewrite the **Harvest ref** bullet in `TESTING.md` so it names the merge case below.

**Out of scope:** The batched archive walk that would replace the per-task `git log --follow`, since it gives up `--follow` across renames inside `archive/` and changes which commits date a task.

## Acceptance

- A harvest test builds a fixture repository by the `TESTING.md` git-test conventions: a first commit adds `tasks/archive/scope_merged.md` with `status: deferred` and a body several lines long; `main` changes the status line to `status: finished`; a branch `stale` from the first commit edits a body line far from the status line, with a committer date after the `main` change; `main` merges `stale` with no conflict; a third branch whose tree lacks the task file is checked out. `harvest_repository` returns one row for that file with kind `finished`. The same test fails on the current argument list, where the row is `deferred`.
- In that fixture, `blob_days` returns a stamp for the merged blob (`git rev-parse main:tasks/archive/scope_merged.md`) equal to the merge commit's committer time.
- `harvest_log_call_sites_pin_output_options_ahead_of_log` passes with `--diff-merges=first-parent` in the `blob_days_log_args` site.
- A harvest test routes a fixture repository holding `main`, a second local branch at the same commit, and `refs/remotes/origin/main` plus a symbolic `refs/remotes/origin/HEAD` at that commit through a logging stand-in git registered with the test-only git override; the log shows exactly one `ls-tree` invocation, and the harvested rows equal those of the same repository read with only `main`.
- `rg -n 'fields.get\(3\)' task-viewer/src/harvest.rs` returns nothing, and `blob_days` takes a set of blob ids rather than a slice.
- The first **Extraction Rules** bullet in `ARCHITECTURE.md` and the **Harvest ref** bullet in `TESTING.md` each state the merge-dating rule once, with no earlier wording left beside it.
