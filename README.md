# tower-sessions-file-based-store

A file-based session store for [`tower-sessions`](https://docs.rs/tower-sessions): one JSON
file per session in a directory you choose, no database, and human-readable on disk.

## Guarantees

* **Atomic writes.** A session is written to a temporary file beside its final path and then
  renamed into place, so a reader never sees a partially written file.  Concurrent saves of the
  same session each write their own temporary file; the last rename wins with a complete record.
* **Expiry is enforced on read.** An expired session file is treated as absent and removed.
  `tower-sessions` itself does not check expiry after a store returns a record, so a store must.
* **Self-healing.** An empty or undecodable session file is removed and treated as absent, so a
  crash artifact or a hand-edited file never wedges a visitor on an error.
* **Sweep.** The store implements `ExpiredDeletion`: `delete_expired` removes expired sessions
  and orphaned temporary files, and `continuously_delete_expired` runs it on an interval.
* **Never blocks the runtime.** All file I/O runs on tokio's blocking thread pool.
* **Owner-only permissions.** The directory is created with mode `0700` and session files with
  `0600`.

Unix hosts only: the permissions above and the atomic rename rely on POSIX semantics (Windows
does not replace an open destination on rename).

## Usage

```rust
use std::time::Duration;
use tower_sessions::{Expiry, ExpiredDeletion, SessionManagerLayer};
use tower_sessions_file_based_store::FileStore;

// Creates the directory if it does not exist.
let store = FileStore::new("/var/lib/myapp/sessions", "sess-", ".json")?;

// Sweep expired sessions and orphaned temp files every ten minutes.
tokio::spawn(store.clone().continuously_delete_expired(Duration::from_secs(600)));

let session_layer = SessionManagerLayer::new(store)
    .with_expiry(Expiry::OnInactivity(time::Duration::hours(2)));
// ... then `Router::new().layer(session_layer)`.
```

Files are named `<prefix><session id><extension>`, for example
`/var/lib/myapp/sessions/sess-CI4afkzk6tVMRb50lMyZAA.json`.  The session id is 22 characters of
URL-safe base64, so a file name can never contain a path separator, and the sweep only touches
files whose names decode to a session id (plus the store's own `.tmp-` files).  Anything else in
the directory is left alone.

Options:

* `FileStore::in_dir(dir)` uses an empty prefix and extension.
* `.with_fsync(true)` flushes each temporary file to disk before the rename.  Off by default:
  sessions are not worth a disk flush per request, and losing the last few seconds of session
  state in a power failure only costs a login.
* `.with_temp_max_age(age)` sets how old a temporary file must be before the sweep treats it as an
  orphan of a crashed write.  Default ten minutes.

## Changes in 0.2.0

Breaking:

* `FileStore::new` and `FileStore::in_dir` take owned paths and strings (anything `Into<PathBuf>`
  and `Into<String>`), create the directory, and return `std::io::Result<FileStore>`.
* `FileStore::path` returns a `PathBuf`.  The struct's fields are private; use `dir()`, `prefix()`,
  and `extension()`.
* Depends on `tower-sessions-core` 0.15 (works with `tower-sessions` 0.15).  The `axum` dependency
  is gone; the crate never used axum types.
* Requires a tokio runtime (file I/O runs on its blocking pool).

Behaviour:

* Writes are atomic; expiry is enforced on load; unreadable files heal to "no session";
  `ExpiredDeletion` is implemented; `delete` of an unknown session succeeds; `create` assigns a
  fresh id if the file for the given id already exists.
* I/O errors are reported as `session_store::Error::Backend` (they were `Decode`).

## License

MIT
