use anyhow::Result;
use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, DefaultBodyLimit, Form, Multipart, Path as AxPath, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use futures::{StreamExt, TryStreamExt};
#[allow(unused_imports)]
use percent_encoding;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{path::PathBuf, sync::Arc};
use tokio::{fs::File, io::AsyncWriteExt};
use tokio_util::io::ReaderStream;

use minijinja::context;

use crate::{
    cache::DiskCache, config::Config, links, passwd, paste, session, shortcode, storage::Storage,
    templates, wordlist,
};

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    pool: PgPool,
    pastes: paste::Registry,
    store: Storage,
    /// Local copies of recently uploaded and served files; None when off.
    cache: Option<DiskCache>,
}

pub async fn run(cfg: Config, pool: PgPool) -> Result<()> {
    crate::db::migrate(&pool).await?;
    let cache = match &cfg.cache {
        Some(c) => {
            let cache = DiskCache::open(c).await?;
            let u = cache.usage();
            tracing::info!(dir = %c.dir.display(), files = u.files, bytes = u.bytes, max_bytes = u.max_bytes, "disk cache on");
            Some(cache)
        }
        None => None,
    };
    let state = AppState {
        cache,
        store: Storage::new(&cfg.r2),
        cfg: Arc::new(cfg.clone()),
        pool,
        pastes: paste::Registry::default(),
    };
    let app = Router::new()
        .route("/", get(root))
        .route(
            "/file",
            get(to_front)
                .post(upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/code", get(to_front).post(create_paste))
        .route("/admin", get(admin_login).post(admin_view))
        .route("/admin/delete", axum::routing::post(admin_delete))
        .route("/admin/cache/clear", axum::routing::post(admin_cache_clear))
        .route("/admin/passwd/add", axum::routing::post(admin_passwd_add))
        .route(
            "/admin/passwd/revoke",
            axum::routing::post(admin_passwd_revoke),
        )
        .route(
            "/admin/passwd/set-admin",
            axum::routing::post(admin_passwd_set_admin),
        )
        .route(
            "/admin/passwd/rotate",
            axum::routing::post(admin_passwd_rotate),
        )
        .route("/signin", get(to_front).post(sign_in))
        .route("/signout", axum::routing::post(sign_out))
        .route("/my", get(to_front).post(my_view))
        .route("/url", get(to_front).post(shorten))
        .route("/:code", get(dispatch_code))
        .route("/:code/ws", get(paste_ws))
        .route(
            "/:code/files",
            get(note_files)
                .post(note_upload)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/:code/settings", axum::routing::post(note_settings))
        .route("/:code/:rest", get(dispatch_code_with_rest))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!("zigi listening on {}", cfg.listen);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let signed_in = match session::current(&st.pool, &headers).await {
        Ok(s) => s.map(|s| s.label),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    front(StatusCode::OK, context! { signed_in => signed_in })
}

#[derive(Deserialize)]
struct SignInForm {
    #[serde(default)]
    password: String,
}

/// Just sign in: the front page's button beside the password box.
async fn sign_in(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SignInForm>,
) -> Response {
    match authenticate(&st, &headers, &form.password).await {
        Ok(Some(a)) => session::with_cookie(Redirect::to("/").into_response(), a.set_cookie),
        Ok(None) => front_unauthorized(!form.password.is_empty()),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    }
}

async fn sign_out(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match session::end(&st.pool, &headers, st.secure_cookies()).await {
        Ok(c) => session::with_cookie(Redirect::to("/").into_response(), Some(c)),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, format!("sign out: {e}")),
    }
}

/// Whose request this is. A typed password wins, and signs the browser in
/// (`set_cookie`) so it isn't needed again; with none, the session cookie
/// decides.
struct Auth {
    id: i64,
    label: String,
    is_admin: bool,
    set_cookie: Option<String>,
}

async fn authenticate(
    st: &AppState,
    headers: &HeaderMap,
    password: &str,
) -> anyhow::Result<Option<Auth>> {
    if password.is_empty() {
        return Ok(session::current(&st.pool, headers).await?.map(|s| Auth {
            id: s.id,
            label: s.label,
            is_admin: s.is_admin,
            set_cookie: None,
        }));
    }
    let Some((id, label)) = passwd::verify_password(&st.pool, password).await? else {
        return Ok(None);
    };
    let is_admin: bool = sqlx::query_scalar("SELECT is_admin FROM passwords WHERE id = $1")
        .bind(id)
        .fetch_one(&st.pool)
        .await?;
    let set_cookie = Some(session::create(&st.pool, id, st.secure_cookies()).await?);
    Ok(Some(Auth {
        id,
        label,
        is_admin,
        set_cookie,
    }))
}

impl AppState {
    fn secure_cookies(&self) -> bool {
        self.cfg.base_url.starts_with("https://")
    }
}

/// The old per-action pages are all on the front page now.
async fn to_front() -> Redirect {
    Redirect::to("/")
}

/// Render the front page. `extra` may carry `error`, `signed_in` (the label
/// the browser is signed in as), a `result_*` block, or `mine`.
fn front(status: StatusCode, extra: minijinja::Value) -> Response {
    (
        status,
        Html(templates::render(
            "index.html",
            context! { title => "zigi.zip", ..extra },
        )),
    )
        .into_response()
}

fn front_error(status: StatusCode, msg: &str) -> Response {
    front(status, context! { error => msg })
}

/// The front page's answer to a password that didn't match, or a session
/// that has ended.
fn front_unauthorized(typed: bool) -> Response {
    let msg = if typed {
        "invalid password"
    } else {
        "enter your password"
    };
    front_error(StatusCode::UNAUTHORIZED, msg)
}

async fn admin_login() -> Html<String> {
    Html(templates::render(
        "admin_login.html",
        context! { title => "zigi.zip · admin", error => "" },
    ))
}

#[derive(Deserialize)]
struct AdminForm {
    password: String,
}

async fn admin_view(State(st): State<AppState>, Form(form): Form<AdminForm>) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => {
            return Html(templates::render(
                "admin_login.html",
                context! { title => "zigi.zip · admin", error => "invalid admin password" },
            ))
            .into_response();
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    render_admin(&st, &form.password, &admin_label, None).await
}

#[derive(Deserialize)]
struct AdminDeleteForm {
    password: String,
    code: String,
}

async fn admin_delete(State(st): State<AppState>, Form(form): Form<AdminDeleteForm>) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => {
            return Html(templates::render(
                "admin_login.html",
                context! { title => "zigi.zip · admin", error => "invalid admin password" },
            ))
            .into_response();
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let msg = match delete_link(&st, &form.code).await {
        Ok(m) => m,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("delete: {e}")),
    };
    render_admin(&st, &form.password, &admin_label, Some(msg)).await
}

async fn admin_cache_clear(State(st): State<AppState>, Form(form): Form<AdminForm>) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => return admin_invalid_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let msg = match &st.cache {
        Some(cache) => {
            let (files, bytes) = cache.clear().await;
            format!("cleared the disk cache: {files} files, {}", mb(bytes))
        }
        None => "the disk cache is off".to_string(),
    };
    render_admin(&st, &form.password, &admin_label, Some(msg)).await
}

/// Bytes as megabytes with one decimal, for the admin page.
fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// Delete the link with `code` and — for file links — remove the R2 object
/// only when no other links still reference that `object_key`.
async fn delete_link(st: &AppState, code: &str) -> anyhow::Result<String> {
    let link: Option<(i64, String)> =
        sqlx::query_as("SELECT id, kind FROM links WHERE short_code = $1")
            .bind(code)
            .fetch_optional(&st.pool)
            .await?;
    let Some((link_id, kind)) = link else {
        return Ok(format!("{code}: not found"));
    };
    let object_key: Option<String> = if kind == links::KIND_FILE {
        sqlx::query_scalar("SELECT object_key FROM files WHERE link_id = $1")
            .bind(link_id)
            .fetch_optional(&st.pool)
            .await?
    } else {
        None
    };
    sqlx::query("DELETE FROM links WHERE id = $1")
        .bind(link_id)
        .execute(&st.pool)
        .await?;
    if let Some(key) = object_key {
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM files WHERE object_key = $1")
            .bind(&key)
            .fetch_one(&st.pool)
            .await?;
        if remaining == 0 {
            if let Some(cache) = &st.cache {
                cache.remove(&key).await;
            }
            if let Err(e) = st.store.delete(&key).await {
                // The row is already gone; a stray object is cheaper than a
                // failed admin action, so log it rather than erroring out.
                tracing::warn!(key = %key, ?e, "failed to delete object from R2");
            }
        }
    }
    Ok(format!("deleted {code}"))
}

#[derive(Deserialize)]
struct AdminPasswdAddForm {
    password: String,
    label: String,
    new_password: String,
    #[serde(default)]
    generate: Option<String>,
    #[serde(default)]
    is_admin: Option<String>,
}

async fn admin_passwd_add(
    State(st): State<AppState>,
    Form(form): Form<AdminPasswdAddForm>,
) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => return admin_invalid_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let label = form.label.trim();
    if label.is_empty() {
        return render_admin(
            &st,
            &form.password,
            &admin_label,
            Some("label required".into()),
        )
        .await;
    }
    let generated = form.generate.is_some();
    let new_pw = if generated {
        wordlist::two_word()
    } else if form.new_password.is_empty() {
        return render_admin(
            &st,
            &form.password,
            &admin_label,
            Some("password required (or tick generate)".into()),
        )
        .await;
    } else {
        form.new_password.clone()
    };
    if let Err(e) = passwd::add(&st.pool, label, &new_pw).await {
        return render_admin(
            &st,
            &form.password,
            &admin_label,
            Some(format!("add failed: {e}")),
        )
        .await;
    }
    if form.is_admin.is_some() {
        if let Err(e) = passwd::set_admin(&st.pool, label, true).await {
            return render_admin(
                &st,
                &form.password,
                &admin_label,
                Some(format!("added '{label}' but promote failed: {e}")),
            )
            .await;
        }
    }
    let flash = if generated {
        format!("added '{label}' — generated password: {new_pw}")
    } else {
        format!("added '{label}'")
    };
    render_admin(&st, &form.password, &admin_label, Some(flash)).await
}

