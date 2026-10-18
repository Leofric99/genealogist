use anyhow::Result;
use serde_yaml::Value;
use std::{collections::HashSet, fs, path::Path};

const FUZZY_NAME_THRESHOLD: f64 = 0.78;

#[derive(Clone, Debug, Default)]
pub struct PersonIdentity {
    pub name: String,
    pub birth_year: Option<i32>,
    pub birth_place: Option<String>,
    pub event_years: Vec<i32>,
    pub event_places: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct StorySource {
    pub relative_path: String,
    pub contents: String,
    pub match_evidence: Vec<String>,
}

pub fn collect_person_sources(records: &Path, person: &PersonIdentity) -> Result<Vec<StorySource>> {
    let mut matches = Vec::new();
    if !records.is_dir() {
        return Ok(matches);
    }

    for entry in walkdir::WalkDir::new(records).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        let contents = fs::read_to_string(entry.path())?;
        let relative_path = entry
            .path()
            .strip_prefix(records)?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = match crate::transcript_data::read_for_transcript(entry.path())? {
            Some(metadata) => crate::transcript_data::to_legacy_metadata(&metadata)?,
            None => {
                let Some(frontmatter) = extract_frontmatter(&contents) else {
                    continue;
                };
                let Ok(metadata) = serde_yaml::from_str::<Value>(frontmatter) else {
                    continue;
                };
                metadata
            }
        };
        if let Some(match_evidence) = matching_evidence(&metadata, &relative_path, person) {
            matches.push(StorySource {
                relative_path,
                contents,
                match_evidence,
            });
        }
    }

    matches.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(matches)
}

fn matching_evidence(
    metadata: &Value,
    relative_path: &str,
    person: &PersonIdentity,
) -> Option<Vec<String>> {
    let source_years = metadata_years(metadata)
        .into_iter()
        .chain(years_in(relative_path))
        .collect::<HashSet<_>>();
    let filename_name_score = name_similarity(&person.name, relative_path);
    let exact_name_in_path = has_name_sequence(relative_path, &person.name);
    let mut path_evidence = Vec::new();
    if person
        .birth_year
        .is_some_and(|year| source_years.contains(&year))
    {
        path_evidence.push("GEDCOM birth year appears in transcript dates or file path".to_owned());
    }
    path_evidence.extend(event_date_evidence(&source_years, person));
    let mut best: Option<(f64, Vec<String>)> = None;

    if exact_name_in_path && !path_evidence.is_empty() {
        best = Some((
            1.05,
            std::iter::once("exact person name in file path".to_owned())
                .chain(path_evidence.iter().cloned())
                .collect(),
        ));
    }

    if let Some(individuals) = metadata
        .get("primary_individuals")
        .and_then(Value::as_sequence)
    {
        for individual in individuals {
            let Some(name) = individual
                .as_mapping()
                .and_then(|mapping| mapping_value(mapping, "name"))
                .and_then(Value::as_str)
            else {
                continue;
            };
            let name_score = name_similarity(&person.name, name);
            if normalize_name(name) == normalize_name(&person.name)
                || has_name_sequence(name, &person.name)
            {
                let evidence = vec!["exact person name in transcript metadata".to_owned()];
                keep_better(&mut best, 1.1, evidence);
                continue;
            }

            let corroboration =
                corroborating_evidence_for_individual(individual, &source_years, person);
            let score = name_score.max(filename_name_score);
            if score >= FUZZY_NAME_THRESHOLD && !corroboration.is_empty() {
                let mut evidence = Vec::new();
                if name_score >= FUZZY_NAME_THRESHOLD {
                    evidence.push(format!(
                        "similar name in transcript metadata ({}% similarity)",
                        (name_score * 100.0).round() as u32
                    ));
                }
                if exact_name_in_path {
                    evidence.push("exact person name in file path".to_owned());
                } else if filename_name_score >= FUZZY_NAME_THRESHOLD {
                    evidence.push(format!(
                        "similar name in file path ({}% similarity)",
                        (filename_name_score * 100.0).round() as u32
                    ));
                }
                evidence.extend(corroboration);
                keep_better(&mut best, score, evidence);
            }
        }
    }

    best.map(|(_, evidence)| evidence)
}

