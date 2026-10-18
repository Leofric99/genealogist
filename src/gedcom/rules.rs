use super::{
    graph::FamilyGraph,
    parser::{date_year, Gedcom},
};
use serde::Serialize;
use std::{collections::HashMap, fs, path::Path};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Anomaly {
    pub rule: String,
    pub severity: String,
    pub people: Vec<String>,
    pub detail: String,
    pub source: Option<String>,
}

pub fn check(gedcom: &Gedcom<'_>, transcript_dir: &Path) -> anyhow::Result<Vec<Anomaly>> {
    let graph = FamilyGraph::build(gedcom);
    let mut anomalies = Vec::new();
    check_parent_timeline(gedcom, &graph, &mut anomalies);
    check_travel(gedcom, &mut anomalies);
    check_duplicate_identity(gedcom, transcript_dir, &mut anomalies)?;
    Ok(anomalies)
}

fn check_parent_timeline(
    gedcom: &Gedcom<'_>,
    graph: &FamilyGraph<'_, '_>,
    anomalies: &mut Vec<Anomaly>,
) {
    let by_id: HashMap<_, _> = gedcom
        .people
        .iter()
        .map(|person| (person.xref, person))
        .collect();
    for child in &gedcom.people {
        let child_year = child
            .birth
            .as_ref()
            .and_then(|event| event.date)
            .and_then(date_year);
        let Some(child_year) = child_year else {
            continue;
        };
        for parent_id in graph.parents.get(child.xref).into_iter().flatten() {
            let Some(parent) = by_id.get(parent_id) else {
                continue;
            };
            let parent_birth = parent
                .birth
                .as_ref()
                .and_then(|event| event.date)
                .and_then(date_year);
            if let Some(parent_birth) = parent_birth {
                let age = child_year - parent_birth;
                if parent.sex == Some("F") && !(12..=52).contains(&age) {
                    anomalies.push(Anomaly {
                        rule: "parent_age_at_birth".to_owned(),
                        severity: "high".to_owned(),
                        people: vec![parent.xref.to_owned(), child.xref.to_owned()],
                        detail: format!("Parent birth year {parent_birth}, child birth year {child_year}: calculated parent age is {age}."),
                        source: None,
                    });
                }
            }
            if let Some(death_year) = parent
                .death
                .as_ref()
                .and_then(|event| event.date)
                .and_then(date_year)
            {
                if child_year > death_year {
                    anomalies.push(Anomaly {
                        rule: "child_after_parent_death".to_owned(),
                        severity: "high".to_owned(),
                        people: vec![parent.xref.to_owned(), child.xref.to_owned()],
                        detail: format!("Child birth year {child_year} is after parent death year {death_year}."),
                        source: None,
                    });
                }
            }
        }
    }
}

fn check_travel(gedcom: &Gedcom<'_>, anomalies: &mut Vec<Anomaly>) {
    for person in &gedcom.people {
        let events: Vec<_> = person
            .events
            .iter()
            .filter_map(|event| {
                let year = event.date.and_then(date_year)?;
                Some((year, event.date.unwrap_or(""), event.place?))
            })
            .collect();
        for pair in events.windows(2) {
            let (year_a, date_a, place_a) = pair[0];
            let (year_b, date_b, place_b) = pair[1];
            if place_a.trim().eq_ignore_ascii_case(place_b.trim()) {
                continue;
            }
            if year_a == year_b && date_a != date_b {
                let day_a = date_day_of_year(date_a);
                let day_b = date_day_of_year(date_b);
                if let (Some(day_a), Some(day_b)) = (day_a, day_b) {
                    if day_a.abs_diff(day_b) <= 1 {
                        anomalies.push(Anomaly {
                            rule: "possible_location_teleportation".to_owned(),
                            severity: "medium".to_owned(),
                            people: vec![person.xref.to_owned()],
                            detail: format!("Events in {place_a} and {place_b} are recorded within one day of each other."),
                            source: None,
                        });
                    }
                }
            }
        }
    }
}

fn date_day_of_year(date: &str) -> Option<u32> {
    let mut parts = date.split_whitespace();
    let day = parts.next()?.parse::<u32>().ok()?;
    let month = parts.next()?.to_ascii_uppercase();
    let month = match month.get(..3)? {
        "JAN" => 1,
        "FEB" => 2,
        "MAR" => 3,
        "APR" => 4,
        "MAY" => 5,
        "JUN" => 6,
        "JUL" => 7,
        "AUG" => 8,
        "SEP" => 9,
        "OCT" => 10,
        "NOV" => 11,
        "DEC" => 12,
        _ => return None,
    };
    Some(month * 32 + day)
}

