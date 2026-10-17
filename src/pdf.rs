use anyhow::{Context, Result};
use lopdf::Document;
use std::path::{Path, PathBuf};

pub fn page_count(path: &Path) -> Result<usize> {
    let document =
        Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    Ok(document.get_pages().len())
}

pub fn split_to_single_pages(path: &Path, output_dir: &Path) -> Result<Vec<PathBuf>> {
    let document =
        Document::load(path).with_context(|| format!("reading PDF {}", path.display()))?;
    let pages: Vec<u32> = document.get_pages().keys().copied().collect();
    std::fs::create_dir_all(output_dir)?;
    let mut results = Vec::with_capacity(pages.len());
    for page in &pages {
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
    Ok(results)
}
