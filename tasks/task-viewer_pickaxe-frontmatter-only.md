---
description: "Confirm a status pickaxe hit against the commit's frontmatter, so a status: line in a body example cannot date a task."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-27T01:09:25Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Date an unusual archived status from its frontmatter only

## Goal

When the status pickaxe finds a commit for an archived status other than `finished` or `deferred`, that commit counts only if the task's frontmatter at that commit actually carries the status. A `status:` line in a fenced example, a table, or a quoted snippet no longer dates the task.

## Context

`first_status_commit_date` in `task-viewer/src/harvest.rs` runs the pickaxe over the whole file. Beside three `-S` literals for the bare, double-quoted, and single-quoted spellings, it passes this anchored `-G` pattern:

```rust
format!(
    r#"^status:[[:space:]]*("[[:space:]]*{escaped}[[:space:]]*"|'[[:space:]]*{escaped}[[:space:]]*'|{escaped})"#
)
```

`-G` is anchored to the start of a line and `-S` is a substring search, so a body line at column 0 containing `status: open` is a hit. The archive walk avoids this by re-parsing the frontmatter with `has_archived_status`, but the pickaxe path has no such guard. An archived task with a non-standard status can therefore be dated from a commit that merely added an example line, earlier than the commit that archived it.

## Approach

1. Make the status pickaxe emit the path at each candidate commit (`-z --name-only --format=%cI %H`), and drop the `--no-patch` flag it carries today, because git rejects `--name-only` and `--no-patch` together and the pickaxe command would fail the scan.
2. For each candidate commit, read the blob at that commit and path through the existing `cat-file` reader, and confirm the status sits inside the frontmatter block using the same frontmatter reader the archive walk uses. Skip a path that holds a newline before forming a `cat-file` expression, as the archive walk does, so one such path cannot shift the answers onto the wrong records.
3. Accept the earliest candidate whose frontmatter carries the status, folding the minimum across every candidate and every accepted spelling as the lookup does today; skip a candidate whose match is only in the body.
4. Keep the raw-path handling and the `:(literal)` pathspec.
5. Bump `CONTENT_HASH_REVISION` from `r5` to `r6` and extend its doc comment with an r6 sentence naming the frontmatter-only pickaxe, so a row stored by an earlier reader re-walks its leave day once.
6. Update the `first_status_commit_date` doc comment, which still promises the earliest commit that "introduces `status`" across every accepted spelling, to say the commit's frontmatter must carry the status.
7. Add harvest tests: an archived `status: open` task whose earlier commit adds a fenced `status: open` example line dates from the commit that writes the real frontmatter status, not the example commit; an uncommitted archived `status: open` task whose only matching line sits in the body falls back to `updated`; and a stored row carrying a body-dated day is re-walked once and stores the frontmatter day after the revision bump.
8. Record the rule in `ARCHITECTURE.md`'s extraction rules and `TESTING.md`'s harvest bullet.

**Out of scope:** changing the accepted status spellings, and the archive walk for `finished` and `deferred`, which already re-parses.

## Acceptance

- A status match in the body does not date an archived task.
- An earlier commit that only adds a body `status:` example is skipped, and the later commit whose frontmatter carries the status supplies the date.
- A task with no frontmatter match on any commit keeps the `updated` fallback.
- A stored row carrying a body-dated day is re-walked once after the revision bump and stores the frontmatter day.
- The `ARCHITECTURE.md` extraction-rule sentence and the `TESTING.md` harvest bullet name the frontmatter condition, with the earlier "earliest commit the pickaxe finds" claim replaced rather than duplicated.
- The new harvest tests pass.
