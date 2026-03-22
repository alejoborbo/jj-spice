/// Domain-specific store for bookmark → change request mappings.
pub mod change_request;

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use jj_lib::file_util::persist_temp_file;
use jj_lib::lock::FileLock;
use prost::Message;
use tempfile::NamedTempFile;

/// Errors from protobuf-backed store I/O.
#[derive(Debug, thiserror::Error)]
pub enum SpiceStoreError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to decode protobuf: {0}")]
    Decode(#[from] prost::DecodeError),

    #[error("failed to acquire lock: {0}")]
    Lock(#[from] jj_lib::lock::FileLockError),
}

/// Generic protobuf-backed storage under `.jj/repo/spice/`.
///
/// Manages the sidecar directory and provides typed `load`/`save` for any
/// `prost::Message`. Domain-specific stores (e.g. [`ChangeRequestStore`])
/// delegate file I/O here and add their own query logic on top.
pub struct SpiceStore {
    dir: PathBuf,
}

impl SpiceStore {
    /// Open or create the spice store directory at a given repo path.
    ///
    /// Creates `.jj/repo/spice/` under the given path.
    pub fn init_at(repo_path: &std::path::Path) -> Result<Self, SpiceStoreError> {
        let dir = repo_path.join("spice");
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Load a protobuf message from `<dir>/<filename>`.
    ///
    /// Returns `T::default()` if the file does not exist yet.
    pub fn load<T: Message + Default>(&self, filename: &str) -> Result<T, SpiceStoreError> {
        let path = self.dir.join(filename);
        match fs::read(&path) {
            Ok(buf) => Ok(T::decode(&*buf)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomically save a protobuf message to `<dir>/<filename>`, holding a
    /// file lock for the duration of the write.
    pub fn save<T: Message>(&self, filename: &str, state: &T) -> Result<(), SpiceStoreError> {
        let lock_name = format!("{filename}.lock");
        let _lock = FileLock::lock(self.dir.join(lock_name))?;
        let mut temp = NamedTempFile::new_in(&self.dir)?;
        temp.write_all(&state.encode_to_vec())?;
        persist_temp_file(temp, self.dir.join(filename))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::change_request::{
        ChangeRequests, ForgeMeta, GitHubMeta, forge_meta::Forge as ForgeOneof,
    };
    use tempfile::TempDir;

    /// Helper: create a SpiceStore backed by a temporary directory.
    fn temp_store() -> (TempDir, SpiceStore) {
        let tmp = TempDir::new().unwrap();
        let store = SpiceStore::init_at(tmp.path()).unwrap();
        (tmp, store)
    }

    #[test]
    fn init_at_creates_spice_directory() {
        let tmp = TempDir::new().unwrap();
        let spice_dir = tmp.path().join("spice");
        assert!(!spice_dir.exists());

        let _store = SpiceStore::init_at(tmp.path()).unwrap();
        assert!(spice_dir.is_dir());
    }

    #[test]
    fn load_returns_default_when_file_missing() {
        let (_tmp, store) = temp_store();
        let loaded: ChangeRequests = store.load("nonexistent.pb").unwrap();
        assert!(loaded.by_bookmark.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_tmp, store) = temp_store();

        let mut state = ChangeRequests::default();
        state.by_bookmark.insert(
            "my-feature".into(),
            ForgeMeta {
                forge: Some(ForgeOneof::Github(GitHubMeta {
                    number: 42,
                    source_branch: "my-feature".into(),
                    target_branch: "main".into(),
                    source_repo: "owner/repo".into(),
                    target_repo: "owner/repo".into(),
                    graphql_id: "PR_abc".into(),
                    comment_id: None,
                })),
            },
        );

        store.save("test.pb", &state).unwrap();
        let loaded: ChangeRequests = store.load("test.pb").unwrap();

        assert_eq!(state, loaded);
    }

    #[test]
    fn save_creates_file_on_disk() {
        let (tmp, store) = temp_store();

        let state = ChangeRequests::default();
        store.save("check.pb", &state).unwrap();

        assert!(tmp.path().join("spice").join("check.pb").exists());
    }

    #[test]
    fn load_returns_decode_error_for_corrupted_data() {
        let (tmp, store) = temp_store();

        // Write garbage bytes to the file.
        let path = tmp.path().join("spice").join("corrupted.pb");
        std::fs::write(&path, b"\xff\xfe\xfd\xfc\x00\x01garbage").unwrap();

        let result: Result<ChangeRequests, _> = store.load("corrupted.pb");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, SpiceStoreError::Decode(_)));
    }
}
