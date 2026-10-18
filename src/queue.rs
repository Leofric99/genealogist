use crate::{
    config::DEFAULT_SYSTEM_PROMPT,
    db::JobStatus,
    scanner::{self, DirectoryExclusions, ScannedFile},
    sidecar::SidecarConfig,
    state::{api_key, AppState},
    vision_filter,
};
use anyhow::{Context, Result};
use futures_util::{stream, StreamExt};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const MAX_PARALLEL_FILES: usize = 4;

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
    publish_status(&state).await;
    let root = state.config.records_dir.clone();
    let sidecars = SidecarConfig::load(&state.db)?;
    let exclusions = DirectoryExclusions::load(&state.db)?;
    let scan_cancellation = cancellation.clone();
    let files = tokio::task::spawn_blocking(move || {
        scanner::scan_and_hash_cancellable_with_exclusions(&root, &exclusions, &scan_cancellation)
    })
    .await??;
    let Some(files) = files else {
        return finish_stopped_scan(&state).await;
    };
    state.db.update_job(Some(files.len()), 0, 0, None, None)?;
    publish_status(&state).await;

    let mut output_groups = BTreeMap::<String, Vec<ScannedFile>>::new();
    for file in files {
        let output = sidecars.transcript_path(&file.path);
        output_groups
            .entry(output.to_string_lossy().to_lowercase())
            .or_default()
            .push(file);
    }
    let active_label = format!("Processing up to {MAX_PARALLEL_FILES} documents concurrently");
    let jobs = stream::iter(output_groups.into_values().map(|group| {
        let state = Arc::clone(&state);
        let active_label = active_label.clone();
        let cancellation = cancellation.clone();
        let sidecars = sidecars.clone();
        async move {
            for file in group {
                let source = file.path.to_string_lossy().into_owned();
                state.db.update_job(None, 0, 0, Some(&active_label), None)?;
                publish_status(&state).await;
                let result = process_one(&state, &file, &sidecars, None, Some(&cancellation)).await;
                let (skipped, message) = match result {
                    Ok(ProcessResult::Processed) => (0, None),
                    Ok(ProcessResult::Skipped(reason)) => (1, Some(reason)),
                    Err(error) => {
                        let message = format!("{error:#}");
                        error!(path = %file.path.display(), %message, "document processing failed");
                        state
                            .db
                            .save_file(&source, &file.sha256, "error", None, Some(&message))?;
                        (0, Some(message))
                    }
                };
                state
                    .db
                    .update_job(None, 1, skipped, Some(&active_label), message.as_deref())?;
                publish_status(&state).await;
            }
            Ok::<_, anyhow::Error>(())
        }
    }))
    .buffer_unordered(MAX_PARALLEL_FILES);
    tokio::pin!(jobs);
    loop {
        let next = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return finish_stopped_scan(&state).await,
            next = jobs.next() => next,
        };
        let Some(result) = next else {
            break;
        };
        result?;
    }

    if cancellation.is_cancelled() {
        return finish_stopped_scan(&state).await;
    }
    state.db.finish_job(Some("Scan complete"))?;
    publish_status(&state).await;
    Ok(())
}

