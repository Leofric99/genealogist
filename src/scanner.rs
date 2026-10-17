use anyhow::{Context, Result};
use rayon::prelude::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Clone, Debug, Serialize)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub sha256: String,
    pub extension: String,
}

pub fn is_supported(path: &Path) -> bool {
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
    if !root.is_dir() {
        anyhow::bail!(
            "records directory does not exist or is not a directory: {}",
            root.display()
        );
    }
    let paths = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| match entry {
            Ok(entry) if entry.file_type().is_file() && is_supported(entry.path()) => {
                Some(Ok(entry.into_path()))
            }
            Ok(_) => None,
            Err(error) => Some(Err(anyhow::anyhow!(error))),
        })
        .collect::<Result<Vec<_>>>()?;
    paths.par_iter().map(|path| hash_file(path)).collect()
}

pub fn hash_file(path: &Path) -> Result<ScannedFile> {
    if !path.is_file() || !is_supported(path) {
        anyhow::bail!("not a supported document file: {}", path.display());
    }
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(ScannedFile {
        path: path.to_path_buf(),
        sha256: hex::encode(digest.finalize()),
        extension: path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase(),
    })
}
