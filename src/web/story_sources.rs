use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::ErrorKind,
    path::{Component, Path},
};

const FUZZY_NAME_THRESHOLD: f64 = 0.78;

#[derive(Clone, Debug, Default)]
pub struct PersonIdentity {
    pub tree_key: String,
    pub xref: String,
    pub name: String,
    pub birth_year: Option<i32>,
    pub birth_place: Option<String>,
    pub event_years: Vec<i32>,
    pub event_places: Vec<String>,
    pub related_names: Vec<String>,
}

impl PersonIdentity {
    pub fn stable_key(&self) -> String {
        let identity = format!(
            "{}\0{}\0{}\0{}",
            self.tree_key,
            self.xref,
            normalize_name(&self.name),
            self.birth_year
                .map_or_else(String::new, |year| year.to_string())
        );
        hex::encode(Sha256::digest(identity.as_bytes()))
    }
}

#[derive(Clone, Debug)]
pub struct StorySource {
    pub relative_path: String,
    pub contents: String,
    pub transcript_sha256: String,
    pub confidence: f64,
    pub match_evidence: Vec<String>,
    pub link_status: String,
}

pub fn collect_person_sources(records: &Path, person: &PersonIdentity) -> Result<Vec<StorySource>> {
    let mut matches = Vec::new();
    if !records.is_dir() {
        return Ok(matches);
    }

    for entry in walkdir::WalkDir::new(records)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file()
            || !entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let raw_contents = fs::read_to_string(entry.path())
            .with_context(|| format!("reading transcript {}", entry.path().display()))?;
        let contents = crate::gemini_client::normalize_transcript(&raw_contents);
        let relative_path = entry
            .path()
            .strip_prefix(records)?
            .to_string_lossy()
            .replace('\\', "/");
        let Some((confidence, match_evidence)) =
            matching_evidence(&relative_path, &contents, person)
        else {
            continue;
        };
        matches.push(StorySource {
            relative_path,
            transcript_sha256: sha256_text(&raw_contents),
            contents,
            confidence,
            match_evidence,
            link_status: "suggested".to_owned(),
        });
    }

    matches.sort_by(|left, right| {
        right
            .confidence
            .total_cmp(&left.confidence)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    Ok(matches)
}

pub fn read_transcript(records: &Path, relative_path: &str) -> Result<Option<StorySource>> {
    let relative = Path::new(relative_path);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("transcript path must be a normalized path relative to the records directory");
    }
    if !relative
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
    {
        bail!("linked source is not a Markdown transcript");
    }
    let root = fs::canonicalize(records)
        .with_context(|| format!("resolving records directory {}", records.display()))?;
    let path = match fs::canonicalize(root.join(relative)) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("resolving linked transcript path"),
    };
    if !path.starts_with(&root) {
        bail!("linked transcript resolves outside the records directory");
    }
    let raw_contents = fs::read_to_string(&path)
        .with_context(|| format!("reading linked transcript {}", path.display()))?;
    let contents = crate::gemini_client::normalize_transcript(&raw_contents);
    Ok(Some(StorySource {
        relative_path: relative_path.replace('\\', "/"),
        transcript_sha256: sha256_text(&raw_contents),
        contents,
        confidence: 0.0,
        match_evidence: Vec::new(),
        link_status: "confirmed".to_owned(),
    }))
}