fn keep_better(best: &mut Option<(f64, Vec<String>)>, score: f64, evidence: Vec<String>) {
    if best.as_ref().map_or(true, |(current, _)| score > *current) {
        *best = Some((score, evidence));
    }
}

fn corroborating_evidence_for_individual(
    individual: &Value,
    source_years: &HashSet<i32>,
    person: &PersonIdentity,
) -> Vec<String> {
    let mut evidence = Vec::new();
    if person
        .birth_year
        .is_some_and(|year| source_years.contains(&year))
    {
        evidence.push("GEDCOM birth year appears in transcript dates or file path".to_owned());
    }

    if let Some(age) = individual
        .as_mapping()
        .and_then(|mapping| mapping_value(mapping, "age"))
        .and_then(value_as_i32)
    {
        if person.birth_year.is_some_and(|birth_year| {
            source_years
                .iter()
                .any(|year| (*year - age - birth_year).abs() <= 1)
        }) {
            evidence.push("transcribed age is compatible with GEDCOM birth year".to_owned());
        }
    }

    let birthplace = individual
        .as_mapping()
        .and_then(|mapping| mapping_value(mapping, "birthplace"));
    if birthplace_matches(birthplace, person) {
        evidence.push("transcript birthplace is consistent with GEDCOM".to_owned());
    }
    evidence.extend(event_date_evidence(source_years, person));
    evidence
}

fn mapping_value<'a>(mapping: &'a serde_yaml::Mapping, key: &str) -> Option<&'a Value> {
    mapping.get(&Value::String(key.to_owned()))
}

fn event_date_evidence(source_years: &HashSet<i32>, person: &PersonIdentity) -> Vec<String> {
    if person
        .event_years
        .iter()
        .any(|year| source_years.contains(year))
    {
        vec!["a GEDCOM event year appears in transcript dates or file path".to_owned()]
    } else {
        Vec::new()
    }
}

fn birthplace_matches(value: Option<&Value>, person: &PersonIdentity) -> bool {
    let Some(candidate) = value.and_then(value_as_text) else {
        return false;
    };
    person
        .birth_place
        .as_deref()
        .is_some_and(|target| place_similarity(target, &candidate) >= 0.6)
        || person
            .event_places
            .iter()
            .any(|place| place_similarity(place, &candidate) >= 0.75)
}

fn place_similarity(left: &str, right: &str) -> f64 {
    let left_tokens = tokens(left).into_iter().collect::<HashSet<_>>();
    let right_tokens = tokens(right).into_iter().collect::<HashSet<_>>();
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0.0;
    }
    let shared = left_tokens.intersection(&right_tokens).count();
    shared as f64 / left_tokens.len().min(right_tokens.len()) as f64
}

fn value_as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn value_as_i32(value: &Value) -> Option<i32> {
    match value {
        Value::Number(number) => number.as_i64()?.try_into().ok(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn metadata_years(metadata: &Value) -> Vec<i32> {
    let mut years = metadata
        .get("title_as_written")
        .and_then(value_as_text)
        .into_iter()
        .flat_map(|title| years_in(&title))
        .chain(
            metadata
                .get("date_recorded")
                .and_then(value_as_text)
                .into_iter()
                .flat_map(|date| years_in(&date)),
        )
        .collect::<Vec<_>>();
    if let Some(dates) = metadata.get("all_extracted_dates") {
        collect_value_years(dates, &mut years);
    }
    years
}

fn collect_value_years(value: &Value, years: &mut Vec<i32>) {
    match value {
        Value::String(text) => years.extend(years_in(text)),
        Value::Sequence(items) => {
            for item in items {
                collect_value_years(item, years);
            }
        }
        Value::Mapping(mapping) => {
            for value in mapping.values() {
                collect_value_years(value, years);
            }
        }
        _ => {}
    }
}

fn years_in(text: &str) -> Vec<i32> {
    text.split(|character: char| !character.is_ascii_digit())
        .filter(|part| part.len() == 4)
        .filter_map(|part| part.parse::<i32>().ok())
        .filter(|year| (1500..=2100).contains(year))
        .collect()
}

fn has_name_sequence(text: &str, name: &str) -> bool {
    let text = tokens(text);
    let name = tokens(name);
    !name.is_empty()
        && text
            .windows(name.len())
            .any(|window| window.iter().zip(&name).all(|(left, right)| left == right))
}

fn name_similarity(left: &str, right: &str) -> f64 {
    let left = tokens(left);
    let right = tokens(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let scores = left
        .iter()
        .map(|target| {
            if right.iter().any(|candidate| candidate == target) {
                1.0
            } else {
                right
                    .iter()
                    .map(|candidate| word_similarity(target, candidate))
                    .fold(0.0, f64::max)
            }
        })
        .collect::<Vec<_>>();
    scores.iter().sum::<f64>() / scores.len() as f64
}

fn word_similarity(left: &str, right: &str) -> f64 {
    let longest = left.chars().count().max(right.chars().count());
    if longest < 4 {
        return 0.0;
    }
    let distance = levenshtein(left, right);
    1.0 - distance as f64 / longest as f64
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1; right.len() + 1];
        for (right_index, right_char) in right.iter().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_char != *right_char));
        }
        previous = current;
    }
    previous[right.len()]
}

