use crate::{
    config::DEFAULT_SYSTEM_PROMPT,
    db::{FileStatus, JobStatus},
    gedcom::{parser, rules},
    queue, scanner,
    sidecar::SidecarConfig,
    state::{api_key, AppState},
    web::{
        story_format,
        story_sources::{self, PersonIdentity},
    },
};
use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Multipart, Path as AxumPath, State},
    http::{header, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::warn;

const LIFE_STORY_REVIEW_PROMPT: &str = r#"You are a careful genealogical researcher reviewing OCR transcriptions of historical records. Read every supplied transcript in full before deciding what it says. Treat the transcription as evidence, not as unquestionable truth; preserve uncertainty, distinguish the subject from people with similar names, and never add facts from outside the supplied GEDCOM and source material.

First identify each source's structure and conventions, then extract only the entries relevant to the requested person. Historical sources vary widely: civil birth, marriage and death registers and indexes; parish baptism, marriage and burial registers; census schedules; probate calendars, wills and inventories; military service, pension and casualty lists; electoral rolls and directories; passenger, immigration and naturalization lists; newspapers and obituaries; school, employment, land and court records; cemetery and memorial inscriptions; family letters, diaries, photographs and captions; and other forms, lists or tables. Do not force a source into a format it does not use.

Read tables, columns, headings, page breaks, ditto marks, braces, indentation and repeated labels as structural evidence. In surname-grouped indexes, the surname may be printed only once at the start of a group, column or page while later entries show only given and middle names. Apply that surname to following entries only while the layout or wording clearly continues the group; stop at a new surname, heading, column or page boundary, or other clear change. Join the inherited surname to each relevant given/middle name when identifying that entry, but make clear when this is a structural reconstruction rather than a fully printed name. Do not mistake a group heading for a person, carry a surname across an unclear boundary, or combine separate entries. Preserve distinct people, rows, dates, registration districts, page/volume references and relationships. For census and household records, distinguish the head, spouse, children, visitors, lodgers and neighboring households. For indexes, distinguish the indexed event subject from informants, relatives, witnesses and other names appearing nearby.

Compare names alongside dates, ages, places, relationships, occupations and record context. OCR spelling similarity alone does not prove identity. Use the provided heuristic match clues only as leads; confirm them against the transcription and GEDCOM, and state when identity or surname expansion remains uncertain. Resolve no conflict by silently choosing one version: cite the competing evidence and explain the uncertainty.

Relationship labels such as Father, Mother, spouse, witness, or informant describe that person's relationship to the record subject; they do not make that person the subject of the recorded event. Keep the relationship direction explicit. For example, a child's baptism date belongs to the child, while the named father is evidence of parentage. Do not transfer the child's birth, age, or event facts to a parent, and do not confuse a relative with the requested person.

Write a clear chronological biography using only supported facts. Use Markdown section headings (## or deeper). Immediately after every section heading, with no blank line between, put a line formatted exactly **Confidence: NN%**. This is a qualitative estimate of how strongly the cited evidence supports that section, not statistical certainty. Cite every historical or biographical claim with an inline Markdown link to one or more allowed transcript files, using exactly the relative paths supplied. Put each factual sentence or list item on its own line and cite it on that same line; do not leave factual introductory or concluding text uncited. Include a Sources section whose entries link to cited transcripts. Return only the biography."#;

#[derive(Serialize)]
pub struct StatusResponse {
    pub job: JobStatus,
    pub files: Vec<FileStatus>,
}

#[derive(Deserialize)]
pub struct RetryErrorFileRequest {
    pub path: String,
}

#[derive(Serialize)]
pub struct SettingsResponse {
    pub has_api_key: bool,
    pub system_prompt: String,
    pub sidecar_mode: String,
    pub sidecar_folder: String,
    pub excluded_folders: String,
}

#[derive(Deserialize)]
pub struct SaveSettingsRequest {
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub sidecar_mode: Option<String>,
    pub sidecar_folder: Option<String>,
    pub excluded_folders: Option<String>,
}

#[derive(Deserialize)]
pub struct TestKeyRequest {
    pub api_key: Option<String>,
}

#[derive(Deserialize)]
pub struct StoryRequest {
    pub destination: Option<String>,
    pub selected_sources: Option<Vec<StorySourceSelection>>,
}

#[derive(Deserialize)]
pub struct StorySourceSelection {
    pub transcript_path: String,
    pub transcript_sha256: String,
}

#[derive(Deserialize)]
pub struct StoryBatchRequest {
    pub destination: Option<String>,
}

#[derive(Deserialize)]
pub struct StorySourceLinkRequest {
    pub transcript_path: String,
    pub transcript_sha256: String,
    pub action: String,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

pub async fn require_login(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    if path == "/login" || path == "/api/login" {
        return next.run(request).await;
    }
    let Some(auth) = state.auth.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Web authentication is not configured" })),
        )
            .into_response();
    };
    if let Some(token) = session_cookie(&request) {
        match auth.valid_session(token) {
            Ok(true) => return next.run(request).await,
            Ok(false) => {}
            Err(error) => {
                tracing::error!(%error, "validating web session failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "Could not validate the web session" })),
                )
                    .into_response();
            }
        }
    }
    if path.starts_with("/api/") || path == "/ws" {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "Authentication required" })),
        )
            .into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(request): Json<LoginRequest>,
) -> Result<Response, ApiError> {
    let Some(auth) = state.auth.as_ref() else {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Web authentication is not configured".to_owned(),
        ));
    };
    let Some(token) = auth
        .authenticate(&request.username, &request.password)
        .await?
    else {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Invalid username or password".to_owned(),
        ));
    };
    let cookie = HeaderValue::from_str(&format!(
        "gos_session={token}; Path=/; Max-Age={}; HttpOnly; SameSite=Strict",
        12 * 60 * 60
    ))
    .map_err(|error| ApiError(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    let mut response = Json(json!({ "authenticated": true })).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    if let (Some(auth), Some(token)) = (state.auth.as_ref(), session_cookie(&request)) {
        auth.logout(token)?;
    }
    let mut response = Json(json!({ "authenticated": false })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("gos_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict"),
    );
    Ok(response)
}

fn session_cookie(request: &Request<Body>) -> Option<&str> {
    request
        .headers()
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|value| value.strip_prefix("gos_session="))
}

