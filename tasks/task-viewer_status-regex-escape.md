---
description: "Escape the frontmatter status before it reaches a git -G regex so unusual statuses cannot fail the whole repository scan."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T23:23:40
status: ready
reported-by: "Andreas Hoffmann"
---

# Escape the status before it reaches a git regex

## Goal

A frontmatter status value can contain regex metacharacters without failing the repository scan. A tight `status:[` or `status:deferred[` currently makes the `-G` lookup die with `fatal: invalid regex` after `-S` misses, and the whole repository keeps its old rows.

## Context

`first_status_commit_date` in `task-viewer/src/harvest.rs` builds `^status:[[:space:]]*{status}` and passes it to `git log -G`. I reproduced `git log -G '^status:[[:space:]]*['` failing with `fatal: invalid regex: brackets ([ ]) not balanced`. `classify` treats any archived status other than `finished` as deferred, so arbitrary values reach this code.

## Approach

Escape the status for POSIX extended regex before formatting the `-G` pattern. Keep the `-S` pass first. Add a small helper that backslash-escapes every POSIX ERE metacharacter — `\ . [ ] ( ) * + ? { } | ^ $` — and a unit test that covers that full set.

**Out of scope:** Rejecting unknown statuses at harvest time; the classification rule stays as it is.

## Acceptance

- A unit test asserts the escaped pattern matches a literal line for each POSIX ERE metacharacter the helper escapes — `\ . [ ] ( ) * + ? { } | ^ $` — and does not match an unrelated status.
- A harvest test archives a task with a tight `status:[` and asserts harvest succeeds and the task is deferred with the archive day.
- A harvest test archives a task with `status:deferred[` and asserts no `HarvestError` and the expected leave day.
- A harvest test hand-writes a task file under `tasks/archive/` with the tight frontmatter line `status:x{1`, an `updated` day earlier than the archive commit, and commits that file as the archive commit, so the `-S` literal `status: x{1` misses; it asserts harvest succeeds and the task is deferred with that archive commit's day as its leave day.
