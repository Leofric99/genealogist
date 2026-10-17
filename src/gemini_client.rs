use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use governor::{
    clock::DefaultClock, state::InMemoryState, DefaultDirectRateLimiter, Quota, RateLimiter,
};
use serde_json::{json, Value};
use std::{num::NonZeroU32, path::Path, sync::Arc};

pub struct GeminiClient {
    http: reqwest::Client,
    limiter: Arc<DefaultDirectRateLimiter>,
    model: String,
    validation_model: String,
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
        })
    }

    async fn generate(
        &self,
        model: &str,
        key: &str,
        parts: Vec<Value>,
        max_tokens: Option<u32>,
    ) -> Result<String> {
        self.limiter.until_ready().await;
        let mut generation_config = json!({ "temperature": 0.1 });
        if let Some(max_tokens) = max_tokens {
            generation_config["maxOutputTokens"] = json!(max_tokens);
        }
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
            .await
            .with_context(|| format!("sending request to Gemini model {model}"))?;
        let status = response.status();
        let body: Value = response.json().await.context("reading Gemini response")?;
        if !status.is_success() {
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Gemini request failed");
            anyhow::bail!("Gemini returned HTTP {status}: {message}");
        }
        if body
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            == Some("MAX_TOKENS")
        {
            anyhow::bail!("Gemini reached its output token limit; refusing to save a potentially truncated response");
        }
        let texts: Vec<&str> = body
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .context("Gemini response contained no candidate text")?
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect();
        if texts.is_empty() {
            anyhow::bail!("Gemini response contained no text");
        }
        Ok(texts.join("\n"))
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
            Some(1),
        ).await?;
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
        let raw = self
            .generate(
                &self.model,
                key,
                vec![
                    json!({ "text": system_prompt }),
                    json!({ "inlineData": { "mimeType": mime, "data": STANDARD.encode(bytes) } }),
                ],
                None,
            )
            .await?;
        Ok(normalize_transcript(&raw))
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
            "{system_prompt}\n\nThe following are complete page-by-page transcriptions of one multi-page record. Consolidate them into one cohesive master transcript, preserving every fact, page sequence, table, note, and all uncertainty. Do not omit repeated or contradictory content; identify it explicitly."
        );
        let raw = self
            .generate(
                &self.model,
                key,
                vec![json!({ "text": format!("{instructions}\n\n{combined}") })],
                None,
            )
            .await?;
        Ok(normalize_transcript(&raw))
    }

    pub async fn test_key(&self, key: &str) -> Result<()> {
        let answer = self
            .generate(
                &self.validation_model,
                key,
                vec![json!({ "text": "Reply with OK." })],
                Some(2),
            )
            .await?;
        if answer.trim().is_empty() {
            anyhow::bail!("Gemini returned an empty test response");
        }
        Ok(())
    }

    pub async fn text_prompt(&self, key: &str, prompt: &str) -> Result<String> {
        self.generate(&self.model, key, vec![json!({ "text": prompt })], None)
            .await
    }
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
    let mut metadata = serde_yaml::Mapping::new();
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let yaml_text = &rest[..end];
            let body = rest[end + 4..]
                .trim_start_matches(|character| character == '\r' || character == '\n');
            if let Ok(serde_yaml::Value::Mapping(parsed)) =
                serde_yaml::from_str::<serde_yaml::Value>(yaml_text)
            {
                metadata = parsed;
                ensure_metadata_fields(&mut metadata);
                return render_transcript(&metadata, body);
            }
        }
    }
    ensure_metadata_fields(&mut metadata);
    render_transcript(&metadata, trimmed)
}

fn ensure_metadata_fields(metadata: &mut serde_yaml::Mapping) {
    let fields = [
        ("document_type", serde_yaml::Value::Null),
        ("date_recorded", serde_yaml::Value::Null),
        ("location_recorded", serde_yaml::Value::Null),
        (
            "primary_individuals",
            serde_yaml::Value::Sequence(Vec::new()),
        ),
        (
            "all_extracted_dates",
            serde_yaml::Value::Sequence(Vec::new()),
        ),
        ("transcription_confidence", serde_yaml::Value::Null),
    ];
    for (key, default) in fields {
        metadata
            .entry(serde_yaml::Value::String(key.to_owned()))
            .or_insert(default);
    }
    if let Some(serde_yaml::Value::Sequence(individuals)) =
        metadata.get_mut(serde_yaml::Value::String("primary_individuals".to_owned()))
    {
        for individual in individuals {
            if let serde_yaml::Value::String(name) = individual {
                let mut person = serde_yaml::Mapping::new();
                person.insert(
                    serde_yaml::Value::String("name".to_owned()),
                    serde_yaml::Value::String(name.clone()),
                );
                *individual = serde_yaml::Value::Mapping(person);
            }
            if let serde_yaml::Value::Mapping(person) = individual {
                for key in ["name", "role", "age", "occupation", "birthplace"] {
                    person
                        .entry(serde_yaml::Value::String(key.to_owned()))
                        .or_insert(serde_yaml::Value::Null);
                }
            }
        }
    }
}

fn render_transcript(metadata: &serde_yaml::Mapping, body: &str) -> String {
    let yaml = serde_yaml::to_string(metadata).unwrap_or_else(|_| String::new());
    format!("---\n{}---\n\n{}\n", yaml, body.trim())
}

#[cfg(test)]
mod tests {
    use super::normalize_transcript;

    #[test]
    fn transcript_always_has_required_frontmatter_fields() {
        let transcript = normalize_transcript(
            "---\ndocument_type: birth register\nprimary_individuals:\n  - name: Ada\n---\n| Name | Age |\n| --- | --- |\n| Ada | 12 |",
        );
        let content = transcript
            .strip_prefix("---\n")
            .expect("frontmatter opener");
        let end = content.find("\n---").expect("frontmatter terminator");
        let metadata: serde_yaml::Value =
            serde_yaml::from_str(&content[..end]).expect("valid generated YAML");
        for field in [
            "document_type",
            "date_recorded",
            "location_recorded",
            "primary_individuals",
            "all_extracted_dates",
            "transcription_confidence",
        ] {
            assert!(metadata.get(field).is_some(), "missing {field}");
        }
        assert!(transcript.contains("| Ada | 12 |"));
    }
}
