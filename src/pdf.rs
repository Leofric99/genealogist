use anyhow::{Context, Result};
use lopdf::Document;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub fn page_count(path: &Path) -> Result<usize> {
    let document =
        Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    Ok(document.get_pages().len())
}

pub fn split_into_page_groups_cancellable(
    path: &Path,
    output_dir: &Path,
    pages_per_group: usize,
    cancellation: &CancellationToken,
) -> Result<Option<Vec<PathBuf>>> {
    if pages_per_group == 0 {
        anyhow::bail!("PDF page group size must be greater than zero");
    }
    let document =
        Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    std::fs::create_dir_all(output_dir)?;
    let mut results = Vec::with_capacity(pages.len().div_ceil(pages_per_group));
    for (group_index, group) in pages.chunks(pages_per_group).enumerate() {
        if cancellation.is_cancelled() {
            return Ok(None);
        }
        let mut subset = Document::load(path)?;
        let keep: std::collections::HashSet<u32> = group.iter().copied().collect();
        let remove: Vec<u32> = pages
            .iter()
            .copied()
            .filter(|page| !keep.contains(page))
            .collect();
        subset.delete_pages(&remove);
        subset.prune_objects();
        let first = group.first().copied().unwrap_or_default();
        let last = group.last().copied().unwrap_or_default();
        let output = output_dir.join(format!(
            "group-{:04}-{first:04}-{last:04}.pdf",
            group_index + 1
        ));
        subset
            .save(&output)
            .with_context(|| format!("writing PDF page group to {}", output.display()))?;
        results.push(output);
    }
    Ok(Some(results))
}

pub fn group_page_ranges(page_count: usize, pages_per_group: usize) -> Result<Vec<(usize, usize)>> {
    if pages_per_group == 0 {
        anyhow::bail!("PDF page group size must be greater than zero");
    }
    Ok((0..page_count)
        .step_by(pages_per_group)
        .map(|start| (start + 1, (start + pages_per_group).min(page_count)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::group_page_ranges;

    #[test]
    fn page_group_ranges_cover_all_pages_in_groups_of_ten() {
        assert_eq!(
            group_page_ranges(23, 10).unwrap(),
            vec![(1, 10), (11, 20), (21, 23)]
        );
        assert_eq!(
            group_page_ranges(0, 10).unwrap(),
            Vec::<(usize, usize)>::new()
        );
        assert!(group_page_ranges(4, 0).is_err());
    }
}