pub async fn status(State(state): State<Arc<AppState>>) -> Result<Json<StatusResponse>, ApiError> {
    Ok(Json(StatusResponse {
        job: state.db.job_status()?,
        files: state.db.file_statuses()?,
    }))
}

pub async fn error_files(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({
        "files": state.db.error_file_statuses()?
    })))
}

pub async fn retry_error_file(
    State(state): State<Arc<AppState>>,
    Json(request): Json<RetryErrorFileRequest>,
) -> Result<Json<Value>, ApiError> {
    if request.path.len() > 4096 {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "File path exceeds 4,096 characters".to_owned(),
        ));
    }
    if state.db.job_status()?.running {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Wait for the current scan or retry to finish before retrying another file".to_owned(),
        ));
    }
    if state.db.file_status(&request.path)?.as_deref() != Some("error") {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "This file is no longer marked as an error".to_owned(),
        ));
    }
    if !state.db.begin_job()? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Another scan or retry started; try again when it finishes".to_owned(),
        ));
    }

    let retry_state = Arc::clone(&state);
    let path = PathBuf::from(request.path);
    tokio::spawn(async move {
        let result = queue::retry_error_file(&retry_state, &path).await;
        let final_message = match result {
            Ok(message) => format!("Retry complete: {message}"),
            Err(error) => {
                let message = format!("Retry failed: {error:#}");
                tracing::error!(%message, "individual file retry failed");
                message
            }
        };
        if let Err(error) = retry_state.db.finish_job(Some(&final_message)) {
            tracing::error!(%error, "failed to finish individual file retry");
        }
        queue::publish_status(&retry_state).await;
    });
    Ok(Json(json!({
        "started": true,
        "message": "Retry started. Progress is available on the Dashboard."
    })))
}

pub async fn status_fragment(
    State(state): State<Arc<AppState>>,
) -> Result<axum::response::Html<String>, ApiError> {
    let job = state.db.job_status()?;
    let percent = if job.total == 0 {
        0
    } else {
        (job.processed * 100 / job.total).min(100)
    };
    let active = job.active.as_deref().unwrap_or("Idle");
    let html = format!(
        "<div class=\"progress-copy\"><strong>{percent}% complete</strong><span>{} discovered · {} processed · {} skipped</span></div><progress max=\"100\" value=\"{percent}\"></progress><p class=\"muted\">Active: {}</p>",
        job.total, job.processed, job.skipped, html_escape(active)
    );
    Ok(axum::response::Html(html))
}

pub async fn start_scan(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    if queue::start_job(&state).await? {
        Ok(Json(json!({ "started": true, "status": "scan queued" })))
    } else {
        Err(ApiError(
            StatusCode::CONFLICT,
            "A scan is already running".to_owned(),
        ))
    }
}

pub async fn stop_scan(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    if queue::request_stop(&state)? {
        Ok(Json(
            json!({ "stopping": true, "status": "Stop requested; cancelling the active operation" }),
        ))
    } else {
        Ok(Json(
            json!({ "stopping": false, "status": "No scan is currently running" }),
        ))
    }
}

