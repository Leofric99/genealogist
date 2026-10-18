use crate::config::{
    DEFAULT_SYSTEM_PROMPT, LAST_DEFAULT_SYSTEM_PROMPT, LEGACY_DEFAULT_SYSTEM_PROMPT,
    PREVIOUS_DEFAULT_SYSTEM_PROMPT, PRIOR_DEFAULT_SYSTEM_PROMPT,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const PRE_METADATA_PROMPT_SHA256: &str =
    "798f051cfd905879abf27cd6df18c2baf817f25e6ef21c4616afe3b155132674";
const PRE_DITTO_PROMPT_SHA256: &str =
    "729d982d4e4490322125dcdddc588c36f3d1d5ebd8025c36d66608dbc689993e";
const PRE_TRANSCRIPT_ONLY_PROMPT_SHA256: &str =
    "486ec5bab3a73546704cb88fe3f0072bd00d2109b89085ad2af5171f239a898a";

pub struct Database {
    connection: Mutex<Connection>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JobStatus {
    pub running: bool,
    pub total: usize,
    pub processed: usize,
    pub skipped: usize,
    pub active: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FileStatus {
    pub path: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TranscriptPersonLink {
    pub person_key: String,
    pub gedcom_xref: String,
    pub person_name: String,
    pub transcript_path: String,
    pub transcript_sha256: String,
    pub status: String,
    pub confidence: f64,
    pub evidence_json: String,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating database directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening database {}", path.display()))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY NOT NULL,
                 value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS files (
                 path TEXT PRIMARY KEY NOT NULL,
                 sha256 TEXT NOT NULL,
                 status TEXT NOT NULL,
                 transcript_path TEXT,
                 error TEXT,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS transcript_person_links (
                 person_key TEXT NOT NULL,
                 gedcom_xref TEXT NOT NULL,
                 person_name TEXT NOT NULL,
                 transcript_path TEXT NOT NULL,
                 transcript_sha256 TEXT NOT NULL,
                 status TEXT NOT NULL CHECK (status IN ('suggested', 'confirmed', 'rejected')),
                 confidence REAL NOT NULL CHECK (confidence >= 0 AND confidence <= 1),
                 evidence_json TEXT NOT NULL,
                 updated_at INTEGER NOT NULL,
                 PRIMARY KEY (person_key, transcript_path)
             );
             CREATE TABLE IF NOT EXISTS job (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 running INTEGER NOT NULL DEFAULT 0,
                 total INTEGER NOT NULL DEFAULT 0,
                 processed INTEGER NOT NULL DEFAULT 0,
                 skipped INTEGER NOT NULL DEFAULT 0,
                 active TEXT,
                 message TEXT
             );
             INSERT OR IGNORE INTO job (id) VALUES (1);",
        )?;
        connection.execute(
            "UPDATE settings SET value = ?1 WHERE key = 'system_prompt' AND value IN (?2, ?3, ?4, ?5)",
            params![
                DEFAULT_SYSTEM_PROMPT,
                LEGACY_DEFAULT_SYSTEM_PROMPT,
                PREVIOUS_DEFAULT_SYSTEM_PROMPT,
                PRIOR_DEFAULT_SYSTEM_PROMPT,
                LAST_DEFAULT_SYSTEM_PROMPT
            ],
        )?;
        let stored_prompt: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key = 'system_prompt'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        // Fingerprints of prior built-in prompts; custom text is not migrated.
        if stored_prompt.as_deref().is_some_and(|prompt| {
            let fingerprint = hex::encode(Sha256::digest(prompt.as_bytes()));
            [
                PRE_METADATA_PROMPT_SHA256,
                PRE_DITTO_PROMPT_SHA256,
                PRE_TRANSCRIPT_ONLY_PROMPT_SHA256,
            ]
            .contains(&fingerprint.as_str())
        }) {
            connection.execute(
                "UPDATE settings SET value = ?1 WHERE key = 'system_prompt'",
                [DEFAULT_SYSTEM_PROMPT],
            )?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection()?.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn suggest_transcript_person_link(
        &self,
        person_key: &str,
        gedcom_xref: &str,
        person_name: &str,
        transcript_path: &str,
        transcript_sha256: &str,
        confidence: f64,
        evidence_json: &str,
    ) -> Result<()> {
        if !(0.0..=1.0).contains(&confidence) {
            anyhow::bail!("transcript-person link confidence must be between 0 and 1");
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        self.connection()?.execute(
            "INSERT INTO transcript_person_links
                (person_key, gedcom_xref, person_name, transcript_path, transcript_sha256,
                 status, confidence, evidence_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'suggested', ?6, ?7, ?8)
             ON CONFLICT(person_key, transcript_path) DO UPDATE SET
                gedcom_xref=excluded.gedcom_xref,
                person_name=excluded.person_name,
                status=CASE
                    WHEN transcript_person_links.transcript_sha256=excluded.transcript_sha256
                    THEN transcript_person_links.status
                    ELSE 'suggested'
                END,
                transcript_sha256=excluded.transcript_sha256,
                confidence=excluded.confidence,
                evidence_json=excluded.evidence_json,
                updated_at=excluded.updated_at",
            params![
                person_key,
                gedcom_xref,
                person_name,
                transcript_path,
                transcript_sha256,
                confidence,
                evidence_json,
                now
            ],
        )?;
        Ok(())
    }

    pub fn transcript_person_links(&self, person_key: &str) -> Result<Vec<TranscriptPersonLink>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT person_key, gedcom_xref, person_name, transcript_path, transcript_sha256,
                    status, confidence, evidence_json
             FROM transcript_person_links
             WHERE person_key=?1
             ORDER BY status='confirmed' DESC, confidence DESC, transcript_path",
        )?;
        let rows = statement.query_map([person_key], |row| {
            Ok(TranscriptPersonLink {
                person_key: row.get(0)?,
                gedcom_xref: row.get(1)?,
                person_name: row.get(2)?,
                transcript_path: row.get(3)?,
                transcript_sha256: row.get(4)?,
                status: row.get(5)?,
                confidence: row.get(6)?,
                evidence_json: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_transcript_person_link_status(
        &self,
        person_key: &str,
        transcript_path: &str,
        transcript_sha256: &str,
        status: &str,
    ) -> Result<bool> {
        if !matches!(status, "confirmed" | "rejected") {
            anyhow::bail!("invalid transcript-person link status");
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let updated = self.connection()?.execute(
            "UPDATE transcript_person_links SET status=?1, updated_at=?2
             WHERE person_key=?3 AND transcript_path=?4 AND transcript_sha256=?5",
            params![status, now, person_key, transcript_path, transcript_sha256],
        )?;
        Ok(updated == 1)
    }

    pub fn set_sidecar_settings(&self, mode: &str, folder: &str) -> Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        for (key, value) in [("sidecar_mode", mode), ("sidecar_folder", folder)] {
            transaction.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn file_hash(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row("SELECT sha256 FROM files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn file_status(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row("SELECT status FROM files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn file_transcript_path(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT transcript_path FROM files WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn file_error(&self, path: &str) -> Result<Option<String>> {
        Ok(self
            .connection()?
            .query_row("SELECT error FROM files WHERE path = ?1", [path], |row| {
                row.get(0)
            })
            .optional()?
            .flatten())
    }

    pub fn save_file(
        &self,
        path: &str,
        hash: &str,
        status: &str,
        transcript: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        self.connection()?.execute(
            "INSERT INTO files (path, sha256, status, transcript_path, error, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET sha256=excluded.sha256, status=excluded.status,
                 transcript_path=excluded.transcript_path, error=excluded.error, updated_at=excluded.updated_at",
            params![path, hash, status, transcript, error, now],
        )?;
        Ok(())
    }

    pub fn begin_job(&self) -> Result<bool> {
        let conn = self.connection()?;
        let running: bool =
            conn.query_row("SELECT running FROM job WHERE id=1", [], |row| row.get(0))?;
        if running {
            return Ok(false);
        }
        conn.execute(
            "UPDATE job SET running=1, total=0, processed=0, skipped=0, active=NULL, message=NULL WHERE id=1",
            [],
        )?;
        Ok(true)
    }

    pub fn update_job(
        &self,
        total: Option<usize>,
        processed_delta: usize,
        skipped_delta: usize,
        active: Option<&str>,
        message: Option<&str>,
    ) -> Result<()> {
        self.connection()?.execute(
            "UPDATE job SET total=COALESCE(?1,total), processed=processed+?2, skipped=skipped+?3,
                 active=?4, message=?5 WHERE id=1",
            params![
                total.map(|n| n as i64),
                processed_delta as i64,
                skipped_delta as i64,
                active,
                message
            ],
        )?;
        Ok(())
    }

    pub fn finish_job(&self, message: Option<&str>) -> Result<()> {
        self.connection()?.execute(
            "UPDATE job SET running=0, active=NULL, message=?1 WHERE id=1",
            [message],
        )?;
        Ok(())
    }

    pub fn reset_job_progress(&self) -> Result<()> {
        self.connection()?.execute(
            "UPDATE job SET total=0, processed=0, skipped=0, active=NULL, message=NULL WHERE id=1",
            [],
        )?;
        Ok(())
    }

    pub fn job_status(&self) -> Result<JobStatus> {
        Ok(self.connection()?.query_row(
            "SELECT running,total,processed,skipped,active,message FROM job WHERE id=1",
            [],
            |row| {
                Ok(JobStatus {
                    running: row.get::<_, bool>(0)?,
                    total: row.get::<_, i64>(1)? as usize,
                    processed: row.get::<_, i64>(2)? as usize,
                    skipped: row.get::<_, i64>(3)? as usize,
                    active: row.get(4)?,
                    message: row.get(5)?,
                })
            },
        )?)
    }

    pub fn file_statuses(&self) -> Result<Vec<FileStatus>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT path,status,error FROM files ORDER BY path")?;
        let rows = stmt.query_map([], |row| {
            Ok(FileStatus {
                path: row.get(0)?,
                status: row.get(1)?,
                error: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_file_index(&self) -> Result<bool> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let running: bool =
            transaction.query_row("SELECT running FROM job WHERE id=1", [], |row| row.get(0))?;
        if running {
            return Ok(false);
        }
        transaction.execute("DELETE FROM files", [])?;
        transaction.execute(
            "UPDATE job SET total=0, processed=0, skipped=0, active=NULL,
             message='Indexed files cleared; transcripts were preserved' WHERE id=1",
            [],
        )?;
        transaction.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{
        DEFAULT_SYSTEM_PROMPT, LAST_DEFAULT_SYSTEM_PROMPT, LEGACY_DEFAULT_SYSTEM_PROMPT,
        PREVIOUS_DEFAULT_SYSTEM_PROMPT, PRIOR_DEFAULT_SYSTEM_PROMPT,
    };

    use super::Database;

    #[test]
    fn opening_database_updates_only_the_previous_default_prompt() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("genealogy.sqlite3");
        {
            let database = Database::open(&path).expect("open database");
            database
                .set_setting("system_prompt", LEGACY_DEFAULT_SYSTEM_PROMPT)
                .expect("save previous default prompt");
        }

        let database = Database::open(&path).expect("reopen database");
        assert_eq!(
            database.setting("system_prompt").expect("read prompt"),
            Some(DEFAULT_SYSTEM_PROMPT.to_owned())
        );

        database
            .set_setting("system_prompt", "custom prompt")
            .expect("save custom prompt");
        drop(database);
        let database = Database::open(&path).expect("reopen with custom prompt");
        assert_eq!(
            database
                .setting("system_prompt")
                .expect("read custom prompt"),
            Some("custom prompt".to_owned())
        );
    }

    #[test]
    fn opening_database_migrates_previous_table_prompt_but_preserves_custom_prompt() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("genealogy.sqlite3");
        let database = Database::open(&path).expect("open database");
        database
            .set_setting("system_prompt", PREVIOUS_DEFAULT_SYSTEM_PROMPT)
            .expect("save previous table prompt");
        drop(database);

        let database = Database::open(&path).expect("reopen database");
        assert_eq!(
            database.setting("system_prompt").expect("read prompt"),
            Some(DEFAULT_SYSTEM_PROMPT.to_owned())
        );
        database
            .set_setting("system_prompt", "custom prompt")
            .expect("save custom prompt");
        drop(database);
        let database = Database::open(&path).expect("reopen with custom prompt");
        assert_eq!(
            database
                .setting("system_prompt")
                .expect("read custom prompt"),
            Some("custom prompt".to_owned())
        );
    }

    #[test]
    fn opening_database_migrates_previous_census_prompt_but_preserves_custom_prompt() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("genealogy.sqlite3");
        let database = Database::open(&path).expect("open database");
        database
            .set_setting("system_prompt", PRIOR_DEFAULT_SYSTEM_PROMPT)
            .expect("save previous prompt");
        drop(database);

        let database = Database::open(&path).expect("reopen database");
        assert_eq!(
            database.setting("system_prompt").expect("read prompt"),
            Some(DEFAULT_SYSTEM_PROMPT.to_owned())
        );
    }

    #[test]
    fn opening_database_migrates_last_default_prompt() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let path = directory.path().join("genealogy.sqlite3");
        let database = Database::open(&path).expect("open database");
        database
            .set_setting("system_prompt", LAST_DEFAULT_SYSTEM_PROMPT)
            .expect("save last prompt");
        drop(database);

        let database = Database::open(&path).expect("reopen database");
        assert_eq!(
            database.setting("system_prompt").expect("read prompt"),
            Some(DEFAULT_SYSTEM_PROMPT.to_owned())
        );
    }

    #[test]
    fn transcript_person_links_keep_review_state_until_transcript_changes() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database =
            Database::open(&directory.path().join("genealogy.sqlite3")).expect("open database");
        database
            .suggest_transcript_person_link(
                "person-key",
                "@I1@",
                "Ada Example",
                "records/birth.md",
                &"a".repeat(64),
                0.8,
                r#"["exact name appears in transcript text"]"#,
            )
            .expect("save source suggestion");
        assert_eq!(
            database.transcript_person_links("person-key").unwrap()[0].status,
            "suggested"
        );
        assert!(database
            .set_transcript_person_link_status(
                "person-key",
                "records/birth.md",
                &"a".repeat(64),
                "confirmed"
            )
            .unwrap());
        database
            .suggest_transcript_person_link(
                "person-key",
                "@I1@",
                "Ada Example",
                "records/birth.md",
                &"a".repeat(64),
                0.9,
                r#"["updated evidence"]"#,
            )
            .expect("refresh source suggestion");
        assert_eq!(
            database.transcript_person_links("person-key").unwrap()[0].status,
            "confirmed"
        );
        database
            .suggest_transcript_person_link(
                "person-key",
                "@I1@",
                "Ada Example",
                "records/birth.md",
                &"b".repeat(64),
                0.75,
                r#"["transcript changed"]"#,
            )
            .expect("refresh changed transcript suggestion");
        let links = database.transcript_person_links("person-key").unwrap();
        let link = &links[0];
        assert_eq!(link.status, "suggested");
        assert_eq!(link.transcript_sha256, "b".repeat(64));
    }

    #[test]
    fn clear_file_index_preserves_settings_and_transcripts_and_rejects_running_jobs() {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database =
            Database::open(&directory.path().join("genealogy.sqlite3")).expect("open database");
        let transcript = directory.path().join("record.md");
        std::fs::write(&transcript, "existing transcript").expect("write transcript");
        database
            .set_setting("system_prompt", "custom prompt")
            .expect("save setting");
        database
            .save_file(
                "record.png",
                "abc123",
                "transcribed",
                transcript.to_str(),
                None,
            )
            .expect("save indexed file");

        assert!(database.begin_job().expect("begin job"));
        assert!(!database.clear_file_index().expect("reject active job"));
        assert_eq!(database.file_statuses().expect("read files").len(), 1);

        database.finish_job(Some("complete")).expect("finish job");
        assert!(database.clear_file_index().expect("clear index"));
        assert!(database
            .file_statuses()
            .expect("read cleared files")
            .is_empty());
        assert_eq!(
            database.setting("system_prompt").expect("read setting"),
            Some("custom prompt".to_owned())
        );
        assert_eq!(
            std::fs::read_to_string(transcript).expect("read transcript"),
            "existing transcript"
        );
        let status = database.job_status().expect("read job status");
        assert_eq!(status.total, 0);
        assert_eq!(status.processed, 0);
        assert_eq!(status.skipped, 0);
    }
}