async fn finish_stopped_scan(state: &AppState) -> Result<()> {
    state.db.finish_job(Some("Scan stopped by user"))?;
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
    sidecars: &SidecarConfig,
    image_score: Option<vision_filter::VisionScore>,
    cancellation: Option<&CancellationToken>,
) -> Result<ProcessResult> {
    let path = &file.path;
    let source = path.to_string_lossy().into_owned();
    let previous_hash = state.db.file_hash(&source)?;
    let previous_status = state.db.file_status(&source)?;
    let previous_error = state.db.file_error(&source)?;
    let transcript_path = sidecars.transcript_path(path);
    let transcript_path_string = transcript_path.to_string_lossy().into_owned();
    let previous_transcript_path = state.db.file_transcript_path(&source)?;
    let source_unchanged = previous_hash.as_deref() == Some(&file.sha256);
    let transcript_status = matches!(
        previous_status.as_deref(),
        Some("transcribed") | Some("external_transcript") | Some("metadata_error")
    );
    let transcript_is_current = source_unchanged
        && transcript_status
        && previous_transcript_path.as_deref() == Some(transcript_path_string.as_str())
        && transcript_path.is_file();

    if transcript_is_current {
        if previous_error.is_some() || previous_status.as_deref() == Some("metadata_error") {
            state.db.save_file(
                &source,
                &file.sha256,
                if previous_status.as_deref() == Some("external_transcript") {
                    "external_transcript"
                } else {
                    "transcribed"
                },
                Some(&transcript_path_string),
                None,
            )?;
        }
        return Ok(ProcessResult::Skipped(
            "Unchanged file already handled".to_owned(),
        ));
    }

    if source_unchanged
        && matches!(previous_status.as_deref(), Some("filtered") | Some("empty"))
        && !transcript_path.is_file()
    {
        return Ok(ProcessResult::Skipped(
            "Unchanged file already handled".to_owned(),
        ));
    }

    let is_pdf = file.extension == "pdf";
    if transcript_path.is_file()
        && previous_hash
            .as_deref()
            .map_or(true, |hash| hash == file.sha256)
        && previous_status.as_deref() != Some("error")
        && previous_transcript_path.as_deref() != Some(transcript_path_string.as_str())
    {
        state.db.save_file(
            &source,
            &file.sha256,
            "external_transcript",
            Some(&transcript_path_string),
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
            let reason = format!(
                "Native image filter classified as non-document (edge density {:.4}, contrast variance {:.4}, bimodal fraction {:.4}, horizontal line rows {}). This may be a false negative for low-contrast or unusual scans.",
                score.edge_density,
                score.normalized_variance,
                score.bimodal_fraction,
                score.horizontal_line_rows
            );
            state
                .db
                .save_file(&source, &file.sha256, "filtered", None, Some(&reason))?;
            info!(path = %path.display(), edge_density = score.edge_density, normalized_variance = score.normalized_variance, bimodal_fraction = score.bimodal_fraction, horizontal_line_rows = score.horizontal_line_rows, "native filter skipped likely non-document image");
            return Ok(ProcessResult::Skipped(reason));
        }
    }

    let key = api_key(state)?;
    let system_prompt = state
        .db
        .setting("system_prompt")?
        .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned());
    let transcript = if is_pdf {
        transcribe_pdf(state, path, &key, &system_prompt, sidecars, cancellation).await?
    } else {
        let is_document = match state.gemini.validate_image(path, &key).await {
            Ok(is_document) => is_document,
            Err(error) if crate::gemini_client::is_no_candidate_text(&error) => {
                return save_empty_image(state, &source, &file.sha256).map(|()| {
                    ProcessResult::Skipped("Gemini returned no text for this image".to_owned())
                });
            }
            Err(error) => return Err(error),
        };
        if !is_document {
            let reason =
                "Gemini Flash-Lite classified this image as a photo, so transcription was skipped."
                    .to_owned();
            state
                .db
                .save_file(&source, &file.sha256, "filtered", None, Some(&reason))?;
            return Ok(ProcessResult::Skipped(reason));
        }
        match state
            .gemini
            .transcribe_file(path, &key, &system_prompt)
            .await
        {
            Ok(transcript) => transcript,
            Err(error) if crate::gemini_client::is_non_document(&error) => {
                let reason = "Gemini transcription safeguard classified this image as a non-document; no transcript was created.".to_owned();
                state
                    .db
                    .save_file(&source, &file.sha256, "filtered", None, Some(&reason))?;
                info!(path = %path.display(), "Gemini transcription safeguard skipped non-document image");
                return Ok(ProcessResult::Skipped(reason));
            }
            Err(error) if crate::gemini_client::is_no_candidate_text(&error) => {
                return save_empty_image(state, &source, &file.sha256).map(|()| {
                    ProcessResult::Skipped("Gemini returned no text for this image".to_owned())
                });
            }
            Err(error) => return Err(error),
        }
    };

    write_sidecar(&transcript_path, &transcript).await?;
    state.db.save_file(
        &source,
        &file.sha256,
        "transcribed",
        Some(&transcript_path_string),
        None,
    )?;
    Ok(ProcessResult::Processed)
}