pub async fn clear_index(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    if !state.db.clear_file_index()? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Stop the running scan before clearing the index".to_owned(),
        ));
    }
    queue::publish_status(&state).await;
    Ok(Json(json!({
        "cleared": true,
        "message": "Index cleared. Source files are preserved; the next web scan will re-transcribe documents and replace existing transcripts as each replacement succeeds."
    })))
}

pub async fn get_settings(
    State(state): State<Arc<AppState>>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let encrypted_key = state.db.setting("gemini_api_key")?;
    let has_api_key = if let Some(value) = encrypted_key {
        match state.cipher.decrypt(&value) {
            Ok(key) => !key.trim().is_empty(),
            Err(error) => {
                warn!(%error, "stored API key could not be decrypted");
                false
            }
        }
    } else {
        state
            .config
            .gemini_api_key
            .as_ref()
            .is_some_and(|key| !key.trim().is_empty())
    };
    let sidecars = SidecarConfig::load(&state.db)?;
    let excluded_folders = scanner::DirectoryExclusions::load(&state.db)?;
    Ok(Json(SettingsResponse {
        has_api_key,
        system_prompt: state
            .db
            .setting("system_prompt")?
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned()),
        sidecar_mode: sidecars.mode().to_owned(),
        sidecar_folder: sidecars.folder().to_owned(),
        excluded_folders: excluded_folders.as_text(),
    }))
}

pub async fn save_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SaveSettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    if request
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty() && key.len() > 4096)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "API key is too long".to_owned(),
        ));
    }
    if request
        .system_prompt
        .as_deref()
        .is_some_and(|prompt| prompt.len() > 100_000)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "System prompt exceeds 100,000 characters".to_owned(),
        ));
    }
    if request
        .excluded_folders
        .as_deref()
        .is_some_and(|folders| folders.len() > 20_000)
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Excluded folder list exceeds 20,000 characters".to_owned(),
        ));
    }
    let excluded_folders = request
        .excluded_folders
        .as_deref()
        .map(scanner::DirectoryExclusions::parse)
        .transpose()
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?;
    let sidecars = if request.sidecar_mode.is_some() || request.sidecar_folder.is_some() {
        let current = SidecarConfig::load(&state.db)?;
        let mode = request
            .sidecar_mode
            .as_deref()
            .unwrap_or_else(|| current.mode());
        let folder = request
            .sidecar_folder
            .as_deref()
            .unwrap_or_else(|| current.folder());
        let folder = if mode == crate::sidecar::MODE_SUBFOLDER || !folder.trim().is_empty() {
            folder
        } else {
            current.folder()
        };
        Some(
            SidecarConfig::new(mode, folder)
                .map_err(|error| ApiError(StatusCode::BAD_REQUEST, error.to_string()))?,
        )
    } else {
        None
    };
    if let Some(sidecars) = sidecars {
        let previous = SidecarConfig::load(&state.db)?;
        if previous != sidecars {
            let records_root = state.config.records_dir.clone();
            let old_layout = previous.clone();
            let new_layout = sidecars.clone();
            let moved = tokio::task::spawn_blocking(move || {
                crate::sidecar::relocate_existing_sidecars(&records_root, &old_layout, &new_layout)
            })
            .await??;
            tracing::info!(moved, "relocated existing transcript sidecars");
        }
        state
            .db
            .set_sidecar_settings(sidecars.mode(), sidecars.folder())?;
    }
    if let Some(key) = request.api_key.filter(|key| !key.trim().is_empty()) {
        let encrypted = state.cipher.encrypt(key.trim())?;
        state.db.set_setting("gemini_api_key", &encrypted)?;
    }
    if let Some(prompt) = request.system_prompt {
        state.db.set_setting("system_prompt", prompt.trim())?;
    }
    if let Some(excluded_folders) = excluded_folders {
        state
            .db
            .set_setting("excluded_folders", &excluded_folders.as_text())?;
    }
    Ok(Json(json!({ "saved": true })))
}

pub async fn test_key(
    State(state): State<Arc<AppState>>,
    Json(request): Json<TestKeyRequest>,
) -> Result<Json<Value>, ApiError> {
    let key = match request.api_key.filter(|key| !key.trim().is_empty()) {
        Some(key) => key,
        None => api_key(&state)?,
    };
    let model = state.gemini.test_key(&key).await?;
    let message = format!("Gemini API key test succeeded using {model}");
    Ok(Json(
        json!({ "valid": true, "model": model, "message": message }),
    ))
}

