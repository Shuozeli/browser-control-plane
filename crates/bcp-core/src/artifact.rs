use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bcp_proto::browsercontrol::v1::{Artifact, ArtifactStatus, LeaseContext};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::id::IdGenerator;
use crate::time::Clock;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("ttl_seconds must be greater than zero")]
    MissingTtl,
    #[error("ttl_seconds exceeds maximum of {0} seconds")]
    TtlTooLong(i64),
    #[error("original_filename is required")]
    MissingFilename,
    #[error("lease is required")]
    MissingLease,
    #[error("lease_id is required")]
    MissingLeaseId,
    #[error("profile_id is required")]
    MissingProfileId,
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
pub struct ArtifactStoreConfig {
    pub machine_id: String,
    pub root_dir: PathBuf,
    pub max_ttl_seconds: i64,
}

#[derive(Debug, Clone)]
pub struct ArtifactWriteTicket {
    pub artifact_id: String,
    pub temp_path: PathBuf,
    pub final_path: PathBuf,
}

pub struct ArtifactStore {
    config: ArtifactStoreConfig,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGenerator>,
    conn: Mutex<Connection>,
}

impl ArtifactStore {
    pub fn open(
        config: ArtifactStoreConfig,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn IdGenerator>,
    ) -> Result<Self, ArtifactError> {
        std::fs::create_dir_all(config.root_dir.join("files"))?;
        std::fs::create_dir_all(config.root_dir.join("tmp"))?;
        let conn = Connection::open(config.root_dir.join("artifacts.sqlite"))?;
        let store = Self {
            config,
            clock,
            ids,
            conn: Mutex::new(conn),
        };
        store.init_schema()?;
        Ok(store)
    }

    pub fn reserve(
        &self,
        lease: &LeaseContext,
        original_filename: &str,
        content_type: &str,
        purpose: &str,
        ttl_seconds: i64,
    ) -> Result<ArtifactWriteTicket, ArtifactError> {
        if ttl_seconds <= 0 {
            return Err(ArtifactError::MissingTtl);
        }
        if ttl_seconds > self.config.max_ttl_seconds {
            return Err(ArtifactError::TtlTooLong(self.config.max_ttl_seconds));
        }
        if lease.lease_id.is_empty() {
            return Err(ArtifactError::MissingLeaseId);
        }
        if lease.profile_id.is_empty() {
            return Err(ArtifactError::MissingProfileId);
        }
        let clean_name = sanitize_filename(original_filename)?;
        let artifact_id = self.ids.next_id("artifact");
        let stored_filename = format!("{artifact_id}-{clean_name}");
        let uploaded_at = self.clock.now_unix_ms();
        let expires_at = uploaded_at + ttl_seconds * 1_000;

        let mut conn = self.conn.lock().expect("artifact sqlite lock poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO artifacts (
                artifact_id, machine_id, lease_id, profile_id, original_filename,
                stored_filename, content_type, purpose, size_bytes, uploaded_at_unix_ms,
                expires_at_unix_ms, status
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, ?11)",
            params![
                artifact_id,
                self.config.machine_id,
                lease.lease_id,
                lease.profile_id,
                clean_name,
                stored_filename,
                content_type,
                purpose,
                uploaded_at,
                expires_at,
                status_name(ArtifactStatus::Uploading),
            ],
        )?;
        tx.commit()?;

