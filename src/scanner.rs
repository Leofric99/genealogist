use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

#[derive(Clone, Debug, Serialize)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub sha256: String,
    pub extension: String,
}

pub fn is_supported(path: &Path) -> bool {
    if path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
    {
        return false;
    }
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "tiff" | "tif" | "webp" | "pdf"
            )
        })
        .unwrap_or(false)
}

pub fn scan_and_hash(root: &Path) -> Result<Vec<ScannedFile>> {
    scan_and_hash_inner(root, None)?.context("directory scan was unexpectedly cancelled")
}

pub fn scan_and_hash_cancellable(
    root: &Path,
    cancellation: &CancellationToken,
) -> Result<Option<Vec<ScannedFile>>> {
    scan_and_hash_inner(root, Some(cancellation))
}

fn scan_and_hash_inner(
    root: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<Option<Vec<ScannedFile>>> {
    if !root.is_dir() {
        anyhow::bail!(
            "records directory does not exist or is not a directory: {}",
            root.display()
        );
    }
    let mut paths = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
    {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Ok(None);
        }
        let entry = entry?;
        if entry.file_type().is_file() && is_supported(entry.path()) {
            paths.push(entry.into_path());
        }
    }
    let files = paths
        .par_iter()
        .map(|path| hash_file_inner(path, cancellation))
        .collect::<Result<Vec<_>>>()?;
    if cancellation.is_some_and(CancellationToken::is_cancelled) {
        return Ok(None);
    }
    Ok(Some(files.into_iter().flatten().collect()))
}

pub fn hash_file(path: &Path) -> Result<ScannedFile> {
    hash_file_inner(path, None)?.context("file hashing was unexpectedly cancelled")
}

fn hash_file_inner(
    path: &Path,
    cancellation: Option<&CancellationToken>,
) -> Result<Option<ScannedFile>> {
    if !path.is_file() || !is_supported(path) {
        anyhow::bail!("not a supported document file: {}", path.display());
    }
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return Ok(None);
        }
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(Some(ScannedFile {
        path: path.to_path_buf(),
        sha256: hex::encode(digest.finalize()),
        extension: path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{is_supported, scan_and_hash, scan_and_hash_cancellable};
    use std::fs;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn cancelled_directory_scan_returns_no_partial_results() {
        let directory = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        assert!(scan_and_hash_cancellable(directory.path(), &cancellation)
            .unwrap()
            .is_none());
    }

    #[test]
    fn scan_skips_dotfiles_and_hidden_directories() {
        let directory = tempfile::tempdir().unwrap();
        let hidden_directory = directory.path().join(".metadata");
        fs::create_dir(&hidden_directory).unwrap();
        fs::write(directory.path().join("visible.png"), b"visible").unwrap();
        fs::write(directory.path().join("._metadata.png"), b"sidecar").unwrap();
        fs::write(hidden_directory.join("inside.png"), b"hidden").unwrap();

        let files = scan_and_hash(directory.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].path.file_name().and_then(|name| name.to_str()),
            Some("visible.png")
        );
        assert!(!is_supported(Path::new(".hidden.pdf")));
    }
}