pub async fn upload_gedcom(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<Json<Value>, ApiError> {
    let mut gedcom_bytes = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError(StatusCode::BAD_REQUEST, format!("invalid upload: {error}")))?
    {
        if field.name() == Some("gedcom") || field.file_name().is_some() {
            let bytes = field.bytes().await.map_err(|error| {
                ApiError(StatusCode::BAD_REQUEST, format!("reading upload: {error}"))
            })?;
            gedcom_bytes = Some(bytes);
            break;
        }
    }
    let bytes = gedcom_bytes.ok_or_else(|| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "No GEDCOM file was uploaded".to_owned(),
        )
    })?;
    if bytes.len() > 25 * 1024 * 1024 {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "GEDCOM file exceeds 25 MiB".to_owned(),
        ));
    }
    let content = std::str::from_utf8(&bytes).map_err(|_| {
        ApiError(
            StatusCode::BAD_REQUEST,
            "GEDCOM file must be UTF-8 text".to_owned(),
        )
    })?;
    let parsed = parser::parse(content);
    if parsed.people.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "GEDCOM file contains no individual records".to_owned(),
        ));
    }
    let path = data_directory(&state).join("family.ged");
    tokio::fs::write(&path, &bytes).await?;
    state
        .db
        .set_setting("gedcom_path", &path.to_string_lossy())?;
    Ok(Json(
        json!({ "uploaded": true, "people": parsed.people.len(), "path": path }),
    ))
}

pub async fn run_audit(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let gedcom_path = state
        .db
        .setting("gedcom_path")?
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Upload a GEDCOM file before running an audit".to_owned(),
            )
        })?;
    let records = state.config.records_dir.clone();
    let exclusions = scanner::DirectoryExclusions::load(&state.db)?;
    let report_path = state.config.output_dir.join("audit-report.md");
    let anomalies = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let gedcom_text = fs::read_to_string(&gedcom_path)?;
        let gedcom = parser::parse(&gedcom_text);
        rules::check_with_exclusions(&gedcom, &records, &exclusions)
    })
    .await??;
    let key = if state.db.setting("gemini_api_key")?.is_some()
        || state
            .config
            .gemini_api_key
            .as_ref()
            .is_some_and(|key| !key.trim().is_empty())
    {
        Some(api_key(&state)?)
    } else {
        None
    };
    let narrative = if let Some(key) = key {
        let prompt = format!(
            "Write a human-readable genealogy research anomaly audit. Explain each finding, distinguish evidence from uncertainty, and suggest cautious corrections. Do not claim facts not present in the data. Any transcript or path references in the rule findings are heuristic leads, not verified identity matches.\n\nRule violations:\n{}",
            serde_json::to_string_pretty(&anomalies)?,
        );
        state.gemini.text_prompt(&key, &prompt).await?
    } else {
        format!(
            "# GEDCOM anomaly audit\n\nGemini narrative generation is unavailable because no API key is configured.\n\n{}",
            serde_json::to_string_pretty(&anomalies)?
        )
    };
    tokio::fs::create_dir_all(&state.config.output_dir).await?;
    tokio::fs::write(&report_path, &narrative).await?;
    Ok(Json(json!({
        "anomalies": anomalies,
        "report": narrative,
        "report_path": report_path
    })))
}

pub async fn generate_story(
    State(state): State<Arc<AppState>>,
    AxumPath(xref): AxumPath<String>,
    Json(request): Json<StoryRequest>,
) -> Result<Json<Value>, ApiError> {
    if xref.len() > 64 || !xref.starts_with('@') || !xref.ends_with('@') {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid GEDCOM individual identifier".to_owned(),
        ));
    }
    let gedcom_path = state
        .db
        .setting("gedcom_path")?
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Upload a GEDCOM file before generating biographies".to_owned(),
            )
        })?;
    let gedcom_text = tokio::fs::read_to_string(gedcom_path).await?;
    let tree_key = hex::encode(Sha256::digest(gedcom_text.as_bytes()));
    let gedcom = parser::parse(&gedcom_text);
    let graph = crate::gedcom::graph::FamilyGraph::build(&gedcom);
    let person = graph.people.get(xref.as_str()).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "GEDCOM individual was not found".to_owned(),
        )
    })?;
    let identity = person_identity(&xref, person, &graph, &tree_key);
    let facts = person_facts(person, &graph);
    let generated = create_story(
        &state,
        &xref,
        &identity,
        &facts,
        request.destination.as_deref(),
        request.selected_sources,
        true,
    )
    .await?;
    Ok(Json(json!({
        "path": generated.path,
        "sources": generated.sources.len(),
        "matched_sources": source_summaries(&generated.sources),
    })))
}

struct GeneratedStory {
    path: PathBuf,
    sources: Vec<story_sources::StorySource>,
}

