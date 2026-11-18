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
    let mut len: usize = 3;
    let mut attempts_at_len: u32 = 0;
    loop {
        let code = shortcode::generate(len);
        if RESERVED.contains(&code.as_str()) {
            continue;
        }
        let res: std::result::Result<(i64,), sqlx::Error> =
            sqlx::query_as("INSERT INTO links (short_code, kind) VALUES ($1, $2) RETURNING id")
                .bind(&code)
                .bind(kind)
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

/// How long a long code is: 20 of the 57 letters, about 117 bits, which no
/// one guesses.
pub const LONG_LEN: usize = 20;

/// Allocate a link with a long code that stops working at `expires_at`.
pub async fn allocate_expiring(
    pool: &PgPool,
    kind: &str,
    expires_at: chrono::DateTime<chrono::Utc>,
) -> Result<(i64, String)> {
    let code = shortcode::generate(LONG_LEN);
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO links (short_code, kind, expires_at) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&code)
    .bind(kind)
    .bind(expires_at)
    .fetch_one(pool)
    .await?;
    Ok((id, code))
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
