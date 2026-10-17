mod config;
mod db;
mod gedcom;
mod gemini_client;
mod pdf;
mod queue;
mod scanner;
mod secret;
mod state;
mod vision_filter;
mod web;

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::{Parser, Subcommand};
use config::Config;
use db::Database;
use gemini_client::GeminiClient;
use secret::SecretCipher;
use state::AppState;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{broadcast, mpsc};
use tower_http::{limit::RequestBodyLimitLayer, trace::TraceLayer};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "genealogy-ocr-studio",
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
        Command::Audit { gedcom, records } => cli_audit(&gedcom, &records),
    }
}

fn build_state(config: Config) -> Result<(Arc<AppState>, mpsc::Receiver<()>)> {
    let db = Database::open(&config.database_path)?;
    let cipher = SecretCipher::load(&config.database_path)?;
    let gemini = GeminiClient::new(config.gemini_model.clone(), config.validation_model.clone())?;
    let (queue_tx, queue_rx) = mpsc::channel(1);
    let (progress_tx, _) = broadcast::channel(64);
    Ok((
        Arc::new(AppState {
            config,
            db,
            cipher,
            gemini,
            queue_tx,
            progress_tx,
        }),
        queue_rx,
    ))
}

async fn serve() -> Result<()> {
    let config = Config::from_env();
    let bind_addr = config.bind_addr.clone();
    let (state, receiver) = build_state(config)?;
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
        .route("/", get(web::views::dashboard))
        .route("/settings", get(web::views::settings))
        .route("/audit", get(web::views::audit))
        .route("/ws", get(web::websocket::handler))
        .route("/partials/status", get(web::handlers::status_fragment))
        .route("/api/status", get(web::handlers::status))
        .route("/api/scan", post(web::handlers::start_scan))
        .route(
            "/api/settings",
            get(web::handlers::get_settings).post(web::handlers::save_settings),
        )
        .route("/api/settings/test", post(web::handlers::test_key))
        .route("/api/gedcom", post(web::handlers::upload_gedcom))
        .route("/api/audit", post(web::handlers::run_audit))
        .route("/api/story/:xref", post(web::handlers::generate_story))
        .layer(RequestBodyLimitLayer::new(25 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding web server at {bind_addr}"))?;
    tracing::info!(%bind_addr, "Genealogy-OCR-Studio is ready");
    axum::serve(listener, app).await?;
    Ok(())
}

fn cli_scan(directory: &std::path::Path, database: &std::path::Path) -> Result<()> {
    let db = Database::open(database)?;
    let files = scanner::scan_and_hash(directory)?;
    let mut new_or_modified = 0usize;
    let mut unchanged = 0usize;
    let mut existing_transcripts = 0usize;
    for file in &files {
        let path = file.path.to_string_lossy().into_owned();
        if db.file_hash(&path)?.as_deref() == Some(&file.sha256) {
            unchanged += 1;
        } else {
            let transcript = file.path.with_extension("md");
            if db.file_hash(&path)?.is_none() && transcript.is_file() {
                db.save_file(
                    &path,
                    &file.sha256,
                    "external_transcript",
                    Some(&transcript.to_string_lossy()),
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
    let (state, _receiver) = build_state(Config::from_env())?;
    let transcript = queue::direct_process(&state, file).await?;
    println!("{transcript}");
    Ok(())
}

fn cli_audit(gedcom_path: &std::path::Path, records: &std::path::Path) -> Result<()> {
    let contents = std::fs::read_to_string(gedcom_path)
        .with_context(|| format!("reading GEDCOM {}", gedcom_path.display()))?;
    let gedcom = gedcom::parser::parse(&contents);
    let anomalies = gedcom::rules::check(&gedcom, records)?;
    println!("{}", serde_json::to_string_pretty(&anomalies)?);
    Ok(())
}
