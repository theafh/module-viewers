---
description: "Neutralize spreadsheet formula prefixes in the CSV export so a hostile task or folder name cannot become a live formula."
scope: task-viewer
created: 2026-11-25T08:39:58Z
updated: 2026-11-25T10:05:40Z
status: implemented
reported-by: "Andreas Hoffmann"
implemented-by: "Andreas Hoffmann"
design-extended: false
---

# Neutralize spreadsheet formulas in the CSV export

## Goal

A value exported by `GET /tasks.csv` never becomes a live spreadsheet formula when the file is opened. Ordinary values export byte-for-byte as they do today.

## Context

`csv_field` in `task-viewer/src/web.rs` quotes a value only when it holds a comma, a double quote, a carriage return, or a line feed:

```rust
if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
```

The exported fields include the repository name, the scope, and the stored path. A repository folder name is stored verbatim, the scope is the file name's prefix before the first underscore, and a path is a stored relative path, so a cloned repository can plant a task file whose name starts with `=`, `+`, `-`, or `@`. A spreadsheet evaluates a cell that begins with one of those characters even when the CSV field is quoted, so opening the export can run a formula the repository author chose.

## Approach

1. In `csv_field`, when the value's first character is `=`, `+`, `-`, `@`, a tab, or a carriage return, prefix one single quote `'` before applying the existing comma/quote/line-break quoting. A spreadsheet reads the prefixed cell as text and keeps the original characters visible after the quote.
2. Apply the rule to every exported field, so the header row and a date cell are unaffected while a hostile repository, scope, or path is neutralized.
3. Add direct `csv_field` tests for each leading character and for a value that both starts with `=` and holds a comma.

**Out of scope:** changing the export's columns, order, filename, content type, or the page list; refusing a repository whose name starts with one of those characters.

## Acceptance

- The cell text after unwrapping any CSV quotes begins with a single quote for each of `=`, `+`, `-`, `@`, a tab, and a carriage return; a value beginning with `=` and holding a comma is both prefixed and quoted, so its exported line reads `"'=a,b"`.
- A plain path such as `tasks/scope_name.md`, a date, and the fixed header export unchanged.
- A test-local CSV reader, added beside the existing `csv_field` tests, reads the exported line back and asserts the neutralized cell's text carries the leading quote and the original characters.