#[derive(Deserialize)]
struct AdminPasswdLabelForm {
    password: String,
    label: String,
}

async fn admin_passwd_revoke(
    State(st): State<AppState>,
    Form(form): Form<AdminPasswdLabelForm>,
) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => return admin_invalid_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let flash = match passwd::revoke(&st.pool, &form.label).await {
        Ok(()) => format!("revoked '{}'", form.label),
        Err(e) => format!("revoke failed: {e}"),
    };
    render_admin(&st, &form.password, &admin_label, Some(flash)).await
}

#[derive(Deserialize)]
struct AdminPasswdSetAdminForm {
    password: String,
    label: String,
    is_admin: String,
}

async fn admin_passwd_set_admin(
    State(st): State<AppState>,
    Form(form): Form<AdminPasswdSetAdminForm>,
) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => return admin_invalid_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let want_admin = form.is_admin == "true";
    let flash = match passwd::set_admin(&st.pool, &form.label, want_admin).await {
        Ok(()) => format!(
            "{} '{}'",
            if want_admin { "promoted" } else { "demoted" },
            form.label
        ),
        Err(e) => format!("update failed: {e}"),
    };
    render_admin(&st, &form.password, &admin_label, Some(flash)).await
}

#[derive(Deserialize)]
struct AdminPasswdRotateForm {
    password: String,
    label: String,
    new_password: String,
    #[serde(default)]
    generate: Option<String>,
}

