use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::{
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

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
    use super::Database;

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