async fn create_story(
    state: &AppState,
    xref: &str,
    identity: &PersonIdentity,
    facts: &str,
    destination: Option<&str>,
    selected_sources: Option<Vec<StorySourceSelection>>,
    discover_sources: bool,
) -> Result<GeneratedStory, ApiError> {
    let records = state.config.records_dir.clone();
    let mut sources = if discover_sources || selected_sources.is_some() {
        select_story_sources(state, &records, identity).await?
    } else {
        load_confirmed_story_sources(state, &records, identity).await?
    };

    if let Some(selected_sources) = selected_sources {
        if selected_sources.is_empty() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "Select at least one transcript file for the biography".to_owned(),
            ));
        }
        let mut selected_by_path = HashMap::new();
        for selection in selected_sources {
            if selection.transcript_path.len() > 4096 || selection.transcript_sha256.len() != 64 {
                return Err(ApiError(
                    StatusCode::BAD_REQUEST,
                    "A selected transcript path or hash is invalid".to_owned(),
                ));
            }
            if let Some(previous_hash) = selected_by_path.insert(
                selection.transcript_path.clone(),
                selection.transcript_sha256.clone(),
            ) {
                if previous_hash != selection.transcript_sha256 {
                    return Err(ApiError(
                        StatusCode::BAD_REQUEST,
                        "A transcript was selected with conflicting versions".to_owned(),
                    ));
                }
            }
        }
        let mut selected = selected_by_path.into_iter().collect::<Vec<_>>();
        selected.sort_by(|left, right| left.0.cmp(&right.0));
        if selected.iter().any(|(path, hash)| {
            !sources
                .iter()
                .any(|source| source.relative_path == *path && source.transcript_sha256 == *hash)
        }) {
            return Err(ApiError(
                StatusCode::CONFLICT,
                "A selected transcript is no longer a current candidate; find sources again"
                    .to_owned(),
            ));
        }
        let person_key = identity.stable_key();
        state
            .db
            .replace_confirmed_transcript_person_links(&person_key, &selected)?;
        sources.retain(|source| {
            selected.iter().any(|(path, hash)| {
                source.relative_path == *path && source.transcript_sha256 == *hash
            })
        });
        for source in &mut sources {
            source.link_status = "confirmed".to_owned();
        }
    } else {
        sources.retain(|source| source.link_status == "confirmed");
    }
    if sources.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "No confirmed transcript sources are available. Find and review transcript candidates, then confirm the sources to use.".to_owned(),
        ));
    }

    let source_text = sources
        .iter()
        .map(|source| {
            format!(
                "SOURCE: {}\nHEURISTIC MATCH EVIDENCE: {}\nTRANSCRIPTION:\n{}",
                source.relative_path,
                source.match_evidence.join("; "),
                source.contents
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    let allowed_sources = sources
        .iter()
        .map(|source| format!("- {}", source.relative_path))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "{LIFE_STORY_REVIEW_PROMPT}\n\nPERSON FACTS:\n{facts}\n\nALLOWED TRANSCRIPT SOURCE PATHS:\n{allowed_sources}\n\nTRANSCRIPTIONS:\n{source_text}"
    );
    let key = api_key(&state)?;
    let story = state.gemini.text_prompt(&key, &prompt).await?;
    let allowed_paths = sources
        .iter()
        .map(|source| source.relative_path.clone())
        .collect::<Vec<_>>();
    story_format::validate(&story, &allowed_paths).map_err(|error| {
        ApiError(
            StatusCode::BAD_GATEWAY,
            format!("Gemini biography failed citation/confidence validation: {error}; no file was written"),
        )
    })?;
    let identifier = safe_component(&xref);
    let stories = state.config.output_dir.join("stories");
    let output = if destination == Some("person") {
        stories.join(&identifier).join("biography.md")
    } else {
        stories.join(format!("{identifier}.md"))
    };
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&output, story).await?;
    Ok(GeneratedStory {
        path: output,
        sources,
    })
}

fn source_summaries(sources: &[story_sources::StorySource]) -> Vec<Value> {
    sources
        .iter()
        .map(|source| {
            json!({
                "path": source.relative_path,
                "evidence": source.match_evidence,
                "confidence": source.confidence,
                "status": source.link_status,
                "transcript_sha256": source.transcript_sha256,
            })
        })
        .collect()
}

struct StoryBatchPerson {
    xref: String,
    identity: PersonIdentity,
    facts: String,
}