async fn admin_passwd_rotate(
    State(st): State<AppState>,
    Form(form): Form<AdminPasswdRotateForm>,
) -> Response {
    let admin_label = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some((_, l))) => l,
        Ok(None) => return admin_invalid_response(),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let label = form.label.trim();
    if label.is_empty() {
        return render_admin(
            &st,
            &form.password,
            &admin_label,
            Some("label required".into()),
        )
        .await;
    }
    let generated = form.generate.is_some();
    let new_pw = if generated {
        wordlist::two_word()
    } else if form.new_password.is_empty() {
        return render_admin(
            &st,
            &form.password,
            &admin_label,
            Some("new password required (or tick generate)".into()),
        )
        .await;
    } else {
        form.new_password.clone()
    };
    if let Err(e) = passwd::rotate(&st.pool, label, &new_pw).await {
        return render_admin(
            &st,
            &form.password,
            &admin_label,
            Some(format!("rotate failed: {e}")),
        )
        .await;
    }
    // If the admin rotated their own password, keep the page functional by
    // echoing the new plaintext into subsequent hidden form fields.
    let echo_password = if label == admin_label {
        new_pw.clone()
    } else {
        form.password.clone()
    };
    let flash = if generated {
        format!("rotated '{label}' — new password: {new_pw}")
    } else {
        format!("rotated '{label}'")
    };
    render_admin(&st, &echo_password, &admin_label, Some(flash)).await
}

fn admin_invalid_response() -> Response {
    Html(templates::render(
        "admin_login.html",
        context! { title => "zigi.zip · admin", error => "invalid admin password" },
    ))
    .into_response()
}

async fn render_admin(
    st: &AppState,
    password: &str,
    admin_label: &str,
    flash: Option<String>,
) -> Response {
    let files: Vec<(String, String, i64, chrono::DateTime<chrono::Utc>, String)> =
        match sqlx::query_as(
            "SELECT l.short_code, f.original_name, f.size_bytes, f.uploaded_at, f.created_by_label
         FROM files f JOIN links l ON l.id = f.link_id
         ORDER BY f.uploaded_at DESC",
        )
        .fetch_all(&st.pool)
        .await
        {
            Ok(v) => v,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("files: {e}")),
        };

    let pastes: Vec<(String, chrono::DateTime<chrono::Utc>, String)> = match sqlx::query_as(
        "SELECT l.short_code, p.updated_at, p.created_by_label
         FROM pastes p JOIN links l ON l.id = p.link_id
         ORDER BY p.updated_at DESC",
    )
    .fetch_all(&st.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("pastes: {e}")),
    };

    let short_links: Vec<(String, String, chrono::DateTime<chrono::Utc>, String)> =
        match sqlx::query_as(
            "SELECT l.short_code, sl.destination_url, sl.created_at, sl.created_by_label
             FROM short_links sl JOIN links l ON l.id = sl.link_id
             ORDER BY sl.created_at DESC",
        )
        .fetch_all(&st.pool)
        .await
        {
            Ok(v) => v,
            Err(e) => {
                return err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("short_links: {e}"),
                )
            }
        };

    let files_ctx: Vec<_> = files
        .into_iter()
        .map(|(code, name, size, at, by)| {
            context! {
                code => code, original_name => name, size_bytes => size,
                uploaded_at => at.to_rfc3339(), created_by_label => by,
            }
        })
        .collect();
    let pastes_ctx: Vec<_> = pastes
        .into_iter()
        .map(|(code, at, by)| {
            context! {
                code => code, updated_at => at.to_rfc3339(), created_by_label => by,
            }
        })
        .collect();
    let short_links_ctx: Vec<_> = short_links
        .into_iter()
        .map(|(code, dest, at, by)| {
            context! {
                code => code, destination_url => dest, created_at => at.to_rfc3339(), created_by_label => by,
            }
        })
        .collect();

    let passwords: Vec<(
        String,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
        bool,
    )> = match sqlx::query_as(
        "SELECT label, created_at, revoked_at, is_admin FROM passwords ORDER BY created_at DESC",
    )
    .fetch_all(&st.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("passwords: {e}")),
    };
    let passwords_ctx: Vec<_> = passwords
        .into_iter()
        .map(|(label, created, revoked, admin)| {
            context! {
                label => label,
                created_at => created.to_rfc3339(),
                revoked_at => revoked.map(|d| d.to_rfc3339()),
                is_admin => admin,
                active => revoked.is_none(),
            }
        })
        .collect();

    Html(templates::render(
        "admin_view.html",
        context! {
            title => "zigi.zip · admin",
            admin_label => admin_label,
            password => password,
            flash => flash.unwrap_or_default(),
            files => files_ctx,
            pastes => pastes_ctx,
            short_links => short_links_ctx,
            passwords => passwords_ctx,
            cache => st.cache.as_ref().map(|c| {
                let u = c.usage();
                context! {
                    files => u.files,
                    used_mb => format!("{:.1}", u.bytes as f64 / (1024.0 * 1024.0)),
                    max_mb => u.max_bytes / (1024 * 1024),
                }
            }),
        },
    ))
    .into_response()
}

