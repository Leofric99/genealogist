use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use governor::{
    clock::DefaultClock, state::InMemoryState, DefaultDirectRateLimiter, Quota, RateLimiter,
};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    num::NonZeroU32,
    path::Path,
    sync::{Arc, Mutex},
};

const MAX_API_ATTEMPTS: usize = 3;
pub const NO_PAGE_TEXT_MARKER: &str =
    "[No transcription text was returned for this page; its content is undetermined.]";
pub const NON_DOCUMENT_MARKER: &str = "GENESLEUTH_NON_DOCUMENT";

const WHOLE_IMAGE_LAYOUT_INVARIANT: &str = "Whole-image page and spread rule: process the complete supplied image; do not assume it contains only one page or ignore regions outside the apparent main page. For an open book or register, identify each physical page separately, then transcribe each printed column or list top-to-bottom before moving to the next column from left to right. Never read horizontally across neighboring columns or merge their same-height rows. Keep a partial neighboring or previous-page strip in a separate clearly labeled section; transcribe only what is visible there, do not continue rows across a gutter or edge, and do not carry headings or surnames across page or column boundaries. Preserve visible page, quarter/year, range, and column headings with the entries they govern. In alphabetical indexes, keep each entry's name, district, volume, and page together in its visible row; carry a printed surname only within its own continuous column/group and reset it at a new heading, column, or page. Distinguish actual print or handwriting from gutter shadows, show-through, page curl, and scan artifacts; do not report repeated ghost impressions as marginal notes or duplicate entries. Do not deduplicate distinct rows just because names or references match. Transcribe every visible block through its last visible entry; mark cut-off or uncertain text locally instead of guessing or omitting it. Do not infer missing text from the filename or expected person.";

const IMAGE_PROMPT_INVARIANT: &str = "Mandatory image-only safety rule: if the supplied image is clearly a general photograph, painting, illustration, or other non-document without meaningful historical text, return exactly GENESLEUTH_NON_DOCUMENT and nothing else. Do not use this marker for a scan, screenshot, or photograph of a document.";

fn transcription_prompt_for_mime(system_prompt: &str, mime: &str) -> String {
    let prompt = format!("{system_prompt}\n\n{WHOLE_IMAGE_LAYOUT_INVARIANT}");
    if mime.starts_with("image/") {
        format!("{prompt}\n\n{IMAGE_PROMPT_INVARIANT}")
    } else {
        prompt
    }
}

pub struct GeminiClient {
    http: reqwest::Client,
    limiter: Arc<DefaultDirectRateLimiter>,
    model: String,
    validation_model: String,
    active_models: Mutex<HashMap<[u8; 32], String>>,
}

struct GeneratedText {
    text: String,
    model: String,
}

#[derive(Debug)]
struct ModelQuotaExceeded {
    model: String,
    message: String,
}

#[derive(Debug)]
struct NoCandidateText;

#[derive(Debug)]
struct NonDocument;

#[derive(Debug)]
struct PdfRequiresChunks {
    message: String,
}

impl fmt::Display for PdfRequiresChunks {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Gemini rejected the PDF as too large: {}",
            self.message
        )
    }
}

impl Error for PdfRequiresChunks {}

impl fmt::Display for NoCandidateText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Gemini response contained no candidate text")
    }
}

impl Error for NoCandidateText {}

impl fmt::Display for NonDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Gemini classified the image as a non-document")
    }
}

impl Error for NonDocument {}

impl fmt::Display for ModelQuotaExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Gemini quota exhausted for model {}: {}",
            self.model, self.message
        )
    }
}

impl Error for ModelQuotaExceeded {}

fn api_key_fingerprint(key: &str) -> [u8; 32] {
    let digest = Sha256::digest(key.as_bytes());
    let mut fingerprint = [0_u8; 32];
    fingerprint.copy_from_slice(&digest);
    fingerprint
}

fn is_text_flash_model(model: &Value) -> Option<String> {
    if !model
        .get("supportedGenerationMethods")
        .and_then(Value::as_array)?
        .iter()
        .any(|method| method.as_str() == Some("generateContent"))
    {
        return None;
    }
    let name = model.get("name")?.as_str()?;
    let name = name.strip_prefix("models/").unwrap_or(name);
    let normalized = name.to_ascii_lowercase();
    if !normalized.contains("flash")
        || normalized.contains("preview")
        || ["image", "audio", "live", "tts"]
            .iter()
            .any(|excluded| normalized.contains(excluded))
    {
        return None;
    }
    Some(name.to_owned())
}

