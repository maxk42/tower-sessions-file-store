# tower-sessions-file-store

`tower-sessions-file-store` is a simple and minimalistic file store backing provider for
`tower-sessions`.  Usage is extremely simple;

## Example:
```
    let session_store = tower_sessions_file_store::FileStore::new("/path/to/sessions/directory", "prefix-", ".json");
    let session_layer = tower_sessions::SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_expiry(tower_sessions::Expiry::OnInactivity(Duration::seconds(15)))
        ;
    Router::new()
        .route("/sess_test", get(handle_sess_test));
        .layer(session_layer)
        ;
    
    /* ... Elsewhere ... */
    async fn handle_sess_test(sess: tower_sessions::Session) -> impl axum::response::IntoResponse {
        let counter: u32 = sess.get("count").await.unwrap().unwrap_or(0u32);
        let _ = sess.insert("count", counter + 1).await;
        format!("Count is {counter}.")
    }
    
```

