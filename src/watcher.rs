use anyhow::Result;
use notify::{EventKind, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{
    path::{Path, PathBuf},
    sync::mpsc as std_mpsc,
};
use tokio::{io::AsyncReadExt, sync::mpsc};

/// Spawn a background task that keeps the `files` table in sync with the
/// contents of the on-disk files directory:
///   * on startup, reconcile (remove rows whose files are gone);
///   * thereafter, watch the directory and remove rows when files disappear.
///
/// The `links` row is intentionally NOT deleted, so its short code stays
/// reserved and can never be handed out to a future upload.
pub fn spawn(pool: PgPool, files_dir: PathBuf) {
    tokio::spawn(async move {
        if let Err(e) = backfill_hashes(&pool, &files_dir).await {
            tracing::warn!(?e, "hash backfill failed");
        }
        if let Err(e) = reconcile(&pool, &files_dir).await {
            tracing::warn!(?e, "initial reconcile failed");
        }
        if let Err(e) = watch(&pool, &files_dir).await {
            tracing::warn!(?e, "file watcher stopped");
        }
    });
}

/// One-shot: for every pre-dedup row (content_hash IS NULL), hash the file,
/// move it to the content-addressed path, and record the hash. Rows whose
/// file is gone are left for reconcile to delete.
async fn backfill_hashes(pool: &PgPool, files_dir: &Path) -> Result<()> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT link_id, stored_path FROM files WHERE content_hash IS NULL")
            .fetch_all(pool)
            .await?;
    if rows.is_empty() {
        return Ok(());
    }
    tracing::info!(count = rows.len(), "backfilling content hashes");
    let mut done = 0u64;
    for (link_id, old_path) in rows {
        let hash = match hash_file(&old_path).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(link_id, path = %old_path, ?e, "backfill: hash failed");
                continue;
            }
        };
        let new_path = files_dir.join(&hash);
        let new_path_str = new_path.to_string_lossy().to_string();

        if old_path != new_path_str {
            if tokio::fs::try_exists(&new_path).await.unwrap_or(false) {
                // Dedup hit: canonical file already exists — discard the duplicate.
                if let Err(e) = tokio::fs::remove_file(&old_path).await {
                    tracing::warn!(link_id, path = %old_path, ?e, "backfill: remove dup failed");
                }
            } else if let Err(e) = tokio::fs::rename(&old_path, &new_path).await {
                tracing::warn!(link_id, ?e, "backfill: rename failed");
                continue;
            }
        }

        sqlx::query("UPDATE files SET stored_path = $1, content_hash = $2 WHERE link_id = $3")
            .bind(&new_path_str)
            .bind(&hash)
            .bind(link_id)
            .execute(pool)
            .await?;
        done += 1;
    }
    tracing::info!(done, "backfill complete");
    Ok(())
}

async fn hash_file(path: &str) -> Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
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