impl GeminiClient {
    pub fn new(model: String, validation_model: String) -> Result<Self> {
        let quota = Quota::per_minute(NonZeroU32::new(9).expect("nine is non-zero"));
        let limiter: RateLimiter<_, InMemoryState, DefaultClock> = RateLimiter::direct(quota);
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(180))
                .build()?,
            limiter: Arc::new(limiter),
            model,
            validation_model,
            active_models: Mutex::new(HashMap::new()),
        })
    }

    async fn generate(
        &self,
        model: &str,
        key: &str,
        parts: Vec<Value>,
        max_tokens: Option<u32>,
        response_mime_type: Option<&str>,
    ) -> Result<GeneratedText> {
        let key_fingerprint = api_key_fingerprint(key);
        let active_model = self
            .active_models
            .lock()
            .map_err(|_| anyhow::anyhow!("Gemini model preference lock poisoned"))?
            .get(&key_fingerprint)
            .cloned()
            .unwrap_or_else(|| model.to_owned());
        match self
            .generate_on_model(&active_model, key, &parts, max_tokens, response_mime_type)
            .await
        {
            Ok(text) => Ok(GeneratedText {
                text,
                model: active_model,
            }),
            Err(error) if error.downcast_ref::<ModelQuotaExceeded>().is_some() => {
                let alternates = self.list_flash_models(key).await.with_context(|| {
                    format!(
                        "model {} hit its quota and alternate Flash models could not be listed",
                        active_model
                    )
                })?;
                let mut exhausted = vec![error.to_string()];
                for alternate in alternates
                    .into_iter()
                    .filter(|alternate| alternate != &active_model)
                {
                    match self
                        .generate_on_model(&alternate, key, &parts, max_tokens, response_mime_type)
                        .await
                    {
                        Ok(text) => {
                            self.active_models
                                .lock()
                                .map_err(|_| {
                                    anyhow::anyhow!("Gemini model preference lock poisoned")
                                })?
                                .insert(key_fingerprint, alternate.clone());
                            tracing::warn!(
                                previous_model = %active_model,
                                selected_model = %alternate,
                                "Gemini quota reached; switched to an available Flash model"
                            );
                            return Ok(GeneratedText {
                                text,
                                model: alternate,
                            });
                        }
                        Err(error) if error.downcast_ref::<ModelQuotaExceeded>().is_some() => {
                            exhausted.push(error.to_string());
                        }
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("trying alternate Gemini Flash model {alternate}")
                            });
                        }
                    }
                }
                anyhow::bail!(
                    "Gemini quotas are exhausted for all available Flash models: {}",
                    exhausted.join("; ")
                )
            }
            Err(error) => Err(error),
        }
    }

    async fn list_flash_models(&self, key: &str) -> Result<Vec<String>> {
        let mut models = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut request = self
                .http
                .get("https://generativelanguage.googleapis.com/v1beta/models")
                .header("x-goog-api-key", key)
                .query(&[("pageSize", "100")]);
            if let Some(token) = page_token.as_deref() {
                request = request.query(&[("pageToken", token)]);
            }
            let response = request
                .send()
                .await
                .context("listing Gemini models available to this API key")?;
            let status = response.status();
            let body: Value = response.json().await.context("reading Gemini model list")?;
            if !status.is_success() {
                let message = body
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Gemini model listing failed");
                anyhow::bail!("Gemini models.list returned HTTP {status}: {message}");
            }
            let page = body
                .get("models")
                .and_then(Value::as_array)
                .context("Gemini model list contained no models array")?;
            models.extend(page.iter().filter_map(is_text_flash_model));
            page_token = body
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if page_token.is_none() {
                break;
            }
        }
        models.sort();
        models.dedup();
        Ok(models)
    }

    async fn generate_on_model(
        &self,
        model: &str,
        key: &str,
        parts: &[Value],
        max_tokens: Option<u32>,
        response_mime_type: Option<&str>,
    ) -> Result<String> {
        let mut generation_config = json!({ "temperature": 0.1 });
        if let Some(max_tokens) = max_tokens {
            generation_config["maxOutputTokens"] = json!(max_tokens);
        }
        if let Some(response_mime_type) = response_mime_type {
            generation_config["responseMimeType"] = json!(response_mime_type);
        }
        for attempt in 0..MAX_API_ATTEMPTS {
            self.limiter.until_ready().await;
            let response = self
                .http
                .post(format!(
                    "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
                ))
                .header("x-goog-api-key", key)
                .json(&json!({
                    "contents": [{ "role": "user", "parts": parts }],
                    "generationConfig": generation_config
                }))
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error)
                    if attempt + 1 < MAX_API_ATTEMPTS
                        && (error.is_timeout() || error.is_connect()) =>
                {
                    tracing::warn!(
                        model,
                        attempt = attempt + 1,
                        error = %error,
                        "transient Gemini transport failure; retrying request"
                    );
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("sending request to Gemini model {model}"));
                }
            };
            let status = response.status();
            let body: Value = response.json().await.context("reading Gemini response")?;
            if !status.is_success() {
                let message = body
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Gemini request failed");
                if status == StatusCode::TOO_MANY_REQUESTS {
                    return Err(ModelQuotaExceeded {
                        model: model.to_owned(),
                        message: message.to_owned(),
                    }
                    .into());
                }
                if has_pdf_attachment(parts) && is_pdf_size_rejection(status, message) {
                    return Err(PdfRequiresChunks {
                        message: message.to_owned(),
                    }
                    .into());
                }
                if Self::is_transient_api_status(status) && attempt + 1 < MAX_API_ATTEMPTS {
                    tracing::warn!(
                        model,
                        %status,
                        attempt = attempt + 1,
                        "transient Gemini API response; retrying request"
                    );
                    tokio::time::sleep(Self::retry_delay(attempt)).await;
                    continue;
                }
                anyhow::bail!("Gemini returned HTTP {status}: {message}");
            }
            if body
                .pointer("/candidates/0/finishReason")
                .and_then(Value::as_str)
                == Some("MAX_TOKENS")
            {
                if has_pdf_attachment(parts) {
                    return Err(PdfRequiresChunks {
                        message: "the PDF response exceeded the model output token limit"
                            .to_owned(),
                    }
                    .into());
                }
                anyhow::bail!("Gemini reached its output token limit; refusing to save a potentially truncated response");
            }
            let texts: Vec<&str> = body
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .ok_or(NoCandidateText)?
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            let text = texts.join("\n");
            if text.trim().is_empty() {
                return Err(NoCandidateText.into());
            }
            return Ok(text);
        }
        unreachable!("Gemini retry loop returns or continues")
    }

    pub async fn validate_image(&self, path: &Path, key: &str) -> Result<bool> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        let mime = mime_for_path(path);
        let answer = self.generate(
            &self.validation_model,
            key,
            vec![
                json!({ "text": "Classify this image. Is it primarily a scanned or photographed historical text document (not a general photo)? Reply with exactly one word: DOCUMENT or PHOTO." }),
                json!({ "inlineData": { "mimeType": mime, "data": STANDARD.encode(bytes) } }),
            ],
            None,
            None,
        ).await?.text;
        let normalized = answer
            .trim()
            .trim_matches(|c: char| !c.is_ascii_alphabetic())
            .to_ascii_uppercase();
        if normalized.starts_with("DOCUMENT") {
            Ok(true)
        } else if normalized.starts_with("PHOTO") {
            Ok(false)
        } else {
            anyhow::bail!(
                "Gemini returned an unrecognized image classification: {}",
                answer.trim()
            )
        }
    }

    fn is_transient_api_status(status: StatusCode) -> bool {
        status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::INTERNAL_SERVER_ERROR
            || status == StatusCode::BAD_GATEWAY
            || status == StatusCode::SERVICE_UNAVAILABLE
            || status == StatusCode::GATEWAY_TIMEOUT
    }

    fn retry_delay(attempt: usize) -> std::time::Duration {
        std::time::Duration::from_secs(1_u64 << attempt)
    }

    pub async fn transcribe_file(
        &self,
        path: &Path,
        key: &str,
        system_prompt: &str,
    ) -> Result<String> {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        self.transcribe_bytes(&bytes, mime_for_path(path), key, system_prompt)
            .await
    }

    pub async fn transcribe_bytes(
        &self,
        bytes: &[u8],
        mime: &str,
        key: &str,
        system_prompt: &str,
    ) -> Result<String> {
        let is_image = mime.starts_with("image/");
        let prompt = transcription_prompt_for_mime(system_prompt, mime);
        let raw = self
            .generate(
                &self.model,
                key,
                vec![
                    json!({ "text": prompt }),
                    json!({ "inlineData": { "mimeType": mime, "data": STANDARD.encode(bytes) } }),
                ],
                None,
                None,
            )
            .await?;
        if is_image && raw.text.trim() == NON_DOCUMENT_MARKER {
            return Err(NonDocument.into());
        }
        Ok(normalize_transcript(&raw.text))
    }

    pub async fn consolidate(
        &self,
        page_transcripts: &[String],
        key: &str,
        system_prompt: &str,
    ) -> Result<String> {
        let combined = page_transcripts
            .iter()
            .enumerate()
            .map(|(index, transcript)| format!("## Page {}\n\n{}", index + 1, transcript))
            .collect::<Vec<_>>()
            .join("\n\n");
        let instructions = format!(
            "{system_prompt}\n\nThe following are page-by-page transcriptions of one multi-page record. Consolidate them into one cohesive master transcript, preserving every transcribed fact, page sequence, table, note, and uncertainty. Do not omit repeated or contradictory content; identify it explicitly. The exact marker {NO_PAGE_TEXT_MARKER} means Gemini returned no transcription text for that page. It is internal processing metadata, not text printed on the record: do not copy it into the master transcript or infer what that page contains. Preserve the surrounding pages in their original order."
        );
        let raw = self
            .generate(
                &self.model,
                key,
                vec![json!({ "text": format!("{instructions}\n\n{combined}") })],
                None,
                None,
            )
            .await?;
        Ok(normalize_transcript(&raw.text))
    }

    pub async fn test_key(&self, key: &str) -> Result<String> {
        let answer = self
            .generate(
                &self.validation_model,
                key,
                vec![json!({ "text": "Reply with OK." })],
                None,
                None,
            )
            .await?;
        if answer.text.trim().is_empty() {
            anyhow::bail!("Gemini returned an empty test response");
        }
        Ok(answer.model)
    }

    pub async fn text_prompt(&self, key: &str, prompt: &str) -> Result<String> {
        Ok(self
            .generate(
                &self.model,
                key,
                vec![json!({ "text": prompt })],
                None,
                None,
            )
            .await?
            .text)
    }
}