pub async fn start_story_batch(
    State(state): State<Arc<AppState>>,
    Json(request): Json<StoryBatchRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    api_key(&state)?;
    let gedcom_path = state
        .db
        .setting("gedcom_path")?
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Upload a GEDCOM file before generating biographies".to_owned(),
            )
        })?;
    let gedcom_text = tokio::fs::read_to_string(gedcom_path).await?;
    let tree_key = hex::encode(Sha256::digest(gedcom_text.as_bytes()));
    let gedcom = parser::parse(&gedcom_text);
    let graph = crate::gedcom::graph::FamilyGraph::build(&gedcom);
    let people = graph
        .people
        .iter()
        .map(|(xref, person)| StoryBatchPerson {
            xref: (*xref).to_owned(),
            identity: person_identity(xref, person, &graph, &tree_key),
            facts: person_facts(person, &graph),
        })
        .collect::<Vec<_>>();
    if people.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "The uploaded GEDCOM contains no individuals".to_owned(),
        ));
    }
    if !state.db.start_story_batch(people.len())? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "A bulk biography job is already running".to_owned(),
        ));
    }
    let status = state.db.story_batch_status()?;
    tokio::spawn(run_story_batch(
        Arc::clone(&state),
        people,
        request.destination,
    ));
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "started": true, "status": status })),
    ))
}

pub async fn story_batch_status(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "status": state.db.story_batch_status()? })))
}

async fn run_story_batch(
    state: Arc<AppState>,
    people: Vec<StoryBatchPerson>,
    destination: Option<String>,
) {
    let mut processed = 0;
    let mut succeeded = 0;
    let mut skipped = 0;
    let mut failed = 0;
    let total = people.len();
    for person in people {
        let message = format!("Generating biography for {}", person.identity.name);
        if let Err(error) = state.db.update_story_batch(
            processed,
            succeeded,
            skipped,
            failed,
            Some(&person.xref),
            Some(&message),
        ) {
            tracing::error!(%error, "updating bulk biography progress failed");
        }
        match create_story(
            &state,
            &person.xref,
            &person.identity,
            &person.facts,
            destination.as_deref(),
            None,
            false,
        )
        .await
        {
            Ok(_) => succeeded += 1,
            Err(error)
                if error.0 == StatusCode::BAD_REQUEST
                    && error.1.starts_with("No confirmed transcript sources") =>
            {
                skipped += 1;
            }
            Err(error) => {
                failed += 1;
                tracing::warn!(
                    person = %person.xref,
                    error = %error.1,
                    "bulk biography generation failed"
                );
            }
        }
        processed += 1;
        if let Err(error) = state.db.update_story_batch(
            processed,
            succeeded,
            skipped,
            failed,
            None,
            Some(&format!("Processed {processed} of {total} individuals")),
        ) {
            tracing::error!(%error, "updating bulk biography progress failed");
        }
    }
    if let Err(error) = state.db.finish_story_batch(succeeded, skipped, failed) {
        tracing::error!(%error, "finishing bulk biography job failed");
    }
}

pub async fn discover_story_sources(
    State(state): State<Arc<AppState>>,
    AxumPath(xref): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    if xref.len() > 64 || !xref.starts_with('@') || !xref.ends_with('@') {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid GEDCOM individual identifier".to_owned(),
        ));
    }
    let gedcom_path = state
        .db
        .setting("gedcom_path")?
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Upload a GEDCOM file before discovering transcript sources".to_owned(),
            )
        })?;
    let gedcom_text = tokio::fs::read_to_string(gedcom_path).await?;
    let tree_key = hex::encode(Sha256::digest(gedcom_text.as_bytes()));
    let gedcom = parser::parse(&gedcom_text);
    let graph = crate::gedcom::graph::FamilyGraph::build(&gedcom);
    let person = graph.people.get(xref.as_str()).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "GEDCOM individual was not found".to_owned(),
        )
    })?;
    let identity = person_identity(&xref, person, &graph, &tree_key);
    let records = state.config.records_dir.clone();
    let sources = select_story_sources(&state, &records, &identity).await?;
    let matched_sources = sources
        .iter()
        .map(|source| {
            json!({
                "path": source.relative_path,
                "evidence": source.match_evidence,
                "confidence": source.confidence,
                "status": source.link_status,
                "transcript_sha256": source.transcript_sha256,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "person_name": identity.name,
        "matched_sources": matched_sources,
    })))
}

