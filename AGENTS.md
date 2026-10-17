# Genealogy-OCR-Studio: AI contributor notes

## Architecture

- This is a single Rust/Axum binary. `serve` runs the web application; `scan`,
  `process`, and `audit` are standalone CLI commands.
- SQLite is the durable source of truth for settings, file hashes/statuses, and
  the current queue job. Keep blocking rusqlite and filesystem work off async
  request paths where it may become long-running.
- Transcripts use YAML frontmatter followed by Markdown and are stored beside
  the source image/PDF. Do not overwrite a transcript for a file whose content
  hash has not changed.
- Gemini credentials are encrypted before they are stored in SQLite. Preserve
  `GOS_MASTER_KEY` key stability or the local `.encryption-key` file when
  persisting the database.
- Queue progress is broadcast to WebSocket clients and can be recovered from
  SQLite after a server restart.

## Operational configuration

- `DATABASE_PATH` defaults to `./data/genealogy.sqlite3`.
- `RECORDS_DIR` defaults to `/data/records` in the container.
- `OUTPUT_DIR` defaults to `/data/output`; stories go in `stories/`.
- `BIND_ADDR` defaults to `0.0.0.0:3000`.
- `GEMINI_API_KEY` can supply an environment fallback; browser settings take
  precedence for saved credentials.
- `GOS_MASTER_KEY` may be a 64-character hexadecimal encryption key. If unset,
  a local key is generated in the database directory.

## Validation and safety

- Run `cargo fmt --check` and `cargo test` after Rust changes.
- Keep filesystem paths user-controlled only where the feature explicitly
  requires it; sanitize GEDCOM identifiers before using them in output paths.
- Treat GEDCOM and OCR input as untrusted. Do not execute document content or
  expose API keys in HTML, logs, or error messages.
- Gemini requests are paced below ten requests per minute. Preserve this limit
  if adding parallel processing.
- PDF operations use `lopdf`; always clean temporary page files with RAII.

