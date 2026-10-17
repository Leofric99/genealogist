pub fn validate(story: &str, allowed_sources: &[String]) -> Result<(), String> {
    let lines = story.lines().collect::<Vec<_>>();
    let mut headings = 0;
    for (index, line) in lines.iter().enumerate() {
        if heading_level(line) < 2 {
            continue;
        }
        headings += 1;
        let confidence = lines
            .get(index + 1)
            .map(|line| is_confidence_line(line.trim()))
            .unwrap_or(false);
        if !confidence {
            return Err(format!(
                "section heading on line {} is not followed immediately by a confidence percentage",
                index + 1
            ));
        }
    }
    if headings == 0 {
        return Err("the biography has no section headings with confidence scores".to_owned());
    }

    for (index, line) in lines.iter().enumerate() {
        let text = line.trim();
        if text.is_empty()
            || heading_level(text) > 0
            || is_confidence_line(text)
            || is_horizontal_rule(text)
        {
            continue;
        }
        if !contains_allowed_citation(text, allowed_sources) {
            return Err(format!(
                "biographical text on line {} has no citation to an allowed transcript",
                index + 1
            ));
        }
    }
    Ok(())
}

fn heading_level(line: &str) -> usize {
    let trimmed = line.trim_start();
    let level = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
        level
    } else {
        0
    }
}

fn is_confidence_line(line: &str) -> bool {
    line.strip_prefix("**Confidence: ")
        .and_then(|value| value.strip_suffix("%**"))
        .and_then(|value| value.parse::<u8>().ok())
        .is_some_and(|confidence| confidence <= 100)
}

fn is_horizontal_rule(line: &str) -> bool {
    matches!(line, "---" | "***" | "___")
}

fn contains_allowed_citation(line: &str, allowed_sources: &[String]) -> bool {
    allowed_sources
        .iter()
        .any(|path| line.contains(&format!("]({path})")) || line.contains(&format!("](<{path}>)")))
}

#[cfg(test)]
mod tests {
    use super::validate;

    fn source() -> Vec<String> {
        vec!["records/Arthur Astle 1901 Census.md".to_owned()]
    }

    #[test]
    fn accepts_cited_claims_with_confidence_under_each_heading() {
        let story = "# Arthur Astle\n\
## Early life\n\
**Confidence: 90%**\n\
Arthur was born in 1870. [1901 census](<records/Arthur Astle 1901 Census.md>)\n\
\n\
## Sources\n\
**Confidence: 90%**\n\
- [1901 census](<records/Arthur Astle 1901 Census.md>)";
        assert!(validate(story, &source()).is_ok());
    }

    #[test]
    fn rejects_heading_without_confidence_and_uncited_claims() {
        let missing_confidence = "## Early life\n\
Arthur was born in 1870. [census](<records/Arthur Astle 1901 Census.md>)";
        assert!(validate(missing_confidence, &source()).is_err());

        let uncited = "## Early life\n\
**Confidence: 90%**\n\
Arthur was born in 1870.";
        assert!(validate(uncited, &source()).is_err());
    }

    #[test]
    fn rejects_citations_to_paths_not_selected_for_this_story() {
        let wrong_source = "## Early life\n\
**Confidence: 90%**\n\
Arthur was born in 1870. [source](other.md)";
        assert!(validate(wrong_source, &source()).is_err());
    }
}
