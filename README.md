# GeneSleuth

A local-first Rust application for indexing genealogy records, transcribing
images and PDFs with Gemini, comparing extracted facts to GEDCOM, and creating
cited biographies.

## Run locally

Install Rust, then run:

```powershell
$env:GOS_USERNAME = "researcher"
$env:GOS_PASSWORD = "replace-with-10-plus-chars-including-Aa1!"
cargo run -- serve
```

The web interface listens at `http://127.0.0.1:3000` by default and requires
the configured username and password. Use a unique password with at least 10
characters, including an uppercase letter, a lowercase letter, a number, and a
symbol. Configure an API key in Settings or set `GEMINI_API_KEY`.
The default indexed folder is `/data/records`; set `RECORDS_DIR` to a local
folder on Windows.

## CLI

```powershell
cargo run -- scan --directory .\research
cargo run -- process --file .\research\record.jpg
cargo run -- audit --gedcom .\research\family.ged --records .\research
```

`process` and AI-generated audit/biography reports require `GEMINI_API_KEY`.
The scanner and deterministic GEDCOM checks run without network access.

## Docker

Edit `docker-compose.yml` to set the host research directory, then:

```powershell
Copy-Item .env.example .env
# Edit .env and set GOS_USERNAME and a unique GOS_PASSWORD meeting the rules above.
docker compose up --build
```

The compose port is localhost-only by default. To access the UI from another
device on a trusted home LAN, set `HOST_BIND_IP` in `.env` to this PC's LAN
IPv4 address (for example `192.168.0.3`), then start or recreate the service:

```dotenv
HOST_BIND_IP=192.168.0.3
GOS_USERNAME=researcher
GOS_PASSWORD=use-a-unique-password-like-Aa1-with-10-or-more-chars
```

Open `http://192.168.0.3:3000` from the other device and sign in. Allow inbound
TCP port 3000 on the Windows firewall only for the private network profile if
Windows blocks access. Keep this service on a trusted LAN: HTTP does not encrypt
the password or session cookie in transit. Do not port-forward it or expose it
to the public internet; use a TLS-terminating reverse proxy for access beyond
the trusted LAN. Keep `.env` private; it contains the login credentials.

The compose file mounts the host research folder at `/data/records` and
generated stories at `/data/output`. Database/settings are stored in a Docker
named volume mounted at `/app/data`, avoiding host bind-mount ownership errors.
That volume persists across `docker compose down`; do not use `docker compose
down -v` unless you intend to delete the database and encryption key.

### Reset GeneSleuth as a new user

To remove the saved database, API-key settings, and encryption key before
rebuilding, run this from the directory containing `docker-compose.yml`:

```powershell
docker compose down --volumes
# Update GOS_USERNAME and GOS_PASSWORD in .env for the new user.
docker compose up --build -d
```

`down --volumes` deletes the Compose named volume containing `/app/data`.
GeneSleuth will create a fresh database and encryption key on startup. This
does **not** delete the host research folder, source files, sidecar transcripts,
or the `./output` folder because those are bind-mounted directories. The reset
also removes any other Compose-managed named volumes for this project; this
Compose setup currently uses `app-data` for persistent application data.
Browse to `http://localhost:3000` when using the default localhost-only binding.
On Linux, set `PUID` and `PGID` to the owner IDs of the mounted research/output
directories before building. Defaults are `1000:1000`:

```sh
PUID=$(id -u) PGID=$(id -g) docker compose up --build
```

## Notes

- Image files are heuristically filtered, then checked by Gemini Flash-Lite;
  PDFs bypass photo filtering. PDFs over ten pages are transcribed page by page
  before a consolidation pass.
- Transcripts and page-level transcripts are written beside the source file.
- The default transcription prompt preserves table columns, row alignment,
  empty cells, and source order without assuming a particular document type.
  Existing saved copies of the previous built-in prompt are upgraded on
  startup; custom prompts are left unchanged.
- API keys are encrypted in SQLite using a local encryption key. Back up the
  `.encryption-key` file along with the database; alternatively supply a stable
  64-hex-character `GOS_MASTER_KEY`.
- Gemini model names can be changed with `GEMINI_MODEL` and
  `GEMINI_VALIDATION_MODEL`.
- On HTTP 429 quota responses, the app discovers Flash models available to the
  configured API key and tries alternatives that support `generateContent`.
  Successful fallback choices are reused for later requests in this process.
  Google quotas are per project, vary by model, and daily quotas reset at
  midnight Pacific; rotating models cannot bypass a project-wide quota.
- An in-progress directory scan can be stopped from the dashboard. The stop
  action cancels the current async processing operation and stops hashing
  discovered files; already completed transcripts remain saved.
- Scans process up to four independent transcript-output groups concurrently.
  Requests still share the Gemini client's limiter of nine requests per minute;
  files that would write to the same Markdown sidecar are kept sequential.
- Dot-prefixed files and directories are excluded from scans. Use **Clear
  indexed files** on the dashboard to reset the SQLite file index; this preserves
  source documents and all transcripts. Existing transcripts will still be
  detected and skipped during a subsequent scan.
- Geographical anomaly checks are conservative string/date heuristics, not
  geocoding. Review flagged items manually.
- Life story source selection can tolerate OCR name variants when other evidence
  corroborates the match; the audit page shows the selected transcripts and
  matching clues. Review those sources and the generated citations. See
  [Life story source matching](./docs/lifestory-source-matching.md) for the
  scoring signals and limitations.