#[derive(Deserialize)]
struct PasteForm {
    #[serde(default)]
    password: String,
}

async fn create_paste(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<PasteForm>,
) -> Response {
    let auth = match authenticate(&st, &headers, &form.password).await {
        Ok(Some(a)) => a,
        Ok(None) => return front_unauthorized(!form.password.is_empty()),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")).into_response(),
    };
    let (pw_id, pw_label) = (auth.id, auth.label);

    let (link_id, code) = match links::allocate(&st.pool, links::KIND_CODE).await {
        Ok(v) => v,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("alloc: {e}")).into_response()
        }
    };

    let init = Vec::<u8>::new();
    if let Err(e) = sqlx::query(
        "INSERT INTO pastes (link_id, doc_state, created_by, created_by_label) VALUES ($1, $2, $3, $4)",
    )
    .bind(link_id)
    .bind(&init)
    .bind(pw_id)
    .bind(&pw_label)
    .execute(&st.pool)
    .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("insert: {e}")).into_response();
    }

    session::with_cookie(
        Redirect::to(&format!("/{}", code)).into_response(),
        auth.set_cookie,
    )
}

async fn dispatch_code(State(st): State<AppState>, AxPath(code): AxPath<String>) -> Response {
    match links::lookup(&st.pool, &code).await {
        Ok(Some((link_id, kind))) => match kind.as_str() {
            "file" => serve_file(&st, link_id).await,
            "code" => serve_paste_editor(&st, &code).await,
            "url" => serve_redirect(&st, link_id).await,
            _ => (StatusCode::NOT_FOUND, "unknown kind").into_response(),
        },
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn dispatch_code_with_rest(
    State(st): State<AppState>,
    AxPath((code, _rest)): AxPath<(String, String)>,
) -> Response {
    match links::lookup(&st.pool, &code).await {
        Ok(Some((link_id, kind))) if kind == "file" => serve_file(&st, link_id).await,
        Ok(Some((_, kind))) if kind == "code" => serve_paste_editor(&st, &code).await,
        Ok(Some((link_id, kind))) if kind == "url" => serve_redirect(&st, link_id).await,
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn serve_file(st: &AppState, link_id: i64) -> Response {
    let row: Option<(String, String, Option<String>, i64, bool)> = match sqlx::query_as(
        "SELECT original_name, object_key, content_type, size_bytes, via_link FROM files WHERE link_id = $1",
    )
    .bind(link_id)
    .fetch_optional(&st.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    };
    let Some((original_name, object_key, content_type, size, via_link)) = row else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let body = match file_body(st, &object_key, size).await {
        Ok(b) => b,
        Err(res) => return res,
    };
    // Files that came in through a note's link without a password are only
    // ever downloads: never rendered here, so an anonymous upload can't put a
    // page (say, a fake password form) on this domain.
    let ct = if via_link {
        "application/octet-stream".to_string()
    } else {
        content_type.unwrap_or_else(|| {
            mime_guess::from_path(&original_name)
                .first_or_octet_stream()
                .to_string()
        })
    };
    let encoded =
        percent_encoding::utf8_percent_encode(&original_name, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    let disposition = if via_link { "attachment" } else { "inline" };
    let disp = format!("{disposition}; filename*=UTF-8''{}", encoded);
    let mut res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, disp);
    if via_link {
        res = res.header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    }
    res.body(body).unwrap()
}

/// A file's bytes: from the disk cache when it has them, else from R2 --
/// keeping a copy in the cache as they stream past, for the next request.
async fn file_body(st: &AppState, key: &str, size: i64) -> Result<Body, Response> {
    if let Some(cache) = &st.cache {
        if let Some(f) = cache.get(key).await {
            return Ok(Body::from_stream(ReaderStream::new(f)));
        }
    }
    let object = match st.store.get(key).await {
        Ok(Some(b)) => b,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "file missing in storage").into_response()),
        Err(e) => {
            tracing::error!(key = %key, ?e, "R2 get failed");
            return Err((StatusCode::BAD_GATEWAY, "storage unavailable").into_response());
        }
    };
    let stream = ReaderStream::new(object.into_async_read());
    let fill = match &st.cache {
        Some(c) => c.start_fill(key, size.max(0) as u64).await,
        None => None,
    };
    let Some(fill) = fill else {
        return Ok(Body::from_stream(stream));
    };
    // Copy each chunk into the cache on its way out. The copy is kept once
    // every byte has passed (checked against the hash); if the client goes
    // away first, the stream is dropped and so is the partial copy.
    let teed = futures::stream::unfold((stream, Some(fill)), |(mut stream, mut fill)| async move {
        match stream.next().await {
            Some(Ok(chunk)) => {
                if let Some(f) = fill.as_mut() {
                    if f.write(&chunk).await.is_err() {
                        fill = None;
                    }
                }
                // Finish as soon as the last byte is in: with a known
                // Content-Length the server may stop polling right after it.
                if fill.as_ref().is_some_and(|f| f.complete()) {
                    if let Some(f) = fill.take() {
                        f.finish().await;
                    }
                }
                Some((Ok::<_, std::io::Error>(chunk), (stream, fill)))
            }
            Some(Err(e)) => Some((Err(e), (stream, None))),
            None => {
                if let Some(f) = fill {
                    f.finish().await;
                }
                None
            }
        }
    });
    Ok(Body::from_stream(teed))
}

async fn serve_paste_editor(st: &AppState, code: &str) -> Response {
    let base = st.cfg.base_url.trim_end_matches('/');
    let url = format!("{}/{}", base, code);
    let code_json = serde_json::to_string(code).unwrap_or_else(|_| "\"\"".into());
    Html(templates::render(
        "editor.html",
        context! { code => code, url => url, code_json => code_json },
    ))
    .into_response()
}

async fn paste_ws(
    State(st): State<AppState>,
    AxPath(code): AxPath<String>,
    ws: WebSocketUpgrade,
) -> Response {
    let (link_id, kind) = match links::lookup(&st.pool, &code).await {
        Ok(Some(v)) => v,
        _ => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    if kind != "code" {
        return (StatusCode::NOT_FOUND, "not a code doc").into_response();
    }
    let doc = match st.pastes.get_or_load(&st.pool, link_id).await {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("load: {e}")).into_response(),
    };
    // No cap on Yjs message/frame size — documents can grow arbitrarily.
    ws.max_message_size(usize::MAX)
        .max_frame_size(usize::MAX)
        .on_upgrade(move |socket| paste::handle_ws(socket, doc))
}

async fn serve_redirect(st: &AppState, link_id: i64) -> Response {
    let dest: Option<String> =
        sqlx::query_scalar("SELECT destination_url FROM short_links WHERE link_id = $1")
            .bind(link_id)
            .fetch_optional(&st.pool)
            .await
            .unwrap_or(None);
    match dest {
        Some(url) => Redirect::to(&url).into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[derive(Deserialize)]
struct MyForm {
    #[serde(default)]
    password: String,
}

async fn my_view(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<MyForm>,
) -> Response {
    let auth = match authenticate(&st, &headers, &form.password).await {
        Ok(Some(a)) => a,
        Ok(None) => return front_unauthorized(!form.password.is_empty()),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let (pw_id, pw_label) = (auth.id, auth.label.clone());

    let files: Vec<(String, String, i64, chrono::DateTime<chrono::Utc>)> = match sqlx::query_as(
        "SELECT l.short_code, f.original_name, f.size_bytes, f.uploaded_at
         FROM files f JOIN links l ON l.id = f.link_id
         WHERE f.uploaded_by = $1
         ORDER BY f.uploaded_at DESC",
    )
    .bind(pw_id)
    .fetch_all(&st.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("files: {e}")),
    };

    let pastes: Vec<(String, chrono::DateTime<chrono::Utc>)> = match sqlx::query_as(
        "SELECT l.short_code, p.updated_at
         FROM pastes p JOIN links l ON l.id = p.link_id
         WHERE p.created_by = $1
         ORDER BY p.updated_at DESC",
    )
    .bind(pw_id)
    .fetch_all(&st.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("pastes: {e}")),
    };

    let short_links: Vec<(String, String, chrono::DateTime<chrono::Utc>)> = match sqlx::query_as(
        "SELECT l.short_code, sl.destination_url, sl.created_at
         FROM short_links sl JOIN links l ON l.id = sl.link_id
         WHERE sl.created_by = $1
         ORDER BY sl.created_at DESC",
    )
    .bind(pw_id)
    .fetch_all(&st.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("short_links: {e}"),
            )
        }
    };

    let files_ctx: Vec<_> = files
        .into_iter()
        .map(|(code, name, size, at)| {
            context! {
                code => code, original_name => name, size_bytes => size,
                uploaded_at => at.to_rfc3339(),
            }
        })
        .collect();
    let pastes_ctx: Vec<_> = pastes
        .into_iter()
        .map(|(code, at)| context! { code => code, updated_at => at.to_rfc3339() })
        .collect();
    let short_links_ctx: Vec<_> = short_links
        .into_iter()
        .map(|(code, dest, at)| {
            context! { code => code, destination_url => dest, created_at => at.to_rfc3339() }
        })
        .collect();

    let page = front(
        StatusCode::OK,
        context! {
            signed_in => auth.label,
            mine => context! {
                label => pw_label,
                files => files_ctx,
                pastes => pastes_ctx,
                short_links => short_links_ctx,
            },
        },
    );
    session::with_cookie(page, auth.set_cookie)
}

#[derive(Deserialize)]
struct ShortenForm {
    #[serde(default)]
    password: String,
    url: String,
}

async fn shorten(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ShortenForm>,
) -> Response {
    // Sign in first, so a typo'd URL doesn't cost the password too.
    let auth = match authenticate(&st, &headers, &form.password).await {
        Ok(Some(a)) => a,
        Ok(None) => return front_unauthorized(!form.password.is_empty()),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let url = form.url.trim().to_string();
    let bad = if url.is_empty() {
        Some("enter a URL to shorten")
    } else if !url.starts_with("http://") && !url.starts_with("https://") {
        Some("url must start with http:// or https://")
    } else {
        None
    };
    if let Some(msg) = bad {
        let page = front(
            StatusCode::BAD_REQUEST,
            context! { error => msg, signed_in => auth.label },
        );
        return session::with_cookie(page, auth.set_cookie);
    }
    let (pw_id, pw_label) = (auth.id, auth.label.clone());

    let (link_id, code) = match links::allocate(&st.pool, links::KIND_URL).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("alloc: {e}")),
    };

    if let Err(e) = sqlx::query(
        "INSERT INTO short_links (link_id, destination_url, created_by, created_by_label) VALUES ($1, $2, $3, $4)",
    )
    .bind(link_id)
    .bind(&url)
    .bind(pw_id)
    .bind(&pw_label)
    .execute(&st.pool)
    .await
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("insert: {e}"));
    }

    let short_url = format!("{}/{}", st.cfg.base_url.trim_end_matches('/'), code);
    let qr_svg = qr_svg(&short_url).unwrap_or_default();
    let page = front(
        StatusCode::OK,
        context! {
            signed_in => auth.label,
            result_kind => "short link",
            result_url => short_url,
            result_destination => url,
            qr_svg => qr_svg,
        },
    );
    session::with_cookie(page, auth.set_cookie)
}

