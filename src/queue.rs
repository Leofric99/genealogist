use crate::{
    config::DEFAULT_SYSTEM_PROMPT,
    db::JobStatus,
    scanner::{self, ScannedFile},
    state::{api_key, AppState},
    vision_filter,
};
use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
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
    let cancellation = state
        .scan_cancellation
        .lock()
        .map_err(|_| anyhow::anyhow!("scan cancellation lock poisoned"))?
        .clone();
    state.db.reset_job_progress()?;
    publish_log(&state, "Directory scan started");
    publish_status(&state).await;
    let root = state.config.records_dir.clone();
    let scan_cancellation = cancellation.clone();
    let files = tokio::task::spawn_blocking(move || {
        scanner::scan_and_hash_cancellable(&root, &scan_cancellation)
    })
    .await??;
    let Some(files) = files else {
        return finish_stopped_scan(&state).await;
    };
    publish_log(
        &state,
        &format!(
            "Discovered {} supported files and completed hashing",
            files.len()
        ),
    );
    state.db.update_job(Some(files.len()), 0, 0, None, None)?;
    publish_status(&state).await;

    for file in files {
        if cancellation.is_cancelled() {
            return finish_stopped_scan(&state).await;
        }
        let source = file.path.to_string_lossy().into_owned();
        publish_log(&state, &format!("Processing {source}"));
        state.db.update_job(None, 0, 0, Some(&source), None)?;
        publish_status(&state).await;
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return finish_stopped_scan(&state).await;
            }
            result = process_one(&state, &file, None, Some(&cancellation)) => result,
        };
        match result {
            Ok(ProcessResult::Processed) => {
                publish_log(&state, &format!("Completed processing {source}"));
                state.db.update_job(None, 1, 0, None, None)?;
            }
            Ok(ProcessResult::Skipped(reason)) => {
                publish_log(&state, &format!("Skipped {source}: {reason}"));
                state.db.update_job(None, 1, 1, None, Some(&reason))?;
            }
            Err(error) => {
                let message = format!("{error:#}");
                error!(path = %file.path.display(), %message, "document processing failed");
                publish_log(&state, &format!("Error processing {source}: {message}"));
                state
                    .db
                    .save_file(&source, &file.sha256, "error", None, Some(&message))?;
                state.db.update_job(None, 1, 0, None, Some(&message))?;
            }
        }
        publish_status(&state).await;
    }

    if cancellation.is_cancelled() {
        return finish_stopped_scan(&state).await;
    }
    state.db.finish_job(Some("Scan complete"))?;
    publish_log(&state, "Directory scan completed");
    publish_status(&state).await;
    Ok(())
}

