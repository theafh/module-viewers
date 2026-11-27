---
description: "Percent-encode the theme form's local next path so a non-ASCII value yields an RFC-valid Location."
scope: task-viewer
created: 2026-11-26T11:55:11Z
updated: 2026-11-27T10:32:23Z
status: implemented
implemented-by: "Andreas Hoffmann"
design-extended: false
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

This is the only redirect target taken from request input; every other redirect interpolates a sanitized id, a fixed code, or a numeric token. Browsers percent-encode non-ASCII in the request target, so `page_next` normally yields an already-encoded path; the raw `next=/café` case reaches `set_theme` through the form body instead, where the form decoder hands over the raw UTF-8 bytes.

**Decision:** The task delivers the percent-encoding.

## Approach

1. Add a sibling path encoder beside `encode_query` in `task-viewer/src/web.rs` and pass the accepted local `next` through it before `Redirect::to`. Keep `encode_query`'s query keep-set `A-Z a-z 0-9 - . _` and every banner redirect built through it unchanged. The path encoder keeps `A-Z a-z 0-9 - . _ ~`, the delimiters `/ ? & = #` so the query structure survives, a literal `+`, and a literal `%` that opens a valid `%XX` escape in either hex case as they are (writing any other `%` as `%25`), and writes every other byte as `%XX` from the uppercase hex table, so a space becomes `%20` and each UTF-8 byte of a non-ASCII character becomes its own escape. A literal `%` is preserved because `page_next` builds `next` from `uri.path_and_query()`, so an already-percent-encoded path arrives encoded and re-encoding `%` as `%25` would double-encode it. A literal `+` is preserved because the page's own GET form encodes a search space as `+`, and rewriting it as `%2B` would change the query's meaning on return.
2. Keep the existing rejection rules and the `/` fallback unchanged.
3. Extend the existing `web_theme_post_sets_the_cookie_and_returns_to_a_safe_next` test with the non-ASCII case instead of adding a second test: assert that `theme=dark&next=/café` answers `303` with a `Location` of `/caf%C3%A9` whose bytes are all ASCII and the theme cookie set, add a plus case whose body is `theme=dark&next=%2Ftasks%3Fq%3Da%2Bb` and whose `Location` is `/tasks?q=a+b`, add a case whose body is `theme=dark&next=%2Ftasks%3Fq%3Dcaf%25C3%25A9` and whose `Location` is `/tasks?q=caf%C3%A9`, add a lowercase-hex case whose body is `theme=dark&next=%2Ftasks%3Fq%3Dcaf%25c3%25a9` and whose `Location` is `/tasks?q=caf%c3%a9`, add a literal-space case whose body is `theme=dark&next=%2Ftasks%3Fq%3Da%20b` and whose `Location` is `/tasks?q=a%20b`, add a case whose body is `theme=dark&next=%2Ftasks%3Fq%3D100%25` and whose `Location` is `/tasks?q=100%25`, and add a case whose body is `theme=dark&next=%2Ftasks%3Fkind%3Dlive%26dir%3Ddesc` and whose `Location` is `/tasks?kind=live&dir=desc`. The test's existing unsafe-`next` cases already cover the `/` fallback, so no duplicate case is added.
4. Record the encoding rule in the paragraph of `FEATURES.md` that contains `posts the opposite theme and the page's own path and query to` (then `` `POST /theme` ``), and in `ARCHITECTURE.md`'s sentence that begins `The theme choice is` (then `` `POST /theme` ``).

**Out of scope:** accepting an absolute URL as `next`, and changing the cookie attributes.

## Acceptance

- `POST /theme` with `theme=dark&next=/café` answers `303`, sets the theme cookie, and returns a `Location` of `/caf%C3%A9` whose bytes are all ASCII.
- `POST /theme` with an unsafe `next` still redirects to `/` (a regression guard; the non-ASCII case is what detects the change).
- A `next` carrying a form-encoded `+` returns with the `+` intact (a regression guard; it already holds at HEAD).
- A `next` that already carries a `%XX` escape in either hex case returns with that escape intact (a regression guard; it already holds at HEAD), and a `%` that opens no escape returns as `%25`.
- A space in `next` returns as `%20`.
- A `next` carrying several query parameters returns with its `&` separators intact.
- `FEATURES.md`'s theme paragraph and `ARCHITECTURE.md`'s theme route sentence state that a local `next` is percent-encoded (a space as `%20`, each non-ASCII UTF-8 byte as `%XX`, and a valid `%XX` and a `+` preserved).
- The extended web test passes.