fn has_pdf_attachment(parts: &[Value]) -> bool {
    parts.iter().any(|part| {
        part.pointer("/inlineData/mimeType").and_then(Value::as_str) == Some("application/pdf")
    })
}

fn is_pdf_size_rejection(status: StatusCode, message: &str) -> bool {
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return true;
    }
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    let message = message.to_ascii_lowercase();
    [
        "too large",
        "size limit",
        "payload size",
        "request size exceeds",
        "input size",
        "input token",
        "too many tokens",
        "token limit",
        "maximum number of tokens",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

pub fn is_pdf_size_rejection_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<PdfRequiresChunks>().is_some()
}

pub fn is_no_candidate_text(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NoCandidateText>().is_some()
}

pub fn is_non_document(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NonDocument>().is_some()
}

pub fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "tif" | "tiff" => "image/tiff",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

pub fn normalize_transcript(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let yaml_text = &rest[..end];
            let body = rest[end + 4..]
                .trim_start_matches(|character| character == '\r' || character == '\n');
            if matches!(
                serde_yaml::from_str::<serde_yaml::Value>(yaml_text),
                Ok(serde_yaml::Value::Mapping(_))
            ) {
                return format!("{}\n", body.trim());
            }
        }
    }
    format!("{}\n", trimmed)
}

