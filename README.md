# Genealogy-OCR-Studio

A local-first Rust application for indexing genealogy records, transcribing
images and PDFs with Gemini, comparing extracted facts to GEDCOM, and creating
cited biographies.

## Run locally

Install Rust, then run:

```powershell
cargo run -- serve
```

The web interface listens at `http://127.0.0.1:3000` by default. Configure an
API key in Settings or set `GEMINI_API_KEY`.
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
docker compose up --build
```

The compose file mounts the host research folder at `/data/records`, persistent
application data at `/app/data`, and generated stories at `/data/output`.
Browse to `http://localhost:3000`.
The compose port is bound to localhost so the unauthenticated local UI is not
published to other network interfaces.

## Notes

- Image files are heuristically filtered, then checked by Gemini Flash-Lite;
  PDFs bypass photo filtering. PDFs over ten pages are transcribed page by page
  before a consolidation pass.
- Transcripts and page-level transcripts are written beside the source file.
- API keys are encrypted in SQLite using a local encryption key. Back up the
  `.encryption-key` file along with the database; alternatively supply a stable
  64-hex-character `GOS_MASTER_KEY`.
- Gemini model names can be changed with `GEMINI_MODEL` and
  `GEMINI_VALIDATION_MODEL`.
- Geographical anomaly checks are conservative string/date heuristics, not
  geocoding. Review flagged items manually.