mod probe;

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    network::{PlaylistChannel, http_url, merge_playlist, parse_playlist},
    now,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::task::JoinHandle;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS live_sources (
            id INTEGER PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL UNIQUE,
            enabled INTEGER NOT NULL DEFAULT 1, updated_at INTEGER,
            channel_count INTEGER NOT NULL DEFAULT 0, error TEXT
        );",
    )?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/live-sources", get(list).post(create))
        .route("/api/live-sources/{id}", put(update).delete(remove))
        .route("/api/live-sources/refresh", post(refresh))
        .route("/api/channels/probe", post(probe::run))
        .route("/api/channels/cleanup", post(probe::cleanup))
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SourceConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

impl SourceConfig {
    pub(crate) fn validate(self) -> Result<Self, AppError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err(AppError::bad_request(
                "Source name must contain 1–100 characters",
            ));
        }
        let raw = self.url.trim();
        if raw.len() > 4096 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(AppError::bad_request("Invalid live source URL"));
        }
        let mut url = http_url(raw)?;
        url.set_fragment(None);
        Ok(Self {
            name: name.to_owned(),
            url: url.into(),
            enabled: self.enabled,
        })
    }
}

#[derive(Serialize)]
struct Source {
    id: i64,
    #[serde(flatten)]
    config: SourceConfig,
    updated_at: Option<i64>,
    channel_count: i64,
    error: Option<String>,
}

fn row_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<Source> {
    Ok(Source {
        id: row.get(0)?,
        config: SourceConfig {
            name: row.get(1)?,
            url: row.get(2)?,
            enabled: row.get(3)?,
        },
        updated_at: row.get(4)?,
        channel_count: row.get(5)?,
        error: row.get(6)?,
    })
}

async fn list(State(state): State<Shared>, user: AuthUser) -> Result<Json<Vec<Source>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let mut stmt = conn.prepare(
        "SELECT id,name,url,enabled,updated_at,channel_count,error FROM live_sources ORDER BY id",
    )?;
    Ok(Json(
        stmt.query_map([], row_source)?.collect::<Result<_, _>>()?,
    ))
}

fn write_error(error: rusqlite::Error) -> AppError {
    if error
        .sqlite_error()
        .is_some_and(|e| e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
    {
        AppError(
            StatusCode::CONFLICT,
            "This live source URL is already configured".into(),
        )
    } else {
        error.into()
    }
}

async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    Json(config): Json<SourceConfig>,
) -> Result<(StatusCode, Json<Source>), AppError> {
    require_admin(&user)?;
    let config = config.validate()?;
    let conn = state.conn()?;
    let row = conn
        .query_row(
            "INSERT INTO live_sources(name,url,enabled) VALUES (?1,?2,?3)
         RETURNING id,name,url,enabled,updated_at,channel_count,error",
            params![config.name, config.url, config.enabled],
            row_source,
        )
        .map_err(write_error)?;
    Ok((StatusCode::CREATED, Json(row)))
}

async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(config): Json<SourceConfig>,
) -> Result<Json<Source>, AppError> {
    require_admin(&user)?;
    let config = config.validate()?;
    let conn = state.conn()?;
    let row = conn
        .query_row(
            "UPDATE live_sources SET name=?1,url=?2,enabled=?3,
         updated_at=CASE WHEN url=?2 THEN updated_at END,
         channel_count=CASE WHEN url=?2 THEN channel_count ELSE 0 END,
         error=CASE WHEN url=?2 THEN error END WHERE id=?4
         RETURNING id,name,url,enabled,updated_at,channel_count,error",
            params![config.name, config.url, config.enabled, id],
            row_source,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => AppError::not_found("Live source not found"),
            e => write_error(e),
        })?;
    Ok(Json(row))
}

async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    if state
        .conn()?
        .execute("DELETE FROM live_sources WHERE id=?1", [id])?
        == 0
    {
        return Err(AppError::not_found("Live source not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn fetch_playlist(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<PlaylistChannel>, String> {
    const MAX_BYTES: usize = 8 * 1024 * 1024;
    let mut response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| error.without_url().to_string())?;
    if !response.status().is_success() {
        return Err(format!("Live source returned HTTP {}", response.status()));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error.without_url().to_string())?
    {
        if chunk.len() > MAX_BYTES - bytes.len() {
            return Err("Live playlist exceeds 8 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "Live playlist must use UTF-8")?;
    parse_playlist(text).map_err(|error| error.1)
}

#[derive(Default, Serialize)]
struct RefreshReport {
    refreshed: usize,
    failed: usize,
}

async fn refresh_all(state: &Shared) -> Result<RefreshReport, AppError> {
    let sources = {
        let conn = state.conn()?;
        let mut stmt =
            conn.prepare("SELECT id,url FROM live_sources WHERE enabled=1 ORDER BY id")?;
        stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut report = RefreshReport::default();
    for (id, url) in sources {
        let playlist = fetch_playlist(&state.client, &url).await;
        let mut conn = state.conn()?;
        let mut tx = conn.transaction()?;
        // The source may have changed while the HTTP request was in flight.
        if tx.execute(
            "UPDATE live_sources SET updated_at=?1 WHERE id=?2 AND url=?3 AND enabled=1",
            params![now(), id, url],
        )? == 0
        {
            continue;
        }
        let merged = match playlist {
            Ok(channels) => {
                let savepoint = tx.savepoint()?;
                match merge_playlist(&savepoint, &channels) {
                    Ok(counts) => {
                        savepoint.commit()?;
                        Ok(counts)
                    }
                    Err(error) => Err(error.1),
                }
            }
            Err(error) => Err(error),
        };
        match merged {
            Ok(counts) => {
                tx.execute(
                    "UPDATE live_sources SET channel_count=?1,error=NULL WHERE id=?2",
                    params![counts.channels, id],
                )?;
                report.refreshed += 1;
            }
            Err(error) => {
                tx.execute(
                    "UPDATE live_sources SET error=?1 WHERE id=?2",
                    params![error, id],
                )?;
                report.failed += 1;
            }
        }
        tx.commit()?;
    }
    Ok(report)
}

async fn refresh(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<RefreshReport>, AppError> {
    require_admin(&user)?;
    Ok(Json(refresh_all(&state).await?))
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(12 * 60 * 60));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            match refresh_all(&state).await {
                Ok(report) if report.failed > 0 => tracing::warn!(
                    failed = report.failed,
                    "Live subscription refresh finished with errors"
                ),
                Err(error) => tracing::warn!(error = %error.1, "Live subscription refresh failed"),
                _ => {}
            }
        }
    })
}