// ---------- file upload ----------

/// A multipart file field, written out to a temp file.
struct Spooled {
    tmp: PathBuf,
    content_hash: String,
    size: i64,
    original_name: String,
    content_type: Option<String>,
}

enum SpoolError {
    /// The field ran past `cap` bytes; the temp file is already gone.
    TooLarge,
    Io(String),
}

/// Where to spool an upload: into the disk cache's directory when it's on,
/// so keeping the file afterwards is a rename; else the system's temporary
/// directory, which also clears out any that a crash leaves behind.
fn spool_path(st: &AppState) -> PathBuf {
    match &st.cache {
        Some(c) => c.spool_path(),
        None => std::env::temp_dir().join(format!("zigi-upload-{}", shortcode::generate(12))),
    }
}

/// Spool a file field to `tmp`: we need the whole body to know its SHA-256
/// (the object key) and its length before R2 will take it. The temp file never
/// outlives the request. With `cap`, stops as soon as the body passes that
/// many bytes.
async fn spool(
    field: &mut axum::extract::multipart::Field<'_>,
    tmp: PathBuf,
    cap: Option<i64>,
) -> std::result::Result<Spooled, SpoolError> {
    let original_name = field
        .file_name()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "file".into());
    let content_type = field.content_type().map(|s| s.to_string());
    let mut f = File::create(&tmp)
        .await
        .map_err(|e| SpoolError::Io(format!("create tmp: {e}")))?;
    let mut hasher = Sha256::new();
    let mut size: i64 = 0;
    loop {
        let chunk = match field.try_next().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(SpoolError::Io(format!("read: {e}")));
            }
        };
        size += chunk.len() as i64;
        if cap.is_some_and(|c| size > c) {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(SpoolError::TooLarge);
        }
        hasher.update(&chunk);
        if let Err(e) = f.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(SpoolError::Io(format!("write: {e}")));
        }
    }
    if let Err(e) = f.flush().await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(SpoolError::Io(format!("flush: {e}")));
    }
    Ok(Spooled {
        tmp,
        content_hash: hex::encode(hasher.finalize()),
        size,
        original_name,
        content_type,
    })
}