#[cfg(test)]
mod tests {
    use super::{is_pdf_size_rejection, is_text_flash_model, normalize_transcript, GeminiClient};
    use reqwest::StatusCode;

    #[test]
    fn empty_candidate_responses_have_a_distinct_error_type() {
        let empty_response = anyhow::Error::new(super::NoCandidateText);
        let other_error = anyhow::anyhow!("Gemini request failed");

        assert!(super::is_no_candidate_text(&empty_response));
        assert!(!super::is_no_candidate_text(&other_error));
    }

    #[test]
    fn non_document_marker_is_a_distinct_error_and_only_appended_for_images() {
        let error = anyhow::Error::new(super::NonDocument);
        let other_error = anyhow::anyhow!("Gemini request failed");
        assert!(super::is_non_document(&error));
        assert!(!super::is_non_document(&other_error));

        let image_prompt = super::transcription_prompt_for_mime("custom prompt", "image/jpeg");
        assert!(image_prompt.starts_with("custom prompt"));
        assert!(image_prompt.contains("Whole-image page and spread rule"));
        assert!(image_prompt.contains(super::NON_DOCUMENT_MARKER));
        let pdf_prompt = super::transcription_prompt_for_mime("custom prompt", "application/pdf");
        assert!(pdf_prompt.contains("Whole-image page and spread rule"));
        assert!(!pdf_prompt.contains(super::NON_DOCUMENT_MARKER));
    }

