use crate::{
    config::DEFAULT_SYSTEM_PROMPT,
    scanner::{self, ScannedFile},
    state::{api_key, AppState},
    vision_filter,
};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub fn spawn_worker(state: Arc<AppState>, mut receiver: mpsc::Receiver<()>) {
    tokio::spawn(async move {
        while receiver.recv().await.is_some() {
            let state = Arc::clone(&state);
            if let Err(error) = run_scan(Arc::clone(&state)).await {
                let message = format!("Scan failed: {error:#}");
                error!(%error, "scan job failed");
                if let Err(finish_error) = state.db.finish_job(Some(&message)) {
                    error!(%finish_error, "failed to persist scan failure");
                }
                publish_status(&state).await;
            }
        }
    });
}

pub async fn run_scan(state: Arc<AppState>) -> Result<()> {
    state.db.reset_job_progress()?;
    publish_status(&state).await;
    let root = state.config.records_dir.clone();
    let files = tokio::task::spawn_blocking(move || scanner::scan_and_hash(&root)).await??;
    state.db.update_job(Some(files.len()), 0, 0, None, None)?;
    publish_status(&state).await;

    for file in files {
        let source = file.path.to_string_lossy().into_owned();
        state.db.update_job(None, 0, 0, Some(&source), None)?;
        publish_status(&state).await;
        match process_one(&state, &file, None).await {
            Ok(ProcessResult::Processed) => {
                state.db.update_job(None, 1, 0, None, None)?;
            }
            Ok(ProcessResult::Skipped(reason)) => {
                state.db.update_job(None, 1, 1, None, Some(&reason))?;
            }
            Err(error) => {
                let message = format!("{error:#}");
                error!(path = %file.path.display(), %message, "document processing failed");
                state
                    .db
                    .save_file(&source, &file.sha256, "error", None, Some(&message))?;
                state.db.update_job(None, 1, 0, None, Some(&message))?;
            }
        }
        publish_status(&state).await;
    }

    state.db.finish_job(Some("Scan complete"))?;
    publish_status(&state).await;
    Ok(())
}

enum ProcessResult {
    Processed,
    Skipped(String),
}

async fn process_one(
    state: &AppState,
    file: &ScannedFile,
    image_score: Option<vision_filter::VisionScore>,
) -> Result<ProcessResult> {
    let path = &file.path;
    let source = path.to_string_lossy().into_owned();
    let previous_hash = state.db.file_hash(&source)?;
    let previous_status = state.db.file_status(&source)?;
    if previous_hash.as_deref() == Some(&file.sha256)
        && matches!(
            previous_status.as_deref(),
            Some("transcribed" | "filtered" | "external_transcript")
        )
    {
        return Ok(ProcessResult::Skipped(
            "Unchanged file already handled".to_owned(),
        ));
    }

    let transcript_path = transcript_path(path);
    let is_pdf = file.extension == "pdf";
    if transcript_path.is_file()
        && previous_hash
            .as_deref()
            .map_or(true, |hash| hash == file.sha256)
        && previous_status.as_deref() != Some("transcribed")
    {
        let transcript = transcript_path.to_string_lossy().into_owned();
        state.db.save_file(
            &source,
            &file.sha256,
            "external_transcript",
            Some(&transcript),
            None,
        )?;
        return Ok(ProcessResult::Skipped(
            "Matching transcript already exists".to_owned(),
        ));
    }
    if !is_pdf {
        let score = match image_score {
            Some(score) => score,
            None => {
                let filter_path = path.clone();
                tokio::task::spawn_blocking(move || vision_filter::inspect(&filter_path)).await??
            }
        };
        if !score.likely_document {
            state
                .db
                .save_file(&source, &file.sha256, "filtered", None, None)?;
            info!(path = %path.display(), edge_density = score.edge_density, "native filter skipped likely non-document image");
            return Ok(ProcessResult::Skipped(
                "Skipped by native image pre-filter".to_owned(),
            ));
        }
    }

    let key = api_key(state)?;
    let system_prompt = state
        .db
        .setting("system_prompt")?
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    let transcript = if is_pdf {
        transcribe_pdf(state, path, &key, &system_prompt).await?
    } else {
        if !state.gemini.validate_image(path, &key).await? {
            state
                .db
                .save_file(&source, &file.sha256, "filtered", None, None)?;
            return Ok(ProcessResult::Skipped(
                "Gemini classified image as a photo".to_owned(),
            ));
        }
        state
            .gemini
            .transcribe_file(path, &key, &system_prompt)
            .await?
    };

    tokio::fs::write(&transcript_path, transcript)
        .await
        .with_context(|| format!("writing transcript {}", transcript_path.display()))?;
    let target = transcript_path.to_string_lossy().into_owned();
    state
        .db
        .save_file(&source, &file.sha256, "transcribed", Some(&target), None)?;
    Ok(ProcessResult::Processed)
}