fn check_duplicate_identity(
    gedcom: &Gedcom<'_>,
    transcript_dir: &Path,
    anomalies: &mut Vec<Anomaly>,
) -> anyhow::Result<()> {
    let mut known: HashMap<(String, i32), Vec<String>> = HashMap::new();
    for person in &gedcom.people {
        let Some(name) = person.name else {
            continue;
        };
        let year = person
            .birth
            .as_ref()
            .and_then(|event| event.date)
            .and_then(date_year);
        if let Some(year) = year {
            known
                .entry((normalize_name(name), year))
                .or_default()
                .push(person.xref.to_owned());
        }
    }
    for ((name, year), ids) in known.iter().filter(|(_, ids)| ids.len() > 1) {
        anomalies.push(Anomaly {
            rule: "duplicate_identity".to_owned(),
            severity: "medium".to_owned(),
            people: ids.to_vec(),
            detail: format!("Multiple GEDCOM individuals share normalized name '{name}' and birth year {year}; confirm source assignments."),
            source: None,
        });
    }
    if !transcript_dir.is_dir() {
        return Ok(());
    }
    for entry in walkdir::WalkDir::new(transcript_dir).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("md")
        {
            continue;
        }
        let text = fs::read_to_string(entry.path())?;
        let normalized_text = format!(" {} ", normalize_name(&text));
        let path = entry.path().to_string_lossy();
        let years = years_in(&text)
            .into_iter()
            .chain(years_in(&path))
            .collect::<std::collections::HashSet<_>>();
        for ((name, year), ids) in known.iter().filter(|(_, ids)| ids.len() > 1) {
            if normalized_text.contains(&format!(" {name} ")) && years.contains(year) {
                anomalies.push(Anomaly {
                    rule: "ambiguous_transcript_identity".to_owned(),
                    severity: "medium".to_owned(),
                    people: ids.clone(),
                    detail: format!(
                        "Transcript text mentions '{name}' and includes year {year}, matching multiple GEDCOM individuals."
                    ),
                    source: Some(entry.path().display().to_string()),
                });
            }
        }
    }
    Ok(())
}

fn years_in(text: &str) -> Vec<i32> {
    text.split(|character: char| !character.is_ascii_digit())
        .filter(|part| part.len() == 4)
        .filter_map(|part| part.parse::<i32>().ok())
        .filter(|year| (1500..=2100).contains(year))
        .collect()
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric() || ch.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::gedcom::parser;

    #[test]
    fn flags_implausible_maternal_age_and_birth_after_death() {
        let input = "0 @I1@ INDI\n1 NAME Ada /Example/\n1 SEX F\n1 BIRT\n2 DATE 1900\n1 DEAT\n2 DATE 1940\n0 @I2@ INDI\n1 NAME Child /Example/\n1 BIRT\n2 DATE 1955\n0 @F1@ FAM\n1 WIFE @I1@\n1 CHIL @I2@\n";
        let gedcom = parser::parse(input);
        let directory = tempfile::tempdir().expect("temporary transcript directory");
        let findings = check(&gedcom, directory.path()).expect("run GEDCOM rule checks");
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "parent_age_at_birth"));
        assert!(findings
            .iter()
            .any(|finding| finding.rule == "child_after_parent_death"));
    }

    #[test]
    fn finds_ambiguous_people_from_body_only_transcripts() {
        let input = "0 @I1@ INDI\n1 NAME Ada /Example/\n1 BIRT\n2 DATE 1900\n0 @I2@ INDI\n1 NAME Ada /Example/\n1 BIRT\n2 DATE 1900\n";
        let gedcom = parser::parse(input);
        let directory = tempfile::tempdir().expect("temporary transcript directory");
        std::fs::write(
            directory.path().join("register.md"),
            "Birth register entry: Ada Example, 1900.",
        )
        .expect("write body-only transcript");

        let findings = check(&gedcom, directory.path()).expect("run GEDCOM rule checks");
        assert!(findings.iter().any(|finding| {
            finding.rule == "ambiguous_transcript_identity"
                && finding
                    .source
                    .as_deref()
                    .is_some_and(|path| path.ends_with("register.md"))
        }));
    }
}
