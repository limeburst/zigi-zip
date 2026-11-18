use anyhow::Result;
use sqlx::PgPool;

use crate::shortcode;

pub const KIND_FILE: &str = "file";
pub const KIND_CODE: &str = "code";
pub const KIND_URL: &str = "url";

/// The paths the server answers itself, which a link's code must never be,
/// or the link would be unreachable (or shadow the page).
const RESERVED: &[&str] = &[
    "api", "file", "code", "admin", "signin", "signout", "my", "url",
];

/// Allocate a new link row with a unique short code. Starts at 3 chars and grows
/// the code length after a few consecutive collisions, so URLs stay as short as
/// possible early and lengthen naturally as the namespace fills.
pub async fn allocate(pool: &PgPool, kind: &str) -> Result<(i64, String)> {
    allocate_until(pool, kind, None).await
}

/// `allocate`, for a link that stops working at `expires_at` (or never).
pub async fn allocate_until(
    pool: &PgPool,
    kind: &str,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(i64, String)> {
    let mut len: usize = 3;
    let mut attempts_at_len: u32 = 0;
    loop {
        let code = shortcode::generate(len);
        if RESERVED.contains(&code.as_str()) {
            continue;
        }
        let res: std::result::Result<(i64,), sqlx::Error> = sqlx::query_as(
            "INSERT INTO links (short_code, kind, expires_at) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(&code)
        .bind(kind)
        .bind(expires_at)
        .fetch_one(pool)
        .await;
        match res {
            Ok((id,)) => return Ok((id, code)),
            Err(sqlx::Error::Database(dbe)) if dbe.constraint() == Some("links_short_code_key") => {
                attempts_at_len += 1;
                // After 4 collisions at a given length, step up by one char.
                if attempts_at_len >= 4 {
                    len += 1;
                    attempts_at_len = 0;
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Set when a link stops working, or that it never does (None).
pub async fn set_expiry(
    pool: &PgPool,
    link_id: i64,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    sqlx::query("UPDATE links SET expires_at = $2 WHERE id = $1")
        .bind(link_id)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

/// Set when a note stops working, and the files in it with it: they expire
/// together.
pub async fn set_note_expiry(
    pool: &PgPool,
    note_link_id: i64,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    sqlx::query(
        "UPDATE links SET expires_at = $2
         WHERE id = $1 OR id IN (SELECT link_id FROM files WHERE note_link_id = $1)",
    )
    .bind(note_link_id)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// (link_id, kind) for a short code, if any, and not past its expiry.
pub async fn lookup(pool: &PgPool, code: &str) -> Result<Option<(i64, String)>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, kind FROM links
         WHERE short_code = $1 AND (expires_at IS NULL OR expires_at > now())",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The codes of links past their expiry, a batch at a time.
pub async fn expired(pool: &PgPool) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT short_code FROM links WHERE expires_at <= now() ORDER BY expires_at LIMIT 100",
    )
    .fetch_all(pool)
    .await?)
}
