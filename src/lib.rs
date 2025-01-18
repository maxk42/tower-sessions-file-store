//! # tower-sessions-file-store
//! 
//! `tower-sessions-file-store` is a simple and minimalistic file store backing provider for
//! `tower-sessions`.  Usage is extremely simple;
//! 
//! ## Example:
//! ```
//!     let session_store = tower_sessions_file_store::FileStore::new("/path/to/sessions/directory", "prefix-", ".json");
//!     let session_layer = tower_sessions::SessionManagerLayer::new(session_store)
//!         .with_secure(false)
//!         .with_expiry(tower_sessions::Expiry::OnInactivity(Duration::seconds(15)))
//!         ;
//!     Router::new()
//!         .route("/sess_test", get(handle_sess_test));
//!         .layer(session_layer)
//!         ;
//!     
//!     /* ... Elsewhere ... */
//!     async fn handle_sess_test(sess: tower_sessions::Session) -> impl axum::response::IntoResponse {
//!         let counter: u32 = sess.get("count").await.unwrap().unwrap_or(0u32);
//!         let _ = sess.insert("count", counter + 1).await;
//!         format!("Count is {counter}.")
//!     }
//!     
//! ```

use axum::async_trait;
use std::fs;
use tower_sessions::{
    self,
    session::{Id, Record},
    session_store::{
        self,
        Error::Decode,
    },
};

/// Creates a FileStore struct and stores its configuration.  Specifying the `dir`, `prefix`, and
/// `extension` fields will define how session.
/// 
/// For example, if you were to use:
/// ```
///     FileStore::new("/path/to/sesssions/directory", "prefix-", ".json")
/// ```
/// to instantiate a new `FileStore` struct, then you would end up with files such as:
/// 
/// ```bash
///     /path/to/sesssions/directory/prefix-CI4afkzk6tVMRb50lMyZAA.json
///     /path/to/sesssions/directory/prefix-Hs8Jb0_zAGrc_rmUYGwlvw.json
///     /path/to/sesssions/directory/prefix-swJdTjvk1os8zAhhc6AVMQ.json
/// ```
/// 
/// 
#[derive(Clone, Debug, Default)]
pub struct FileStore {
    /// Directory to use for session storage.  Omit any trailing slashes or path separators.
    pub dir: &'static str,
    /// Optional prefix for session files.  If not empty, all files will begin with this prefix
    pub prefix: &'static str,
    pub extension: &'static str,
}

impl FileStore {
    /// Creates a new `FileStore` struct with the specified `dir`, `prefix`, and `extensions` fields.
    pub fn new(dir: &'static str, prefix: &'static str, extension: &'static str) -> Self {
        Self {
            dir,
            prefix,
            extension,
        }
    }
    /// Creates a new `FileStore` struct with the specified `dir` field and blank `prefix` and
    /// `extension` fields.
    pub fn in_dir(d: &'static str) -> Self {
        Self {
            dir: d,
            prefix: "",
            extension: "",
        }
    }
    /// Returns the full path a session with the given `session_id` should be found or created at.
    pub fn path(&self, session_id: &Id) -> String {
        String::new()
            + self.dir
            + std::path::MAIN_SEPARATOR.to_string().as_str()
            + self.prefix
            + session_id.to_string().as_str()
            + self.extension
    }
    /// Internal function for saving a session.  Note that depending on the host file system, this
    /// could be susceptible to clobbering / race conditions.  If you expect multiple concurrent
    /// saves to the same session ID, this may not be the ideal tool for you to use.  Its chief
    /// goals are simplicity and lack of reliance upon external tooling and if you seek stronger
    /// ACID guarantees you should consider another storage system.
    fn save(&self, record: &Record) -> session_store::Result<()> {
        let serialized = serde_json::to_string(&record).map_err(|e| Decode(e.to_string()))?;
        fs::write(self.path(&record.id), serialized).map_err(|e| Decode(e.to_string()))
    }
}

#[async_trait]
/// Implementation of tower_sessions::SessionStore
/// This powers the session storage and deletion system.
/// Note that the self.save() and self.path() calls refer to `impl FileStore`
impl session_store::SessionStore for FileStore {
    async fn create(&self, record: &mut Record) -> session_store::Result<()> {
        self.save(record)
    }
    async fn save(&self, record: &Record) -> session_store::Result<()> {
        self.save(record)
    }
    async fn load(&self, session_id: &Id) -> session_store::Result<Option<Record>> {
        let data: String = fs::read_to_string(self.path(session_id)).map_err(|e| Decode(e.to_string()))?;
        let record: Record = serde_json::from_str(data.as_str()).map_err(|e| Decode(e.to_string()))?;
        Ok(Some(record))
    }
    async fn delete(&self, session_id: &Id) -> session_store::Result<()> {
        fs::remove_file(self.path(session_id)).map_err(|e| Decode(e.to_string()))
    }
}


