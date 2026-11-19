---
description: "Match a status quoted with padding inside the quotes in the archived pickaxe lookup, so a non-finished archived task dates from its archive commit instead of the updated fallback."
scope: task-viewer/src
created: 2026-11-19T14:15:38Z
updated: 2026-11-19T15:26:44Z
status: ready
reported-by: Andreas Hoffmann
---

# Date an archived non-finished task from a padded quoted status commit

## Goal

An archived task whose status is neither `finished` nor `deferred` takes its leave day from the commit that introduced its status, including when that commit writes the status quoted with padding inside the quotes (`status: " open "`, `status: ' open '`). Today that spelling falls back to the frontmatter `updated` day, so the pickaxe lookup disagrees with the classification and archive-walk paths that already trim the value.

## Context

- `task-viewer/src/harvest.rs` holds the whole surface. `fn first_status_commit_date` builds three `-S` literals and one anchored `-G` ERE from the status value it receives, and `fn leave_dates` sends every archived task whose status is not `finished` or `deferred` to it, while `first_archived_commit_date` serves the other two.
- The value reaching that lookup is already trimmed: the frontmatter reader unquotes a value and the classification arm `fields.get("status").map(|value| value.trim().to_string())` trims it, so the padded commit text `status: " open "` arrives as `open` and the padding sits inside the quotes in the committed bytes, not in the lookup argument. `fn has_archived_status` trims its own copy for the same reason.
- Reproduced in a scratch repository whose one archive commit carries `status: " open "` on 2026-05-10 with `updated: 2026-06-01`: the `-S` literals `status: open`, `status: "open"`, and `status: 'open'` each print no commit, the anchored ERE prints no commit, and a `-G` pattern naming the padded spelling prints that commit's day. The archived task therefore charts 2026-06-01 while the same task written `status: open` charts 2026-05-10.
- `ARCHITECTURE.md` names this lookup as the one that governs the case, in the leave-day bullet that states "an archived status other than `finished` or `deferred` still dates from the pickaxe that introduces it". `FEATURES.md` already promises the padded spelling charts like its plain form ("a quoted status padded with spaces inside its quotes"), so the pickaxe path is the remaining break of a shipped promise.
- Precedent: [task-viewer_frontmatter-status-robustness.md](archive/task-viewer_frontmatter-status-robustness.md) trimmed the value on the classification and archive-walk paths and bumped `CONTENT_HASH_REVISION` to `r4`; it left the pickaxe argument alone because that argument is already trimmed. [task-viewer_harvest-input-robustness.md](archive/task-viewer_harvest-input-robustness.md) added the quoted pickaxe spellings and their two tests, which is why only the padded form remains open.
- This task adds a different `TESTING.md` sentence than the live `task-viewer_guardrail-doc-sync.md`, so the two need no order.

## Approach

- Widen the anchored alternative in `fn first_status_commit_date` so it tolerates whitespace inside the quotes, and keep the three `-S` literals as the exact-spelling fast path. The `-G` pattern becomes `^status:[[:space:]]*("[[:space:]]*{escaped}[[:space:]]*"|'[[:space:]]*{escaped}[[:space:]]*'|{escaped})`, which keeps each quoted branch anchored by both quotes (so `open` still never answers `status: "openx"`) and keeps the bare branch's existing prefix match.
- Leave the argument as it arrives. Trimming inside `fn first_status_commit_date` is a no-op here, because the padding lives inside the quotes of the commit text rather than in the lookup value.
- Keep `CONTENT_HASH_REVISION` at `r4`. A datable path whose leave day falls back stores `content_hash = None` in `fn finish`, so the next Update re-walks that row and stores the now-correct day; no reader-revision change is needed to invalidate it.
- Extend the `Harvest:` bullet of `TESTING.md` in place so it names the padded quoted status in the pickaxe clause that reads "a status carrying a POSIX ERE metacharacter reaches `git log -G` escaped", beside that clause's metacharacter cases; the archive-walk sentence already names the padded status and stays as written.

**Out of scope:** replacing the pickaxe with the archived-path content walk for a status that is neither `finished` nor `deferred`; this task widens the existing pattern and keeps the lookup's mechanism.

## Acceptance

- A harvest test builds a repository whose archived task carries `status: " open "` on an archive commit dated 2026-05-10, with frontmatter `updated: 2026-06-01`, and asserts the row's `leave_date` is the archive commit's day, `2026-05-10`.
- The same fixture written `status: ' open '` asserts the same leave day.
- A control fixture whose archived status reads `status: "openx"` asserts `fn first_status_commit_date` called with `open` still returns `None`, and the same control written `status: ' openx'` asserts `None` too, so both quoted branches' closing-quote guarantee holds.
- The existing tests `harvest_first_status_commit_matches_a_double_quoted_status` and `harvest_first_status_commit_matches_a_single_quoted_status` keep passing unchanged.
- A harvest test harvests the padded fixture with no prior row and asserts the returned row's `content_hash` is `Some`, so a later Update reuses the corrected day instead of re-walking.
- `CONTENT_HASH_REVISION` in `task-viewer/src/harvest.rs` still reads `r4`.
- A fixture whose archived task reads the plain spelling `status: open` asserts the row's `leave_date` is that commit's day, so the bare spelling the widened pattern keeps matching still dates from its commit.
- A read of `fn first_status_commit_date` in `task-viewer/src/harvest.rs` shows all three `-S` literals still tried before the widened `-G` pattern, so the widening extends the exact-spelling fast path instead of replacing it.
- `TESTING.md`'s `Harvest:` bullet names the padded quoted status on the pickaxe path.
