use anyhow::Result;
use notify::{EventKind, RecursiveMode, Watcher};
use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    sync::mpsc as std_mpsc,
};
use tokio::sync::mpsc;

/// Spawn a background task that keeps the `files` table in sync with the
/// contents of the on-disk files directory:
///   * on startup, reconcile (remove rows whose files are gone);
///   * thereafter, watch the directory and remove rows when files disappear.
///
/// The `links` row is intentionally NOT deleted, so its short code stays
/// reserved and can never be handed out to a future upload.
pub fn spawn(pool: PgPool, files_dir: PathBuf) {
    tokio::spawn(async move {
        if let Err(e) = reconcile(&pool, &files_dir).await {
            tracing::warn!(?e, "initial reconcile failed");
        }
        if let Err(e) = watch(&pool, &files_dir).await {
            tracing::warn!(?e, "file watcher stopped");
        }
    });
}

async fn reconcile(pool: &PgPool, _files_dir: &Path) -> Result<()> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT link_id, stored_path FROM files")
            .fetch_all(pool)
            .await?;
    let mut removed = 0u64;
    for (link_id, path) in rows {
        if !Path::new(&path).exists() {
            let res = sqlx::query("DELETE FROM files WHERE link_id = $1")
                .bind(link_id)
                .execute(pool)
                .await?;
            removed += res.rows_affected();
            tracing::info!(link_id, path = %path, "reconcile: removed missing file row");
        }
    }
    if removed > 0 {
        tracing::info!(removed, "reconcile complete");
    }
    Ok(())
}

async fn watch(pool: &PgPool, files_dir: &Path) -> Result<()> {
    // notify is synchronous + uses a std mpsc; bridge it to a tokio channel.
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Event>();
    let (notify_tx, notify_rx) =
        std_mpsc::channel::<std::result::Result<notify::Event, notify::Error>>();

    let mut watcher = notify::recommended_watcher(notify_tx)?;
    watcher.watch(files_dir, RecursiveMode::NonRecursive)?;

    std::thread::spawn(move || {
        for res in notify_rx {
            match res {
                Ok(ev) => { let _ = tx.send(ev); }
                Err(e) => tracing::warn!(?e, "notify error"),
            }
        }
    });

    tracing::info!(dir = %files_dir.display(), "watching files dir");

    while let Some(event) = rx.recv().await {
        if !matches!(event.kind, EventKind::Remove(_)) {
            continue;
        }
        for path in event.paths {
            // Ignore in-flight tmp uploads; the DB only has final paths.
            if path.file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with(".tmp-"))
                .unwrap_or(false)
            {
                continue;
            }
            on_removed(pool, &path).await;
        }
    }

    drop(watcher);
    Ok(())
}

async fn on_removed(pool: &PgPool, path: &Path) {
    let path_str = path.to_string_lossy().to_string();
    match sqlx::query("DELETE FROM files WHERE stored_path = $1")
        .bind(&path_str)
        .execute(pool)
        .await
    {
        Ok(res) if res.rows_affected() > 0 => {
            tracing::info!(path = %path_str, "file deleted on disk, removed DB row");
        }
        Ok(_) => {} // not ours (tmp file, unrelated)
        Err(e) => tracing::warn!(?e, path = %path_str, "failed to remove files row"),
    }
}
