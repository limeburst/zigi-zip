use anyhow::Result;
use sqlx::PgPool;

use crate::shortcode;

pub const KIND_FILE: &str = "file";
pub const KIND_CODE: &str = "code";
pub const KIND_URL: &str = "url";

/// Allocate a new link row with a unique short code. Starts at 3 chars and grows
/// the code length after a few consecutive collisions, so URLs stay as short as
/// possible early and lengthen naturally as the namespace fills.
pub async fn allocate(pool: &PgPool, kind: &str) -> Result<(i64, String)> {
    let mut len: usize = 3;
    let mut attempts_at_len: u32 = 0;
    loop {
        let code = shortcode::generate(len);
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

/// (link_id, kind) for a short code, if any.
pub async fn lookup(pool: &PgPool, code: &str) -> Result<Option<(i64, String)>> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, kind FROM links WHERE short_code = $1")
            .bind(code)
            .fetch_optional(pool)
            .await?;
    Ok(row)
}
