//! `GET /api/ops/db/snapshot` — a consistent copy of the live SQLite DB, streamed.
//!
//! Exists because the runtime image is distroless (no shell, so `railway ssh`
//! cannot copy the volume) and the Railway plan has no volume snapshots.
//!
//! `VACUUM INTO` writes a transactionally consistent copy while the service keeps
//! serving: it reads one snapshot, and WAL mode lets writers continue meanwhile.
//! The copy goes to the container's temp dir (not the volume, so a large DB
//! cannot fill it). The handler opens the copy and unlinks it before streaming:
//! the open fd keeps the bytes readable, and nothing is left behind even if the
//! client disconnects mid-download.
//!
//! Read-only against the live DB. Admin key only (404 otherwise), low rate limit.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;
use tokio_util::io::ReaderStream;

use crate::app::AppState;

static SEQ: AtomicU64 = AtomicU64::new(0);

pub async fn ops_db_backup(State(state): State<AppState>) -> Response {
    let dir = std::env::temp_dir();
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::error!(error = %e, "backup: temp dir unavailable");
        return fail();
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!(
        "apex-analytics-backup-{stamp}-{}.db",
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path_str = path.to_string_lossy().to_string();

    let started = std::time::Instant::now();
    if let Err(e) = sqlx::query("VACUUM INTO ?")
        .bind(&path_str)
        .execute(state.db.pool())
        .await
    {
        tracing::error!(error = %e, "backup: VACUUM INTO failed");
        let _ = tokio::fs::remove_file(&path).await;
        return fail();
    }

    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "backup: open copy failed");
            let _ = tokio::fs::remove_file(&path).await;
            return fail();
        }
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => {
            tracing::error!(error = %e, "backup: stat copy failed");
            let _ = tokio::fs::remove_file(&path).await;
            return fail();
        }
    };
    // Unlink now; the open fd keeps the data until the stream is dropped.
    let _ = tokio::fs::remove_file(&path).await;
    tracing::info!(bytes = len, ms = started.elapsed().as_millis() as u64, "backup: streaming DB copy");

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.sqlite3")
        .header(header::CONTENT_LENGTH, len)
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"dottie-api.db\"",
        )
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(ReaderStream::new(file)))
        .unwrap_or_else(|_| fail())
}

fn fail() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({ "error": "backup failed" })),
    )
        .into_response()
}
