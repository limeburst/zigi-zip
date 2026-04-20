use anyhow::{Context, Result};
use bcrypt::{hash, verify, DEFAULT_COST};
use sqlx::PgPool;

pub async fn add(pool: &PgPool, label: &str, password: &str) -> Result<()> {
    let h = hash(password, DEFAULT_COST).context("hashing password")?;
    sqlx::query("INSERT INTO passwords (label, bcrypt_hash) VALUES ($1, $2)")
        .bind(label)
        .bind(h)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list(pool: &PgPool) -> Result<()> {
    let rows = sqlx::query_as::<_, (String, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)>(
        "SELECT label, created_at, revoked_at FROM passwords ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    println!("{:<24} {:<28} {}", "LABEL", "CREATED", "REVOKED");
    for (label, created, revoked) in rows {
        println!(
            "{:<24} {:<28} {}",
            label,
            created.to_rfc3339(),
            revoked.map(|d| d.to_rfc3339()).unwrap_or_else(|| "-".into())
        );
    }
    Ok(())
}

pub async fn revoke(pool: &PgPool, label: &str) -> Result<()> {
    let res = sqlx::query("UPDATE passwords SET revoked_at = now() WHERE label = $1 AND revoked_at IS NULL")
        .bind(label)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("no active password with label '{}'", label);
    }
    Ok(())
}

/// Returns the matching active password's (id, label), or None if no active password matches.
pub async fn verify_password(pool: &PgPool, password: &str) -> Result<Option<(i64, String)>> {
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT id, label, bcrypt_hash FROM passwords WHERE revoked_at IS NULL",
    )
    .fetch_all(pool)
    .await?;
    for (id, label, h) in rows {
        if verify(password, &h).unwrap_or(false) {
            return Ok(Some((id, label)));
        }
    }
    Ok(None)
}