pub async fn update_story_source_link(
    State(state): State<Arc<AppState>>,
    AxumPath(xref): AxumPath<String>,
    Json(request): Json<StorySourceLinkRequest>,
) -> Result<Json<Value>, ApiError> {
    if xref.len() > 64 || !xref.starts_with('@') || !xref.ends_with('@') {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid GEDCOM individual identifier".to_owned(),
        ));
    }
    let status = match request.action.as_str() {
        "confirm" => "confirmed",
        "reject" => "rejected",
        _ => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "Source action must be confirm or reject".to_owned(),
            ))
        }
    };
    let gedcom_path = state
        .db
        .setting("gedcom_path")?
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Upload a GEDCOM file before mapping transcript sources".to_owned(),
            )
        })?;
    let gedcom_text = tokio::fs::read_to_string(gedcom_path).await?;
    let tree_key = hex::encode(Sha256::digest(gedcom_text.as_bytes()));
    let gedcom = parser::parse(&gedcom_text);
    let graph = crate::gedcom::graph::FamilyGraph::build(&gedcom);
    let person = graph.people.get(xref.as_str()).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "GEDCOM individual was not found".to_owned(),
        )
    })?;
    let identity = person_identity(&xref, person, &graph, &tree_key);
    let records = state.config.records_dir.clone();
    let exclusions = scanner::DirectoryExclusions::load(&state.db)?;
    let relative_path = request.transcript_path.clone();
    if exclusions.contains_relative(Path::new(&relative_path)) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Transcript is inside an excluded folder".to_owned(),
        ));
    }
    let transcript = tokio::task::spawn_blocking(move || {
        story_sources::read_transcript_with_exclusions(&records, &relative_path, &exclusions)
    })
    .await??
    .ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "Transcript no longer exists in the records directory".to_owned(),
        )
    })?;
    if transcript.transcript_sha256 != request.transcript_sha256 {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Transcript changed; discover sources again before mapping it".to_owned(),
        ));
    }
    let person_key = identity.stable_key();
    let existing_links = state.db.transcript_person_links(&person_key)?;
    if !existing_links.iter().any(|link| {
        link.transcript_path == transcript.relative_path
            && link.transcript_sha256 == transcript.transcript_sha256
    }) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Only a discovered transcript candidate can be confirmed or rejected".to_owned(),
        ));
    }
    if !state.db.set_transcript_person_link_status(
        &person_key,
        &transcript.relative_path,
        &transcript.transcript_sha256,
        status,
    )? {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "Transcript-person mapping changed; retry the action".to_owned(),
        ));
    }
    Ok(Json(json!({
        "path": transcript.relative_path,
        "status": status
    })))
}

async fn select_story_sources(
    state: &AppState,
    records: &Path,
    identity: &PersonIdentity,
) -> Result<Vec<story_sources::StorySource>> {
    let records_root = records.to_path_buf();
    let person = identity.clone();
    let exclusions = scanner::DirectoryExclusions::load(&state.db)?;
    let discovery_exclusions = exclusions.clone();
    let discovered = tokio::task::spawn_blocking(move || {
        story_sources::collect_person_sources_with_exclusions(
            &records_root,
            &person,
            &discovery_exclusions,
        )
    })
    .await??;
    let person_key = identity.stable_key();
    for source in &discovered {
        let evidence = serde_json::to_string(&source.match_evidence)?;
        state.db.suggest_transcript_person_link(
            &person_key,
            &identity.xref,
            &identity.name,
            &source.relative_path,
            &source.transcript_sha256,
            source.confidence,
            &evidence,
        )?;
    }

    let links = state.db.transcript_person_links(&person_key)?;
    let mut selected = discovered
        .into_iter()
        .map(|source| (source.relative_path.clone(), source))
        .collect::<HashMap<_, _>>();
    let mut confirmed_to_load = Vec::new();
    for link in &links {
        if link.status == "confirmed"
            && !selected.contains_key(&link.transcript_path)
            && !exclusions.contains_relative(Path::new(&link.transcript_path))
        {
            confirmed_to_load.push(link.clone());
        }
        if let Some(source) = selected.get_mut(&link.transcript_path) {
            if source.transcript_sha256 == link.transcript_sha256 {
                apply_persisted_link(source, link)?;
            }
        }
    }

    if !confirmed_to_load.is_empty() {
        let records_root = records.to_path_buf();
        let link_exclusions = exclusions;
        let loaded = tokio::task::spawn_blocking(move || {
            confirmed_to_load
                .into_iter()
                .filter_map(|link| {
                    let result = story_sources::read_transcript_with_exclusions(
                        &records_root,
                        &link.transcript_path,
                        &link_exclusions,
                    )
                    .map(|source| source.map(|source| (link, source)));
                    Some(result)
                })
                .collect::<Result<Vec<_>>>()
        })
        .await??;
        for item in loaded {
            if let Some((link, mut source)) = item {
                if source.transcript_sha256 == link.transcript_sha256 {
                    apply_persisted_link(&mut source, &link)?;
                    selected.insert(source.relative_path.clone(), source);
                }
            }
        }
    }

    let mut selected = selected.into_values().collect::<Vec<_>>();
    selected.sort_by(|left, right| {
        right
            .confidence
            .total_cmp(&left.confidence)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    Ok(selected)
}

async fn load_confirmed_story_sources(
    state: &AppState,
    records: &Path,
    identity: &PersonIdentity,
) -> Result<Vec<story_sources::StorySource>> {
    let exclusions = scanner::DirectoryExclusions::load(&state.db)?;
    let links = state
        .db
        .transcript_person_links(&identity.stable_key())?
        .into_iter()
        .filter(|link| {
            link.status == "confirmed"
                && !exclusions.contains_relative(Path::new(&link.transcript_path))
        })
        .collect::<Vec<_>>();
    let records_root = records.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut sources = Vec::new();
        for link in links {
            let Some(mut source) = story_sources::read_transcript_with_exclusions(
                &records_root,
                &link.transcript_path,
                &exclusions,
            )?
            else {
                continue;
            };
            if source.transcript_sha256 == link.transcript_sha256 {
                apply_persisted_link(&mut source, &link)?;
                sources.push(source);
            }
        }
        Ok::<_, anyhow::Error>(sources)
    })
    .await?
}

