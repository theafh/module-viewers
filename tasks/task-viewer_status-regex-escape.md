---
description: "Escape the frontmatter status before it reaches a git -G regex so unusual statuses cannot fail the whole repository scan."
scope: task-viewer
created: 2026-11-05T07:00:45
updated: 2026-11-05T07:00:45
status: open
reported-by: "Andreas Hoffmann"
---

# Escape the status before it reaches a git regex

## Goal

A frontmatter status value can contain regex metacharacters without failing the repository scan. A tight `status:[` or `status:deferred[` currently makes the `-G` lookup die with `fatal: invalid regex` after `-S` misses, and the whole repository keeps its old rows.

## Context

`first_status_commit_date` in `task-viewer/src/harvest.rs` builds `^status:[[:space:]]*{status}` and passes it to `git log -G`. I reproduced `git log -G '^status:[[:space:]]*['` failing with `fatal: invalid regex: brackets ([ ]) not balanced`. `classify` treats any archived status other than `finished` as deferred, so arbitrary values reach this code.

## Approach

Escape the status for POSIX extended regex before formatting the `-G` pattern, or restrict the regex to a fixed alternation of known statuses and use the literal `-S` pass for others. Keep the `-S` pass first. Add a small helper with a unit test covering `[`, `(`, `.`, `*`, and a backslash.

**Out of scope:** Rejecting unknown statuses at harvest time; the classification rule stays as it is.

## Acceptance

- A unit test asserts the escaped pattern matches a literal `status:[` line and does not match an unrelated status.
- A harvest test archives a task with a tight `status:[` and asserts harvest succeeds and the task is deferred with the archive day.
- A harvest test archives a task with `status:deferred[` and asserts no `HarvestError` and the expected leave day.
