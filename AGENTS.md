# GeneSleuth: AI contributor notes

## Architecture

- This is a single Rust/Axum binary. `serve` runs the web application; `scan`,
  `process`, and `audit` are standalone CLI commands.
- SQLite is the durable source of truth for settings, file hashes/statuses, and
  the current queue job. Keep blocking rusqlite and filesystem work off async
  request paths where it may become long-running.
- Transcripts use YAML frontmatter followed by Markdown and are stored beside
  the source image/PDF or in the user's configured sidecar subfolder. Resolve
  all master and page-level transcript paths through `src/sidecar.rs`. Settings
  are persisted in SQLite; scanner delta checks, direct/CLI processing, GEDCOM
  audits, and Life Story discovery must remain compatible with either layout.
  Do not overwrite a transcript for an unchanged file when it already exists
  at the configured path.
- Keep the default transcription prompt format-agnostic. Its table guidance
  should preserve source row/column relationships, require consistent cell
  counts, and use compact Markdown separators rather than content-width padding.
- When changing the built-in prompt, migrate only an exact match of the prior
  built-in text so saved custom prompts remain untouched.
- Life story source matching lives in `src/web/story_sources.rs`; keep fuzzy
  name matches dependent on independent GEDCOM/transcript/path corroboration.
  `src/web/story_format.rs` validates per-section confidence and transcript
  citations before a generated biography is saved.
- Gemini credentials are encrypted before they are stored in SQLite. Preserve
  `GOS_MASTER_KEY` key stability or the local `.encryption-key` file when
  persisting the database.
- Queue progress is broadcast to WebSocket clients and can be recovered from
  SQLite after a server restart. Keep the WebSocket status event consistent
  with the dashboard.
- Hidden dot-prefixed files and directories are excluded from recursive scans.
  The clear-index operation removes only SQLite file rows and resets counters;
  it must preserve originals and sidecar transcripts.

## Operational configuration

- `DATABASE_PATH` defaults to `./data/genealogy.sqlite3`.
- `RECORDS_DIR` defaults to `/data/records` in the container.
- `OUTPUT_DIR` defaults to `/data/output`; stories go in `stories/`.
- `BIND_ADDR` defaults to `0.0.0.0:3000`.
- `GEMINI_API_KEY` can supply an environment fallback; browser settings take
  precedence for saved credentials.
- `GOS_MASTER_KEY` may be a 64-character hexadecimal encryption key. If unset,
  a local key is generated in the database directory.
- `GOS_USERNAME` and `GOS_PASSWORD` are required to start the web server. The
  password must be at least 10 characters and include uppercase and lowercase
  letters, a number, and a symbol (maximum 4096 bytes). Web sessions are
  HttpOnly, SameSite=Strict cookies held in memory and expire after 12 hours.
- Docker Compose binds to `127.0.0.1` by default. `HOST_BIND_IP` can select a
  host interface, but only use non-loopback bindings on trusted private networks.
  The app uses HTTP; never expose it directly to the public internet.

## Validation and safety

- Keep filesystem paths user-controlled only where the feature explicitly
  requires it; sanitize GEDCOM identifiers before using them in output paths.
- Treat GEDCOM and OCR input as untrusted. Do not execute document content or
  expose API keys in HTML, logs, or error messages.
- Keep all web routes (including REST APIs and WebSockets) behind the session
  authentication middleware; only `/login` and `/api/login` are public.
- Gemini requests are paced below ten requests per minute. Preserve this limit
  if adding parallel processing. The scan worker runs at most four independent
  source-output groups concurrently and shares one Gemini rate limiter across
  all requests.
- On Gemini HTTP 429 responses, `GeminiClient` may list generation-capable Flash
  models for the current API key and try alternatives; exclude preview models
  and only update the per-key active model after an alternate succeeds.
  Transient transport failures and HTTP 408/5xx responses are retried with
  backoff. A persistent file failure must be saved with status `error`, never
  `transcribed`, so a later scan retries it.
- The Life Story review instructions are in
  `LIFE_STORY_REVIEW_PROMPT` in `src/web/handlers.rs`. Keep the document-type
  guidance format-agnostic and preserve explicit surname inheritance rules for
  grouped indexes and tables.
- For long PDFs, a Gemini response with no candidate text is an empty-page
  marker, not a page-processing error. Continue processing and consolidate
  available page text; only fail the PDF when no page produced any text.
- Directory scans use a cancellation token. Keep cancellation checks in
  filesystem traversal/hashing and make processing futures cancellable.
- PDF operations use `lopdf`; always clean temporary page files with RAII.