async fn finish_stopped_scan(state: &AppState) -> Result<()> {
    state.db.finish_job(Some("Scan stopped by user"))?;
    publish_log(state, "Directory scan stopped by user");
    publish_status(state).await;
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
    cancellation: Option<&CancellationToken>,
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
                let filter_cancellation = cancellation.cloned();
                tokio::task::spawn_blocking(move || match filter_cancellation {
                    Some(cancellation) => {
                        vision_filter::inspect_cancellable(&filter_path, &cancellation)
                    }
                    None => vision_filter::inspect(&filter_path).map(Some),
                })
                .await??
                .context("image inspection was cancelled")?
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
        transcribe_pdf(state, path, &key, &system_prompt, cancellation).await?
    } else {
        publish_log(
            state,
            &format!("Sent image validation API request to Gemini for {source}"),
        );
        let is_document = state.gemini.validate_image(path, &key).await?;
        publish_log(
            state,
            &format!("Received image validation API response from Gemini for {source}"),
        );
        if !is_document {
            state
                .db
                .save_file(&source, &file.sha256, "filtered", None, None)?;
            return Ok(ProcessResult::Skipped(
                "Gemini classified image as a photo".to_owned(),
            ));
        }
        publish_log(
            state,
            &format!("Sent transcription API request to Gemini for {source}"),
        );
        let transcript = state
            .gemini
            .transcribe_file(path, &key, &system_prompt)
            .await?;
        publish_log(
            state,
            &format!("Received transcription API response from Gemini for {source}"),
        );
        transcript
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

async fn transcribe_pdf(
    state: &AppState,
    path: &Path,
    key: &str,
    prompt: &str,
    cancellation: Option<&CancellationToken>,
) -> Result<String> {
    let count_path = path.to_path_buf();
    let pages = tokio::task::spawn_blocking(move || crate::pdf::page_count(&count_path)).await??;
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        anyhow::bail!("scan cancelled");
    }
    if pages <= 10 {
        let source = path.to_string_lossy();
        publish_log(
            state,
            &format!("Sent transcription API request to Gemini for {source}"),
        );
        let transcript = state.gemini.transcribe_file(path, key, prompt).await?;
        publish_log(
            state,
            &format!("Received transcription API response from Gemini for {source}"),
        );
        return Ok(transcript);
    }
    let tempdir = tempfile::tempdir().context("creating temporary PDF page directory")?;
    let source = path.to_path_buf();
    let page_dir = tempdir.path().to_path_buf();
    let split_cancellation = cancellation.cloned();
    let page_paths = tokio::task::spawn_blocking(move || match split_cancellation {
        Some(cancellation) => {
            crate::pdf::split_to_single_pages_cancellable(&source, &page_dir, &cancellation)
        }
        None => crate::pdf::split_to_single_pages(&source, &page_dir).map(Some),
    })
    .await??
    .context("PDF page splitting was cancelled")?;
    let mut transcripts = Vec::with_capacity(page_paths.len());
    for (index, page_path) in page_paths.iter().enumerate() {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            anyhow::bail!("scan cancelled");
        }
        let source = path.to_string_lossy();
        publish_log(
            state,
            &format!(
                "Sent transcription API request to Gemini for {source} (page {}/{})",
                index + 1,
                page_paths.len()
            ),
        );
        let transcript = state.gemini.transcribe_file(page_path, key, prompt).await?;
        publish_log(
            state,
            &format!(
                "Received transcription API response from Gemini for {source} (page {}/{})",
                index + 1,
                page_paths.len()
            ),
        );
        let page_target = page_transcript_path(path, index + 1);
        tokio::fs::write(&page_target, &transcript)
            .await
            .with_context(|| format!("writing page transcript {}", page_target.display()))?;
        transcripts.push(transcript);
    }
    let source = path.to_string_lossy();
    publish_log(
        state,
        &format!("Sent transcription consolidation request to Gemini for {source}"),
    );
    let master = state.gemini.consolidate(&transcripts, key, prompt).await?;
    publish_log(
        state,
        &format!("Received transcription consolidation response from Gemini for {source}"),
    );
    Ok(master)
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
        Ok(status) => match serde_json::to_string(&serde_json::json!({
            "type": "status",
            "job": status,
        })) {
            Ok(message) => {
                let _ = state.progress_tx.send(message);
            }
            Err(error) => warn!(%error, "could not serialize job progress"),
        },
        Err(error) => warn!(%error, "could not read persisted job progress"),
    }
}

pub fn publish_log(state: &AppState, message: &str) {
    let timestamp = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(timestamp) => timestamp.as_millis(),
        Err(error) => {
            warn!(%error, "could not timestamp activity log event");
            return;
        }
    };
    match serde_json::to_string(&serde_json::json!({
        "type": "log",
        "timestamp": timestamp,
        "message": message,
    })) {
        Ok(event) => {
            match state.activity_log.lock() {
                Ok(mut history) => {
                    if history.len() == 100 {
                        history.pop_front();
                    }
                    history.push_back(event.clone());
                }
                Err(error) => warn!(%error, "could not retain activity log event"),
            }
            let _ = state.progress_tx.send(event);
        }
        Err(error) => warn!(%error, "could not serialize activity log event"),
    }
}

pub async fn start_job(state: &Arc<AppState>) -> Result<bool> {
    if !state.db.begin_job()? {
        return Ok(false);
    }
    *state
        .scan_cancellation
        .lock()
        .map_err(|_| anyhow::anyhow!("scan cancellation lock poisoned"))? =
        CancellationToken::new();
    publish_status(state).await;
    if state.queue_tx.send(()).await.is_err() {
        state
            .db
            .finish_job(Some("Background worker is unavailable"))?;
        anyhow::bail!("background worker is unavailable");
    }
    Ok(true)
}

pub fn request_stop(state: &AppState) -> Result<bool> {
    if !state.db.job_status()?.running {
        return Ok(false);
    }
    state
        .scan_cancellation
        .lock()
        .map_err(|_| anyhow::anyhow!("scan cancellation lock poisoned"))?
        .cancel();
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
    let result = match process_one(state, &one, image_score, None).await? {
        ProcessResult::Processed => format!("Transcript: {}", transcript_path(path).display()),
        ProcessResult::Skipped(reason) => format!("Skipped: {reason}"),
    };
    Ok(format!("{score_summary}{result}"))
}
