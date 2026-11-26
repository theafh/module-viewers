---
description: "Percent-encode the theme form's local next path so a non-ASCII value yields an RFC-valid Location."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-26T16:18:16Z
status: checked
reported-by: "Andreas Hoffmann"
---

# Redirect the theme form safely for a non-ASCII next path

## Goal

`POST /theme` always answers with a redirect for a local `next`, whatever bytes it holds. A non-ASCII local path is percent-encoded into a valid `Location`; an unsafe path still falls back to `/`.

## Context

`safe_theme_next` accepts any string that starts with one `/`, does not start with `//`, carries no scheme, no backslash, and no control character:

```rust
fn safe_theme_next(next: &str) -> &str {
    let local = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains("://")
        && !next.contains('\\')
        && !next.chars().any(char::is_control);
    if local { next } else { "/" }
}
```

(`safe_theme_next`). `set_theme` passes the result straight to `Redirect::to`. At HEAD a post with `next=/café` already answers `303 See Other`: `HeaderValue` accepts every byte from `0x20` to `0xFF` except `0x7F`, every UTF-8 lead and continuation byte is `0x80` or above, and `safe_theme_next` refuses every control character, so the `HeaderValue::try_from` failure arm in `Redirect`'s `IntoResponse` is unreachable. The `Location` therefore carries the raw UTF-8 bytes `2F 63 61 66 C3 A9`, which hyper writes to the wire unchanged and which is not a valid URI reference. The 500 this task was first filed for does not occur at HEAD, so the percent-encoding is a URI-compliance fix rather than a crash fix.

This is the only redirect target taken from request input; every other redirect interpolates a sanitized id, a fixed code, or a numeric token.

**Open decision:** The 500 premise is refuted at HEAD: `POST /theme` with `next=/café` already answers `303 See Other` with raw UTF-8 bytes in `Location`, so there is no crash to fix. This fork is open because the evidence cannot settle what the work should deliver: it is a product call on whether a working but non-RFC-conformant `Location` is worth a change. Options: (a) re-premise as a URI-compliance fix, keeping the percent-encoding and accepting a change for a non-crashing defect (suggested default, since the current `Location` is not a valid URI reference); (b) narrow to a test-only change that pins today's `303` and raw bytes so a later encoder change is visible, reading `Location` through `as_bytes()` because the `redirect_location` test helper's `to_str()` panics on the raw bytes; (c) defer and archive the task, leaving the raw-byte `Location` in place. Until this is settled, the Approach and Acceptance below describe option (a).

## Approach

1. Add a sibling path encoder beside `encode_query` in `task-viewer/src/web.rs` and pass the accepted local `next` through it before `Redirect::to`. Keep `encode_query`'s query keep-set `A-Z a-z 0-9 - . _` and every banner redirect built through it unchanged. The path encoder keeps `A-Z a-z 0-9 - . _ ~`, the delimiters `/ ? & = #`, and a literal `%` that opens a valid `%XX` escape as they are (writing any other `%` as `%25`), and writes every other byte as `%XX` from the uppercase hex table, so a space becomes `%20` and each UTF-8 byte of a non-ASCII character becomes its own escape. A literal `%` is preserved because `page_next` builds `next` from `uri.path_and_query()`, so an already-percent-encoded path arrives encoded and re-encoding `%` as `%25` would double-encode it.
2. Keep the existing rejection rules and the `/` fallback unchanged.
3. Extend the existing `web_theme_post_sets_the_cookie_and_returns_to_a_safe_next` test with the non-ASCII case instead of adding a second test: assert that `theme=dark&next=%2Fcaf%C3%A9` answers `303` with a `Location` of `/caf%C3%A9` and the theme cookie set. The test's existing unsafe-`next` cases already cover the `/` fallback, so no duplicate case is added.
4. Record the encoding rule in `FEATURES.md`'s theme paragraph and `ARCHITECTURE.md`'s theme route sentence.

**Out of scope:** accepting an absolute URL as `next`, and changing the cookie attributes.

## Acceptance

- `POST /theme` with `theme=dark&next=/café` answers `303`, sets the theme cookie, and returns a `Location` of `/caf%C3%A9` whose bytes are all ASCII.
- `POST /theme` with an unsafe `next` still redirects to `/`.
- The extended web test, `make lint`, and `make test` pass.