        Ok(ArtifactWriteTicket {
            artifact_id,
            temp_path: self
                .config
                .root_dir
                .join("tmp")
                .join(stored_filename.clone()),
            final_path: self.config.root_dir.join("files").join(stored_filename),
        })
    }

    pub fn commit(
        &self,
        ticket: &ArtifactWriteTicket,
        size_bytes: i64,
    ) -> Result<Artifact, ArtifactError> {
        if let Some(parent) = ticket.final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&ticket.temp_path, &ticket.final_path)?;
        self.update_status_and_size(
            &ticket.artifact_id,
            ArtifactStatus::Available,
            Some(size_bytes),
        )?;
        self.get(&ticket.artifact_id)?
            .ok_or_else(|| ArtifactError::NotFound(ticket.artifact_id.clone()))
    }

    pub fn fail_upload(&self, ticket: &ArtifactWriteTicket) -> Result<(), ArtifactError> {
        let _ = std::fs::remove_file(&ticket.temp_path);
        self.update_status_and_size(&ticket.artifact_id, ArtifactStatus::Failed, None)
    }

    pub fn list(
        &self,
        profile_id: Option<&str>,
        lease_id: Option<&str>,
        include_expired: bool,
    ) -> Result<Vec<Artifact>, ArtifactError> {
        self.mark_expired_rows()?;
        let now = self.clock.now_unix_ms();
        let mut conn = self.conn.lock().expect("artifact sqlite lock poisoned");
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "SELECT artifact_id, machine_id, lease_id, profile_id, original_filename,
                    stored_filename, content_type, purpose, size_bytes,
                    uploaded_at_unix_ms, expires_at_unix_ms, status
             FROM artifacts
             ORDER BY uploaded_at_unix_ms DESC, artifact_id ASC",
        )?;
        let rows = stmt.query_map([], artifact_from_row)?;
        let mut artifacts = Vec::new();
        for row in rows {
            let artifact = row?;
            if let Some(profile_id) = profile_id
                && artifact.profile_id != profile_id
            {
                continue;
            }
            if let Some(lease_id) = lease_id
                && artifact.lease_id != lease_id
            {
                continue;
            }
            if !include_expired
                && (artifact.status == ArtifactStatus::Expired as i32
                    || artifact.status == ArtifactStatus::Deleted as i32
                    || artifact.expires_at_unix_ms <= now)
            {
                continue;
            }
            artifacts.push(artifact);
        }
        drop(stmt);
        tx.commit()?;
        Ok(artifacts)
    }

    pub fn delete(&self, artifact_id: &str) -> Result<bool, ArtifactError> {
        let artifact = match self.get(artifact_id)? {
            Some(artifact) => artifact,
            None => return Ok(false),
        };
        let path = self
            .config
            .root_dir
            .join("files")
            .join(&artifact.stored_filename);
        let _ = std::fs::remove_file(path);
        self.update_status_and_size(artifact_id, ArtifactStatus::Deleted, None)?;
        Ok(true)
    }

    pub fn cleanup_expired(&self) -> Result<Vec<Artifact>, ArtifactError> {
        self.mark_expired_rows()?;
        let expired = self.list(None, None, true)?;
        let mut deleted = Vec::new();
        for artifact in expired
            .into_iter()
            .filter(|artifact| artifact.status == ArtifactStatus::Expired as i32)
        {
            let path = self
                .config
                .root_dir
                .join("files")
                .join(&artifact.stored_filename);
            let _ = std::fs::remove_file(path);
            self.update_status_and_size(&artifact.artifact_id, ArtifactStatus::Deleted, None)?;
            let mut marked = artifact;
            marked.status = ArtifactStatus::Deleted as i32;
            deleted.push(marked);
        }
        Ok(deleted)
    }

    fn init_schema(&self) -> Result<(), ArtifactError> {
        let mut conn = self.conn.lock().expect("artifact sqlite lock poisoned");
        let tx = conn.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS artifacts (
                artifact_id TEXT PRIMARY KEY,
                machine_id TEXT NOT NULL,
                lease_id TEXT NOT NULL,
                profile_id TEXT NOT NULL,
                original_filename TEXT NOT NULL,
                stored_filename TEXT NOT NULL,
                content_type TEXT NOT NULL,
                purpose TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                uploaded_at_unix_ms INTEGER NOT NULL,
                expires_at_unix_ms INTEGER NOT NULL,
                status TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_artifacts_machine_profile
                ON artifacts(machine_id, profile_id);
            CREATE INDEX IF NOT EXISTS idx_artifacts_expires
                ON artifacts(expires_at_unix_ms, status);",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Resolve an artifact's metadata and on-disk path for streaming it back to
    /// a client (the read side of the out-of-band file bypass — e.g. a file the
    /// browser downloaded to this machine). Returns `NotFound` if unknown.
    pub fn open_for_read(&self, artifact_id: &str) -> Result<(Artifact, PathBuf), ArtifactError> {
        let artifact = self
            .get(artifact_id)?
            .ok_or_else(|| ArtifactError::NotFound(artifact_id.to_string()))?;
        let path = self
            .config
            .root_dir
            .join("files")
            .join(&artifact.stored_filename);
        Ok((artifact, path))
    }

    fn get(&self, artifact_id: &str) -> Result<Option<Artifact>, ArtifactError> {
        let mut conn = self.conn.lock().expect("artifact sqlite lock poisoned");
        let tx = conn.transaction()?;
        let artifact = {
            let mut stmt = tx.prepare(
                "SELECT artifact_id, machine_id, lease_id, profile_id, original_filename,
                        stored_filename, content_type, purpose, size_bytes,
                        uploaded_at_unix_ms, expires_at_unix_ms, status
                 FROM artifacts
                 WHERE artifact_id = ?1",
            )?;
            stmt.query_row(params![artifact_id], artifact_from_row)
                .optional()?
        };
        tx.commit()?;
        Ok(artifact)
    }

    fn update_status_and_size(
        &self,
        artifact_id: &str,
        status: ArtifactStatus,
        size_bytes: Option<i64>,
    ) -> Result<(), ArtifactError> {
        let mut conn = self.conn.lock().expect("artifact sqlite lock poisoned");
        let tx = conn.transaction()?;
        match size_bytes {
            Some(size_bytes) => {
                tx.execute(
                    "UPDATE artifacts SET status = ?1, size_bytes = ?2 WHERE artifact_id = ?3",
                    params![status_name(status), size_bytes, artifact_id],
                )?;
            }
            None => {
                tx.execute(
                    "UPDATE artifacts SET status = ?1 WHERE artifact_id = ?2",
                    params![status_name(status), artifact_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn mark_expired_rows(&self) -> Result<(), ArtifactError> {
        let now = self.clock.now_unix_ms();
        let mut conn = self.conn.lock().expect("artifact sqlite lock poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE artifacts
             SET status = ?1
             WHERE expires_at_unix_ms <= ?2 AND status IN (?3, ?4)",
            params![
                status_name(ArtifactStatus::Expired),
                now,
                status_name(ArtifactStatus::Available),
                status_name(ArtifactStatus::Uploading),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
}

pub fn sanitize_filename(input: &str) -> Result<String, ArtifactError> {
    let base = Path::new(input)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(ArtifactError::MissingFilename)?;
    let sanitized: String = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('.');
    if trimmed.is_empty() {
        return Err(ArtifactError::MissingFilename);
    }
    Ok(trimmed.to_string())
}

fn artifact_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Artifact> {
    let status: String = row.get(11)?;
    Ok(Artifact {
        artifact_id: row.get(0)?,
        machine_id: row.get(1)?,
        lease_id: row.get(2)?,
        profile_id: row.get(3)?,
        original_filename: row.get(4)?,
        stored_filename: row.get(5)?,
        content_type: row.get(6)?,
        purpose: row.get(7)?,
        size_bytes: row.get(8)?,
        uploaded_at_unix_ms: row.get(9)?,
        expires_at_unix_ms: row.get(10)?,
        status: status_value(&status) as i32,
    })
}

fn status_name(status: ArtifactStatus) -> &'static str {
    match status {
        ArtifactStatus::Uploading => "uploading",
        ArtifactStatus::Available => "available",
        ArtifactStatus::Expired => "expired",
        ArtifactStatus::Deleted => "deleted",
        ArtifactStatus::Failed => "failed",
        ArtifactStatus::Unspecified => "unspecified",
    }
}

fn status_value(status: &str) -> ArtifactStatus {
    match status {
        "uploading" => ArtifactStatus::Uploading,
        "available" => ArtifactStatus::Available,
        "expired" => ArtifactStatus::Expired,
        "deleted" => ArtifactStatus::Deleted,
        "failed" => ArtifactStatus::Failed,
        _ => ArtifactStatus::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;

    use crate::id::FakeIdGenerator;
    use crate::time::FakeClock;

    use super::*;

    fn lease() -> LeaseContext {
        LeaseContext {
            lease_id: "lease-1".to_string(),
            profile_id: "profile-1".to_string(),
            fencing_token: "fence-1".to_string(),
            expires_at_unix_ms: 0,
        }
    }

    fn store(clock: Arc<FakeClock>) -> (tempfile::TempDir, ArtifactStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(
            ArtifactStoreConfig {
                machine_id: "machine-a".to_string(),
                root_dir: dir.path().to_path_buf(),
                max_ttl_seconds: 60,
            },
            clock,
            Arc::new(FakeIdGenerator::new(["artifact-1", "artifact-2"])),
        )
        .unwrap();
        (dir, store)
    }

    #[test]
    fn reserve_rejects_missing_ttl() {
        // Arrange
        let (_dir, store) = store(Arc::new(FakeClock::new(1_000)));

        // Act
        let result = store.reserve(&lease(), "video.mp4", "video/mp4", "upload", 0);

        // Assert
        assert!(matches!(result, Err(ArtifactError::MissingTtl)));
    }

    #[test]
    fn reserve_sanitizes_filename_and_records_metadata() {
        // Arrange
        let (_dir, store) = store(Arc::new(FakeClock::new(1_000)));

        // Act
        let ticket = store
            .reserve(
                &lease(),
                "../my video.mp4",
                "video/mp4",
                "youtube-upload",
                10,
            )
            .unwrap();

        // Assert
        let artifact = store.get(&ticket.artifact_id).unwrap().unwrap();
        assert_eq!(artifact.original_filename, "my_video.mp4");
        assert_eq!(artifact.stored_filename, "artifact-1-my_video.mp4");
        assert_eq!(artifact.status, ArtifactStatus::Uploading as i32);
        assert_eq!(artifact.expires_at_unix_ms, 11_000);
    }

    #[test]
    fn commit_moves_file_and_marks_available() {
        // Arrange
        let (_dir, store) = store(Arc::new(FakeClock::new(1_000)));
        let ticket = store
            .reserve(&lease(), "video.mp4", "video/mp4", "youtube-upload", 10)
            .unwrap();
        let mut file = std::fs::File::create(&ticket.temp_path).unwrap();
        file.write_all(b"abc").unwrap();

        // Act
        let artifact = store.commit(&ticket, 3).unwrap();

        // Assert
        assert_eq!(artifact.status, ArtifactStatus::Available as i32);
        assert_eq!(artifact.size_bytes, 3);
        assert!(ticket.final_path.exists());
        assert!(!ticket.temp_path.exists());
    }

    #[test]
    fn cleanup_expired_removes_files_and_marks_deleted() {
        // Arrange
        let clock = Arc::new(FakeClock::new(1_000));
        let (_dir, store) = store(clock.clone());
        let ticket = store
            .reserve(&lease(), "video.mp4", "video/mp4", "youtube-upload", 1)
            .unwrap();
        std::fs::write(&ticket.temp_path, b"abc").unwrap();
        store.commit(&ticket, 3).unwrap();
        clock.advance_ms(1_001);

        // Act
        let deleted = store.cleanup_expired().unwrap();

        // Assert
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].status, ArtifactStatus::Deleted as i32);
        assert!(!ticket.final_path.exists());
        let listed = store.list(None, None, true).unwrap();
        assert_eq!(listed[0].status, ArtifactStatus::Deleted as i32);
    }
}