/// Who a stored file is filed under.
struct Uploader {
    by: Option<i64>,
    label: String,
    /// Set for files added through a note's link without a password.
    note_link_id: Option<i64>,
    via_link: bool,
}

/// Move a spooled file into R2 and give it a link. Returns its short code.
/// The temp file is kept in the disk cache when that's on, else removed.
async fn store_spooled(
    st: &AppState,
    s: Spooled,
    who: &Uploader,
) -> std::result::Result<String, (StatusCode, String)> {
    // Content-addressed storage: the object key is the hash, so identical
    // content always lands at the same key and dedup is a property of the
    // key space. put_file_if_absent skips the transfer on a dedup hit.
    let stored = st
        .store
        .put_file_if_absent(&s.content_hash, &s.tmp, s.content_type.as_deref())
        .await;
    match (&stored, &st.cache) {
        // Just uploaded, so likely about to be fetched: keep a local copy.
        (Ok(_), Some(cache)) => cache.adopt(&s.content_hash, &s.tmp).await,
        _ => {
            let _ = tokio::fs::remove_file(&s.tmp).await;
        }
    }
    let fresh_object = match stored {
        Ok(fresh) => fresh,
        Err(e) => {
            tracing::error!(key = %s.content_hash, ?e, "R2 upload failed");
            return Err((StatusCode::BAD_GATEWAY, "storage upload failed".into()));
        }
    };

    let (link_id, code) = links::allocate(&st.pool, links::KIND_FILE)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("alloc: {e}")))?;

    let insert = sqlx::query(
        "INSERT INTO files (link_id, original_name, object_key, content_type, size_bytes,
                            uploaded_by, created_by_label, content_hash, note_link_id, via_link)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(link_id)
    .bind(&s.original_name)
    .bind(&s.content_hash)
    .bind(s.content_type.as_deref())
    .bind(s.size)
    .bind(who.by)
    .bind(&who.label)
    .bind(&s.content_hash)
    .bind(who.note_link_id)
    .bind(who.via_link)
    .execute(&st.pool)
    .await;
    if let Err(e) = insert {
        if fresh_object {
            let _ = st.store.delete(&s.content_hash).await;
            if let Some(cache) = &st.cache {
                cache.remove(&s.content_hash).await;
            }
        }
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("db insert: {e}")));
    }
    Ok(code)
}

