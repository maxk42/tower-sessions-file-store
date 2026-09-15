//! # tower-sessions-file-based-store
//!
//! A file-based session store for [`tower-sessions`]: one JSON file per session in a directory
//! you choose, no database, and human-readable on disk.
//!
//! Guarantees:
//!
//! * **Atomic writes.** A session is written to a temporary file beside its final path and then
//!   renamed into place, so a reader never sees a partially written file.
//! * **Expiry is enforced on read.** An expired session file is treated as absent and removed;
//!   `tower-sessions` itself does not check expiry after a store returns a record.
//! * **Self-healing.** An empty or undecodable session file is removed and treated as absent,
//!   so a crash artifact or a hand-edited file never wedges a visitor on an error.
//! * **Sweep.** [`ExpiredDeletion`] removes expired sessions and orphaned temporary files;
//!   run `continuously_delete_expired` on a tokio task.
//! * **Never blocks the runtime.** All file I/O runs on tokio's blocking thread pool.
//!
//! Unix hosts only: the directory is created with mode `0700`, session files with `0600`, and
//! the atomic rename relies on POSIX semantics (Windows does not replace an open destination).
//!
//! ## Example
//!
//! ```no_run
//! use std::time::Duration;
//! use tower_sessions::{Expiry, ExpiredDeletion, SessionManagerLayer};
//! use tower_sessions_file_based_store::FileStore;
//!
//! # async fn example() -> std::io::Result<()> {
//! // Creates the directory if it does not exist.
//! let store = FileStore::new("/var/lib/myapp/sessions", "sess-", ".json")?;
//!
//! // Sweep expired sessions and orphaned temp files every ten minutes.
//! tokio::spawn(store.clone().continuously_delete_expired(Duration::from_secs(600)));
//!
//! let session_layer = SessionManagerLayer::new(store)
//!     .with_expiry(Expiry::OnInactivity(time::Duration::hours(2)));
//! // ... then `Router::new().layer(session_layer)`.
//! # Ok(())
//! # }
//! ```
//!
//! [`tower-sessions`]: https://docs.rs/tower-sessions

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::{
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use time::OffsetDateTime;
use tower_sessions_core::{
    session::{Id, Record},
    session_store::{self, Error, ExpiredDeletion, SessionStore},
};

/// Name prefix of the temporary files the store writes before renaming them into place.  The
/// leading dot keeps them out of plain directory listings, and the prefix lets the sweep tell
/// them apart from session files.
const TEMP_PREFIX: &str = ".tmp-";

/// Default age after which the sweep removes a temporary file as the orphan of a crashed write.
const DEFAULT_TEMP_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// Number of fresh ids `create` tries when the file for the record's id already exists.
const CREATE_ATTEMPTS: usize = 4;

/// A session store keeping one JSON file per session in a directory.
///
/// Files are named `<prefix><session id><extension>`.  The session id is 22 characters of
/// URL-safe base64 (`A-Z`, `a-z`, `0-9`, `-`, `_`), so a file name can never contain a path
/// separator or a `.` component, and the store never builds a path from untrusted input.
/// Cloning the store is cheap and every clone writes to the same directory.
///
/// USAGE: let store = FileStore::new("/var/lib/app/sessions", "sess-", ".json")?;
///
#[derive(Clone, Debug)]
pub struct FileStore {
    /// Directory holding the session files.
    dir: PathBuf,
    /// Prefix of every session file name; may be empty.
    prefix: String,
    /// Suffix of every session file name, including its dot if one is wanted; may be empty.
    extension: String,
    /// Whether to flush each temporary file to disk before renaming it into place.
    fsync: bool,
    /// Age after which the sweep removes a temporary file as an orphan.
    temp_max_age: Duration,
}

impl FileStore {
    /// Creates a store that keeps its session files in `dir`, creating the directory (and any
    /// missing parents) with mode `0700` if it does not exist.
    ///
    /// USAGE: let store = FileStore::new("/var/lib/app/sessions", "sess-", ".json")?;
    ///
    /// PARAMETERS:
    ///     dir: `impl Into<PathBuf>`      -> Directory for the session files.
    ///     prefix: `impl Into<String>`    -> Prefix of every session file name; may be empty.
    ///     extension: `impl Into<String>` -> Suffix of every session file name, dot included if
    ///                                     wanted; may be empty.
    ///
    /// RETURN VALUE:
    ///     The store, or the I/O error that prevented creating or using the directory.
    ///
    pub fn new(
        dir: impl Into<PathBuf>,
        prefix: impl Into<String>,
        extension: impl Into<String>,
    ) -> io::Result<Self> {
        let store = Self {
            dir: dir.into(),
            prefix: prefix.into(),
            extension: extension.into(),
            fsync: false,
            temp_max_age: DEFAULT_TEMP_MAX_AGE,
        };
        store.ensure_dir()?;
        Ok(store)
    }

    /// Creates a store in `dir` with an empty prefix and extension, so files are named by the
    /// session id alone.  See [`FileStore::new`].
    ///
    /// USAGE: let store = FileStore::in_dir("/var/lib/app/sessions")?;
    ///
    /// PARAMETERS:
    ///     dir: `impl Into<PathBuf>` -> Directory for the session files.
    ///
    /// RETURN VALUE:
    ///     The store, or the I/O error that prevented creating or using the directory.
    ///
    pub fn in_dir(dir: impl Into<PathBuf>) -> io::Result<Self> {
        Self::new(dir, "", "")
    }

    /// Sets whether each temporary file is flushed to disk (`fsync`) before the rename.  Off by
    /// default: sessions are not worth a disk flush per request, and losing the last few
    /// seconds of session state in a power failure only costs a login.
    ///
    /// PARAMETERS:
    ///     fsync: bool -> true to flush before every rename.
    ///
    /// RETURN VALUE:
    ///     The store, for chaining.
    ///
    pub fn with_fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }

    /// Sets how old a temporary file must be before the sweep treats it as the orphan of a
    /// crashed write and removes it.  Default ten minutes.
    ///
    /// PARAMETERS:
    ///     age: Duration -> Minimum age of a temporary file for the sweep to remove it.
    ///
    /// RETURN VALUE:
    ///     The store, for chaining.
    ///
    pub fn with_temp_max_age(mut self, age: Duration) -> Self {
        self.temp_max_age = age;
        self
    }

    /// Returns the directory holding the session files.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the prefix of every session file name.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Returns the suffix of every session file name.
    pub fn extension(&self) -> &str {
        &self.extension
    }

    /// Returns the path at which the session with the given id is stored, whether or not it
    /// exists.
    ///
    /// USAGE: let path = store.path(&session_id);
    ///
    /// PARAMETERS:
    ///     session_id: &Id -> The session id.
    ///
    /// RETURN VALUE:
    ///     `<dir>/<prefix><session id><extension>`.
    ///
    pub fn path(&self, session_id: &Id) -> PathBuf {
        self.dir
            .join(format!("{}{}{}", self.prefix, session_id, self.extension))
    }

    /// Creates the session directory, with mode `0700` on Unix, if it does not exist, and
    /// checks that the path is a directory.
    ///
    /// RETURN VALUE:
    ///     Ok if the directory exists afterwards, else the I/O error.
    ///
    fn ensure_dir(&self) -> io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.dir)?;
        if !self.dir.is_dir() {
            return Err(io::Error::new(
                ErrorKind::NotADirectory,
                format!("{} is not a directory", self.dir.display()),
            ));
        }
        Ok(())
    }

    /// Recovers the session id from a file name in the session directory, if the name has this
    /// store's prefix and extension around a valid session id.  Used by the sweep so that it
    /// never touches a file the store did not write.
    ///
    /// PARAMETERS:
    ///     name: &str -> A file name (no directory part).
    ///
    /// RETURN VALUE:
    ///     The session id, or None if the name is not a session file of this store.
    ///
    fn session_id_from_file_name(&self, name: &str) -> Option<Id> {
        name.strip_prefix(self.prefix.as_str())?
            .strip_suffix(self.extension.as_str())?
            .parse()
            .ok()
    }

    /// Writes `json` to a fresh temporary file beside the session's final path.  The name is
    /// `.tmp-<session id>-<random nonce>`, so concurrent writers of one session never share a
    /// file; it is created exclusively with mode `0600` and, if `fsync` is on, flushed.
    ///
    /// PARAMETERS:
    ///     session_id: &Id -> The session being written.
    ///     json: &[u8]     -> The serialized record.
    ///
    /// RETURN VALUE:
    ///     The temporary file's path, ready to be renamed or linked into place.  On error the
    ///     temporary file has been removed.
    ///
    fn write_temp(&self, session_id: &Id, json: &[u8]) -> session_store::Result<PathBuf> {
        let temp = self
            .dir
            .join(format!("{TEMP_PREFIX}{session_id}-{}", Id::default()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options.open(&temp).and_then(|mut file| {
            file.write_all(json)?;
            if self.fsync {
                file.sync_all()?;
            }
            Ok(())
        });
        if let Err(e) = written {
            remove_quietly(&temp);
            return Err(backend("write", &temp, e));
        }
        Ok(temp)
    }

    /// Blocking body of `save`: serializes the record, writes it to a temporary file, and
    /// renames that over the session's final path.  The rename is atomic, so a reader sees
    /// either the previous file or the new one, never a partial write.
    ///
    /// PARAMETERS:
    ///     record: &Record -> The session record to store.
    ///
    /// RETURN VALUE:
    ///     Ok once the file is in place.
    ///
    fn save_blocking(&self, record: &Record) -> session_store::Result<()> {
        let json = serde_json::to_vec(record).map_err(|e| Error::Encode(e.to_string()))?;
        let temp = self.write_temp(&record.id, &json)?;
        let path = self.path(&record.id);
        fs::rename(&temp, &path).map_err(|e| {
            remove_quietly(&temp);
            backend("rename", &path, e)
        })
    }

    /// Blocking body of `create`: like `save_blocking`, but the temporary file is hard-linked to
    /// the final path instead of renamed.  A hard link fails atomically if the target exists,
    /// which is how an id collision is detected; the record then gets a fresh id and the write
    /// is retried, up to `CREATE_ATTEMPTS` times.
    ///
    /// PARAMETERS:
    ///     record: &mut Record -> The new session record; its id may be replaced.
    ///
    /// RETURN VALUE:
    ///     Ok once the file is in place under the id now in `record`.
    ///
    fn create_blocking(&self, record: &mut Record) -> session_store::Result<()> {
        for _ in 0..CREATE_ATTEMPTS {
            let json = serde_json::to_vec(&*record).map_err(|e| Error::Encode(e.to_string()))?;
            let temp = self.write_temp(&record.id, &json)?;
            let path = self.path(&record.id);
            let linked = fs::hard_link(&temp, &path);
            remove_quietly(&temp);
            match linked {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    tracing::warn!(id = %record.id, "session id already in use; choosing another");
                    record.id = Id::default();
                }
                Err(e) => return Err(backend("link", &path, e)),
            }
        }
        Err(Error::Backend(format!(
            "could not find an unused session id after {CREATE_ATTEMPTS} attempts in {}",
            self.dir.display()
        )))
    }

    /// Blocking body of `load`: reads and decodes the session file.  A missing file is `None`.
    /// An empty or undecodable file is removed and reported as `None`, so a crash artifact or a
    /// hand-edited file heals into a fresh session instead of an error on every request.  An
    /// expired record is removed and reported as `None`, because `tower-sessions` does not check
    /// expiry after a store returns a record.
    ///
    /// PARAMETERS:
    ///     session_id: &Id -> The session to load.
    ///
    /// RETURN VALUE:
    ///     The live record, or None.
    ///
    fn load_blocking(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let path = self.path(session_id);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(backend("read", &path, e)),
        };
        let record: Record = match serde_json::from_slice(&bytes) {
            Ok(record) => record,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "removing unreadable session file",
                );
                remove_quietly(&path);
                return Ok(None);
            }
        };
        if !is_active(record.expiry_date) {
            remove_quietly(&path);
            return Ok(None);
        }
        Ok(Some(record))
    }

    /// Blocking body of `delete`: removes the session file.  Removing a session that does not
    /// exist is not an error.
    ///
    /// PARAMETERS:
    ///     session_id: &Id -> The session to delete.
    ///
    /// RETURN VALUE:
    ///     Ok once no file exists for the id.
    ///
    fn delete_blocking(&self, session_id: &Id) -> session_store::Result<()> {
        let path = self.path(session_id);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(backend("remove", &path, e)),
        }
    }

    /// Blocking body of `delete_expired`: one pass over the directory.  Session files (names
    /// that decode to a session id between this store's prefix and extension) are removed when
    /// expired or unreadable; temporary files older than `temp_max_age` are removed as orphans;
    /// everything else is left alone.
    ///
    /// RETURN VALUE:
    ///     Ok once the pass is complete.  A file that cannot be read or removed is logged and
    ///     skipped rather than aborting the sweep.
    ///
    fn delete_expired_blocking(&self) -> session_store::Result<()> {
        let now = OffsetDateTime::now_utc();
        let system_now = SystemTime::now();
        let entries = fs::read_dir(&self.dir).map_err(|e| backend("list", &self.dir, e))?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!(
                        dir = %self.dir.display(),
                        error = %e,
                        "skipping unreadable entry",
                    );
                    continue;
                }
            };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with(TEMP_PREFIX) {
                let age = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|modified| system_now.duration_since(modified).ok());
                if age.is_some_and(|age| age > self.temp_max_age) {
                    tracing::warn!(
                        path = %path.display(),
                        "removing orphaned temporary session file",
                    );
                    remove_quietly(&path);
                }
                continue;
            }
            if self.session_id_from_file_name(name).is_none() {
                continue;
            }
            match fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<Record>(&bytes) {
                    Ok(record) if record.expiry_date <= now => remove_quietly(&path),
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "removing unreadable session file",
                        );
                        remove_quietly(&path);
                    }
                },
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "could not read session file",
                    );
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SessionStore for FileStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        let store = self.clone();
        let mut owned = record.clone();
        let owned = spawn(move || store.create_blocking(&mut owned).map(|()| owned)).await??;
        record.id = owned.id;
        Ok(())
    }

    async fn save(&self, record: &Record) -> session_store::Result<()> {
        let store = self.clone();
        let record = record.clone();
        spawn(move || store.save_blocking(&record)).await?
    }

    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let store = self.clone();
        let session_id = *session_id;
        spawn(move || store.load_blocking(&session_id)).await?
    }

    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        let store = self.clone();
        let session_id = *session_id;
        spawn(move || store.delete_blocking(&session_id)).await?
    }
}

