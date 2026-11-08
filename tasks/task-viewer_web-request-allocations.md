---
description: "Cap the flash store, build the loopback host and origin strings once from PORT, and escape the chart JSON while serializing instead of copying it."
scope: task-viewer
created: 2026-11-08T14:13:24
updated: 2026-11-08T14:35:33
status: open
reported-by: "Andreas Hoffmann"
---

# Bound the flash store and drop per-request allocations

## Goal

The server's in-memory banner store holds a bounded number of unread banners, the `Host` and `Origin` checks run without building strings on every request, and the dashboard embeds its chart JSON from one serialization buffer. Every accepted and rejected request, every banner a person actually reads, and every rendered page stay byte-for-byte as they are today.

## Context

All three sites are in `task-viewer/src/web.rs`.

`FlashStore` keeps `slots: HashMap<String, (&'static str, String)>` keyed by the decimal string of an increasing `next_token`. `insert` adds a slot and only `take` (through `claim`) removes one, so a redirect whose dashboard read never happens, such as a client that does not follow the redirect or a navigation cancelled between the redirect and the read, leaves its slot for the life of the process. The growth is one slot per Update, Add, or Remove outcome. `restore` puts a claimed slot back under its own token after a failed render.

`is_local_host` matches the bare loopback names and then compares the host against three `format!("…:{PORT}")` strings, and `loopback_origins` returns three freshly formatted `http://…:{PORT}` strings. `is_local_host` runs for every request, pages and `/static` alike, and `loopback_origins` runs for every state-changing `POST` and every dashboard read that names a `flash` token. Its doc comment says the origins are "built from `PORT` so the allowlist cannot drift from the listener", and that property stays.

The dashboard handler serializes the board with `serde_json::to_string(&board)?` and then runs `data_json.replace('<', "\\u003c")`, which allocates a second buffer the size of the whole payload. The `<` escape is the contract `SECURITY.md` names under **Rendered pages escape text**: "The chart JSON escapes `<` before it is embedded."

## Approach

1. Key `FlashStore.slots` by the numeric token in a `BTreeMap<u64, (&'static str, String)>` and parse the incoming token string to `u64` in `take` and `claim`, where a token that does not parse names no slot. After `insert` adds a slot, evict the smallest token while the map holds more than a `FLASH_CAPACITY` of 32 slots. That capacity sits far above the number of outcomes one person can produce between a redirect and the dashboard read it leads to, so eviction never removes a banner whose redirect is still being followed, and the read-once behaviour `FEATURES.md` describes ("That outcome is shown once, on the first dashboard read") stays. `restore` keeps its behaviour, because a claim has already removed the slot it puts back. Tokens stay the decimal strings the redirects carry today.
2. Build the three `host:PORT` strings and the three `http://host:PORT` origins once each in a `std::sync::LazyLock` initialised from `PORT`, and have `is_local_host`, `is_same_origin_post`, and `is_same_origin_flash_read` compare against those. Accepted values stay exact string matches, so `localhost:07880` and `127.0.0.1:7881` stay rejected.
3. Serialize the board with `serde_json::to_writer` into a `Vec<u8>` through a small `std::io::Write` adapter that writes the six-character escape `\u003c` in place of every `<` byte as the bytes pass, then turn the buffer into a `String` with `String::from_utf8(..)?`. JSON places `<` only inside string values, so escaping the byte stream gives the same text as the current `replace`. The serialization stays inside the `spawn_blocking` render, where the archived `task-viewer_series-cpu-offload` task placed it.

## Acceptance

- A `web` test inserts `FLASH_CAPACITY + 5` banners and finds that the store holds `FLASH_CAPACITY` slots, `take` returns `None` for the five oldest tokens and returns the newest banner, and a non-numeric token returns `None`.
- The existing claim-and-restore test that reads a restored banner still passes against the `BTreeMap` store.
- A host-check test accepts `127.0.0.1`, `localhost`, `[::1]`, and each with `:7880`, and rejects `localhost:07880`, `127.0.0.1:7881`, and `example.com:7880`.
- `is_local_host`, `loopback_origins` or its replacement, `is_same_origin_post`, and `is_same_origin_flash_read` contain no `format!`, checked by reading those functions.
- A dashboard test renders a repository whose name holds `</script><b>` and asserts that the embedded payload equals `serde_json::to_string(&board)` with every `<` replaced by the escape `\u003c`, and that it holds no raw `<`.
- Outside `#[cfg(test)]` code, `task-viewer/src/web.rs` no longer calls `.replace('<'` on the payload.