async fn upload(State(st): State<AppState>, headers: HeaderMap, mut mp: Multipart) -> Response {
    let mut password = String::new();
    let mut spooled: Option<Spooled> = None;

    while let Ok(Some(mut field)) = mp.next_field().await {
        match field.name().unwrap_or("") {
            "password" => password = field.text().await.unwrap_or_default(),
            "file" => match spool(&mut field, spool_path(&st), None).await {
                Ok(s) => {
                    if let Some(old) = spooled.replace(s) {
                        let _ = tokio::fs::remove_file(&old.tmp).await;
                    }
                }
                Err(SpoolError::Io(m)) => {
                    discard(&spooled).await;
                    return err(StatusCode::INTERNAL_SERVER_ERROR, m);
                }
                Err(SpoolError::TooLarge) => unreachable!("no cap"),
            },
            _ => {}
        }
    }

    let auth = match authenticate(&st, &headers, &password).await {
        Ok(Some(a)) => a,
        Ok(None) => {
            discard(&spooled).await;
            return front_unauthorized(!password.is_empty());
        }
        Err(e) => {
            discard(&spooled).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}"));
        }
    };
    let Some(s) = spooled else {
        let page = front(
            StatusCode::BAD_REQUEST,
            context! { error => "choose a file to upload", signed_in => auth.label },
        );
        return session::with_cookie(page, auth.set_cookie);
    };

    let who = Uploader {
        by: Some(auth.id),
        label: auth.label.clone(),
        note_link_id: None,
        via_link: false,
    };
    let code = match store_spooled(&st, s, &who).await {
        Ok(c) => c,
        Err((status, msg)) => return err(status, msg),
    };

    let url = format!("{}/{}", st.cfg.base_url.trim_end_matches('/'), code);
    let qr_svg = qr_svg(&url).unwrap_or_default();
    let page = front(
        StatusCode::OK,
        context! { signed_in => auth.label, result_kind => "uploaded", result_url => url, qr_svg => qr_svg },
    );
    session::with_cookie(page, auth.set_cookie)
}

async fn discard(s: &Option<Spooled>) {
    if let Some(s) = s {
        let _ = tokio::fs::remove_file(&s.tmp).await;
    }
}

// ---------- files in a note ----------

/// Per-file cap on uploads made through a note's link without a password.
const LINK_UPLOAD_MAX_FILE: i64 = 25 * 1024 * 1024;
/// Cap on everything uploaded that way into one note.
const LINK_UPLOAD_MAX_TOTAL: i64 = 500 * 1024 * 1024;

struct Note {
    link_id: i64,
    owner: Option<i64>,
    owner_label: String,
    open_uploads: bool,
}

async fn note_by_code(pool: &PgPool, code: &str) -> anyhow::Result<Option<Note>> {
    let row: Option<(i64, Option<i64>, String, bool)> = sqlx::query_as(
        "SELECT p.link_id, p.created_by, p.created_by_label, p.open_uploads
         FROM pastes p JOIN links l ON l.id = p.link_id
         WHERE l.short_code = $1 AND l.kind = 'code'",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(link_id, owner, owner_label, open_uploads)| Note {
        link_id,
        owner,
        owner_label,
        open_uploads,
    }))
}

fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// Tell the note's open editors to reload its file list.
async fn notify_files_changed(st: &AppState, note_link_id: i64) {
    if let Some(doc) = st.pastes.get_loaded(note_link_id).await {
        doc.bump_files_rev();
    }
}

async fn note_files(
    State(st): State<AppState>,
    AxPath(code): AxPath<String>,
    headers: HeaderMap,
) -> Response {
    let note = match note_by_code(&st.pool, &code).await {
        Ok(Some(n)) => n,
        // Not a note: this is a file link with a name after it, as before.
        Ok(None) => return dispatch_code(State(st), AxPath(code)).await,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
    };
    let files: Vec<(String, String, i64, chrono::DateTime<chrono::Utc>)> = match sqlx::query_as(
        "SELECT l.short_code, f.original_name, f.size_bytes, f.uploaded_at
         FROM files f JOIN links l ON l.id = f.link_id
         WHERE f.note_link_id = $1
         ORDER BY f.uploaded_at",
    )
    .bind(note.link_id)
    .fetch_all(&st.pool)
    .await
    {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("files: {e}")),
    };
    let files: Vec<_> = files
        .into_iter()
        .map(|(code, name, size, at)| {
            json!({ "code": code, "name": name, "size": size, "uploaded_at": at.to_rfc3339() })
        })
        .collect();
    // Lets the page skip the password box for a signed-in browser, and show
    // owner settings as usable for its owner (or an admin).
    let me = match session::current(&st.pool, &headers).await {
        Ok(m) => m,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    let can_manage = me
        .as_ref()
        .is_some_and(|m| note.owner == Some(m.id) || m.is_admin);
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "signed_in": me.map(|m| m.label),
            "can_manage": can_manage,
            "open_uploads": note.open_uploads,
            "max_file": LINK_UPLOAD_MAX_FILE,
            "files": files,
        })),
    )
        .into_response()
}

