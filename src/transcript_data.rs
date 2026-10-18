use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::HashSet, fs, path::Path};

pub const SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionPayload {
    pub document_type: String,
    #[serde(default)]
    pub title_as_written: Option<String>,
    #[serde(default)]
    pub person_mentions: Vec<ExtractedPersonMention>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedPersonMention {
    pub name_as_written: String,
    #[serde(default)]
    pub facts: Vec<ExtractedFact>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedFact {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub concept: Option<String>,
    pub evidence: Evidence,
    #[serde(default)]
    pub attribution: Option<String>,
    #[serde(default)]
    pub assertion_status: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptMetadata {
    pub schema_version: String,
    pub document_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_as_written: Option<String>,
    pub source_filename: String,
    pub source_sha256: String,
    pub transcript_sha256: String,
    pub person_mentions: Vec<PersonMention>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonMention {
    pub mention_id: String,
    pub name_as_written: String,
    pub facts: Vec<Fact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub label: String,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concept: Option<String>,
    pub evidence: Evidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assertion_status: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub quote: String,
    pub location: String,
}

pub fn json_path_for_transcript(transcript_path: &Path) -> std::path::PathBuf {
    transcript_path.with_extension("json")
}

pub fn sha256_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

pub fn transcription_body(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("---") else {
        return trimmed;
    };
    let Some(end) = rest.find("\n---") else {
        return trimmed;
    };
    let yaml = &rest[..end];
    if !matches!(
        serde_yaml::from_str::<serde_yaml::Value>(yaml),
        Ok(serde_yaml::Value::Mapping(_))
    ) {
        return trimmed;
    }
    rest[end + 4..].trim()
}

pub fn parse_extraction(raw: &str, transcript: &str) -> Result<ExtractionPayload> {
    let json = raw
        .trim()
        .strip_prefix("```json")
        .and_then(|text| text.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or_else(|| raw.trim());
    let payload: ExtractionPayload =
        serde_json::from_str(json).context("Gemini returned invalid transcript JSON")?;
    validate_payload(&payload, transcript)?;
    Ok(payload)
}

pub fn build_metadata(
    payload: ExtractionPayload,
    source_filename: &str,
    source_sha256: &str,
    transcript: &str,
) -> Result<TranscriptMetadata> {
    validate_sha256(source_sha256, "source")?;
    let document_type = payload.document_type.trim();
    if document_type.is_empty() {
        bail!("transcript JSON has an empty document_type");
    }

    let person_mentions = payload
        .person_mentions
        .into_iter()
        .enumerate()
        .map(|(index, person)| PersonMention {
            mention_id: format!("person-{:04}", index + 1),
            name_as_written: person.name_as_written.trim().to_owned(),
            facts: person
                .facts
                .into_iter()
                .map(|fact| Fact {
                    label: fact.label.trim().to_owned(),
                    value: fact.value.trim().to_owned(),
                    concept: non_empty(fact.concept),
                    evidence: fact.evidence,
                    attribution: non_empty(fact.attribution),
                    assertion_status: non_empty(fact.assertion_status),
                })
                .collect(),
        })
        .collect();

    let metadata = TranscriptMetadata {
        schema_version: SCHEMA_VERSION.to_owned(),
        document_type: document_type.to_owned(),
        title_as_written: non_empty(payload.title_as_written),
        source_filename: source_filename.to_owned(),
        source_sha256: source_sha256.to_ascii_lowercase(),
        transcript_sha256: sha256_text(transcript),
        person_mentions,
    };
    validate_metadata(&metadata, transcript)?;
    Ok(metadata)
}

pub fn read_for_transcript(transcript_path: &Path) -> Result<Option<TranscriptMetadata>> {
    let json_path = json_path_for_transcript(transcript_path);
    if !json_path.is_file() {
        return Ok(None);
    }
    let json = fs::read_to_string(&json_path)
        .with_context(|| format!("reading transcript metadata {}", json_path.display()))?;
    let metadata: TranscriptMetadata = serde_json::from_str(&json)
        .with_context(|| format!("parsing transcript metadata {}", json_path.display()))?;
    let transcript = fs::read_to_string(transcript_path)
        .with_context(|| format!("reading transcript {}", transcript_path.display()))?;
    validate_metadata(&metadata, &transcript).with_context(|| {
        format!(
            "validating transcript metadata {} against {}",
            json_path.display(),
            transcript_path.display()
        )
    })?;
    Ok(Some(metadata))
}

pub fn is_current(metadata: &TranscriptMetadata, source_sha256: &str) -> bool {
    metadata.source_sha256.eq_ignore_ascii_case(source_sha256)
}

pub fn to_legacy_metadata(metadata: &TranscriptMetadata) -> Result<serde_yaml::Value> {
    let people = metadata
        .person_mentions
        .iter()
        .map(|person| {
            let age = fact_value(person, &["age"]);
            let birthplace = fact_value(person, &["birthplace", "birth_place", "place_of_birth"]);
            let mut individual = serde_json::Map::new();
            individual.insert("name".to_owned(), json!(person.name_as_written));
            if let Some(age) = age {
                individual.insert("age".to_owned(), json!(age));
            }
            if let Some(birthplace) = birthplace {
                individual.insert("birthplace".to_owned(), json!(birthplace));
            }
            serde_json::Value::Object(individual)
        })
        .collect::<Vec<_>>();
    let dates = metadata
        .person_mentions
        .iter()
        .flat_map(|person| person.facts.iter())
        .filter(|fact| is_date_fact(fact))
        .map(|fact| json!({ "label": fact.label, "date": fact.value }))
        .collect::<Vec<_>>();
    serde_yaml::to_value(json!({
        "document_type": metadata.document_type,
        "title_as_written": metadata.title_as_written,
        "primary_individuals": people,
        "all_extracted_dates": dates
    }))
    .context("converting transcript JSON for legacy metadata consumers")
}

fn is_date_fact(fact: &Fact) -> bool {
    let label = fact
        .concept
        .as_deref()
        .unwrap_or(&fact.label)
        .to_ascii_lowercase();
    ["date", "year", "quarter", "period", "registered"]
        .iter()
        .any(|term| label.contains(term))
}

fn fact_value<'a>(person: &'a PersonMention, concepts: &[&str]) -> Option<&'a str> {
    person.facts.iter().find_map(|fact| {
        let concept = fact
            .concept
            .as_deref()
            .unwrap_or(&fact.label)
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .split('_')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        concepts
            .iter()
            .any(|candidate| concept == *candidate || concept.starts_with(&format!("{candidate}_")))
            .then_some(fact.value.as_str())
    })
}

fn validate_payload(payload: &ExtractionPayload, transcript: &str) -> Result<()> {
    if payload.document_type.trim().is_empty() {
        bail!("transcript JSON has an empty document_type");
    }
    for person in &payload.person_mentions {
        if person.name_as_written.trim().is_empty() {
            bail!("transcript JSON contains a person mention without a name");
        }
        for fact in &person.facts {
            validate_fact(
                &fact.label,
                &fact.value,
                &fact.evidence,
                fact.concept.as_deref(),
                fact.attribution.as_deref(),
                fact.assertion_status.as_deref(),
                transcript,
            )?;
        }
    }
    Ok(())
}

fn validate_metadata(metadata: &TranscriptMetadata, transcript: &str) -> Result<()> {
    if metadata.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported transcript JSON schema version: {}",
            metadata.schema_version
        );
    }
    if metadata.document_type.trim().is_empty() || metadata.source_filename.trim().is_empty() {
        bail!("transcript JSON is missing its document type or source filename");
    }
    validate_sha256(&metadata.source_sha256, "source")?;
    validate_sha256(&metadata.transcript_sha256, "transcript")?;
    if metadata.transcript_sha256 != sha256_text(transcript) {
        bail!("transcript JSON is stale because its Markdown transcript changed");
    }
    let mut mention_ids = HashSet::new();
    for person in &metadata.person_mentions {
        if person.mention_id.trim().is_empty() || person.name_as_written.trim().is_empty() {
            bail!("transcript JSON contains a person mention without an ID or name");
        }
        if !mention_ids.insert(&person.mention_id) {
            bail!(
                "transcript JSON contains a duplicate person mention ID: {}",
                person.mention_id
            );
        }
        for fact in &person.facts {
            validate_fact(
                &fact.label,
                &fact.value,
                &fact.evidence,
                fact.concept.as_deref(),
                fact.attribution.as_deref(),
                fact.assertion_status.as_deref(),
                transcript,
            )?;
        }
    }
    Ok(())
}

fn validate_fact(
    label: &str,
    value: &str,
    evidence: &Evidence,
    concept: Option<&str>,
    attribution: Option<&str>,
    assertion_status: Option<&str>,
    transcript: &str,
) -> Result<()> {
    if label.trim().is_empty() || value.trim().is_empty() {
        bail!("transcript JSON contains a fact without a label or value");
    }
    if concept.is_some_and(|value| value.trim().is_empty())
        || attribution.is_some_and(|value| value.trim().is_empty())
        || assertion_status.is_some_and(|value| value.trim().is_empty())
    {
        bail!("transcript JSON contains an empty optional fact field");
    }
    if evidence.quote.trim().is_empty() || evidence.location.trim().is_empty() {
        bail!("transcript JSON contains a fact without quoted evidence and a location");
    }
    if !whitespace_normalized(transcript).contains(&whitespace_normalized(&evidence.quote)) {
        bail!(
            "transcript JSON evidence quote is not present in the Markdown transcript: {}",
            evidence.location
        );
    }
    Ok(())
}

fn validate_sha256(value: &str, label: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("{label} SHA-256 must contain exactly 64 hexadecimal characters");
    }
    Ok(())
}

