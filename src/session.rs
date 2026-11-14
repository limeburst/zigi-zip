//! Sign-in sessions: the first time a browser gives a valid password, it gets
//! a cookie, and later requests are filed under that password without it.

use anyhow::Result;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::Response;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub const COOKIE: &str = "zigi_session";
/// How long a sign-in lasts.
const MAX_AGE_DAYS: i64 = 365;

/// The password a request is signed in as.
pub struct SignedIn {
    pub id: i64,
    pub label: String,
    pub is_admin: bool,
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// The session token in the request's cookie, if any.
pub fn token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
}

/// Who the request's cookie signs it in as. Sessions of revoked passwords,
/// and ones past their age, don't count.
pub async fn current(pool: &PgPool, headers: &HeaderMap) -> Result<Option<SignedIn>> {
    let Some(t) = token(headers) else {
        return Ok(None);
    };
    let row: Option<(i64, String, bool)> = sqlx::query_as(
        "SELECT p.id, p.label, p.is_admin
         FROM sessions s JOIN passwords p ON p.id = s.password_id
         WHERE s.token_hash = $1 AND p.revoked_at IS NULL
           AND s.created_at > now() - make_interval(days => $2)",
    )
    .bind(hash(&t))
    .bind(MAX_AGE_DAYS as i32)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, label, is_admin)| SignedIn {
        id,
        label,
        is_admin,
    }))
}

/// Start a session for `password_id`; returns the Set-Cookie value.
pub async fn create(pool: &PgPool, password_id: i64, secure: bool) -> Result<String> {
    let mut raw = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut raw);
    let t = hex::encode(raw);
    sqlx::query("INSERT INTO sessions (token_hash, password_id) VALUES ($1, $2)")
        .bind(hash(&t))
        .bind(password_id)
        .execute(pool)
        .await?;
    Ok(cookie(&t, MAX_AGE_DAYS * 86400, secure))
}

/// End the request's session; returns the Set-Cookie value that clears it.
pub async fn end(pool: &PgPool, headers: &HeaderMap, secure: bool) -> Result<String> {
    if let Some(t) = token(headers) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(hash(&t))
            .execute(pool)
            .await?;
    }
    Ok(cookie("", 0, secure))
}

/// End every session of the password with `label`.
pub async fn end_all_for(pool: &PgPool, label: &str) -> Result<()> {
    sqlx::query(
        "DELETE FROM sessions WHERE password_id IN (SELECT id FROM passwords WHERE label = $1)",
    )
    .bind(label)
    .execute(pool)
    .await?;
    Ok(())
}

// SameSite=Lax keeps other sites' forms from posting as the signed-in user.
fn cookie(value: &str, max_age: i64, secure: bool) -> String {
    format!(
        "{COOKIE}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    )
}

/// Add a Set-Cookie header to `res` when there is one.
pub fn with_cookie(mut res: Response, set_cookie: Option<String>) -> Response {
    if let Some(c) = set_cookie.and_then(|c| HeaderValue::from_str(&c).ok()) {
        res.headers_mut().append(header::SET_COOKIE, c);
    }
    res
}
