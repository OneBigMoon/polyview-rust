mod config;
mod resolve;

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    playback,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post, put},
};
use config::{COLUMNS, Parser};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub(crate) use config::validate_config;
pub(crate) use resolve::{has_matching_parser, resolve_media};

pub struct ResolvedMedia {
    pub url: String,
    pub headers: HeaderMap,
}

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS parser_cache (
            input TEXT NOT NULL, flag TEXT NOT NULL, media TEXT NOT NULL, headers TEXT NOT NULL,
            expires INTEGER NOT NULL, PRIMARY KEY(input,flag)
        );
        CREATE TABLE IF NOT EXISTS parser_revision (
            id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO parser_revision VALUES(1,0);",
    )?;
    for action in ["INSERT", "UPDATE", "DELETE"] {
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS parser_invalidate_{action} AFTER {action} ON parsers
             BEGIN DELETE FROM parser_cache; UPDATE parser_revision SET revision=revision+1 WHERE id=1; END;"
        ))?;
    }
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/parsers", get(list).post(create))
        .route("/api/parsers/{id}", put(update).delete(remove))
        .route("/api/parsers/{id}/test", post(test_parser))
        .route("/api/playback/resolve", post(resolve_playback))
}

fn parser(conn: &Connection, id: i64) -> Result<Parser, AppError> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM parsers WHERE id=?"),
        [id],
        Parser::row,
    )
    .optional()?
    .ok_or_else(|| AppError::not_found("Parser does not exist"))
}

async fn list(State(state): State<Shared>, user: AuthUser) -> Result<Json<Vec<Parser>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM parsers ORDER BY priority DESC,id"
    ))?;
    Ok(Json(
        stmt.query_map([], Parser::row)?.collect::<Result<_, _>>()?,
    ))
}

#[derive(Deserialize)]
struct ParserInput {
    name: String,
    url_template: Option<String>,
    headers_json: Option<String>,
    flags: Option<String>,
    priority: Option<i64>,
    timeout_ms: Option<i64>,
    enabled: Option<bool>,
}

impl ParserInput {
    fn apply(self, previous: Option<Parser>) -> Result<Parser, AppError> {
        let old = previous.unwrap_or_else(|| Parser {
            id: 0,
            name: String::new(),
            url_template: String::new(),
            headers_json: "{}".into(),
            flags: "*".into(),
            priority: 100,
            timeout_ms: 10000,
            enabled: true,
        });
        let config = Parser {
            id: old.id,
            name: self.name.trim().to_owned(),
            url_template: self.url_template.unwrap_or(old.url_template),
            headers_json: self.headers_json.unwrap_or(old.headers_json),
            flags: self.flags.unwrap_or(old.flags),
            priority: self.priority.unwrap_or(old.priority),
            timeout_ms: self.timeout_ms.unwrap_or(old.timeout_ms),
            enabled: self.enabled.unwrap_or(old.enabled),
        };
        validate_config(
            &config.name,
            &config.url_template,
            &config.headers_json,
            &config.flags,
            config.timeout_ms,
        )?;
        Ok(config)
    }
}

fn write_error(error: rusqlite::Error) -> AppError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        AppError(StatusCode::CONFLICT, "Parser URL already exists".into())
    } else {
        error.into()
    }
}

async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<ParserInput>,
) -> Result<(StatusCode, Json<Parser>), AppError> {
    require_admin(&user)?;
    let mut p = input.apply(None)?;
    let conn = state.conn()?;
    conn.execute("INSERT INTO parsers(name,url_template,headers_json,flags,priority,timeout_ms,enabled) VALUES(?,?,?,?,?,?,?)",
        params![p.name,p.url_template,p.headers_json,p.flags,p.priority,p.timeout_ms,p.enabled]).map_err(write_error)?;
    p.id = conn.last_insert_rowid();
    Ok((StatusCode::CREATED, Json(p)))
}

async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<ParserInput>,
) -> Result<Json<Parser>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let p = input.apply(Some(parser(&conn, id)?))?;
    conn.execute("UPDATE parsers SET name=?,url_template=?,headers_json=?,flags=?,priority=?,timeout_ms=?,enabled=? WHERE id=?",
        params![p.name,p.url_template,p.headers_json,p.flags,p.priority,p.timeout_ms,p.enabled,id]).map_err(write_error)?;
    state.playback_grants.clear();
    Ok(Json(p))
}

async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    if state
        .conn()?
        .execute("DELETE FROM parsers WHERE id=?", [id])?
        == 0
    {
        return Err(AppError::not_found("Parser does not exist"));
    }
    state.playback_grants.clear();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ResolveInput {
    url: String,
    #[serde(default)]
    flag: String,
}

#[derive(Serialize)]
struct PlaybackResult {
    url: String,
    playback_url: String,
    is_hls: bool,
}

fn grant(
    state: &Shared,
    user: &AuthUser,
    media: ResolvedMedia,
) -> Result<Json<PlaybackResult>, AppError> {
    let is_hls = config::checked_url(&media.url)?
        .path()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("m3u8"));
    let playback_url = playback::grant_url(state, user.id, &media.url, &media.headers)?;
    Ok(Json(PlaybackResult {
        url: playback_url.clone(),
        playback_url,
        is_hls,
    }))
}

async fn resolve_playback(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<ResolveInput>,
) -> Result<Json<PlaybackResult>, AppError> {
    let media = resolve_media(&state, &input.url, &input.flag).await?;
    grant(&state, &user, media)
}

async fn test_parser(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<ResolveInput>,
) -> Result<Json<PlaybackResult>, AppError> {
    require_admin(&user)?;
    let parser = parser(&*state.conn()?, id)?;
    let media = resolve::request(&parser, &config::checked_url(&input.url)?).await?;
    grant(&state, &user, media)
}
