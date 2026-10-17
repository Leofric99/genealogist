use anyhow::{Context, Result};
use lopdf::Document;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub fn page_count(path: &Path) -> Result<usize> {
    let document =
        Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    Ok(document.get_pages().len())
}

pub fn split_to_single_pages(path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    split_to_single_pages_inner(path, output_dir, None)?
        .context("PDF page splitting was unexpectedly cancelled")
}

pub fn split_to_single_pages_cancellable(
    path: &Path,
    output_dir: &Path,
    cancellation: &CancellationToken,
) -> Result<Option<Vec<PathBuf>>> {
    split_to_single_pages_inner(path, output_dir, Some(cancellation))
}

fn split_to_single_pages_inner(
    path: &Path,
    output_dir: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<Option<Vec<PathBuf>>> {
    let document =
        Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    std::fs::create_dir_all(output_dir)?;
    let mut results = Vec::with_capacity(pages.len());
    for page in &pages {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Ok(None);
        }
        let mut single = Document::load(path)?;
        let remove: Vec<u32> = pages
            .iter()
            .copied()
            .filter(|candidate| candidate != page)
            .collect();
        single.delete_pages(&remove);
        single.prune_objects();
        let output = output_dir.join(format!("page-{page:04}.pdf"));
        single
            .save(&output)
            .with_context(|| format!("writing PDF page to {}", output.display()))?;
        results.push(output);
    }
    Ok(Some(results))
}