fn save_empty_image(state: &AppState, source: &str, sha256: &str) -> Result<()> {
    state.db.save_file(source, sha256, "empty", None, None)
}

async fn transcribe_pdf(
    state: &AppState,
    path: &Path,
    key: &str,
    prompt: &str,
    sidecars: &SidecarConfig,
    cancellation: Option<&CancellationToken>,
) -> Result<String> {
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        anyhow::bail!("scan cancelled");
    }
    match state.gemini.transcribe_file(path, key, prompt).await {
        Ok(transcript) => return Ok(transcript),
        Err(error) if crate::gemini_client::is_pdf_size_rejection_error(&error) => {
            warn!(
                path = %path.display(),
                %error,
                "Gemini rejected the full PDF; retrying in groups of ten pages"
            );
        }
        Err(error) => return Err(error),
    }

    let count_path = path.to_path_buf();
    let pages = tokio::task::spawn_blocking(move || crate::pdf::page_count(&count_path)).await??;
    if pages == 0 {
        anyhow::bail!("PDF contains no pages");
    }
    let tempdir = tempfile::tempdir().context("creating temporary PDF page directory")?;
    let source = path.to_path_buf();
    let page_dir = tempdir.path().to_path_buf();
    let split_cancellation = cancellation.cloned();
    let page_paths = tokio::task::spawn_blocking(move || {
        let cancellation = split_cancellation.unwrap_or_else(CancellationToken::new);
        crate::pdf::split_into_page_groups_cancellable(&source, &page_dir, 10, &cancellation)
    })
    .await??
    .context("PDF page splitting was cancelled")?;
    let mut transcripts = Vec::with_capacity(page_paths.len());
    let mut has_group_output = false;
    for (index, page_path) in page_paths.iter().enumerate() {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            anyhow::bail!("scan cancelled");
        }
        let transcript = match state.gemini.transcribe_file(page_path, key, prompt).await {
            Ok(transcript) if !transcript.trim().is_empty() => {
                has_group_output = true;
                transcript
            }
            Ok(_) => {
                info!(
                    pdf = %path.display(),
                    group = index + 1,
                    "Gemini returned no transcription text for PDF page group"
                );
                no_page_text_marker()
            }
            Err(error) if crate::gemini_client::is_no_candidate_text(&error) => {
                info!(
                    pdf = %path.display(),
                    group = index + 1,
                    "Gemini returned no transcription text for PDF page group"
                );
                no_page_text_marker()
            }
            Err(error) => return Err(error),
        };
        let first_page = index * 10 + 1;
        let last_page = (first_page + 9).min(pages);
        let group_target = sidecars.page_group_transcript_path(path, first_page, last_page);
        write_sidecar(&group_target, &transcript).await?;
        transcripts.push(transcript);
    }
    if !has_group_output {
        anyhow::bail!("Gemini returned no transcription text for any PDF page group");
    }
    state.gemini.consolidate(&transcripts, key, prompt).await
}

fn no_page_text_marker() -> String {
    crate::gemini_client::NO_PAGE_TEXT_MARKER.to_owned()
}

async fn write_sidecar(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating transcript directory {}", parent.display()))?;
        }
    }
    tokio::fs::write(path, contents)
        .await
        .with_context(|| format!("writing transcript {}", path.display()))
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
    let exclusions = DirectoryExclusions::load(&state.db)?;
    if exclusions.contains_path(&state.config.records_dir, path) {
        anyhow::bail!("file is inside an excluded folder: {}", path.display());
    }
    let hash_path = path.to_path_buf();
    let one = tokio::task::spawn_blocking(move || scanner::hash_file(&hash_path)).await??;
    let sidecars = SidecarConfig::load(&state.db)?;
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
    let result = match process_one(state, &one, &sidecars, image_score, None).await? {
        ProcessResult::Processed => {
            let transcript = sidecars.transcript_path(path);
            format!("Transcript: {}", transcript.display())
        }
        ProcessResult::Skipped(reason) => format!("Skipped: {reason}"),
    };
    Ok(format!("{score_summary}{result}"))
}
