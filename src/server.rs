use anyhow::Result;
use axum::{
    body::Body,
    extract::{ws::WebSocketUpgrade, DefaultBodyLimit, Form, Multipart, Path as AxPath, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
#[allow(unused_imports)]
use percent_encoding;
use futures::TryStreamExt;
use serde::Deserialize;
use sqlx::PgPool;
use std::{path::PathBuf, sync::Arc};
use sha2::{Digest, Sha256};
use tokio::{fs::File, io::AsyncWriteExt};
use tokio_util::io::ReaderStream;

use minijinja::context;

use crate::{config::Config, links, passwd, paste, shortcode, templates, watcher};

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    pool: PgPool,
    pastes: paste::Registry,
}

pub async fn run(cfg: Config, pool: PgPool) -> Result<()> {
    crate::db::migrate(&pool).await?;
    watcher::spawn(pool.clone(), cfg.files_dir());
    let state = AppState {
        cfg: Arc::new(cfg.clone()),
        pool,
        pastes: paste::Registry::default(),
    };
    let app = Router::new()
        .route("/", get(root))
        .route(
            "/file",
            get(upload_form).post(upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/code", get(paste_form).post(create_paste))
        .route("/admin", get(admin_login).post(admin_view))
        .route("/:code", get(dispatch_code))
        .route("/:code/ws", get(paste_ws))
        .route("/:code/:rest", get(dispatch_code_with_rest))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    tracing::info!("zigi listening on {}", cfg.listen);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> Html<String> {
    Html(templates::render("index.html", context! { title => "zigi.zip" }))
}

async fn upload_form() -> Html<String> {
    Html(templates::render(
        "upload_form.html",
        context! { title => "zigi.zip · upload" },
    ))
}

async fn paste_form() -> Html<String> {
    Html(templates::render(
        "paste_form.html",
        context! { title => "zigi.zip · new paste" },
    ))
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
    let (_, admin_label) = match passwd::verify_admin(&st.pool, &form.password).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Html(templates::render(
                "admin_login.html",
                context! { title => "zigi.zip · admin", error => "invalid admin password" },
            ))
            .into_response();
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")),
    };

    let files: Vec<(String, String, i64, chrono::DateTime<chrono::Utc>, String)> = match sqlx::query_as(
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

    Html(templates::render(
        "admin_view.html",
        context! {
            title => "zigi.zip · admin",
            admin_label => admin_label,
            files => files_ctx,
            pastes => pastes_ctx,
        },
    ))
    .into_response()
}

#[derive(Deserialize)]
struct PasteForm {
    password: String,
}

async fn create_paste(State(st): State<AppState>, Form(form): Form<PasteForm>) -> Response {
    let (pw_id, pw_label) = match passwd::verify_password(&st.pool, &form.password).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::UNAUTHORIZED, "invalid password").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}")).into_response(),
    };

    let (link_id, code) = match links::allocate(&st.pool, links::KIND_CODE).await {
        Ok(v) => v,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("alloc: {e}")).into_response(),
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

    Redirect::to(&format!("/{}", code)).into_response()
}

async fn dispatch_code(
    State(st): State<AppState>,
    AxPath(code): AxPath<String>,
) -> Response {
    match links::lookup(&st.pool, &code).await {
        Ok(Some((link_id, kind))) => match kind.as_str() {
            "file" => serve_file(&st, link_id).await,
            "code" => serve_paste_editor(&st, &code).await,
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
        Ok(Some((_, kind))) if kind == "code" => {
            // For pastes, any extra path after the code just shows the editor.
            serve_paste_editor(&st, &code).await
        }
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn serve_file(st: &AppState, link_id: i64) -> Response {
    let row: Option<(String, String, Option<String>, i64)> = match sqlx::query_as(
        "SELECT original_name, stored_path, content_type, size_bytes FROM files WHERE link_id = $1",
    )
    .bind(link_id)
    .fetch_optional(&st.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")).into_response(),
    };
    let Some((original_name, stored_path, content_type, size)) = row else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let f = match File::open(&stored_path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "file missing on disk").into_response(),
    };
    let ct = content_type.unwrap_or_else(|| {
        mime_guess::from_path(&original_name)
            .first_or_octet_stream()
            .to_string()
    });
    let stream = ReaderStream::new(f);
    let body = Body::from_stream(stream);
    let encoded = percent_encoding::utf8_percent_encode(&original_name, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    let disp = format!("inline; filename*=UTF-8''{}", encoded);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .header(header::CONTENT_LENGTH, size)
        .header(header::CONTENT_DISPOSITION, disp)
        .body(body)
        .unwrap()
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

// ---------- file upload (unchanged logic, updated to use links table) ----------

async fn upload(State(st): State<AppState>, mut mp: Multipart) -> Response {
    let mut password: Option<String> = None;
    let mut file_meta: Option<(String, Option<String>)> = None;
    let mut tmp_path: Option<PathBuf> = None;
    let mut content_hash: Option<String> = None;
    let mut size: i64 = 0;

    while let Ok(Some(mut field)) = mp.next_field().await {
        match field.name().unwrap_or("") {
            "password" => password = Some(field.text().await.unwrap_or_default()),
            "file" => {
                let original_name = field.file_name().map(|s| s.to_string()).unwrap_or_else(|| "file".into());
                let content_type = field.content_type().map(|s| s.to_string());
                let tmp = st.cfg.files_dir().join(format!(".tmp-{}", shortcode::generate(12)));
                let mut f = match File::create(&tmp).await {
                    Ok(f) => f,
                    Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("create tmp: {e}")),
                };
                let mut hasher = Sha256::new();
                while let Ok(Some(chunk)) = field.try_next().await {
                    size += chunk.len() as i64;
                    hasher.update(&chunk);
                    if let Err(e) = f.write_all(&chunk).await {
                        let _ = tokio::fs::remove_file(&tmp).await;
                        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("write: {e}"));
                    }
                }
                if let Err(e) = f.flush().await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return err(StatusCode::INTERNAL_SERVER_ERROR, format!("flush: {e}"));
                }
                tmp_path = Some(tmp);
                file_meta = Some((original_name, content_type));
                content_hash = Some(hex::encode(hasher.finalize()));
            }
            _ => {}
        }
    }

    let Some(pw) = password else {
        cleanup(&tmp_path).await;
        return err(StatusCode::BAD_REQUEST, "missing password".into());
    };
    let Some((original_name, content_type)) = file_meta else {
        cleanup(&tmp_path).await;
        return err(StatusCode::BAD_REQUEST, "missing file".into());
    };
    let Some(tmp) = tmp_path else {
        return err(StatusCode::BAD_REQUEST, "missing file".into());
    };
    let Some(content_hash) = content_hash else {
        let _ = tokio::fs::remove_file(&tmp).await;
        return err(StatusCode::BAD_REQUEST, "missing file".into());
    };

    let (pw_id, pw_label) = match passwd::verify_password(&st.pool, &pw).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::UNAUTHORIZED, "invalid password".into());
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("auth: {e}"));
        }
    };

    let (link_id, code) = match links::allocate(&st.pool, links::KIND_FILE).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("alloc: {e}"));
        }
    };

    // Content-addressed storage: path is derived from the hash, so identical
    // content always lands at the same path and dedup is a filesystem property.
    let final_path = st.cfg.files_dir().join(&content_hash);
    let fresh_file = !tokio::fs::try_exists(&final_path).await.unwrap_or(false);
    if fresh_file {
        if let Err(e) = tokio::fs::rename(&tmp, &final_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}"));
        }
    } else {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    let stored_path = final_path.to_string_lossy().to_string();

    let insert = sqlx::query(
        "INSERT INTO files (link_id, original_name, stored_path, content_type, size_bytes, uploaded_by, created_by_label, content_hash)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(link_id)
    .bind(&original_name)
    .bind(&stored_path)
    .bind(content_type.as_deref())
    .bind(size)
    .bind(pw_id)
    .bind(&pw_label)
    .bind(&content_hash)
    .execute(&st.pool)
    .await;
    if let Err(e) = insert {
        if fresh_file {
            let _ = tokio::fs::remove_file(&final_path).await;
        }
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("db insert: {e}"));
    }

    let url = format!("{}/{}", st.cfg.base_url.trim_end_matches('/'), code);
    let qr_svg = qr_svg(&url).unwrap_or_default();
    Html(templates::render(
        "upload_success.html",
        context! { title => "uploaded", url => url, qr_svg => qr_svg },
    ))
    .into_response()
}

fn qr_svg(url: &str) -> Option<String> {
    let qr = qrcode::QrCode::new(url).ok()?;
    Some(
        qr.render::<qrcode::render::svg::Color>()
            .quiet_zone(true)
            .build(),
    )
}

async fn cleanup(tmp: &Option<PathBuf>) {
    if let Some(p) = tmp {
        let _ = tokio::fs::remove_file(p).await;
    }
}

fn err(code: StatusCode, msg: String) -> Response {
    (code, msg).into_response()
}