#[async_trait]
impl ExpiredDeletion for FileStore {
    async fn delete_expired(&self) -> session_store::Result<()> {
        let store = self.clone();
        spawn(move || store.delete_expired_blocking()).await?
    }
}

/// Runs a blocking closure on tokio's blocking thread pool so file I/O never stalls the
/// async runtime.
///
/// PARAMETERS:
///     f: FnOnce() -> T -> The blocking work.
///
/// RETURN VALUE:
///     The closure's value, or a backend error if the task was cancelled or panicked.
///
async fn spawn<T, F>(f: F) -> session_store::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::Backend(format!("session store blocking task failed: {e}")))
}

/// Builds the store's backend error for a failed file operation.
///
/// PARAMETERS:
///     action: &str  -> What was attempted, e.g. "rename".
///     path: &Path   -> The file involved.
///     err: io::Error -> The underlying error.
///
/// RETURN VALUE:
///     `Error::Backend` with a message naming the action, the path, and the cause.
///
fn backend(action: &str, path: &Path, err: io::Error) -> Error {
    Error::Backend(format!("could not {action} {}: {err}", path.display()))
}

/// Removes a file, logging any failure other than the file already being gone.
///
/// PARAMETERS:
///     path: &Path -> The file to remove.
///
fn remove_quietly(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), error = %e, "could not remove file");
        }
    }
}