fn tokens(text: &str) -> Vec<String> {
    normalize_name(text)
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric() || character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_frontmatter(text: &str) -> Option<&str> {
    let content = text.strip_prefix("---")?;
    let end = content.find("\n---")?;
    Some(&content[..end])
}

#[cfg(test)]
mod tests {
    use super::{collect_person_sources, name_similarity, PersonIdentity};
    use std::fs;

    #[test]
    fn fuzzy_name_is_linked_when_filename_birth_year_confirms_identity() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        let person_directory = directory.path().join("Arthur Astle (1870)");
        fs::create_dir(&person_directory).expect("create person directory");
        let transcript = person_directory.join("1901 Census.md");
        fs::write(
            &transcript,
            "---\nprimary_individuals:\n  - name: Arthur Aalle\n    age: 30\n    birthplace: West Ham\n---\nCensus row",
        )
        .expect("write transcript");
        let person = PersonIdentity {
            name: "Arthur /Astle/".to_owned(),
            birth_year: Some(1870),
            birth_place: Some("West Ham".to_owned()),
            ..PersonIdentity::default()
        };

        let sources = collect_person_sources(directory.path(), &person).expect("find sources");
        assert_eq!(sources.len(), 1);
        assert!(sources[0]
            .match_evidence
            .iter()
            .any(|evidence| evidence.contains("birth year")));
    }

    #[test]
    fn age_and_birthplace_can_corroborate_a_fuzzy_name() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        fs::write(
            directory.path().join("1901 Census.md"),
            "---\ndate_recorded: 1901\nprimary_individuals:\n  - name: Arthur Aalle\n    age: 30\n    birthplace: West Ham\n---\nCensus row",
        )
        .expect("write transcript");
        let person = PersonIdentity {
            name: "Arthur /Astle/".to_owned(),
            birth_year: Some(1870),
            birth_place: Some("West Ham".to_owned()),
            ..PersonIdentity::default()
        };

        let sources = collect_person_sources(directory.path(), &person).expect("find sources");
        assert_eq!(sources.len(), 1);
        assert!(sources[0]
            .match_evidence
            .iter()
            .any(|evidence| evidence.contains("age")));
        assert!(sources[0]
            .match_evidence
            .iter()
            .any(|evidence| evidence.contains("birthplace")));
    }

    #[test]
    fn fuzzy_name_alone_is_not_enough_to_link_a_source() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        fs::write(
            directory.path().join("Arthur Aalle.md"),
            "---\nprimary_individuals:\n  - name: Arthur Aalle\n---\nRecord",
        )
        .expect("write transcript");
        let person = PersonIdentity {
            name: "Arthur /Astle/".to_owned(),
            birth_year: Some(1870),
            ..PersonIdentity::default()
        };

        assert!(collect_person_sources(directory.path(), &person)
            .expect("find sources")
            .is_empty());
    }

    #[test]
    fn fuzzy_name_score_handles_single_letter_ocr_errors() {
        assert!(name_similarity("Arthur /Astle/", "Arthur Aalle") >= 0.78);
        assert!(name_similarity("Arthur /Astle/", "Arthur Jones") < 0.78);
    }
}
