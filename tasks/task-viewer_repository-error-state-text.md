---
description: "Say last error (current) or last error (recovered) on the Repositories page, so the state is not carried by colour alone."
scope: task-viewer
created: 2026-11-26T13:14:16Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Name a repository's error state in words

## Goal

A repository row says whether its stored error is current or history in words, so a screen-reader or colour-blind reader can tell a failing repository from a recovered one without relying on the colour.

## Context

`task-viewer/templates/repositories.html` renders the same text for both states and changes only the class:

```html
<span class="{% if project.failing %}error-text{% else %}muted{% endif %}">last error ...: ...</span>
```

`FEATURES.md` promises the page shows "whether that error is current". The dashboard's failure list is textual, but the Repositories page — the only place a recovered error is shown as history — conveys the distinction by colour alone.

## Approach

1. Render `last error (current) <time>: <message>` while the repository is failing, and `last error (recovered) <time>: <message>` once a later success has cleared it.
2. Keep the existing `error-text` and `muted` classes as reinforcement, not as the only signal.
3. Keep a repository with no stored error rendering no error line.
4. Update the existing page test `web_repositories_page_renders_last_success_and_last_error_history` rather than adding a second one: its recovered assertion pins the bare `last error <time>: <message>` string today, so re-point it to `last error (recovered) <time>: <message>` with the `muted` class, and extend the failing case to `last error (current)` with the `error-text` class, keeping both stamps; name the wording in the test's `Covers:` comment. Add an assertion that an error-free repository renders no `last error` line.
5. Record the wording in `FEATURES.md`'s Repositories paragraph and `TESTING.md`'s Repositories bullet.

**Out of scope:** changing when an error counts as current, and moving the error line.

## Acceptance

- A repository whose most recent scan failed renders `last error (current)`.
- A repository with a later success and a stored error renders `last error (recovered)`.
- A repository with no error still renders no error line, kept as a regression guard with an assertion that an error-free row renders no `last error` line.
- The `web_repositories_page_renders_last_success_and_last_error_history` test passes with its `(current)` and `(recovered)` assertions.
- `FEATURES.md`'s Repositories bullet that promises `whether that error is current`, and `TESTING.md`'s `- Repositories:` bullet, both name the `(current)` and `(recovered)` wording.
