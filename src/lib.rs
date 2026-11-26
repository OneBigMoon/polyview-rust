pub mod auth;
pub mod catalog;
pub mod epg;
pub mod legacy;
pub mod library;
pub mod network;
pub mod playback;
pub mod settings;
pub mod tracking;
pub mod tvbox;
pub mod user_data;

use axum::{
    Json, Router,
    extract::DefaultBodyLimit,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use rusqlite::Connection;
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

pub type Shared = Arc<AppState>;

pub struct AppState {
    pub db: Mutex<Connection>,
    pub media_dir: PathBuf,
    pub download_dir: PathBuf,
    pub client: reqwest::Client,
    pub secure_cookie: bool,
}

impl AppState {
    pub fn conn(&self) -> Result<MutexGuard<'_, Connection>, AppError> {
        self.db
            .lock()
            .map_err(|_| AppError::internal("Database lock failed"))
    }
}

#[derive(Debug)]
pub struct AppError(pub StatusCode, pub String);

impl AppError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!(error = %self.1, "Request failed");
        }
        // Upstream (502/503) failures are actionable for the user: they name
        // the source's HTTP status or content problem. Only mask 500s, which
        // can leak internal paths or SQL details.
        let message = if self.0 == StatusCode::INTERNAL_SERVER_ERROR {
            "An internal error occurred".to_string()
        } else {
            self.1
        };
        (self.0, Json(json!({"error": message}))).into_response()
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(error.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error.to_string())
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Adds a column from a trusted schema declaration without replacing existing data.
pub fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), AppError> {
    let exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name=?2 COLLATE NOCASE)",
        [table, column],
        |row| row.get::<_, bool>(0),
    )?;
    if !exists {
        let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
        conn.execute(
            &format!(
                "ALTER TABLE {} ADD COLUMN {} {definition}",
                quote(table),
                quote(column)
            ),
            [],
        )?;
    }
    Ok(())
}

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA journal_mode=TRUNCATE; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;",
    )?;
    auth::init_schema(conn)?;
    library::init_schema(conn)?;
    network::init_schema(conn)?;
    catalog::init_schema(conn)?;
    tracking::init_schema(conn)?;
    tvbox::init_schema(conn)?;
    epg::init_schema(conn)?;
    user_data::init_schema(conn)?;
    settings::init_schema(conn)?;
    Ok(())
}

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/style.css", get(styles))
        .route("/three-bg.js", get(three_background))
        .route("/vendor/three.module.min.js", get(three_module))
        .route("/vendor/hls.min.js", get(hls_script))
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .merge(auth::router())
        .merge(library::router())
        .merge(network::router())
        .merge(epg::router())
        .merge(playback::router())
        .merge(catalog::router())
        .merge(tracking::router())
        .merge(tvbox::router())
        .merge(user_data::router())
        .merge(settings::router())
        .fallback(|| async { AppError::not_found("Not found") })
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(request: axum::extract::Request, next: Next) -> Response {
    if request.uri().path().starts_with("/api/")
        && !matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD
        )
        && request
            .headers()
            .get("x-polyview-request")
            .and_then(|v| v.to_str().ok())
            != Some("1")
    {
        return AppError(StatusCode::FORBIDDEN, "Missing request header".into()).into_response();
    }
    let image_request = request.uri().path() == "/api/image";
    let mut response = next.run(request).await;
    let cache_image = image_request && response.status().is_success();
    let headers = response.headers_mut();
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    headers.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "same-origin".parse().unwrap());
    headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' https: http: data:; media-src 'self' https: http: blob:; connect-src 'self' https: http:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'".parse().unwrap());
    if !cache_image {
        headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

async fn index() -> impl IntoResponse {
    axum::response::Html(include_str!("../web/index.html"))
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/app.js"),
    )
}
async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}
async fn hls_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/vendor/hls.min.js"),
    )
}

async fn three_background() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/three-bg.js"),
    )
}

async fn three_module() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../web/vendor/three.module.min.js"),
    )
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn repeated_column_upgrade_preserves_existing_rows_and_values() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY); INSERT INTO items VALUES(1);",
        )
        .unwrap();
        ensure_column(&conn, "items", "priority", "INTEGER NOT NULL DEFAULT 100").unwrap();
        let priority = || {
            conn.query_row("SELECT priority FROM items WHERE id=1", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(priority(), 100);
        conn.execute("UPDATE items SET priority=3 WHERE id=1", [])
            .unwrap();
        ensure_column(&conn, "items", "priority", "INTEGER NOT NULL DEFAULT 100").unwrap();
        assert_eq!(priority(), 3);
    }
}
