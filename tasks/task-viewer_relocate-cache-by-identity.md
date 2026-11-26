---
description: "Reuse a cached leave day only when the repository identity, the folder's place in it, and the ref tips match the history it was computed from."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T15:16:30Z
status: ready
reported-by: "Andreas Hoffmann"
---

# Key cached leave days to the repository's history

## Goal

A stored leave day is reused only when the repository's identity, the configured folder's place inside that repository, and the current ref tips are the same as when the day was computed. Relocating to a clone whose history differs — a different origin, or the same origin at different ref tips — re-walks every archived path instead of reusing a day from a history the dashboard no longer reads.

## Context

`leave_dates` in `task-viewer/src/harvest.rs` reuses a prior row's day when its relative path and `content_hash` match:

```rust
if let Some(row) = cached.get(rel.as_str())
    && row.content_hash.as_deref() == Some(hash.as_str())
{
    return Ok((*index, row.leave_date));
}
```

Nothing in the key names the repository's identity, its remote, its ref tips, or the folder's place inside an enclosing repository. Relocate keeps the repository id and re-harvests the new folder, so relocating to a different clone or fork whose task files are byte-identical reuses every cached day whose content hash matches and skips the archive walk for those rows.

`harvest::repository_identity` is the wrong source for the key on its own: it deliberately answers `RepositoryIdentity::None` for a folder without its own `.git`, so Add does not report two configured subfolders of one enclosing repository as clones, while `GitContext::resolve` charts exactly such a folder from its enclosing repository. The key has to carry the enclosing repository's identity and the folder's prefix without changing `repository_identity`.

## Approach

1. Compute a per-repository `history_key` inside the harvest module, which owns the private `GitContext`, from three parts:
   - The repository identity, by the same rule `repository_identity` applies to a folder that has its own `.git`: the canonical `origin` URL when the repository has an `origin` remote, otherwise the canonical git common directory. A folder without its own `.git` takes this identity from its resolved enclosing repository. Leave `repository_identity` unchanged for Add's duplicate detection.
   - `GitContext.prefix`, the folder's place inside that repository, empty for a folder with its own `.git`, because the archive walk reads the path against that place.
   - The sorted tip commit ids of the ref set `git log --all` actually walks: every ref under `refs/*` plus `HEAD`, with annotated tags peeled to their commit, since `%(objectname)` on a tag names the tag object. The heads-and-remotes listing in `ref_task_files` is not that set.
2. Store the key with the repository through a new schema step (`repositories.history_key TEXT`), raising `SCHEMA_VERSION` from four to five and adding the guarded migration step.
3. Pass the current key into the leave-day cache check and reuse a cached leave day only when the stored key equals it. On a mismatch, ignore every cached day for that repository, re-walk the archived paths, and store the new key with the rows.
4. A folder with no git history at all, where `GitContext::inside_repository` is false, has no history key and keeps the `updated` fallback.
5. Add tests: two clones with identical task bytes but different archive commits produce their own days after Relocate; a same-origin clone at identical ref tips reuses its cached days; an unchanged repository still reuses its cached day; a forced ref move re-walks; a commit reachable only from a tag or `HEAD` changes the key; relocating `repo/a` to `repo/b` in one clone re-walks; the existing migration tests that assert the current schema version expect version 5; and a version-4 database migrates to version 5 with the `repositories.history_key` column and keeps every repository and task row.
6. Record the rule in three `ARCHITECTURE.md` passages — the Data Layout sentence that starts ``Update reuses a row's stored leave day``, the Extraction Rules, and the `db.rs` component paragraph that enumerates the guarded schema steps — and in two `TESTING.md` bullets, the harvest bullet and the Database bullet that starts ``- Database: versioned migrations upgrade``.

**Out of scope:** changing the content-hash reuse for unchanged files within one history.

## Acceptance

- Relocating a repository to a clone with byte-identical task files but a different history — a different origin, or the same origin at different ref tips — re-walks its archived paths and stores that clone's ship/leave days.
- Relocating to a same-origin clone at identical ref tips, and an unchanged repository with unchanged refs, still reuse the stored days without a history walk.
- A changed ref tip re-walks the archived paths, including a commit reachable only from a tag or from `HEAD`.
- Relocating a configured subfolder of one repository to a different subfolder of the same repository at the same ref tips re-walks the archived paths.
- A version-4 database migrates to version 5 with the `repositories.history_key` column and keeps every repository and task row.
- The new tests, `make lint`, and `make test` pass.
