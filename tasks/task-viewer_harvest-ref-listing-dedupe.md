---
description: "Dedupe the ref union by path and blob so a branch-per-task repository copies each existing task's path once, not once per branch; each distinct tasks/ subtree still pays one recursive listing."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Dedupe the ref union by path and blob

## Goal

The ref union holds each distinct `(path, blob)` once, so a repository with many branches that each touched `tasks/` does not grow its listing and resident memory with branches × tasks. The newest-version rule still picks the right version.

## Context

`ref_task_files` in `task-viewer/src/harvest.rs` dedupes refs by commit id and by `tasks` subtree id; `union_files` and `holds_tasks` call it, and `union_files` builds candidates from what it returns. A branch that adds or edits one task file produces a distinct subtree, so every such branch pays a full recursive `ls-tree` of the whole `tasks/` tree, and every listed path is appended to the union and then copied into the candidate set. The union's size and the candidate copies therefore scale with task-touching branches × task files, which is how a repository that keeps one branch per task is used. Each distinct `tasks/` subtree still pays one recursive `ls-tree`, because the pairs are known only after that listing.

## Approach

1. Dedupe the union by `(path, blob id)` as it accumulates, keeping one occurrence, since identical path+blob pairs rank identically, through a seen-set beside the union (a `HashSet<(String, String)>` that filters inserts and is drained into the Vec), preserving first-seen order, because the candidate set's later stable sort and tie-break depend on that order; the deduped union costs one constant factor of itself.
2. Keep the per-commit version ordering that lets the working tree, an archived path, and the newest commit win in that order; dedupe only identical path+blob pairs, never different versions of one path.
3. Skip a ref whose subtree yields no new `(path, blob)` pair.
4. Add a harvest test and name its seam: add `ref_task_files` to the inline test import list in `task-viewer/src/harvest.rs` and assert the length of its returned Vec, or add a `#[cfg(test)]` union-count counter beside `ls_tree_count`. The test covers a repository with many branches each touching one task, whose union is bounded by the distinct path+blob pairs; a task with two different versions still charts the version the current rule chooses.
5. Record the rule in `ARCHITECTURE.md`'s extraction rules and `TESTING.md`'s harvest ref bullet.

**Out of scope:** changing which version wins, and the working-tree walk.

## Acceptance

- Identical path+blob pairs appear once in the union, asserted through the `ref_task_files` seam the Approach names.
- A task with two different versions still charts the version the current rule chooses.
- A branch that adds one task adds only its own pair to the union, not a copy of every existing task's pair, checked as a union length on the fixture.
- The new harvest test, `make lint`, and `make test` pass.
