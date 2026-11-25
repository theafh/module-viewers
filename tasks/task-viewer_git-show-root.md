---
description: "Pin log.showRoot so a user or repo setting cannot drop the root commit from a harvest walk and move a ship day to the updated fallback."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T10:22:36Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Pin log.showRoot on every harvest walk

## Goal

A user or repository `log.showRoot=false` setting cannot change a charted ship or leave day. A root commit that archives a task still dates that task, and its blob still reaches the blob-day lookup.

## Context

`git_log_args` in `task-viewer/src/harvest.rs` overrides the `git log` output options a user config could change, but it pins only two:

```rust
"-c".to_string(),
"log.showSignature=false".to_string(),
"-c".to_string(),
"log.decorate=false".to_string(),
```

With `log.showRoot=false`, `git log` omits the root commit's name-only and raw lines. In a one-commit repository whose only commit archives a task, the archive walk prints nothing, so `archived_leave_lookup` returns `None` and the row keeps the frontmatter `updated` day instead of the commit day. The same setting empties the pickaxe walk and drops the blob from the blob-day lookup. The repository's own config and the user's global config both take effect; the inherited-environment scrub does not remove config.

## Approach

1. Add one pinned pair to `git_log_args`, before the `log` subcommand: `-c` and `log.showRoot=true`.
2. Extend the arguments test beside `git_log_args` to assert the new pair, and keep the two existing pins.
3. Add a harvest test whose repository config carries `log.showRoot=false` and whose root commit creates an archived finished task; assert the row's ship day is the commit day, not the frontmatter `updated` day.

**Out of scope:** changing the walks' other arguments, the environment scrub, or the fallback rule for a history git cannot see.

## Acceptance

- `git_log_args` carries `-c log.showRoot=true` before `log`, and its test asserts the pair.
- A one-commit repository with `log.showRoot=false` charts the archived task on the commit's day, not the frontmatter `updated` day.
- The same fixture's blob still reaches the blob-day lookup, so a history walk over it returns the commit day.
- A repository without the setting keeps every current day.