fn matching_evidence(
    relative_path: &str,
    contents: &str,
    person: &PersonIdentity,
) -> Option<(f64, Vec<String>)> {
    let exact_name_in_text = has_name_sequence(contents, &person.name);
    let exact_name_in_path = has_name_sequence(relative_path, &person.name);
    let text_similarity = best_phrase_similarity(&person.name, contents);
    let path_similarity = name_similarity(&person.name, relative_path);
    let source_years = years_in(contents)
        .into_iter()
        .chain(years_in(relative_path))
        .collect::<HashSet<_>>();
    let year_match = person
        .birth_year
        .is_some_and(|year| source_years.contains(&year))
        || person
            .event_years
            .iter()
            .any(|year| source_years.contains(year));
    let place_match = person
        .birth_place
        .as_deref()
        .is_some_and(|place| contains_place(contents, place))
        || person
            .event_places
            .iter()
            .any(|place| contains_place(contents, place));
    let related_names = person
        .related_names
        .iter()
        .filter(|name| has_name_sequence(contents, name))
        .collect::<Vec<_>>();
    let all_name_tokens_present = tokens(&person.name)
        .iter()
        .all(|token| tokens(contents).iter().any(|source| source == token));
    let corroborated = year_match || place_match || !related_names.is_empty();
    let fuzzy_name_in_text = text_similarity >= FUZZY_NAME_THRESHOLD && corroborated;
    let path_match_with_context =
        (exact_name_in_path || path_similarity >= FUZZY_NAME_THRESHOLD) && corroborated;
    let grouped_name_match = all_name_tokens_present
        && (exact_name_in_path || corroborated)
        && tokens(&person.name).len() > 1;

    if !exact_name_in_text && !fuzzy_name_in_text && !path_match_with_context && !grouped_name_match
    {
        return None;
    }

    let mut evidence = Vec::new();
    if exact_name_in_text {
        evidence.push("exact name appears in transcript text".to_owned());
    } else if fuzzy_name_in_text || grouped_name_match {
        evidence.push("name tokens or a close OCR variant appear in transcript text".to_owned());
    }
    if exact_name_in_path {
        evidence.push("exact name appears in transcript path".to_owned());
    } else if path_similarity >= FUZZY_NAME_THRESHOLD {
        evidence.push("similar name appears in transcript path".to_owned());
    }
    if year_match {
        evidence.push("a GEDCOM birth or event year appears in transcript text or path".to_owned());
    }
    if place_match {
        evidence.push("a GEDCOM birth or event place appears in transcript text".to_owned());
    }
    if !related_names.is_empty() {
        evidence.push(format!(
            "GEDCOM relative name(s) also appear: {}",
            related_names
                .into_iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut confidence: f64 = if exact_name_in_text {
        0.68
    } else {
        0.52 + text_similarity.min(path_similarity) * 0.25
    };
    if year_match {
        confidence += 0.12;
    }
    if place_match {
        confidence += 0.08;
    }
    if exact_name_in_path {
        confidence += 0.08;
    }
    if !related_names.is_empty() {
        confidence += 0.08;
    }
    Some((confidence.min(0.99), evidence))
}

fn best_phrase_similarity(name: &str, text: &str) -> f64 {
    let name_tokens = tokens(name);
    let source_tokens = tokens(text);
    if name_tokens.is_empty() || source_tokens.len() < name_tokens.len() {
        return 0.0;
    }
    source_tokens
        .windows(name_tokens.len())
        .map(|window| name_similarity(&name_tokens.join(" "), &window.join(" ")))
        .fold(0.0, f64::max)
}

fn contains_place(text: &str, place: &str) -> bool {
    let text_tokens = tokens(text).into_iter().collect::<HashSet<_>>();
    let place_tokens = tokens(place);
    has_name_sequence(text, place)
        || place_tokens.len() > 1 && place_tokens.iter().all(|token| text_tokens.contains(token))
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
    left.iter()
        .map(|target| {
            right
                .iter()
                .map(|candidate| word_similarity(target, candidate))
                .fold(0.0, f64::max)
        })
        .sum::<f64>()
        / left.len() as f64
}

fn word_similarity(left: &str, right: &str) -> f64 {
    let longest = left.chars().count().max(right.chars().count());
    if longest < 4 {
        return if left == right { 1.0 } else { 0.0 };
    }
    1.0 - levenshtein(left, right) as f64 / longest as f64
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

fn sha256_text(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::{collect_person_sources, read_transcript, PersonIdentity};
    use std::fs;

    #[test]
    fn exact_name_and_year_in_body_suggest_a_body_only_transcript() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        let person = PersonIdentity {
            xref: "@I1@".to_owned(),
            name: "Arthur /Astle/".to_owned(),
            birth_year: Some(1870),
            ..PersonIdentity::default()
        };
        fs::write(
            directory.path().join("baptism.md"),
            "Baptism register\nArthur Astle, born 1870.",
        )
        .expect("write transcript");

        let sources = collect_person_sources(directory.path(), &person).expect("find sources");
        assert_eq!(sources.len(), 1);
        assert!(sources[0]
            .match_evidence
            .iter()
            .any(|evidence| evidence.contains("exact name")));
        assert_eq!(sources[0].link_status, "suggested");
    }

    #[test]
    fn fuzzy_name_requires_independent_year_or_family_context() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        fs::write(
            directory.path().join("record.md"),
            "Arthur Aalle appears in the record.",
        )
        .expect("write transcript");
        let person = PersonIdentity {
            xref: "@I1@".to_owned(),
            name: "Arthur /Astle/".to_owned(),
            birth_year: Some(1870),
            ..PersonIdentity::default()
        };
        assert!(collect_person_sources(directory.path(), &person)
            .expect("find sources")
            .is_empty());

        fs::write(
            directory.path().join("record.md"),
            "Arthur Aalle appears in 1901; his father John Astle is listed.",
        )
        .expect("update transcript");
        let mut person = person;
        person.related_names.push("John Astle".to_owned());
        let sources = collect_person_sources(directory.path(), &person).expect("find sources");
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn legacy_frontmatter_does_not_drive_matching_or_reach_story_sources() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        let person = PersonIdentity {
            xref: "@I1@".to_owned(),
            name: "Ada Example".to_owned(),
            ..PersonIdentity::default()
        };
        fs::write(
            directory.path().join("unrelated.md"),
            "---\nprimary_individuals:\n  - name: Ada Example\n---\nAn unrelated record.",
        )
        .expect("write legacy transcript");
        assert!(collect_person_sources(directory.path(), &person)
            .expect("find sources")
            .is_empty());

        fs::write(
            directory.path().join("legacy.md"),
            "---\ndocument_type: birth register\n---\nAda Example, born in 1900.",
        )
        .expect("write legacy transcript with body match");
        let sources = collect_person_sources(directory.path(), &person).expect("find sources");
        let source = sources
            .iter()
            .find(|source| source.relative_path == "legacy.md")
            .expect("body match should be discovered");
        assert!(!source.contents.starts_with("---"));
    }

    #[test]
    fn confirmed_transcript_paths_cannot_escape_records_root() {
        let directory = tempfile::tempdir().expect("temporary records directory");
        assert!(read_transcript(directory.path(), "../outside.md").is_err());
        assert!(read_transcript(directory.path(), "source.jpg").is_err());
    }
}
