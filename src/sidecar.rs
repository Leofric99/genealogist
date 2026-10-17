use crate::db::Database;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub const MODE_SAME_DIRECTORY: &str = "same_directory";
pub const MODE_SUBFOLDER: &str = "subfolder";
const DEFAULT_SUBFOLDER: &str = "transcripts";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarConfig {
    mode: String,
    folder: String,
}

impl SidecarConfig {
    pub fn load(database: &Database) -> Result<Self> {
        let mode = database
            .setting("sidecar_mode")?
            .unwrap_or_else(|| MODE_SAME_DIRECTORY.to_owned());
        let folder = database
            .setting("sidecar_folder")?
            .unwrap_or_else(|| DEFAULT_SUBFOLDER.to_owned());
        Self::new(&mode, &folder)
    }

    pub fn new(mode: &str, folder: &str) -> Result<Self> {
        if mode != MODE_SAME_DIRECTORY && mode != MODE_SUBFOLDER {
            bail!("invalid sidecar storage mode");
        }
        let folder = validate_folder_name(folder)?;
        Ok(Self {
            mode: mode.to_owned(),
            folder,
        })
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn folder(&self) -> &str {
        &self.folder
    }

    pub fn transcript_path(&self, source: &Path) -> PathBuf {
        let transcript_name = source.with_extension("md");
        let filename = transcript_name
            .file_name()
            .map(std::ffi::OsStr::to_os_string);
        match self.mode.as_str() {
            MODE_SAME_DIRECTORY => transcript_name,
            MODE_SUBFOLDER => {
                if let Some(filename) = filename {
                    source
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(&self.folder)
                        .join(filename)
                } else {
                    transcript_name
                }
            }
            _ => unreachable!("sidecar mode is validated when loaded"),
        }
    }

    pub fn page_transcript_path(&self, source: &Path, page: usize) -> PathBuf {
        let transcript = self.transcript_path(source);
        let stem = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("document");
        transcript.with_file_name(format!("{stem}.page-{page:04}.md"))
    }
}

fn validate_folder_name(folder: &str) -> Result<String> {
    let folder = folder.trim();
    if folder.is_empty()
        || folder.chars().count() > 80
        || folder.starts_with('.')
        || folder == "."
        || folder == ".."
        || folder.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        bail!("subfolder name must be a simple folder name, 1 to 80 characters long");
    }
    Ok(folder.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{SidecarConfig, MODE_SAME_DIRECTORY, MODE_SUBFOLDER};
    use crate::db::Database;
    use std::path::Path;

    #[test]
    fn sidecars_resolve_in_same_directory_or_named_child_folder() {
        let source = Path::new("records").join("person").join("record.pdf");
        let same = SidecarConfig::new(MODE_SAME_DIRECTORY, "transcripts").unwrap();
        let nested = SidecarConfig::new(MODE_SUBFOLDER, "OCR Transcripts").unwrap();

        assert_eq!(
            same.transcript_path(&source),
            Path::new("records").join("person").join("record.md")
        );
        assert_eq!(
            nested.transcript_path(&source),
            Path::new("records")
                .join("person")
                .join("OCR Transcripts")
                .join("record.md")
        );
        assert_eq!(
            nested.page_transcript_path(&source, 3),
            Path::new("records")
                .join("person")
                .join("OCR Transcripts")
                .join("record.page-0003.md")
        );
    }

    #[test]
    fn sidecar_folder_name_cannot_escape_the_document_directory() {
        for invalid in ["", ".", "..", "../outside", "nested/folder", ".hidden"] {
            assert!(SidecarConfig::new(MODE_SUBFOLDER, invalid).is_err());
        }
    }

    #[test]
    fn sidecar_settings_are_loaded_from_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::open(&directory.path().join("genealogy.sqlite3")).unwrap();
        database
            .set_sidecar_settings(MODE_SUBFOLDER, "Transcriptions")
            .unwrap();

        let loaded = SidecarConfig::load(&database).unwrap();
        assert_eq!(loaded.mode(), MODE_SUBFOLDER);
        assert_eq!(loaded.folder(), "Transcriptions");
    }
}