fn whitespace_normalized(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim().to_owned();
        (!value.is_empty()).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::{build_metadata, parse_extraction, read_for_transcript, sha256_text};

    #[test]
    fn extraction_requires_evidence_present_in_the_transcript() {
        let transcript = "| Name | Age |\n| --- | --- |\n| Ada | 12 |";
        let output = r#"{
            "document_type": "register",
            "title_as_written": null,
            "person_mentions": [{
                "name_as_written": "Ada",
                "facts": [{
                    "label": "Age",
                    "value": "12",
                    "concept": "age",
                    "evidence": {
                        "quote": "Ada | 12",
                        "location": "table row 1"
                    }
                }]
            }]
        }"#;
        let payload = parse_extraction(output, transcript).expect("valid extraction");
        let metadata = build_metadata(payload, "register.jpg", &"a".repeat(64), transcript)
            .expect("valid metadata");

        assert_eq!(metadata.person_mentions[0].mention_id, "person-0001");
        assert_eq!(metadata.person_mentions[0].facts[0].value, "12");
    }

    #[test]
    fn extraction_rejects_evidence_not_present_in_the_transcript() {
        let transcript = "Ada was recorded in the register.";
        let output = r#"{
            "document_type": "register",
            "person_mentions": [{
                "name_as_written": "Ada",
                "facts": [{
                    "label": "Age",
                    "value": "12",
                    "evidence": {
                        "quote": "Ada | 12",
                        "location": "table row 1"
                    }
                }]
            }]
        }"#;

        assert!(parse_extraction(output, transcript).is_err());
    }

    #[test]
    fn metadata_is_bound_to_the_exact_transcript_text() {
        let directory = tempfile::tempdir().expect("temporary transcript directory");
        let transcript_path = directory.path().join("record.md");
        let transcript = "Ada was recorded in the register.";
        std::fs::write(&transcript_path, transcript).expect("write transcript");
        let json_path = super::json_path_for_transcript(&transcript_path);
        let metadata = serde_json::json!({
            "schema_version": "1.0",
            "document_type": "register",
            "source_filename": "record.jpg",
            "source_sha256": "a".repeat(64),
            "transcript_sha256": sha256_text(transcript),
            "person_mentions": []
        });
        std::fs::write(
            &json_path,
            serde_json::to_string(&metadata).expect("serialize metadata"),
        )
        .expect("write metadata");

        assert!(read_for_transcript(&transcript_path)
            .expect("read valid metadata")
            .is_some());

        std::fs::write(
            &transcript_path,
            "Ada was recorded in a different register.",
        )
        .expect("edit transcript");
        assert!(read_for_transcript(&transcript_path).is_err());
    }
}
