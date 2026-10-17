use crate::{
    auth::AuthManager,
    config::DEFAULT_SYSTEM_PROMPT,
    db::{FileStatus, JobStatus},
    gedcom::{parser, rules},
    queue,
    state::{api_key, AppState},
    web::{
        story_format,
        story_sources::{self, PersonIdentity},
    },
};
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
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::warn;

#[derive(Serialize)]
pub struct StatusResponse {
    pub job: JobStatus,
    pub files: Vec<FileStatus>,
}

#[derive(Serialize)]
pub struct SettingsResponse {
    pub has_api_key: bool,
    pub system_prompt: String,
}

#[derive(Deserialize)]
pub struct SaveSettingsRequest {
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
}

#[derive(Deserialize)]
pub struct TestKeyRequest {
    pub api_key: Option<String>,
}

#[derive(Deserialize)]
pub struct StoryRequest {
    pub destination: Option<String>,
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
        "message": "Indexed files cleared. Source files and transcripts were preserved."
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
    Ok(Json(SettingsResponse {
        has_api_key,
        system_prompt: state
            .db
            .setting("system_prompt")?
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_owned()),
    }))
}

pub async fn save_settings(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SaveSettingsRequest>,
) -> Result<Json<Value>, ApiError> {
    if let Some(key) = request.api_key.filter(|key| !key.trim().is_empty()) {
        if key.len() > 4096 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "API key is too long".to_owned(),
            ));
        }
        let encrypted = state.cipher.encrypt(key.trim())?;
        state.db.set_setting("gemini_api_key", &encrypted)?;
    }
    if let Some(prompt) = request.system_prompt {
        if prompt.len() > 100_000 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "System prompt exceeds 100,000 characters".to_owned(),
            ));
        }
        state.db.set_setting("system_prompt", prompt.trim())?;
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
    let report_path = state.config.output_dir.join("audit-report.md");
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let gedcom_text = fs::read_to_string(&gedcom_path)?;
        let gedcom = parser::parse(&gedcom_text);
        let anomalies = rules::check(&gedcom, &records)?;
        let facts = collect_frontmatter(&records)?;
        Ok((anomalies, facts))
    })
    .await??;
    let (anomalies, facts) = result;
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
            "Write a human-readable genealogy research anomaly audit. Explain each finding, distinguish evidence from uncertainty, and suggest cautious corrections. Do not claim facts not present in the data.\n\nRule violations:\n{}\n\nTranscript YAML frontmatter facts:\n{}",
            serde_json::to_string_pretty(&anomalies)?,
            facts,
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
    let gedcom = parser::parse(&gedcom_text);
    let graph = crate::gedcom::graph::FamilyGraph::build(&gedcom);
    let person = graph.people.get(xref.as_str()).ok_or_else(|| {
        ApiError(
            StatusCode::NOT_FOUND,
            "GEDCOM individual was not found".to_owned(),
        )
    })?;
    let name = person.name.unwrap_or("Unknown individual");
    let identity = PersonIdentity {
        name: name.to_owned(),
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
    };
    let records = state.config.records_dir.clone();
    let sources = tokio::task::spawn_blocking(move || {
        story_sources::collect_person_sources(&records, &identity)
    })
    .await??;
    if sources.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "No matching transcript frontmatter was found; transcribe relevant records before generating a cited biography".to_owned(),
        ));
    }
    let facts = person_facts(person, &graph);
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
        "Write a clear chronological biography using only the supplied GEDCOM facts and transcriptions. Use Markdown section headings (## or deeper). Immediately after every section heading, with no blank line between, put a line formatted exactly **Confidence: NN%**. This percentage is a qualitative estimate of how strongly the cited evidence supports the claims in that section, not statistical certainty. Cite every historical or biographical claim with an inline Markdown link to one or more source transcripts, using exactly the relative paths listed below. Put each factual sentence or list item on its own line and cite it on that same line; do not leave factual introductory or concluding text uncited. Do not make unsupported claims. Explain conflicting evidence and uncertainty. Heuristic source-match evidence is not proof of identity: compare names, dates, ages, places, and other facts before attributing a source to the subject. If identity remains uncertain, say so and lower the section confidence. Include a Sources section whose entries link to the cited transcripts. Return only the biography.\n\nPERSON FACTS:\n{facts}\n\nALLOWED TRANSCRIPT SOURCE PATHS:\n{allowed_sources}\n\nTRANSCRIPTIONS:\n{source_text}"
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
    let output = if request.destination.as_deref() == Some("person") {
        stories.join(&identifier).join("biography.md")
    } else {
        stories.join(format!("{identifier}.md"))
    };
    if let Some(parent) = output.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&output, story).await?;
    let matched_sources = sources
        .iter()
        .map(|source| {
            json!({
                "path": source.relative_path,
                "evidence": source.match_evidence,
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "path": output,
        "sources": sources.len(),
        "matched_sources": matched_sources,
    })))
}

fn data_directory(state: &AppState) -> PathBuf {
    state
        .config
        .database_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn collect_frontmatter(records: &Path) -> anyhow::Result<String> {
    let mut facts = Vec::new();
    if !records.is_dir() {
        return Ok(String::new());
    }
    for entry in walkdir::WalkDir::new(records).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        let text = fs::read_to_string(entry.path())?;
        if let Some(yaml) = frontmatter(&text) {
            facts.push(format!("{}:\n{}", entry.path().display(), yaml));
        }
    }
    Ok(facts.join("\n\n"))
}

fn frontmatter(text: &str) -> Option<&str> {
    let content = text.strip_prefix("---")?;
    let end = content.find("\n---")?;
    Some(&content[..end])
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