    #[test]
    fn transient_server_errors_are_retryable_but_client_errors_are_not() {
        assert!(GeminiClient::is_transient_api_status(
            StatusCode::SERVICE_UNAVAILABLE
        ));
        assert!(GeminiClient::is_transient_api_status(
            StatusCode::BAD_GATEWAY
        ));
        assert!(GeminiClient::is_transient_api_status(
            StatusCode::REQUEST_TIMEOUT
        ));
        assert!(!GeminiClient::is_transient_api_status(
            StatusCode::BAD_REQUEST
        ));
        assert!(!GeminiClient::is_transient_api_status(
            StatusCode::UNAUTHORIZED
        ));
    }

    #[test]
    fn pdf_size_rejections_are_limited_to_payload_and_token_limits() {
        assert!(is_pdf_size_rejection(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request rejected"
        ));
        assert!(is_pdf_size_rejection(
            StatusCode::BAD_REQUEST,
            "Input token count exceeds the limit"
        ));
        assert!(!is_pdf_size_rejection(
            StatusCode::BAD_REQUEST,
            "Invalid PDF document"
        ));
        assert!(!is_pdf_size_rejection(
            StatusCode::INTERNAL_SERVER_ERROR,
            "temporary server failure"
        ));
    }

    #[test]
    fn only_generation_capable_text_flash_models_are_fallbacks() {
        let flash = serde_json::json!({
            "name": "models/gemini-2.5-flash",
            "supportedGenerationMethods": ["generateContent", "countTokens"]
        });
        let flash_image = serde_json::json!({
            "name": "models/gemini-2.5-flash-image",
            "supportedGenerationMethods": ["generateContent"]
        });
        let preview = serde_json::json!({
            "name": "models/gemini-3-flash-preview",
            "supportedGenerationMethods": ["generateContent"]
        });
        let embeddings = serde_json::json!({
            "name": "models/gemini-embedding-001",
            "supportedGenerationMethods": ["embedContent"]
        });

        assert_eq!(
            is_text_flash_model(&flash).as_deref(),
            Some("gemini-2.5-flash")
        );
        assert_eq!(is_text_flash_model(&flash_image), None);
        assert_eq!(is_text_flash_model(&preview), None);
        assert_eq!(is_text_flash_model(&embeddings), None);
    }

    #[test]
    fn transcript_removes_legacy_frontmatter() {
        let transcript = normalize_transcript(
            "---\ndocument_type: birth register\nprimary_individuals:\n  - name: Ada\n---\n| Name | Age |\n| --- | --- |\n| Ada | 12 |",
        );
        assert!(!transcript.starts_with("---"));
        assert!(transcript.contains("| Ada | 12 |"));
    }

    #[test]
    fn flash_model_names_may_be_full_resource_names_or_ids() {
        let model = serde_json::json!({
            "name": "gemini-2.5-flash-lite",
            "supportedGenerationMethods": ["generateContent"]
        });
        assert_eq!(
            is_text_flash_model(&model).as_deref(),
            Some("gemini-2.5-flash-lite")
        );
    }
}