/// Reports whether a record with the given expiry is still live.
///
/// PARAMETERS:
///     expiry_date: OffsetDateTime -> The record's expiry.
///
/// RETURN VALUE:
///     true if the expiry is in the future.
///
fn is_active(expiry_date: OffsetDateTime) -> bool {
    expiry_date > OffsetDateTime::now_utc()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs::File};

    use super::*;

    /// A unique directory under the system temp dir, removed when dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("tsfbs-test-{}", Id::default())))
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store(dir: &TestDir) -> FileStore {
        FileStore::new(&dir.0, "sess-", ".json").expect("store")
    }

    fn record_expiring_in(seconds: i64) -> Record {
        let mut data = HashMap::new();
        data.insert("count".to_string(), serde_json::json!(1));
        Record {
            id: Id::default(),
            data,
            expiry_date: OffsetDateTime::now_utc() + time::Duration::seconds(seconds),
        }
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_creates_the_directory_with_owner_only_permissions() {
        let dir = TestDir::new();
        let nested = dir.0.join("a").join("b");
        let store = FileStore::new(&nested, "", "").unwrap();
        assert!(store.dir().is_dir());
        #[cfg(unix)]
        assert_eq!(mode_of(&nested), 0o700);
    }

    #[test]
    fn new_fails_when_the_path_is_a_file() {
        let dir = TestDir::new();
        fs::create_dir_all(&dir.0).unwrap();
        let file = dir.0.join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        assert!(FileStore::new(&file, "", "").is_err());
    }

    #[test]
    fn path_joins_prefix_id_and_extension() {
        let dir = TestDir::new();
        let store = store(&dir);
        let id = Id::default();
        assert_eq!(store.path(&id), dir.0.join(format!("sess-{id}.json")));
        assert_eq!(
            store.session_id_from_file_name(&format!("sess-{id}.json")),
            Some(id)
        );
        assert_eq!(store.session_id_from_file_name(&format!("{id}.json")), None);
        assert_eq!(store.session_id_from_file_name("sess-README.json"), None);
        assert_eq!(store.session_id_from_file_name(&format!("sess-{id}")), None);
    }

    #[tokio::test]
    async fn create_then_load_round_trips() {
        let dir = TestDir::new();
        let store = store(&dir);
        let mut record = record_expiring_in(60);
        store.create(&mut record).await.unwrap();
        assert_eq!(store.load(&record.id).await.unwrap(), Some(record.clone()));
        assert_eq!(file_names(&dir.0), vec![format!("sess-{}.json", record.id)]);
        #[cfg(unix)]
        assert_eq!(mode_of(&store.path(&record.id)), 0o600);
    }

    #[tokio::test]
    async fn save_replaces_the_file_and_leaves_no_temp_files() {
        let dir = TestDir::new();
        let store = store(&dir);
        let mut record = record_expiring_in(60);
        store.create(&mut record).await.unwrap();
        record
            .data
            .insert("count".to_string(), serde_json::json!(2));
        store.save(&record).await.unwrap();
        let loaded = store.load(&record.id).await.unwrap().unwrap();
        assert_eq!(loaded.data["count"], serde_json::json!(2));
        assert_eq!(file_names(&dir.0), vec![format!("sess-{}.json", record.id)]);
        let on_disk: serde_json::Value =
            serde_json::from_slice(&fs::read(store.path(&record.id)).unwrap()).unwrap();
        assert_eq!(on_disk["data"]["count"], serde_json::json!(2));
        #[cfg(unix)]
        assert_eq!(mode_of(&store.path(&record.id)), 0o600);
    }

    #[tokio::test]
    async fn save_with_fsync_works() {
        let dir = TestDir::new();
        let store = store(&dir).with_fsync(true);
        let mut record = record_expiring_in(60);
        store.create(&mut record).await.unwrap();
        store.save(&record).await.unwrap();
        assert_eq!(store.load(&record.id).await.unwrap(), Some(record));
    }

    #[tokio::test]
    async fn load_of_an_unknown_id_is_none() {
        let dir = TestDir::new();
        let store = store(&dir);
        assert_eq!(store.load(&Id::default()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn load_of_an_expired_session_is_none_and_removes_the_file() {
        let dir = TestDir::new();
        let store = store(&dir);
        let record = record_expiring_in(-1);
        store.save(&record).await.unwrap();
        assert!(store.path(&record.id).exists());
        assert_eq!(store.load(&record.id).await.unwrap(), None);
        assert!(!store.path(&record.id).exists());
    }

    #[tokio::test]
    async fn load_of_an_unreadable_file_is_none_and_removes_it() {
        let dir = TestDir::new();
        let store = store(&dir);
        for garbage in [&b""[..], &b"{not json"[..], &b"[1, 2, 3]"[..]] {
            let id = Id::default();
            fs::write(store.path(&id), garbage).unwrap();
            assert_eq!(store.load(&id).await.unwrap(), None);
            assert!(
                !store.path(&id).exists(),
                "{garbage:?} should have been removed"
            );
        }
    }

    #[tokio::test]
    async fn delete_removes_the_file_and_is_idempotent() {
        let dir = TestDir::new();
        let store = store(&dir);
        let mut record = record_expiring_in(60);
        store.create(&mut record).await.unwrap();
        store.delete(&record.id).await.unwrap();
        assert!(!store.path(&record.id).exists());
        store.delete(&record.id).await.unwrap();
        assert_eq!(store.load(&record.id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn create_picks_a_new_id_when_the_file_already_exists() {
        let dir = TestDir::new();
        let store = store(&dir);
        let taken = Id::default();
        fs::write(store.path(&taken), b"occupied").unwrap();
        let mut record = record_expiring_in(60);
        record.id = taken;
        store.create(&mut record).await.unwrap();
        assert_ne!(record.id, taken);
        assert_eq!(fs::read(store.path(&taken)).unwrap(), b"occupied");
        assert_eq!(store.load(&record.id).await.unwrap(), Some(record.clone()));
        assert_eq!(file_names(&dir.0).len(), 2);
    }

    #[tokio::test]
    async fn delete_expired_removes_only_expired_sessions_and_stale_temp_files() {
        let dir = TestDir::new();
        let store = store(&dir);
        let live = record_expiring_in(60);
        store.save(&live).await.unwrap();
        let expired = record_expiring_in(-1);
        store.save(&expired).await.unwrap();
        let corrupt = Id::default();
        fs::write(store.path(&corrupt), b"???").unwrap();
        let unrelated = dir.0.join("README");
        fs::write(&unrelated, b"not a session").unwrap();
        let foreign = dir.0.join(format!("{}.json", Id::default()));
        fs::write(&foreign, b"???").unwrap();
        let fresh_temp = dir.0.join(".tmp-fresh");
        fs::write(&fresh_temp, b"x").unwrap();
        let old_temp = dir.0.join(".tmp-old");
        fs::write(&old_temp, b"x").unwrap();
        File::options()
            .write(true)
            .open(&old_temp)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();

        store.delete_expired().await.unwrap();

        assert!(store.path(&live.id).exists(), "live session kept");
        assert!(!store.path(&expired.id).exists(), "expired session removed");
        assert!(!store.path(&corrupt).exists(), "unreadable session removed");
        assert!(unrelated.exists(), "unrelated file kept");
        assert!(foreign.exists(), "file without the store's prefix kept");
        assert!(fresh_temp.exists(), "fresh temp file kept");
        assert!(!old_temp.exists(), "stale temp file removed");
        assert_eq!(store.load(&live.id).await.unwrap(), Some(live));
    }

    #[tokio::test]
    async fn delete_expired_honours_the_temp_max_age_option() {
        let dir = TestDir::new();
        let store = store(&dir).with_temp_max_age(Duration::ZERO);
        let temp = dir.0.join(".tmp-anything");
        fs::write(&temp, b"x").unwrap();
        File::options()
            .write(true)
            .open(&temp)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2))
            .unwrap();
        store.delete_expired().await.unwrap();
        assert!(!temp.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_saves_never_leave_a_torn_file() {
        let dir = TestDir::new();
        let store = store(&dir);
        let mut record = record_expiring_in(60);
        store.create(&mut record).await.unwrap();
        let writers: Vec<_> = (0..32)
            .map(|i| {
                let store = store.clone();
                let mut record = record.clone();
                record
                    .data
                    .insert("count".to_string(), serde_json::json!(i));
                tokio::spawn(async move { store.save(&record).await })
            })
            .collect();
        for writer in writers {
            writer.await.unwrap().unwrap();
        }
        let loaded = store
            .load(&record.id)
            .await
            .unwrap()
            .expect("a complete record");
        let count = loaded.data["count"].as_i64().unwrap();
        assert!((0..32).contains(&count));
        assert_eq!(file_names(&dir.0), vec![format!("sess-{}.json", record.id)]);
    }
}