/// Upload one file into a note. With a valid password, or signed in, the file
/// is filed under that password, as on the front page. Otherwise the note's
/// owner must have opened uploads to link holders; the file is then filed
/// under the owner, capped in size, and only ever served as a download.
async fn note_upload(
    State(st): State<AppState>,
    AxPath(code): AxPath<String>,
    headers: HeaderMap,
    mp: Multipart,
) -> Response {
    let mut set_cookie = None;
    let res = note_upload_inner(&st, &code, &headers, mp, &mut set_cookie).await;
    // A typed password signs the browser in, even if the upload then fails.
    session::with_cookie(res, set_cookie)
}

async fn note_upload_inner(
    st: &AppState,
    code: &str,
    headers: &HeaderMap,
    mut mp: Multipart,
    set_cookie: &mut Option<String>,
) -> Response {
    let note = match note_by_code(&st.pool, &code).await {
        Ok(Some(n)) => n,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
    };

    let mut password = String::new();
    while let Ok(Some(mut field)) = mp.next_field().await {
        match field.name().unwrap_or("") {
            // The page sends the password ahead of the file, so we know whose
            // upload this is (and its cap) before taking any bytes.
            "password" => password = field.text().await.unwrap_or_default(),
            "file" => {
                let auth = match authenticate(&st, headers, &password).await {
                    Ok(a) => a,
                    Err(e) => {
                        return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}"))
                    }
                };
                let who = if let Some(a) = auth {
                    *set_cookie = a.set_cookie;
                    Uploader {
                        by: Some(a.id),
                        label: a.label,
                        note_link_id: Some(note.link_id),
                        via_link: false,
                    }
                } else if !password.is_empty() {
                    return json_err(StatusCode::UNAUTHORIZED, "invalid password");
                } else if note.open_uploads {
                    Uploader {
                        by: note.owner,
                        label: format!("{} (via {})", note.owner_label, code),
                        note_link_id: Some(note.link_id),
                        via_link: true,
                    }
                } else {
                    return json_err(
                        StatusCode::UNAUTHORIZED,
                        "enter your zigi.zip password to upload here",
                    );
                };

                let cap = if who.via_link {
                    let used: i64 = match sqlx::query_scalar(
                        "SELECT COALESCE(SUM(size_bytes), 0)::BIGINT FROM files
                         WHERE note_link_id = $1 AND via_link",
                    )
                    .bind(note.link_id)
                    .fetch_one(&st.pool)
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}"))
                        }
                    };
                    let left = LINK_UPLOAD_MAX_TOTAL - used;
                    if left <= 0 {
                        return json_err(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "this note is full for uploads without a password (500 MB)",
                        );
                    }
                    Some(LINK_UPLOAD_MAX_FILE.min(left))
                } else {
                    None
                };

                let s = match spool(&mut field, spool_path(&st), cap).await {
                    Ok(s) => s,
                    Err(SpoolError::TooLarge) => {
                        return json_err(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "too large: without a password, files are capped at 25 MB each and 500 MB per note",
                        )
                    }
                    Err(SpoolError::Io(m)) => {
                        return json_err(StatusCode::INTERNAL_SERVER_ERROR, m)
                    }
                };
                let (name, size) = (s.original_name.clone(), s.size);
                let file_code = match store_spooled(&st, s, &who).await {
                    Ok(c) => c,
                    Err((status, msg)) => return json_err(status, msg),
                };
                notify_files_changed(&st, note.link_id).await;
                let url = format!("{}/{}", st.cfg.base_url.trim_end_matches('/'), file_code);
                return Json(json!({ "code": file_code, "url": url, "name": name, "size": size }))
                    .into_response();
            }
            _ => {}
        }
    }
    json_err(StatusCode::BAD_REQUEST, "choose a file to upload")
}

#[derive(Deserialize)]
struct NoteSettingsForm {
    #[serde(default)]
    password: String,
    open_uploads: String,
}

/// Owner-only: open or close uploads without a password. An admin works too.
async fn note_settings(
    State(st): State<AppState>,
    AxPath(code): AxPath<String>,
    headers: HeaderMap,
    Form(form): Form<NoteSettingsForm>,
) -> Response {
    let note = match note_by_code(&st.pool, &code).await {
        Ok(Some(n)) => n,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "not found"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
    };
    let auth = match authenticate(&st, &headers, &form.password).await {
        Ok(Some(a)) => a,
        Ok(None) if form.password.is_empty() => {
            return json_err(StatusCode::UNAUTHORIZED, "enter the owner's password")
        }
        Ok(None) => return json_err(StatusCode::UNAUTHORIZED, "invalid password"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };
    if note.owner != Some(auth.id) && !auth.is_admin {
        return session::with_cookie(
            json_err(
                StatusCode::FORBIDDEN,
                "only the note's owner can change this",
            ),
            auth.set_cookie,
        );
    }
    let set_cookie = auth.set_cookie;
    let open = form.open_uploads == "true";
    if let Err(e) = sqlx::query("UPDATE pastes SET open_uploads = $1 WHERE link_id = $2")
        .bind(open)
        .bind(note.link_id)
        .execute(&st.pool)
        .await
    {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, format!("update: {e}"));
    }
    notify_files_changed(&st, note.link_id).await;
    session::with_cookie(
        Json(json!({ "open_uploads": open })).into_response(),
        set_cookie,
    )
}

fn qr_svg(url: &str) -> Option<String> {
    let qr = qrcode::QrCode::new(url).ok()?;
    Some(
        qr.render::<qrcode::render::svg::Color>()
            .quiet_zone(true)
            .build(),
    )
}

fn err(code: StatusCode, msg: String) -> Response {
    (code, msg).into_response()
}
