use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

pub const SCHEMA_VERSION: &str = "1.2";
const PREVIOUS_SCHEMA_VERSIONS: &[&str] = &["1.1", "1.0"];
const MAX_EXTRACTION_ROWS_PER_CHUNK: usize = 4;
const MAX_EXTRACTION_CHARS_PER_CHUNK: usize = 12_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractionPayload {
    pub document_type: String,
    #[serde(default)]
    pub title_as_written: Option<String>,
    #[serde(default)]
    pub person_mentions: Vec<ExtractedPersonMention>,
    #[serde(default)]
    pub non_person_source_row_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedPersonMention {
    pub name_as_written: String,
    #[serde(default)]
    pub name_evidence: Option<Evidence>,
    #[serde(default)]
    pub name_source_row_id: Option<String>,
    #[serde(default)]
    pub name_source_column_index: Option<usize>,
    #[serde(default)]
    pub name_assertion_status: Option<String>,
    #[serde(default)]
    pub facts: Vec<ExtractedFact>,
    #[serde(default)]
    pub source_row_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractedFact {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub source_row_id: Option<String>,
    #[serde(default)]
    pub source_column_index: Option<usize>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub non_person_source_row_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersonMention {
    pub mention_id: String,
    pub name_as_written: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_evidence: Option<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_source_row_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_source_column_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_assertion_status: Option<String>,
    pub facts: Vec<Fact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_row_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub label: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_row_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_column_index: Option<usize>,
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

#[derive(Clone, Debug)]
pub struct ExtractionSourceRow {
    pub id: String,
    pub text: String,
    pub cells: Vec<String>,
    pub previous_values: Vec<Option<String>>,
    pub resolved_cells: Vec<Option<String>>,
}

#[derive(Clone, Debug)]
pub struct ExtractionChunk {
    pub text: String,
    pub source_rows: Vec<ExtractionSourceRow>,
}

pub fn json_path_for_transcript(transcript_path: &Path) -> std::path::PathBuf {
    transcript_path.with_extension("json")
}

pub fn sha256_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

pub fn extraction_chunks(transcript: &str) -> Vec<ExtractionChunk> {
    let lines = transcript.lines().collect::<Vec<_>>();
    let mut consumed = vec![false; lines.len()];
    let mut table_chunks = Vec::new();
    let mut table_index = 0;
    let mut line_index = 0;

    while line_index < lines.len() {
        if !is_markdown_table_separator(lines[line_index]) {
            line_index += 1;
            continue;
        }

        let mut table_start = line_index;
        while table_start > 0
            && is_markdown_table_line(lines[table_start - 1])
            && !consumed[table_start - 1]
        {
            table_start -= 1;
        }

        let mut table_end = line_index + 1;
        let mut source_rows = Vec::new();
        let mut row_number = 0;
        let mut previous_values: Vec<Option<String>> = Vec::new();
        while table_end < lines.len()
            && is_markdown_table_line(lines[table_end])
            && !is_markdown_table_separator(lines[table_end])
        {
            row_number += 1;
            let cells = markdown_table_cells(lines[table_end]);
            if cells.iter().any(|cell| has_cell_content(cell)) {
                if previous_values.len() < cells.len() {
                    previous_values.resize(cells.len(), None);
                }
                let row_previous_values = previous_values.clone();
                let mut resolved_cells = vec![None; cells.len()];
                for (column, cell) in cells.iter().enumerate() {
                    if is_ditto_marker(cell) {
                        resolved_cells[column] = previous_values[column].clone();
                    } else if has_cell_content(cell) && !is_ditto_or_partial(cell) {
                        previous_values[column] = Some(cell.clone());
                    }
                }
                source_rows.push(ExtractionSourceRow {
                    id: format!("table-{:03}-row-{row_number:04}", table_index + 1),
                    text: lines[table_end].to_owned(),
                    cells,
                    previous_values: row_previous_values,
                    resolved_cells,
                });
            }
            table_end += 1;
        }

        table_index += 1;
        for consumed_line in &mut consumed[table_start..table_end] {
            *consumed_line = true;
        }

        if !source_rows.is_empty() {
            let header = lines[table_start..=line_index].join("\n");
            let context = lines[..table_start]
                .iter()
                .enumerate()
                .filter_map(|(index, line)| (!consumed[index]).then_some(*line))
                .collect::<Vec<_>>()
                .join("\n");
            let context = trailing_context(&context);
            table_chunks.extend(build_table_chunks(&header, &context, source_rows));
        }
        line_index = table_end;
    }

    let prose = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (!consumed[index]).then_some(*line))
        .collect::<Vec<_>>()
        .join("\n");
    let mut chunks = split_prose_chunks(&prose)
        .into_iter()
        .map(|text| ExtractionChunk {
            text,
            source_rows: Vec::new(),
        })
        .collect::<Vec<_>>();
    chunks.extend(table_chunks);

    if chunks.is_empty() {
        chunks.push(ExtractionChunk {
            text: transcript.to_owned(),
            source_rows: Vec::new(),
        });
    }
    chunks
}

fn build_table_chunks(
    header: &str,
    context: &str,
    source_rows: Vec<ExtractionSourceRow>,
) -> Vec<ExtractionChunk> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    for row in source_rows {
        let mut candidate = current.clone();
        candidate.push(row.clone());
        let candidate_text = render_table_chunk(header, context, &candidate);
        if !current.is_empty()
            && (current.len() >= MAX_EXTRACTION_ROWS_PER_CHUNK
                || candidate_text.len() > MAX_EXTRACTION_CHARS_PER_CHUNK)
        {
            chunks.push(ExtractionChunk {
                text: render_table_chunk(header, context, &current),
                source_rows: std::mem::take(&mut current),
            });
        }
        current.push(row);
    }
    if !current.is_empty() {
        chunks.push(ExtractionChunk {
            text: render_table_chunk(header, context, &current),
            source_rows: current,
        });
    }
    chunks
}

fn render_table_chunk(header: &str, context: &str, source_rows: &[ExtractionSourceRow]) -> String {
    let rows = source_rows
        .iter()
        .map(|row| format!("[Source row ID: {}]\n{}", row.id, row.text))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "TABLE HEADER (context only):\n{header}\n\nSOURCE DATA ROWS:\n{rows}\n\nDOCUMENT CONTEXT:\n{context}"
    )
}

fn trailing_context(context: &str) -> String {
    const MAX_CONTEXT_CHARS: usize = 2_000;
    if context.len() <= MAX_CONTEXT_CHARS {
        return context.to_owned();
    }
    let start = context
        .char_indices()
        .find(|(index, _)| *index >= context.len() - MAX_CONTEXT_CHARS)
        .map(|(index, _)| index)
        .unwrap_or(0);
    context[start..].to_owned()
}

fn split_prose_chunks(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        for part in split_oversized_text(paragraph) {
            let separator_length = if current.is_empty() { 0 } else { 2 };
            let additional_length = part.len() + separator_length;
            if !current.is_empty()
                && current.len() + additional_length > MAX_EXTRACTION_CHARS_PER_CHUNK
            {
                chunks.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(&part);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn split_oversized_text(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut remaining = text.trim();
    while remaining.len() > MAX_EXTRACTION_CHARS_PER_CHUNK {
        let safe_boundary = remaining
            .char_indices()
            .take_while(|(index, _)| *index <= MAX_EXTRACTION_CHARS_PER_CHUNK)
            .map(|(index, _)| index)
            .last()
            .unwrap_or(0);
        let boundary = remaining[..safe_boundary]
            .rfind(char::is_whitespace)
            .unwrap_or(safe_boundary);
        if boundary == 0 {
            break;
        }
        let (part, rest) = remaining.split_at(boundary);
        parts.push(part.trim().to_owned());
        remaining = rest.trim_start();
    }
    if !remaining.is_empty() {
        parts.push(remaining.to_owned());
    }
    parts
}

fn is_markdown_table_line(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|') && line.ends_with('|')
}

fn is_markdown_table_separator(line: &str) -> bool {
    is_markdown_table_line(line)
        && markdown_table_cells(line).iter().all(|cell| {
            let cell = cell.trim().trim_matches(':');
            cell.len() >= 3 && cell.chars().all(|character| character == '-')
        })
}

fn markdown_table_cells(line: &str) -> Vec<String> {
    let line = line.trim().trim_start_matches('|').trim_end_matches('|');
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for character in line.chars() {
        if character == '|' && !escaped {
            cells.push(cell.trim().replace("\\|", "|"));
            cell.clear();
        } else {
            cell.push(character);
        }
        if character == '\\' && !escaped {
            escaped = true;
        } else {
            escaped = false;
        }
    }
    cells.push(cell.trim().replace("\\|", "|"));
    cells
}

fn is_ditto_word(word: &str) -> bool {
    matches!(word, "do" | "DO" | "ditto" | "Ditto" | "DITTO")
}

fn is_ditto_marker(value: &str) -> bool {
    let normalized = value
        .trim()
        .replace("d°", "do")
        .replace("D°", "DO")
        .replace("dº", "do")
        .replace("Dº", "DO");
    let words = normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if !words.is_empty() {
        return words.iter().all(|word| is_ditto_word(word));
    }
    !normalized.is_empty()
        && normalized.chars().all(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '"' | '\''
                        | '\u{201c}'
                        | '\u{201d}'
                        | '\u{2018}'
                        | '\u{2019}'
                        | '\u{3003}'
                        | '〞'
                        | '〟'
                )
        })
}

fn trailing_ditto_prefix(value: &str) -> Option<&str> {
    let value = value.trim_end();
    let mut prefix_end = value.len();
    let mut found_marker = false;
    loop {
        let remaining = value[..prefix_end].trim_end();
        let token_start = remaining
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        let token = &remaining[token_start..];
        if !is_ditto_marker(token) {
            break;
        }
        found_marker = true;
        prefix_end = token_start;
    }
    let prefix = value[..prefix_end].trim_end();
    (found_marker && !prefix.is_empty()).then_some(prefix)
}

fn is_partial_ditto(value: &str) -> bool {
    trailing_ditto_prefix(value).is_some()
}

fn is_ditto_or_partial(value: &str) -> bool {
    is_ditto_marker(value) || is_partial_ditto(value)
}

fn resolve_partial_name(value: &str, previous_value: Option<&str>) -> Option<String> {
    let prefix = trailing_ditto_prefix(value)?;
    let previous_value = previous_value?;
    let surname = previous_value
        .split_whitespace()
        .last()?
        .trim_matches(|character: char| !character.is_alphanumeric());
    if surname.is_empty() {
        return None;
    }
    Some(format!("{prefix} {surname}"))
}

fn has_cell_content(cell: &str) -> bool {
    let normalized = cell.trim();
    !normalized.is_empty() && !normalized.eq_ignore_ascii_case("[blank]")
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

pub fn parse_extraction_for_rows(
    raw: &str,
    transcript: &str,
    source_rows: &[ExtractionSourceRow],
) -> Result<ExtractionPayload> {
    let json = raw
        .trim()
        .strip_prefix("```json")
        .and_then(|text| text.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or_else(|| raw.trim());
    let mut payload: ExtractionPayload =
        serde_json::from_str(json).context("Gemini returned invalid transcript JSON")?;
    resolve_payload_ditto_values(&mut payload, source_rows)?;
    validate_payload(&payload, transcript, source_rows)?;
    Ok(payload)
}

fn resolve_payload_ditto_values(
    payload: &mut ExtractionPayload,
    source_rows: &[ExtractionSourceRow],
) -> Result<()> {
    if source_rows.is_empty() {
        return Ok(());
    }
    for person in &mut payload.person_mentions {
        if let (Some(row_id), Some(column_index)) = (
            person.name_source_row_id.as_deref(),
            person.name_source_column_index,
        ) {
            let row = source_rows
                .iter()
                .find(|row| row.id == row_id)
                .with_context(|| format!("name refers to unknown source row {row_id}"))?;
            let cell_index = column_index
                .checked_sub(1)
                .context("name source column indexes must be 1-based")?;
            let cell = row
                .cells
                .get(cell_index)
                .with_context(|| format!("name refers to unknown source column {column_index}"))?;
            let resolved = row
                .resolved_cells
                .get(cell_index)
                .and_then(Option::as_deref)
                .map(str::to_owned)
                .or_else(|| {
                    resolve_partial_name(
                        cell,
                        row.previous_values
                            .get(cell_index)
                            .and_then(Option::as_deref),
                    )
                });
            if let Some(resolved) = resolved {
                person.name_as_written = resolved;
                person.name_assertion_status = Some("inferred from ditto marker".to_owned());
            }
        }
        for fact in &mut person.facts {
            if let (Some(row_id), Some(column_index)) =
                (fact.source_row_id.as_deref(), fact.source_column_index)
            {
                let row = source_rows
                    .iter()
                    .find(|row| row.id == row_id)
                    .with_context(|| format!("fact refers to unknown source row {row_id}"))?;
                let cell_index = column_index
                    .checked_sub(1)
                    .context("fact source column indexes must be 1-based")?;
                let _cell = row.cells.get(cell_index).with_context(|| {
                    format!("fact refers to unknown source column {column_index}")
                })?;
                if let Some(resolved) = row.resolved_cells.get(cell_index).and_then(Option::as_ref)
                {
                    fact.value.clone_from(resolved);
                    fact.assertion_status = Some("inferred from ditto marker".to_owned());
                }
            }
        }
    }
    Ok(())
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
            name_evidence: person.name_evidence,
            name_source_row_id: person.name_source_row_id,
            name_source_column_index: person.name_source_column_index,
            name_assertion_status: non_empty(person.name_assertion_status),
            facts: person
                .facts
                .into_iter()
                .map(|fact| Fact {
                    label: fact.label.trim().to_owned(),
                    value: fact.value.trim().to_owned(),
                    source_row_id: fact.source_row_id,
                    source_column_index: fact.source_column_index,
                    concept: non_empty(fact.concept),
                    evidence: fact.evidence,
                    attribution: non_empty(fact.attribution),
                    assertion_status: non_empty(fact.assertion_status),
                })
                .collect(),
            source_row_ids: person.source_row_ids,
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
        non_person_source_row_ids: payload.non_person_source_row_ids,
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
    metadata.schema_version == SCHEMA_VERSION
        && metadata.source_sha256.eq_ignore_ascii_case(source_sha256)
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

fn validate_payload(
    payload: &ExtractionPayload,
    transcript: &str,
    source_rows: &[ExtractionSourceRow],
) -> Result<()> {
    if payload.document_type.trim().is_empty() {
        bail!("transcript JSON has an empty document_type");
    }
    let expected_rows = source_rows
        .iter()
        .map(|row| row.id.clone())
        .collect::<HashSet<_>>();
    let mut non_person_rows = HashSet::new();
    for row_id in &payload.non_person_source_row_ids {
        if !expected_rows.contains(row_id) {
            bail!("transcript JSON refers to unknown source row {row_id}");
        }
        if !non_person_rows.insert(row_id.clone()) {
            bail!("transcript JSON lists source row {row_id} as non-person more than once");
        }
    }

    let mut people_by_row =
        std::collections::HashMap::<String, Vec<&ExtractedPersonMention>>::new();
    let mut covered_columns = std::collections::HashMap::<String, HashSet<usize>>::new();
    for person in &payload.person_mentions {
        if person.name_as_written.trim().is_empty() {
            bail!("transcript JSON contains a person mention without a name");
        }
        let mut person_rows = HashSet::new();
        for row_id in &person.source_row_ids {
            if !expected_rows.contains(row_id) {
                bail!(
                    "transcript JSON assigns {} to unknown source row {row_id}",
                    person.name_as_written
                );
            }
            if !person_rows.insert(row_id.clone()) {
                bail!(
                    "transcript JSON assigns {} to source row {row_id} more than once",
                    person.name_as_written
                );
            }
            people_by_row
                .entry(row_id.clone())
                .or_default()
                .push(person);
        }
        if !source_rows.is_empty() && person.source_row_ids.is_empty() {
            bail!(
                "transcript JSON does not associate person {} with a source row",
                person.name_as_written
            );
        }
        if !source_rows.is_empty() {
            let row_id = person
                .name_source_row_id
                .as_deref()
                .context("table person mention is missing name_source_row_id")?;
            let column_index = person
                .name_source_column_index
                .context("table person mention is missing name_source_column_index")?;
            if !person_rows.contains(row_id) {
                bail!(
                    "name source row {row_id} is not assigned to {}",
                    person.name_as_written
                );
            }
            let row = source_rows
                .iter()
                .find(|row| row.id == row_id)
                .with_context(|| format!("name refers to unknown source row {row_id}"))?;
            let cell_index = column_index
                .checked_sub(1)
                .context("name source column indexes must be 1-based")?;
            let cell = row
                .cells
                .get(cell_index)
                .with_context(|| format!("name refers to unknown source column {column_index}"))?;
            let name_evidence = person
                .name_evidence
                .as_ref()
                .context("table person mention is missing name_evidence")?;
            if !quote_is_in_text(&name_evidence.quote, cell)
                || !quote_is_in_text(&name_evidence.quote, transcript)
            {
                bail!(
                    "name evidence for {} is not contained in its mapped source cell",
                    person.name_as_written
                );
            }
            if person.name_assertion_status.as_deref() == Some("inferred from ditto marker") {
                let resolved = row
                    .resolved_cells
                    .get(cell_index)
                    .and_then(Option::as_deref)
                    .map(str::to_owned)
                    .or_else(|| {
                        resolve_partial_name(
                            cell,
                            row.previous_values
                                .get(cell_index)
                                .and_then(Option::as_deref),
                        )
                    });
                if resolved.as_deref() != Some(person.name_as_written.as_str()) {
                    bail!(
                        "name for {} is not supported by its mapped ditto source cell",
                        person.name_as_written
                    );
                }
            }
            covered_columns
                .entry(row_id.to_owned())
                .or_default()
                .insert(column_index);
        }
        for fact in &person.facts {
            let mut inferred_ditto = false;
            if !source_rows.is_empty() {
                let row_id = fact
                    .source_row_id
                    .as_deref()
                    .context("table fact is missing source_row_id")?;
                let column_index = fact
                    .source_column_index
                    .context("table fact is missing source_column_index")?;
                if !person_rows.contains(row_id) {
                    bail!(
                        "fact source row {row_id} is not assigned to {}",
                        person.name_as_written
                    );
                }
                let row = source_rows
                    .iter()
                    .find(|row| row.id == row_id)
                    .with_context(|| format!("fact refers to unknown source row {row_id}"))?;
                let cell_index = column_index
                    .checked_sub(1)
                    .context("fact source column indexes must be 1-based")?;
                let cell = row.cells.get(cell_index).with_context(|| {
                    format!("fact refers to unknown source column {column_index}")
                })?;
                if !quote_is_in_text(&fact.evidence.quote, cell) {
                    bail!(
                        "fact evidence for {} is not contained in its mapped source cell",
                        person.name_as_written
                    );
                }
                if fact.assertion_status.as_deref() == Some("inferred from ditto marker") {
                    inferred_ditto = row
                        .resolved_cells
                        .get(cell_index)
                        .and_then(Option::as_deref)
                        == Some(fact.value.as_str());
                    if !inferred_ditto {
                        bail!(
                            "fact for {} is not supported by its mapped ditto source cell",
                            person.name_as_written
                        );
                    }
                } else if row
                    .resolved_cells
                    .get(cell_index)
                    .is_some_and(Option::is_some)
                {
                    bail!(
                        "ditto-derived fact for {} was not marked as inferred",
                        person.name_as_written
                    );
                }
                covered_columns
                    .entry(row_id.to_owned())
                    .or_default()
                    .insert(column_index);
            }
            validate_fact(
                &fact.label,
                &fact.value,
                &fact.evidence,
                fact.concept.as_deref(),
                fact.attribution.as_deref(),
                fact.assertion_status.as_deref(),
                inferred_ditto,
                transcript,
            )?;
            if !person.source_row_ids.is_empty()
                && !person.source_row_ids.iter().any(|row_id| {
                    source_rows
                        .iter()
                        .find(|row| &row.id == row_id)
                        .is_some_and(|row| quote_is_in_text(&fact.evidence.quote, &row.text))
                })
            {
                bail!(
                    "transcript JSON evidence for {} is not contained in one of its assigned source rows",
                    person.name_as_written
                );
            }
        }
    }

    for source_row in source_rows {
        let people = people_by_row
            .get(&source_row.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let marked_non_person = non_person_rows.contains(&source_row.id);
        if marked_non_person && !people.is_empty() {
            bail!(
                "transcript JSON classifies source row {} as both person and non-person",
                source_row.id
            );
        }
        if !marked_non_person && people.is_empty() {
            bail!(
                "transcript JSON did not account for source row {}",
                source_row.id
            );
        }
        if !marked_non_person {
            validate_row_cells(source_row, people)?;
            let columns = covered_columns
                .get(&source_row.id)
                .cloned()
                .unwrap_or_default();
            for (index, cell) in source_row.cells.iter().enumerate() {
                if has_cell_content(cell) && !columns.contains(&(index + 1)) {
                    bail!(
                        "transcript JSON did not map populated column {} in source row {}",
                        index + 1,
                        source_row.id
                    );
                }
            }
        }
    }
    Ok(())
}

fn validate_metadata(metadata: &TranscriptMetadata, transcript: &str) -> Result<()> {
    if metadata.schema_version != SCHEMA_VERSION
        && !PREVIOUS_SCHEMA_VERSIONS.contains(&metadata.schema_version.as_str())
    {
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
    let mut non_person_rows = HashSet::new();
    let source_rows_by_id = extraction_chunks(transcript)
        .into_iter()
        .flat_map(|chunk| chunk.source_rows)
        .map(|row| (row.id.clone(), row))
        .collect::<HashMap<_, _>>();
    for source_row_id in &metadata.non_person_source_row_ids {
        if source_row_id.trim().is_empty() || !non_person_rows.insert(source_row_id.clone()) {
            bail!("transcript JSON contains an empty or duplicate non-person source row ID");
        }
    }
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
        let mut source_row_ids = HashSet::new();
        if person.source_row_ids.iter().any(|source_row_id| {
            source_row_id.trim().is_empty()
                || !source_row_ids.insert(source_row_id.clone())
                || non_person_rows.contains(source_row_id)
        }) {
            bail!(
                "transcript JSON contains empty, duplicate, or conflicting source row IDs for {}",
                person.name_as_written
            );
        }
        let has_name_mapping = person.name_source_row_id.is_some()
            || person.name_source_column_index.is_some()
            || person.name_evidence.is_some();
        if metadata.schema_version == SCHEMA_VERSION && !person.source_row_ids.is_empty()
            || has_name_mapping
        {
            let row_id = person
                .name_source_row_id
                .as_deref()
                .context("transcript JSON is missing name_source_row_id")?;
            let column_index = person
                .name_source_column_index
                .context("transcript JSON is missing name_source_column_index")?;
            if !person.source_row_ids.iter().any(|id| id == row_id) {
                bail!(
                    "name source row {row_id} is not assigned to {}",
                    person.name_as_written
                );
            }
            let row = source_rows_by_id.get(row_id).with_context(|| {
                format!("transcript JSON refers to unknown source row {row_id}")
            })?;
            let cell_index = column_index
                .checked_sub(1)
                .context("name source column indexes must be 1-based")?;
            let cell = row.cells.get(cell_index).with_context(|| {
                format!("transcript JSON refers to unknown source column {column_index}")
            })?;
            let evidence = person
                .name_evidence
                .as_ref()
                .context("transcript JSON is missing name_evidence")?;
            if !quote_is_in_text(&evidence.quote, cell)
                || !quote_is_in_text(&evidence.quote, transcript)
            {
                bail!("transcript JSON name evidence is not in its mapped source cell");
            }
            if person.name_assertion_status.as_deref() == Some("inferred from ditto marker") {
                let resolved = row
                    .resolved_cells
                    .get(cell_index)
                    .and_then(Option::as_deref)
                    .map(str::to_owned)
                    .or_else(|| {
                        resolve_partial_name(
                            cell,
                            row.previous_values
                                .get(cell_index)
                                .and_then(Option::as_deref),
                        )
                    });
                if resolved.as_deref() != Some(person.name_as_written.as_str()) {
                    bail!("transcript JSON name inference does not match its source row");
                }
            }
        }
        for fact in &person.facts {
            let has_fact_mapping =
                fact.source_row_id.is_some() || fact.source_column_index.is_some();
            let mut inferred_ditto = false;
            if metadata.schema_version == SCHEMA_VERSION && !person.source_row_ids.is_empty()
                || has_fact_mapping
            {
                let row_id = fact
                    .source_row_id
                    .as_deref()
                    .context("transcript JSON is missing fact source_row_id")?;
                let column_index = fact
                    .source_column_index
                    .context("transcript JSON is missing fact source_column_index")?;
                if !person.source_row_ids.iter().any(|id| id == row_id) {
                    bail!(
                        "fact source row {row_id} is not assigned to {}",
                        person.name_as_written
                    );
                }
                let row = source_rows_by_id.get(row_id).with_context(|| {
                    format!("transcript JSON refers to unknown source row {row_id}")
                })?;
                let cell_index = column_index
                    .checked_sub(1)
                    .context("fact source column indexes must be 1-based")?;
                let cell = row.cells.get(cell_index).with_context(|| {
                    format!("transcript JSON refers to unknown source column {column_index}")
                })?;
                if !quote_is_in_text(&fact.evidence.quote, cell) {
                    bail!("transcript JSON fact evidence is not in its mapped source cell");
                }
                if fact.assertion_status.as_deref() == Some("inferred from ditto marker") {
                    inferred_ditto = row
                        .resolved_cells
                        .get(cell_index)
                        .and_then(Option::as_deref)
                        == Some(fact.value.as_str());
                    if !inferred_ditto {
                        bail!("transcript JSON ditto inference does not match its source row");
                    }
                } else if row
                    .resolved_cells
                    .get(cell_index)
                    .is_some_and(Option::is_some)
                {
                    bail!("transcript JSON omitted a ditto inference status");
                }
            }
            validate_fact(
                &fact.label,
                &fact.value,
                &fact.evidence,
                fact.concept.as_deref(),
                fact.attribution.as_deref(),
                fact.assertion_status.as_deref(),
                inferred_ditto,
                transcript,
            )?;
        }
    }
    Ok(())
}

fn validate_row_cells(
    source_row: &ExtractionSourceRow,
    people: &[&ExtractedPersonMention],
) -> Result<()> {
    let names = people
        .iter()
        .map(|person| person.name_as_written.as_str())
        .collect::<Vec<_>>();
    let facts = people
        .iter()
        .flat_map(|person| {
            person
                .facts
                .iter()
                .flat_map(|fact| [fact.label.as_str(), fact.value.as_str()])
        })
        .map(normalized_cell_value)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    for (index, cell) in source_row.cells.iter().enumerate() {
        if !has_cell_content(cell) {
            continue;
        }
        let effective_cell = source_row
            .resolved_cells
            .get(index)
            .and_then(Option::as_deref)
            .unwrap_or(cell);
        let effective_name = source_row
            .resolved_cells
            .get(index)
            .and_then(Option::as_deref)
            .map(str::to_owned)
            .or_else(|| {
                resolve_partial_name(
                    cell,
                    source_row
                        .previous_values
                        .get(index)
                        .and_then(Option::as_deref),
                )
            });
        let normalized = normalized_cell_value(effective_cell);
        if !facts.iter().any(|value| value == &normalized)
            && !names
                .iter()
                .any(|name| name_covers_cell(effective_name.as_deref().unwrap_or(cell), name))
        {
            bail!(
                "transcript JSON omitted populated cell {cell:?} from source row {}",
                source_row.id
            );
        }
    }
    Ok(())
}

fn normalized_cell_value(value: &str) -> String {
    let normalized = value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if normalized.is_empty() {
        value.trim().to_ascii_lowercase()
    } else {
        normalized
    }
}

fn name_covers_cell(cell: &str, extracted_name: &str) -> bool {
    let cell = name_tokens(cell)
        .into_iter()
        .skip_while(|token| token.chars().all(char::is_numeric))
        .collect::<Vec<_>>();
    let name = name_tokens(extracted_name);
    if cell.is_empty() || name.is_empty() {
        return false;
    }
    cell.len() == 1 && name.contains(&cell[0])
        || (cell.len() > 1 && cell.len() <= name.len() && name.ends_with(&cell))
}

fn name_tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn validate_fact(
    label: &str,
    value: &str,
    evidence: &Evidence,
    concept: Option<&str>,
    attribution: Option<&str>,
    assertion_status: Option<&str>,
    valid_ditto_inference: bool,
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
    if !valid_ditto_inference
        && !whitespace_normalized(&evidence.quote)
            .to_ascii_lowercase()
            .contains(&whitespace_normalized(value).to_ascii_lowercase())
    {
        bail!(
            "transcript JSON fact value is not present in its evidence quote: {}",
            evidence.location
        );
    }
    Ok(())
}

fn quote_is_in_text(quote: &str, text: &str) -> bool {
    whitespace_normalized(text).contains(&whitespace_normalized(quote))
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
    use super::{
        build_metadata, extraction_chunks, parse_extraction_for_rows, read_for_transcript,
        sha256_text,
    };

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
        let payload = parse_extraction_for_rows(output, transcript, &[]).expect("valid extraction");
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

        assert!(parse_extraction_for_rows(output, transcript, &[]).is_err());
    }

    #[test]
    fn extraction_rejects_fact_values_not_supported_by_their_quotes() {
        let transcript = "Ada was recorded as age 12.";
        let output = r#"{
            "document_type": "register",
            "person_mentions": [{
                "name_as_written": "Ada",
                "facts": [{
                    "label": "Age",
                    "value": "13",
                    "evidence": {
                        "quote": "Ada was recorded as age 12",
                        "location": "record entry"
                    }
                }]
            }]
        }"#;

        assert!(parse_extraction_for_rows(output, transcript, &[]).is_err());
    }

    #[test]
    fn long_tables_are_split_into_bounded_row_chunks() {
        let mut transcript = String::from("| Name | Age |\n| --- | --- |\n");
        for age in 1..=10 {
            transcript.push_str(&format!("| Person {age} | {age} |\n"));
        }

        let chunks = extraction_chunks(&transcript);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].source_rows.len(), 4);
        assert_eq!(chunks[1].source_rows.len(), 4);
        assert_eq!(chunks[2].source_rows.len(), 2);
        assert_eq!(chunks[0].source_rows[0].id, "table-001-row-0001");
        assert_eq!(chunks[2].source_rows[0].id, "table-001-row-0009");
    }

    #[test]
    fn ditto_values_carry_by_column_and_partial_names_expand_without_changing_evidence() {
        let transcript = "| Name | Birthplace | Age |\n| --- | --- | --- |\n| Emma Hawkesworth | Southampton | 36 |\n| Emma M do | do | 37 |\n| Mary do | \" | 4 |\n";
        let chunk = extraction_chunks(transcript).remove(0);
        let rows = &chunk.source_rows;
        let output = serde_json::json!({
            "document_type": "census",
            "person_mentions": [
                {
                    "name_as_written": "Emma Hawkesworth",
                    "source_row_ids": [&rows[0].id],
                    "name_evidence": { "quote": "Emma Hawkesworth", "location": "row 1, name" },
                    "name_source_row_id": &rows[0].id,
                    "name_source_column_index": 1,
                    "facts": [
                        {
                            "label": "Birthplace", "value": "Southampton",
                            "source_row_id": &rows[0].id, "source_column_index": 2,
                            "evidence": { "quote": "Southampton", "location": "row 1, birthplace" }
                        },
                        {
                            "label": "Age", "value": "36",
                            "source_row_id": &rows[0].id, "source_column_index": 3,
                            "evidence": { "quote": "36", "location": "row 1, age" }
                        }
                    ]
                },
                {
                    "name_as_written": "Emma M do",
                    "source_row_ids": [&rows[1].id],
                    "name_evidence": { "quote": "Emma M do", "location": "row 2, name" },
                    "name_source_row_id": &rows[1].id,
                    "name_source_column_index": 1,
                    "facts": [
                        {
                            "label": "Birthplace", "value": "do",
                            "source_row_id": &rows[1].id, "source_column_index": 2,
                            "evidence": { "quote": "do", "location": "row 2, birthplace" }
                        },
                        {
                            "label": "Age", "value": "37",
                            "source_row_id": &rows[1].id, "source_column_index": 3,
                            "evidence": { "quote": "37", "location": "row 2, age" }
                        }
                    ]
                },
                {
                    "name_as_written": "Mary do",
                    "source_row_ids": [&rows[2].id],
                    "name_evidence": { "quote": "Mary do", "location": "row 3, name" },
                    "name_source_row_id": &rows[2].id,
                    "name_source_column_index": 1,
                    "facts": [
                        {
                            "label": "Birthplace", "value": "\"",
                            "source_row_id": &rows[2].id, "source_column_index": 2,
                            "evidence": { "quote": "\"", "location": "row 3, birthplace" }
                        },
                        {
                            "label": "Age", "value": "4",
                            "source_row_id": &rows[2].id, "source_column_index": 3,
                            "evidence": { "quote": "4", "location": "row 3, age" }
                        }
                    ]
                }
            ],
            "non_person_source_row_ids": []
        });
        let payload =
            parse_extraction_for_rows(&output.to_string(), transcript, &chunk.source_rows)
                .expect("ditto extraction");
        let metadata = build_metadata(payload, "census.jpg", &"a".repeat(64), transcript)
            .expect("valid ditto metadata");

        let second = &metadata.person_mentions[1];
        assert_eq!(second.name_as_written, "Emma M Hawkesworth");
        assert_eq!(
            second.name_evidence.as_ref().expect("name evidence").quote,
            "Emma M do"
        );
        assert_eq!(
            second.name_assertion_status.as_deref(),
            Some("inferred from ditto marker")
        );
        assert_eq!(second.facts[0].value, "Southampton");
        assert_eq!(second.facts[0].evidence.quote, "do");
        assert_eq!(
            second.facts[0].assertion_status.as_deref(),
            Some("inferred from ditto marker")
        );
        let third = &metadata.person_mentions[2];
        assert_eq!(third.name_as_written, "Mary Hawkesworth");
        assert_eq!(third.facts[0].value, "Southampton");
        assert_eq!(third.facts[0].evidence.quote, "\"");
    }

    #[test]
    fn ditto_carry_forward_resets_at_each_table() {
        let transcript = "| Name | Place |\n| --- | --- |\n| Ada | London |\n\n| Name | Place |\n| --- | --- |\n| do | do |\n";
        let rows = extraction_chunks(transcript)
            .into_iter()
            .flat_map(|chunk| chunk.source_rows)
            .collect::<Vec<_>>();

        assert_eq!(rows[0].resolved_cells, vec![None, None]);
        assert_eq!(rows[1].resolved_cells, vec![None, None]);
    }

    #[test]
    fn table_extraction_must_account_for_each_row_and_populated_cell() {
        let transcript =
            "| Name | Age | Birthplace |\n| --- | --- | --- |\n| Ada | 12 | London |\n";
        let chunk = extraction_chunks(transcript).remove(0);
        let row_id = &chunk.source_rows[0].id;
        let output = format!(
            r#"{{
                "document_type": "census",
                "person_mentions": [{{
                    "name_as_written": "Ada",
                    "source_row_ids": ["{row_id}"],
                    "name_evidence": {{"quote":"Ada","location":"row 1, name"}},
                    "name_source_row_id": "{row_id}",
                    "name_source_column_index": 1,
                    "facts": [
                        {{"label":"Age","value":"12","source_row_id":"{row_id}","source_column_index":2,"evidence":{{"quote":"12","location":"row 1, age"}}}},
                        {{"label":"Birthplace","value":"London","source_row_id":"{row_id}","source_column_index":3,"evidence":{{"quote":"London","location":"row 1, birthplace"}}}}
                    ]
                }}],
                "non_person_source_row_ids": []
            }}"#
        );
        let payload = parse_extraction_for_rows(&output, transcript, &chunk.source_rows)
            .expect("complete row extraction");
        let metadata = build_metadata(payload, "census.jpg", &"a".repeat(64), transcript)
            .expect("complete metadata");
        assert_eq!(
            metadata.person_mentions[0].source_row_ids,
            vec![row_id.clone()]
        );

        let incomplete = output.replace(r#""value":"London""#, r#""value":"elsewhere""#);
        assert!(parse_extraction_for_rows(&incomplete, transcript, &chunk.source_rows).is_err());
    }

    #[test]
    fn metadata_is_bound_to_the_exact_transcript_text() {
        let directory = tempfile::tempdir().expect("temporary transcript directory");
        let transcript_path = directory.path().join("record.md");
        let transcript = "Ada was recorded in the register.";
        std::fs::write(&transcript_path, transcript).expect("write transcript");
        let json_path = super::json_path_for_transcript(&transcript_path);
        let metadata = serde_json::json!({
            "schema_version": super::SCHEMA_VERSION,
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

        let current = read_for_transcript(&transcript_path)
            .expect("read valid metadata")
            .expect("metadata exists");
        assert!(super::is_current(&current, &"a".repeat(64)));
        let mut previous = current;
        previous.schema_version = "1.1".to_owned();
        assert!(super::validate_metadata(&previous, transcript).is_ok());
        assert!(!super::is_current(&previous, &"a".repeat(64)));

        std::fs::write(
            &transcript_path,
            "Ada was recorded in a different register.",
        )
        .expect("edit transcript");
        assert!(read_for_transcript(&transcript_path).is_err());
    }
}
