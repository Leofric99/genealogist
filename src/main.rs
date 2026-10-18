mod auth;
mod config;
mod db;
mod gedcom;
mod gemini_client;
mod pdf;
mod queue;
mod scanner;
mod secret;
mod sidecar;
mod state;
mod vision_filter;
mod web;

use anyhow::{Context, Result};
use auth::AuthManager;
use axum::{
    middleware,
    routing::{get, post},
    Router,
};
use clap::{Parser, Subcommand};
use config::Config;
use db::Database;
use gemini_client::GeminiClient;
use secret::SecretCipher;
use sidecar::SidecarConfig;
use state::AppState;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "genesleuth",
    version,
    about = "Genealogy document OCR, indexing, and research auditing"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Scan {
        #[arg(long)]
        directory: PathBuf,
        #[arg(long, default_value = "./data/genealogy.sqlite3")]
        database: PathBuf,
    },
    Process {
        #[arg(long)]
        file: PathBuf,
    },
    Audit {
        #[arg(long)]
        gedcom: PathBuf,
        #[arg(long)]
        records: PathBuf,
        #[arg(long, default_value = "./data/genealogy.sqlite3")]
        database: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Scan {
            directory,
            database,
        } => cli_scan(&directory, &database),
        Command::Process { file } => cli_process(&file).await,
        Command::Audit {
            gedcom,
            records,
            database,
        } => cli_audit(&gedcom, &records, &database),
    }
}

fn build_state(
    config: Config,
    auth: Option<Arc<AuthManager>>,
) -> Result<(Arc<AppState>, mpsc::Receiver<()>)> {
    let db = Database::open(&config.database_path)?;
    let cipher = SecretCipher::load(&config.database_path)?;
    let gemini = GeminiClient::new(config.gemini_model.clone(), config.validation_model.clone())?;
    let (queue_tx, queue_rx) = mpsc::channel(1);
    let (progress_tx, _) = broadcast::channel(64);
    Ok((
        Arc::new(AppState {
            config,
            auth,
            db,
            cipher,
            gemini,
            queue_tx,
            progress_tx,
            scan_cancellation: std::sync::Mutex::new(CancellationToken::new()),
        }),
        queue_rx,
    ))
}

async fn serve() -> Result<()> {
    let config = Config::from_env();
    let bind_addr = config.bind_addr.clone();
    let auth = Arc::new(AuthManager::from_env()?);
    let (state, receiver) = build_state(config, Some(auth))?;
    let resume = state.db.job_status()?.running;
    queue::spawn_worker(Arc::clone(&state), receiver);
    if resume {
        state
            .queue_tx
            .send(())
            .await
            .context("resuming persisted scan job")?;
    }
    let app = Router::new()
        .route("/login", get(web::views::login))
        .route("/api/login", post(web::handlers::login))
        .route("/api/logout", post(web::handlers::logout))
        .route("/", get(web::views::dashboard))
        .route("/settings", get(web::views::settings))
        .route("/audit", get(web::views::audit))
        .route("/ws", get(web::websocket::handler))
        .route("/partials/status", get(web::handlers::status_fragment))
        .route("/api/status", get(web::handlers::status))
        .route("/api/scan", post(web::handlers::start_scan))
        .route("/api/scan/stop", post(web::handlers::stop_scan))
        .route("/api/index/clear", post(web::handlers::clear_index))
        .route(
            "/api/settings",
            get(web::handlers::get_settings).post(web::handlers::save_settings),
        )
        .route("/api/settings/test", post(web::handlers::test_key))
        .route("/api/gedcom", post(web::handlers::upload_gedcom))
        .route("/api/audit", post(web::handlers::run_audit))
        .route("/api/story/:xref", post(web::handlers::generate_story))
        .route(
            "/api/story/:xref/sources",
            get(web::handlers::discover_story_sources)
                .post(web::handlers::update_story_source_link),
        )
        .route("/api/stories/batch", post(web::handlers::start_story_batch))
        .route(
            "/api/stories/batch/status",
            get(web::handlers::story_batch_status),
        )
        .layer(RequestBodyLimitLayer::new(25 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            web::handlers::require_login,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding web server at {bind_addr}"))?;
    tracing::info!(%bind_addr, "GeneSleuth is ready");
    axum::serve(listener, app).await?;
    Ok(())
}

fn cli_scan(directory: &std::path::Path, database: &std::path::Path) -> Result<()> {
    let db = Database::open(database)?;
    let sidecars = SidecarConfig::load(&db)?;
    let exclusions = scanner::DirectoryExclusions::load(&db)?;
    let files = scanner::scan_and_hash_with_exclusions(directory, &exclusions)?;
    let mut new_or_modified = 0usize;
    let mut unchanged = 0usize;
    let mut existing_transcripts = 0usize;
    for file in &files {
        let path = file.path.to_string_lossy().into_owned();
        let indexed_hash = db.file_hash(&path)?;
        let status = db.file_status(&path)?;
        let transcript = sidecars.transcript_path(&file.path);
        let transcript_string = transcript.to_string_lossy();
        let stored_transcript = db.file_transcript_path(&path)?;
        let unchanged_and_complete = indexed_hash.as_deref() == Some(&file.sha256)
            && match status.as_deref() {
                Some("filtered") => !transcript.is_file(),
                Some("empty") => !transcript.is_file(),
                Some("transcribed") | Some("external_transcript") | Some("metadata_error") => {
                    stored_transcript.as_deref() == Some(transcript_string.as_ref())
                        && transcript.is_file()
                }
                _ => false,
            };
        if unchanged_and_complete {
            unchanged += 1;
        } else {
            if indexed_hash.as_deref() == Some(&file.sha256) && status.as_deref() == Some("error") {
                new_or_modified += 1;
            } else if transcript.is_file()
                && status.as_deref() != Some("error")
                && stored_transcript.as_deref() != Some(transcript_string.as_ref())
            {
                db.save_file(
                    &path,
                    &file.sha256,
                    "external_transcript",
                    Some(transcript_string.as_ref()),
                    None,
                )?;
                existing_transcripts += 1;
            } else {
                db.save_file(&path, &file.sha256, "discovered", None, None)?;
                new_or_modified += 1;
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "directory": directory.display().to_string(),
            "discovered": files.len(),
            "new_or_modified": new_or_modified,
            "unchanged": unchanged,
            "matching_transcript": existing_transcripts
        }))?
    );
    Ok(())
}

async fn cli_process(file: &std::path::Path) -> Result<()> {
    let (state, _receiver) = build_state(Config::from_env(), None)?;
    let transcript = queue::direct_process(&state, file).await?;
    println!("{transcript}");
    Ok(())
}

fn cli_audit(
    gedcom_path: &std::path::Path,
    records: &std::path::Path,
    database: &std::path::Path,
) -> Result<()> {
    let db = Database::open(database)?;
    let exclusions = scanner::DirectoryExclusions::load(&db)?;
    let contents = std::fs::read_to_string(gedcom_path)
        .with_context(|| format!("reading GEDCOM {}", gedcom_path.display()))?;
    let gedcom = gedcom::parser::parse(&contents);
    let anomalies = gedcom::rules::check_with_exclusions(&gedcom, records, &exclusions)?;
    println!("{}", serde_json::to_string_pretty(&anomalies)?);
    Ok(())
}
