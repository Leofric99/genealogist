use crate::db::Database;
use anyhow::{bail, Context, Result};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

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

    pub fn page_group_transcript_path(
        &self,
        source: &Path,
        first_page: usize,
        last_page: usize,
    ) -> PathBuf {
        let transcript = self.transcript_path(source);
        let stem = source
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("document");
        transcript.with_file_name(format!("{stem}.pages-{first_page:04}-{last_page:04}.md"))
    }
}

pub fn relocate_existing_sidecars(
    records_root: &Path,
    old: &SidecarConfig,
    new: &SidecarConfig,
) -> Result<usize> {
    if old == new || !records_root.exists() {
        return Ok(0);
    }
    if !records_root.is_dir() {
        bail!(
            "records path is not a directory: {}",
            records_root.display()
        );
    }

    let mut moves = Vec::new();
    for entry in WalkDir::new(records_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with('.')
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() || !crate::scanner::is_supported(entry.path()) {
            continue;
        }

        let source = entry.path();
        let old_master = old.transcript_path(source);
        let new_master = new.transcript_path(source);
        if old_master.is_file() {
            moves.push((old_master, new_master));
        }

        let old_transcript_dir = old.transcript_path(source).parent().map(Path::to_path_buf);
        if let Some(directory) = old_transcript_dir.filter(|path| path.is_dir()) {
            let stem = source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("document");
            let page_prefix = format!("{stem}.page-");
            let page_group_prefix = format!("{stem}.pages-");
            for transcript in fs::read_dir(&directory)
                .with_context(|| format!("reading sidecar directory {}", directory.display()))?
            {
                let transcript = transcript?.path();
                let is_page_transcript = transcript.is_file()
                    && transcript.extension().and_then(|ext| ext.to_str()) == Some("md")
                    && transcript
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            (name.starts_with(&page_prefix) || name.starts_with(&page_group_prefix))
                                && name.ends_with(".md")
                        });
                if is_page_transcript {
                    let destination = new
                        .transcript_path(source)
                        .with_file_name(transcript.file_name().unwrap_or_default());
                    moves.push((transcript, destination));
                }
            }
        }
    }

    let mut destinations = HashSet::new();
    for (source, destination) in &moves {
        if source == destination {
            continue;
        }
        let normalized = destination.to_string_lossy().to_lowercase();
        if !destinations.insert(normalized) {
            bail!(
                "multiple transcript files would move to {}",
                destination.display()
            );
        }
        if destination.exists() && !moves.iter().any(|(other, _)| other == destination) {
            bail!(
                "cannot move transcript {} because {} already exists",
                source.display(),
                destination.display()
            );
        }
    }

    for (_, destination) in &moves {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating sidecar directory {}", parent.display()))?;
        }
    }

    let mut completed = Vec::new();
    for (source, destination) in moves {
        if source == destination {
            continue;
        }
        if let Err(error) = fs::rename(&source, &destination) {
            let mut rollback_errors = Vec::new();
            for (moved_from, moved_to) in completed.iter().rev() {
                if let Err(rollback_error) = fs::rename(moved_to, moved_from) {
                    rollback_errors.push(format!(
                        "{} -> {}: {rollback_error}",
                        moved_to.display(),
                        moved_from.display()
                    ));
                }
            }
            if rollback_errors.is_empty() {
                return Err(error).with_context(|| {
                    format!(
                        "moving transcript {} to {}",
                        source.display(),
                        destination.display()
                    )
                });
            }
            bail!(
                "moving transcript {} to {} failed: {error}; rollback also failed for: {}",
                source.display(),
                destination.display(),
                rollback_errors.join("; ")
            );
        }
        completed.push((source, destination));
    }
    Ok(completed.len())
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
    use super::{relocate_existing_sidecars, SidecarConfig, MODE_SAME_DIRECTORY, MODE_SUBFOLDER};
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
        assert_eq!(
            nested.page_group_transcript_path(&source, 1, 10),
            Path::new("records")
                .join("person")
                .join("OCR Transcripts")
                .join("record.pages-0001-0010.md")
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

    #[test]
    fn changing_sidecar_layout_moves_master_and_page_transcripts_both_ways() {
        let directory = tempfile::tempdir().unwrap();
        let source_dir = directory.path().join("records").join("person");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = source_dir.join("record.pdf");
        std::fs::write(&source, b"pdf source").unwrap();
        let old = SidecarConfig::new(MODE_SAME_DIRECTORY, "transcripts").unwrap();
        let new = SidecarConfig::new(MODE_SUBFOLDER, "OCR").unwrap();
        let master = old.transcript_path(&source);
        let page = old.page_transcript_path(&source, 2);
        std::fs::write(&master, "master transcript").unwrap();
        std::fs::write(&page, "page transcript").unwrap();

        assert_eq!(
            relocate_existing_sidecars(directory.path(), &old, &new).unwrap(),
            2
        );
        assert!(!master.exists());
        assert!(!page.exists());
        assert_eq!(
            std::fs::read_to_string(new.transcript_path(&source)).unwrap(),
            "master transcript"
        );
        assert_eq!(
            std::fs::read_to_string(new.page_transcript_path(&source, 2)).unwrap(),
            "page transcript"
        );

        assert_eq!(
            relocate_existing_sidecars(directory.path(), &new, &old).unwrap(),
            2
        );
        assert_eq!(
            std::fs::read_to_string(master).unwrap(),
            "master transcript"
        );
        assert_eq!(std::fs::read_to_string(page).unwrap(), "page transcript");
    }

    #[test]
    fn relocation_refuses_to_overwrite_an_existing_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let source_dir = directory.path().join("records");
        let subfolder = source_dir.join("OCR");
        std::fs::create_dir_all(&subfolder).unwrap();
        let source = source_dir.join("record.jpg");
        std::fs::write(&source, b"source image").unwrap();
        let old = SidecarConfig::new(MODE_SAME_DIRECTORY, "OCR").unwrap();
        let new = SidecarConfig::new(MODE_SUBFOLDER, "OCR").unwrap();
        std::fs::write(old.transcript_path(&source), "old").unwrap();
        std::fs::write(new.transcript_path(&source), "keep existing").unwrap();

        assert!(relocate_existing_sidecars(directory.path(), &old, &new).is_err());
        assert_eq!(
            std::fs::read_to_string(old.transcript_path(&source)).unwrap(),
            "old"
        );
        assert_eq!(
            std::fs::read_to_string(new.transcript_path(&source)).unwrap(),
            "keep existing"
        );
    }
}