fn apply_persisted_link(
    source: &mut story_sources::StorySource,
    link: &crate::db::TranscriptPersonLink,
) -> Result<()> {
    source.link_status.clone_from(&link.status);
    source.confidence = link.confidence;
    source.match_evidence = serde_json::from_str(&link.evidence_json)
        .context("parsing stored transcript-person link evidence")?;
    Ok(())
}

fn person_identity(
    xref: &str,
    person: &crate::gedcom::parser::Person<'_>,
    graph: &crate::gedcom::graph::FamilyGraph<'_, '_>,
    tree_key: &str,
) -> PersonIdentity {
    let mut related_names = graph
        .parents
        .get(xref)
        .into_iter()
        .flatten()
        .chain(graph.children.get(xref).into_iter().flatten())
        .chain(graph.spouses.get(xref).into_iter().flatten())
        .filter_map(|related| graph.people.get(related).and_then(|person| person.name))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    related_names.sort();
    related_names.dedup();
    PersonIdentity {
        tree_key: tree_key.to_owned(),
        xref: xref.to_owned(),
        name: person.name.unwrap_or("Unknown individual").to_owned(),
        birth_year: person
            .birth
            .as_ref()
            .and_then(|event| event.date)
            .and_then(parser::date_year),
        birth_place: person
            .birth
            .as_ref()
            .and_then(|event| event.place)
            .map(str::to_owned),
        event_years: person
            .events
            .iter()
            .filter_map(|event| event.date.and_then(parser::date_year))
            .collect(),
        event_places: person
            .events
            .iter()
            .filter_map(|event| event.place.map(str::to_owned))
            .collect(),
        related_names,
    }
}

fn data_directory(state: &AppState) -> PathBuf {
    state
        .config
        .database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn person_facts(
    person: &crate::gedcom::parser::Person<'_>,
    graph: &crate::gedcom::graph::FamilyGraph<'_, '_>,
) -> String {
    let mut lines = vec![
        format!("GEDCOM ID: {}", person.xref),
        format!("Name: {}", person.name.unwrap_or("unknown")),
    ];
    if let Some(event) = &person.birth {
        lines.push(format!(
            "Birth: {}; {}",
            event.date.unwrap_or("unknown date"),
            event.place.unwrap_or("unknown location")
        ));
    }
    if let Some(event) = &person.death {
        lines.push(format!(
            "Death: {}; {}",
            event.date.unwrap_or("unknown date"),
            event.place.unwrap_or("unknown location")
        ));
    }
    if let Some(parents) = graph.parents.get(person.xref) {
        lines.push(format!(
            "Parents: {}",
            parents
                .iter()
                .filter_map(|id| graph.people.get(id).and_then(|p| p.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(children) = graph.children.get(person.xref) {
        lines.push(format!(
            "Children: {}",
            children
                .iter()
                .filter_map(|id| graph.people.get(id).and_then(|p| p.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(spouses) = graph.spouses.get(person.xref) {
        lines.push(format!(
            "Spouses: {}",
            spouses
                .iter()
                .filter_map(|id| graph.people.get(id).and_then(|p| p.name))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for event in &person.events {
        if event.kind != "BIRT" && event.kind != "DEAT" {
            lines.push(format!(
                "{}: {}; {}",
                event.kind,
                event.date.unwrap_or("unknown date"),
                event.place.unwrap_or("unknown location")
            ));
        }
    }
    lines.join("\n")
}

fn safe_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect();
    if sanitized.is_empty() {
        "person".to_owned()
    } else {
        sanitized
    }
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub struct ApiError(pub StatusCode, pub String);

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl From<(StatusCode, String)> for ApiError {
    fn from((status, message): (StatusCode, String)) -> Self {
        Self(status, message)
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        axum::response::IntoResponse::into_response((self.0, Json(json!({ "error": self.1 }))))
    }
}