async fn transcribe_pdf(state: &AppState, path: &Path, key: &str, prompt: &str) -> Result<String> {
    let count_path = path.to_path_buf();
    let pages = tokio::task::spawn_blocking(move || crate::pdf::page_count(&count_path)).await??;
    if pages <= 10 {
        return state.gemini.transcribe_file(path, key, prompt).await;
    }
    let tempdir = tempfile::tempdir().context("creating temporary PDF page directory")?;
    let source = path.to_path_buf();
    let page_dir = tempdir.path().to_path_buf();
    let page_paths =
        tokio::task::spawn_blocking(move || crate::pdf::split_to_single_pages(&source, &page_dir))
            .await??;
    let mut transcripts = Vec::with_capacity(page_paths.len());
    for (index, page_path) in page_paths.iter().enumerate() {
        let transcript = state.gemini.transcribe_file(page_path, key, prompt).await?;
        let page_target = page_transcript_path(path, index + 1);
        tokio::fs::write(&page_target, &transcript)
            .await
            .with_context(|| format!("writing page transcript {}", page_target.display()))?;
        transcripts.push(transcript);
    }
    state.gemini.consolidate(&transcripts, key, prompt).await
}

fn transcript_path(source: &Path) -> PathBuf {
    source.with_extension("md")
}

fn page_transcript_path(source: &Path, page: usize) -> PathBuf {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("document");
    source.with_file_name(format!("{stem}.page-{page:04}.md"))
}

pub async fn publish_status(state: &AppState) {
    match state.db.job_status() {
        Ok(status) => match serde_json::to_string(&status) {
            Ok(message) => {
                let _ = state.progress_tx.send(message);
            }
            Err(error) => warn!(%error, "could not serialize job progress"),
        },
        Err(error) => warn!(%error, "could not read persisted job progress"),
    }
}

pub async fn start_job(state: &Arc<AppState>) -> Result<bool> {
    if !state.db.begin_job()? {
        return Ok(false);
    }
    publish_status(state).await;
    if state.queue_tx.send(()).await.is_err() {
        state
            .db
            .finish_job(Some("Background worker is unavailable"))?;
        anyhow::bail!("background worker is unavailable");
    }
    Ok(true)
}

pub fn progress_snapshot(state: &AppState) -> Result<JobStatus> {
    state.db.job_status()
}

pub async fn direct_process(state: &AppState, path: &Path) -> Result<String> {
    if !scanner::is_supported(path) {
        anyhow::bail!("unsupported file extension: {}", path.display());
    }
    let hash_path = path.to_path_buf();
    let one = tokio::task::spawn_blocking(move || scanner::hash_file(&hash_path)).await??;
    let image_score = if one.extension == "pdf" {
        None
    } else {
        let filter_path = one.path.clone();
        Some(tokio::task::spawn_blocking(move || vision_filter::inspect(&filter_path)).await??)
    };
    let score_summary = image_score.as_ref().map(|score| {
        format!(
            "Vision filter: likely_document={}, edge_density={:.4}, normalized_variance={:.4}, bimodal_fraction={:.4}, horizontal_line_rows={}\n",
            score.likely_document,
            score.edge_density,
            score.normalized_variance,
            score.bimodal_fraction,
            score.horizontal_line_rows
        )
    }).unwrap_or_default();
    let result = match process_one(state, &one, image_score).await? {
        ProcessResult::Processed => format!("Transcript: {}", transcript_path(path).display()),
        ProcessResult::Skipped(reason) => format!("Skipped: {reason}"),
    };
    Ok(format!("{score_summary}{result}"))
}
