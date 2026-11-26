mod fallback;
mod recommend;

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use futures_util::{StreamExt, TryStreamExt, stream};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use url::Url;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const PROBE_CONCURRENCY: usize = 8;
const HEALTH_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const HEALTHY_PROBE_SECS: i64 = 15 * 60;
const FAILED_PROBE_SECS: i64 = 120;
const SOURCE_SELECT: &str = "SELECT id,name,url,enabled,adult,priority,searchable,custom_ua,
    custom_referer,custom_cookie,proxy_url,timeout_secs,last_latency_ms,last_probe_ok,last_probe_at,
    fail_streak,cooldown_until,health_score,
    1.0 * successful_probes / NULLIF(total_probes,0) FROM catalog_sources";

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalog_sources (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1)),
            adult INTEGER NOT NULL DEFAULT 0 CHECK(adult IN (0,1))
        );
        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY, value TEXT NOT NULL
        );",
    )?;
    crate::ensure_column(
        conn,
        "catalog_sources",
        "adult",
        "INTEGER NOT NULL DEFAULT 0 CHECK(adult IN (0,1))",
    )?;
    for (column, definition) in [
        ("priority", "INTEGER NOT NULL DEFAULT 100"),
        ("searchable", "INTEGER NOT NULL DEFAULT 1"),
        ("custom_ua", "TEXT NOT NULL DEFAULT ''"),
        ("custom_referer", "TEXT NOT NULL DEFAULT ''"),
        ("custom_cookie", "TEXT NOT NULL DEFAULT ''"),
        ("proxy_url", "TEXT NOT NULL DEFAULT ''"),
        ("timeout_secs", "INTEGER NOT NULL DEFAULT 12"),
        ("last_latency_ms", "INTEGER"),
        ("last_probe_ok", "INTEGER"),
        ("last_probe_at", "INTEGER"),
        ("fail_streak", "INTEGER NOT NULL DEFAULT 0"),
        ("cooldown_until", "INTEGER NOT NULL DEFAULT 0"),
        ("health_score", "INTEGER NOT NULL DEFAULT 100"),
        ("successful_probes", "INTEGER NOT NULL DEFAULT 0"),
        ("total_probes", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        crate::ensure_column(conn, "catalog_sources", column, definition)?;
    }
    conn.execute(
        "INSERT OR IGNORE INTO settings(key,value) VALUES ('adult_sources_enabled','0')",
        [],
    )?;
    // An older database retained only the last probe. Count that known observation once.
    conn.execute(
        "UPDATE catalog_sources SET total_probes=1,successful_probes=last_probe_ok,
         health_score=CASE WHEN last_probe_ok THEN 100-MIN(30,COALESCE(last_latency_ms,0)/250)
                           ELSE 50 END
         WHERE total_probes=0 AND last_probe_ok IS NOT NULL",
        [],
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalog_types (
            source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE CASCADE,
            type_id INTEGER NOT NULL,
            type_name TEXT NOT NULL,
            PRIMARY KEY(source_id, type_id)
        );
        CREATE INDEX IF NOT EXISTS catalog_types_name ON catalog_types(type_name);
        CREATE TRIGGER IF NOT EXISTS catalog_reset_endpoint_health AFTER UPDATE OF url ON catalog_sources
        WHEN OLD.url <> NEW.url BEGIN
            UPDATE catalog_sources SET health_score=100,successful_probes=0,total_probes=0,
                last_latency_ms=NULL,last_probe_at=NULL,last_probe_ok=NULL,fail_streak=0,cooldown_until=0
                WHERE id=NEW.id;
        END;",
    )?;
    fallback::init_cache(conn)
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/sources", get(list_sources).post(create_source))
        .route("/api/sources/probe", post(probe_sources))
        .route("/api/sources/reorder", post(reorder_sources))
        .route("/api/sources/batch", post(batch_sources))
        .route(
            "/api/sources/{id}",
            delete(delete_source).put(update_source),
        )
        .route(
            "/api/catalog/settings",
            get(settings_handler).put(save_settings_handler),
        )
        .route("/api/catalog/search", get(search_handler))
        .route("/api/catalog/suggest", get(suggest_handler))
        .route("/api/catalog/discover", get(discover_handler))
        .route("/api/catalog/recommend", get(recommend::handler))
        .route("/api/catalog/categories", get(categories_handler))
        .route("/api/catalog/fallback", get(fallback::handler))
        .route("/api/catalog/{source_id}/detail", get(detail_handler))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub enabled: bool,
    pub adult: bool,
    pub priority: i64,
    pub searchable: bool,
    custom_ua: String,
    custom_referer: String,
    custom_cookie: String,
    proxy_url: String,
    timeout_secs: u64,
    last_latency_ms: Option<i64>,
    last_probe_ok: Option<bool>,
    last_probe_at: Option<i64>,
    fail_streak: i64,
    cooldown_until: i64,
    health_score: i64,
    success_rate: Option<f64>,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

fn source_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Source> {
    Ok(Source {
        id: row.get(0)?,
        name: row.get(1)?,
        url: row.get(2)?,
        enabled: row.get(3)?,
        adult: row.get(4)?,
        priority: row.get(5)?,
        searchable: row.get(6)?,
        custom_ua: row.get(7)?,
        custom_referer: row.get(8)?,
        custom_cookie: row.get(9)?,
        proxy_url: row.get(10)?,
        timeout_secs: row.get(11)?,
        last_latency_ms: row.get(12)?,
        last_probe_ok: row.get(13)?,
        last_probe_at: row.get(14)?,
        fail_streak: row.get(15)?,
        cooldown_until: row.get(16)?,
        health_score: row.get(17)?,
        success_rate: row.get(18)?,
    })
}

pub fn sources(state: &Shared) -> Result<Vec<Source>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!("{SOURCE_SELECT} ORDER BY priority,id"))?;
    Ok(stmt.query_map([], source_row)?.collect::<Result<_, _>>()?)
}

fn source_by_id(conn: &Connection, id: i64) -> Result<Source, AppError> {
    conn.query_row(&format!("{SOURCE_SELECT} WHERE id=?"), [id], source_row)
        .optional()?
        .ok_or_else(|| AppError::not_found("Source not found"))
}

pub fn adult_sources_enabled(conn: &Connection) -> Result<bool, AppError> {
    Ok(conn
        .query_row(
            "SELECT value FROM settings WHERE key='adult_sources_enabled'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        == Some("1"))
}

pub fn active_sources(state: &Shared) -> Result<Vec<Source>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!(
        "{SOURCE_SELECT} WHERE enabled=1 AND searchable=1 AND (adult=0 OR ?1)
         ORDER BY health_score DESC,priority,id"
    ))?;
    Ok(stmt
        .query_map([adult_sources_enabled(&conn)?], source_row)?
        .collect::<Result<_, _>>()?)
}

fn ensure_source_visible(conn: &Connection, source: &Source) -> Result<(), AppError> {
    if !source.enabled || (source.adult && !adult_sources_enabled(conn)?) {
        return Err(AppError::not_found("Source not found or disabled"));
    }
    Ok(())
}

async fn list_sources(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Value>>, AppError> {
    if user.role == "admin" {
        return Ok(Json(
            sources(&state)?
                .into_iter()
                .map(|source| serde_json::json!(source))
                .collect(),
        ));
    }
    Ok(Json(active_sources(&state)?.into_iter().map(|source| serde_json::json!({
        "id": source.id, "name": source.name, "enabled": source.enabled, "adult": source.adult,
        "priority": source.priority, "searchable": source.searchable
    })).collect()))
}

#[derive(Debug, Serialize)]
struct CatalogSettings {
    adult_sources_enabled: bool,
}

#[derive(Deserialize)]
struct CatalogSettingsInput {
    adult_sources_enabled: bool,
}

async fn settings_handler(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<CatalogSettings>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(CatalogSettings {
        adult_sources_enabled: adult_sources_enabled(&conn)?,
    }))
}

async fn save_settings_handler(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<CatalogSettingsInput>,
) -> Result<Json<CatalogSettings>, AppError> {
    require_admin(&user)?;
    state.conn()?.execute(
        "INSERT INTO settings(key,value) VALUES('adult_sources_enabled',?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [if input.adult_sources_enabled {
            "1"
        } else {
            "0"
        }],
    )?;
    Ok(Json(CatalogSettings {
        adult_sources_enabled: input.adult_sources_enabled,
    }))
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct SourceInput {
    pub(crate) name: String,
    pub(crate) url: String,
    #[serde(default = "enabled_by_default")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) adult: bool,
    pub(crate) priority: Option<i64>,
    pub(crate) searchable: Option<bool>,
    pub(crate) custom_ua: Option<String>,
    pub(crate) custom_referer: Option<String>,
    pub(crate) custom_cookie: Option<String>,
    pub(crate) proxy_url: Option<String>,
    pub(crate) timeout_secs: Option<u64>,
}

fn enabled_by_default() -> bool {
    true
}

fn http_url(raw: &str) -> Result<Url, AppError> {
    if raw.len() > 4096 || raw.chars().any(char::is_control) {
        return Err(AppError::bad_request(
            "URL is too long or contains control characters",
        ));
    }
    let url =
        Url::parse(raw).map_err(|_| AppError::bad_request("Enter a valid HTTP or HTTPS URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::bad_request(
            "Only HTTP and HTTPS URLs without embedded credentials are supported",
        ));
    }
    Ok(url)
}

pub(crate) fn validate_source(input: &SourceInput) -> Result<(String, String), AppError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(AppError::bad_request(
            "Source name must contain 1–100 characters",
        ));
    }
    if input.timeout_secs.is_some_and(|n| !(3..=60).contains(&n)) {
        return Err(AppError::bad_request("Source timeout must be 3–60 seconds"));
    }
    for value in [
        &input.custom_ua,
        &input.custom_referer,
        &input.custom_cookie,
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > 8192
            || value.chars().any(char::is_control)
            || reqwest::header::HeaderValue::from_str(value).is_err()
        {
            return Err(AppError::bad_request("Invalid source header"));
        }
    }
    if let Some(raw) = input.proxy_url.as_deref().filter(|value| !value.is_empty()) {
        let valid = raw.len() <= 4096
            && !raw.chars().any(char::is_control)
            && Url::parse(raw).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
            });
        if !valid {
            return Err(AppError::bad_request("Invalid HTTP proxy URL"));
        }
    }
    let mut url = http_url(input.url.trim())?;
    url.set_fragment(None);
    Ok((name.to_owned(), url.to_string()))
}

fn source_write_error(error: rusqlite::Error) -> AppError {
    match error {
        rusqlite::Error::SqliteFailure(code, _)
            if code.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            AppError(
                StatusCode::CONFLICT,
                "This source URL is already configured".into(),
            )
        }
        error => error.into(),
    }
}

async fn create_source(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<SourceInput>,
) -> Result<(StatusCode, Json<Source>), AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let id = insert_source(&conn, &input)?;
    Ok((StatusCode::CREATED, Json(source_by_id(&conn, id)?)))
}

fn insert_source(conn: &Connection, input: &SourceInput) -> Result<i64, AppError> {
    let (name, url) = validate_source(input)?;
    conn.execute(
        "INSERT INTO catalog_sources(name,url,enabled,adult,priority,searchable,custom_ua,
        custom_referer,custom_cookie,proxy_url,timeout_secs) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        params![
            name,
            url,
            input.enabled,
            input.adult,
            input.priority.unwrap_or(100),
            input.searchable.unwrap_or(true),
            input.custom_ua.as_deref().unwrap_or_default(),
            input.custom_referer.as_deref().unwrap_or_default(),
            input.custom_cookie.as_deref().unwrap_or_default(),
            input.proxy_url.as_deref().unwrap_or_default(),
            input.timeout_secs.unwrap_or(12)
        ],
    )
    .map_err(source_write_error)?;
    Ok(conn.last_insert_rowid())
}

async fn update_source(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<SourceInput>,
) -> Result<Json<Source>, AppError> {
    require_admin(&user)?;
    let (name, url) = validate_source(&input)?;
    let conn = state.conn()?;
    if conn.execute(
        "UPDATE catalog_sources SET name=?1,url=?2,enabled=?3,adult=?4,priority=COALESCE(?5,priority),
        searchable=COALESCE(?6,searchable),custom_ua=COALESCE(?7,custom_ua),custom_referer=COALESCE(?8,custom_referer),
        custom_cookie=COALESCE(?9,custom_cookie),proxy_url=COALESCE(?10,proxy_url),timeout_secs=COALESCE(?11,timeout_secs)
        WHERE id=?12",
        params![name,url,input.enabled,input.adult,input.priority,input.searchable,input.custom_ua,
            input.custom_referer,input.custom_cookie,input.proxy_url,input.timeout_secs,id],
    ).map_err(source_write_error)? == 0 {
        return Err(AppError::not_found("Source not found"));
    }
    Ok(Json(source_by_id(&conn, id)?))
}

#[derive(Deserialize)]
struct BatchOrder {
    ids: Vec<i64>,
}

async fn reorder_sources(
    State(state): State<Shared>,
    user: AuthUser,
    Json(order): Json<BatchOrder>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let count: usize = tx.query_row("SELECT COUNT(*) FROM catalog_sources", [], |r| r.get(0))?;
    if order.ids.len() != count || order.ids.iter().collect::<BTreeSet<_>>().len() != count {
        return Err(AppError::bad_request("Include every source exactly once"));
    }
    for (priority, id) in order.ids.into_iter().enumerate() {
        if tx.execute(
            "UPDATE catalog_sources SET priority=? WHERE id=?",
            params![priority, id],
        )? == 0
        {
            return Err(AppError::bad_request("Unknown source in reorder request"));
        }
    }
    tx.commit()?;
    Ok(Json(serde_json::json!({"ok":true})))
}

async fn batch_sources(
    State(state): State<Shared>,
    user: AuthUser,
    Json(rows): Json<Vec<Value>>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let total = rows.len();
    let mut imported = 0;
    for row in rows {
        let Ok(input) = serde_json::from_value(row) else {
            continue;
        };
        match insert_source(&tx, &input) {
            Ok(_) => imported += 1,
            Err(AppError(StatusCode::BAD_REQUEST | StatusCode::CONFLICT, _)) => (),
            Err(error) => return Err(error),
        }
    }
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"imported":imported,"skipped":total-imported}),
    ))
}

#[derive(Deserialize)]
struct ProbeInput {
    source_id: Option<i64>,
}

#[derive(Serialize)]
struct ProbeResult {
    source_id: i64,
    ok: bool,
    latency_ms: i64,
    error: Option<String>,
}

async fn probe_source(state: &Shared, source: Source) -> Result<ProbeResult, AppError> {
    let start = Instant::now();
    let result = request_cms(state, &source, &[("ac", "list")]).await;
    let latency_ms = start.elapsed().as_millis() as i64;
    let ok = result.is_ok();
    record_health(state, &source, ok, Some(latency_ms))?;
    Ok(ProbeResult {
        source_id: source.id,
        ok,
        latency_ms,
        error: result.err(),
    })
}

async fn inspect_due_sources(state: &Shared) -> Result<(), AppError> {
    let due = {
        let conn = state.conn()?;
        let mut stmt = conn.prepare(&format!(
            "{SOURCE_SELECT} WHERE enabled=1 AND (adult=0 OR ?1) AND
             (last_probe_at IS NULL OR last_probe_at <= CASE WHEN fail_streak>0 THEN ?2 ELSE ?3 END)
             ORDER BY last_probe_at,id"
        ))?;
        let now = crate::now();
        stmt.query_map(
            params![
                adult_sources_enabled(&conn)?,
                now - FAILED_PROBE_SECS,
                now - HEALTHY_PROBE_SECS
            ],
            source_row,
        )?
        .collect::<Result<Vec<_>, _>>()?
    };
    let mut probes = stream::iter(due.into_iter().map(|source| probe_source(state, source)))
        .buffer_unordered(PROBE_CONCURRENCY);
    while let Some(result) = probes.next().await {
        result?;
    }
    Ok(())
}

pub fn start_health_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(error) = inspect_due_sources(&state).await {
                tracing::error!(message = %error.1, "Could not inspect catalog sources");
            }
            tokio::time::sleep(HEALTH_SCAN_INTERVAL).await;
        }
    })
}

async fn probe_sources(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<ProbeInput>,
) -> Result<Json<Vec<ProbeResult>>, AppError> {
    require_admin(&user)?;
    let selected = if let Some(id) = input.source_id {
        let conn = state.conn()?;
        vec![source_by_id(&conn, id)?]
    } else {
        sources(&state)?.into_iter().filter(|s| s.enabled).collect()
    };
    let probes = selected
        .into_iter()
        .map(|source| probe_source(&state, source));
    Ok(Json(
        stream::iter(probes)
            .buffered(PROBE_CONCURRENCY)
            .try_collect()
            .await?,
    ))
}

async fn delete_source(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    if state
        .conn()?
        .execute("DELETE FROM catalog_sources WHERE id=?", [id])
        .map_err(|error| match error {
            rusqlite::Error::SqliteFailure(code, _)
                if code.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                AppError(
                    StatusCode::CONFLICT,
                    "Cancel subscriptions using this source before removing it".into(),
                )
            }
            error => error.into(),
        })?
        == 0
    {
        return Err(AppError::not_found("Source not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogSummary {
    pub id: String,
    pub source_id: i64,
    pub source_name: String,
    pub health_score: i64,
    pub source_priority: i64,
    pub title: String,
    pub cover: String,
    pub year: String,
    pub remark: String,
    pub kind: String,
    pub lang: String,
    #[serde(default)]
    pub area: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SourceError {
    pub source_id: i64,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<CatalogSummary>,
    pub errors: Vec<SourceError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CategoriesResponse {
    pub categories: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DiscoverResponse {
    pub results: Vec<CatalogSummary>,
    pub errors: Vec<SourceError>,
    pub source_id: Option<i64>,
    pub page: u32,
    pub page_count: u64,
    pub total: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    pub name: String,
    pub url: String,
    pub direct: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayGroup {
    pub name: String,
    pub episodes: Vec<Episode>,
    /// Every episode URL ends in a browser/TV-playable media extension rather
    /// than a CMS share/parser page.
    pub direct: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogDetail {
    pub id: String,
    pub source_id: i64,
    pub source_name: String,
    pub title: String,
    pub cover: String,
    pub description: String,
    pub year: String,
    pub remark: String,
    pub groups: Vec<PlayGroup>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct DetailQuery {
    id: String,
}

#[derive(Deserialize)]
struct DiscoverQuery {
    source_id: Option<i64>,
    #[serde(default = "first_page")]
    page: u32,
    kind: Option<String>,
    category: Option<String>,
    year: Option<String>,
}

#[derive(Deserialize)]
struct CategoriesQuery {
    refresh: Option<bool>,
    kind: Option<String>,
}

fn first_page() -> u32 {
    1
}

fn validate_text<'a>(raw: &'a str, label: &str, max: usize) -> Result<&'a str, AppError> {
    let text = raw.trim();
    if text.is_empty() || text.chars().count() > max || text.chars().any(char::is_control) {
        return Err(AppError::bad_request(format!(
            "{label} must contain 1–{max} characters"
        )));
    }
    Ok(text)
}

async fn search_handler(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<SearchQuery>,
) -> Result<Json<SearchResponse>, AppError> {
    search(&state, &query.q).await.map(Json)
}

#[derive(Serialize)]
struct Suggestion {
    title: String,
    year: String,
    kind: &'static str,
}

async fn suggest_handler(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<Suggestion>>, AppError> {
    let query = validate_text(&query.q, "Search query", 100)?;
    let needle = query.to_lowercase();
    let state = &state;
    let mut pages = stream::iter(active_sources(state)?.into_iter().map(|source| async move {
        fetch_cms_value(state, &source, &[("ac", "list"), ("wd", query)]).await
    }))
    .buffer_unordered(6);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(2500);
    let mut suggestions = Vec::new();
    let mut seen = BTreeSet::new();
    while let Ok(Some(response)) = tokio::time::timeout_at(deadline, pages.next()).await {
        let Ok(page) = response else { continue };
        let Some(items) = page.get("list").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(title) = item.get("vod_name").and_then(Value::as_str) else {
                continue;
            };
            let title = title.trim();
            let normalized = title.to_lowercase();
            if normalized.contains(&needle) && seen.insert(normalized) {
                suggestions.push(Suggestion {
                    title: title.to_owned(),
                    year: text(item, "vod_year"),
                    kind: media_kind(
                        item.get("type_name")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                });
                if suggestions.len() == 8 {
                    return Ok(Json(suggestions));
                }
            }
        }
    }
    Ok(Json(suggestions))
}

pub async fn search(state: &Shared, query: &str) -> Result<SearchResponse, AppError> {
    let query = validate_text(query, "Search query", 100)?;
    let active = active_sources(state)?;
    let mut responses = stream::iter(active.into_iter().map(|source| async move {
        let result = fetch_items(state, &source, &[("ac", "detail"), ("wd", query)]).await;
        (source, result)
    }))
    .buffered(6);
    let mut response = SearchResponse {
        results: Vec::new(),
        errors: Vec::new(),
    };
    while let Some((source, result)) = responses.next().await {
        if let Ok(page) = &result
            && let Ok(conn) = state.conn()
        {
            let _ = remember_types(&conn, source.id, &page.items);
        }
        match result {
            Ok(page) => {
                let start = response.results.len();
                response.results.extend(
                    page.items
                        .iter()
                        .filter_map(|item| summary(&source, item))
                        .take(1000),
                );
                response.results[start..].sort_by(|a, b| (&a.title, &a.id).cmp(&(&b.title, &b.id)));
            }
            Err(message) => response.errors.push(SourceError {
                source_id: source.id,
                message,
            }),
        }
    }
    Ok(response)
}

fn remember_types(conn: &Connection, source_id: i64, items: &[Value]) -> Result<(), AppError> {
    let mut statement = conn.prepare_cached(
        "INSERT OR REPLACE INTO catalog_types(source_id,type_id,type_name) VALUES(?1,?2,?3)",
    )?;
    for item in items {
        let Some(type_id) = item.get("type_id").and_then(Value::as_u64) else {
            continue;
        };
        let name = text(item, "type_name");
        if name.trim().is_empty() {
            continue;
        }
        statement.execute(params![source_id, type_id as i64, name.trim()])?;
    }
    Ok(())
}

fn remember_classes(conn: &Connection, source_id: i64, classes: &[Value]) -> Result<(), AppError> {
    let mut statement = conn.prepare_cached(
        "INSERT OR REPLACE INTO catalog_types(source_id,type_id,type_name) VALUES(?1,?2,?3)",
    )?;
    for class in classes {
        let Some(type_id) = unsigned(class.get("type_id")) else {
            continue;
        };
        let name = text(class, "type_name");
        if name.trim().is_empty() {
            continue;
        }
        statement.execute(params![source_id, type_id as i64, name.trim()])?;
    }
    Ok(())
}
async fn refresh_categories(state: &Shared) -> Result<(), AppError> {
    let active: Vec<Source> = active_sources(state)?;
    let class_jobs = active.iter().cloned().map(|source| async move {
        let result = fetch_cms_value(state, &source, &[("ac", "list")]).await;
        (source.id, result)
    });
    let classes = stream::iter(class_jobs)
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
    let mut jobs = Vec::new();
    for source in &active {
        for page in 1..=4 {
            jobs.push((source.clone(), page));
        }
    }
    let listings = stream::iter(jobs.into_iter().map(|(source, page)| async move {
        let page = page.to_string();
        let result = fetch_items(state, &source, &[("ac", "detail"), ("pg", &page)]).await;
        (source.id, result)
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    let conn = state.conn()?;
    for (source_id, result) in classes {
        if let Ok(value) = result
            && let Some(classes) = value.get("class").and_then(Value::as_array)
        {
            let _ = remember_classes(&conn, source_id, classes);
        }
    }
    for (source_id, result) in listings {
        if let Ok(listing) = result
            && remember_types(&conn, source_id, &listing.items).is_err()
        {
            continue;
        }
    }
    Ok(())
}

async fn categories_handler(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<CategoriesQuery>,
) -> Result<Json<CategoriesResponse>, AppError> {
    let existing = state
        .conn()?
        .query_row("SELECT COUNT(*) FROM catalog_types", [], |row| {
            row.get::<_, i64>(0)
        })?;
    if query.refresh.unwrap_or(false) || existing == 0 {
        refresh_categories(&state).await?;
    }
    let conn = state.conn()?;
    let kind = query.kind.as_deref().and_then(validate_filter);
    if let Some(value) = kind
        && !matches!(
            value,
            "movie" | "series" | "variety" | "anime" | "child" | "other"
        )
    {
        return Err(AppError::bad_request("Unknown media kind"));
    }
    let mut statement = conn.prepare(
        "SELECT DISTINCT type_name FROM catalog_types JOIN catalog_sources ON catalog_sources.id=catalog_types.source_id
         WHERE enabled=1 AND searchable=1 AND (adult=0 OR ?1) ORDER BY type_name",
    )?;
    let rows = statement.query_map([adult_sources_enabled(&conn)?], |row| {
        row.get::<_, String>(0)
    })?;
    let mut categories = BTreeSet::new();
    for row in rows {
        let name = row?;
        if kind.is_none_or(|value| media_kind(&name) == value) {
            categories.insert(name);
        }
    }
    Ok(Json(CategoriesResponse {
        categories: categories.into_iter().collect(),
    }))
}

async fn discover_handler(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<DiscoverQuery>,
) -> Result<Json<DiscoverResponse>, AppError> {
    let kind = query.kind.as_deref().and_then(validate_filter);
    let category = query.category.as_deref().and_then(validate_filter);
    let year = query.year.as_deref().and_then(validate_filter);
    discover(&state, query.source_id, query.page, kind, category, year)
        .await
        .map(Json)
}

fn validate_filter(raw: &str) -> Option<&str> {
    let value = raw.trim();
    (!value.is_empty()).then_some(value)
}

pub async fn discover(
    state: &Shared,
    source_id: Option<i64>,
    page: u32,
    kind: Option<&str>,
    category: Option<&str>,
    year: Option<&str>,
) -> Result<DiscoverResponse, AppError> {
    if !(1..=10_000).contains(&page) {
        return Err(AppError::bad_request("Page must be between 1 and 10000"));
    }
    if let Some(value) = kind
        && !matches!(
            value,
            "movie" | "series" | "variety" | "anime" | "child" | "other"
        )
    {
        return Err(AppError::bad_request("Unknown media kind"));
    }
    if let Some(value) = category {
        validate_text(value, "Category", 100)?;
    }
    if let Some(value) = year {
        validate_text(value, "Year", 4)?;
        let parsed: u16 = value
            .parse()
            .map_err(|_| AppError::bad_request("Year must be four digits"))?;
        if !(1900..=2100).contains(&parsed) {
            return Err(AppError::bad_request("Year must be between 1900 and 2100"));
        }
    }
    let mut type_ids: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    if let Some(category) = category {
        let conn = state.conn()?;
        let mut statement = conn.prepare(
            "SELECT source_id,type_id,type_name FROM catalog_types WHERE type_name=?1 ORDER BY source_id,type_id",
        )?;
        let rows = statement.query_map([category], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (source_id, type_id, type_name) = row?;
            if kind.is_some_and(|value| media_kind(&type_name) != value) {
                continue;
            }
            let ids = type_ids.entry(source_id).or_default();
            if !ids.contains(&type_id) {
                ids.push(type_id);
            }
        }
    } else if let Some(kind) = kind
        && kind != "other"
    {
        let conn = state.conn()?;
        let mut statement = conn.prepare(
            "SELECT source_id,type_id,type_name FROM catalog_types ORDER BY source_id,type_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (source_id, type_id, type_name) = row?;
            if media_kind(&type_name) != kind {
                continue;
            }
            let ids = type_ids.entry(source_id).or_default();
            if !ids.contains(&type_id) {
                ids.push(type_id);
            }
        }
    }
    let require_type = category.is_some() || kind.is_some_and(|kind| kind != "other");
    let active: Vec<Source> = active_sources(state)?
        .into_iter()
        .filter(|source| source_id.is_none_or(|id| source.id == id))
        .filter(|source| {
            !require_type || type_ids.get(&source.id).is_some_and(|ids| !ids.is_empty())
        })
        .collect();
    if source_id.is_some() && active.is_empty() {
        return Err(AppError::not_found("Source not found or disabled"));
    }
    let page_string = page.to_string();
    let mut jobs = Vec::new();
    for source in active {
        match type_ids.get(&source.id) {
            Some(ids) if !ids.is_empty() => {
                for type_id in ids {
                    jobs.push((source.clone(), Some(type_id.to_string())));
                }
            }
            _ => jobs.push((source, None)),
        }
    }
    let mut responses = stream::iter(jobs.into_iter().map(|(source, type_string)| {
        let page_string = page_string.clone();
        async move {
            let mut query = vec![("ac", "detail"), ("pg", page_string.as_str())];
            if let Some(type_id) = type_string.as_deref() {
                query.push(("t", type_id));
            }
            if let Some(year) = year {
                query.push(("year", year));
            }
            let result = fetch_items(state, &source, &query).await;
            (source, result)
        }
    }))
    .buffered(32);
    let mut seen = BTreeSet::new();
    let mut result = DiscoverResponse {
        results: Vec::new(),
        errors: Vec::new(),
        source_id,
        page,
        page_count: 0,
        total: 0,
    };
    while let Some((source, fetch)) = responses.next().await {
        if let Ok(listing) = &fetch
            && let Ok(conn) = state.conn()
        {
            let _ = remember_types(&conn, source.id, &listing.items);
        }
        match fetch {
            Ok(listing) => {
                for item in listing
                    .items
                    .iter()
                    .filter(|item| {
                        kind.is_none_or(|kind| media_kind(&text(item, "type_name")) == kind)
                    })
                    .filter_map(|item| summary(&source, item))
                {
                    if seen.insert((item.source_id, item.id.clone())) {
                        result.results.push(item);
                    }
                }
                result.page_count = result.page_count.max(listing.page_count);
                result.total = result.total.saturating_add(listing.total);
            }
            Err(message) => {
                if !result
                    .errors
                    .iter()
                    .any(|error| error.source_id == source.id)
                {
                    result.errors.push(SourceError {
                        source_id: source.id,
                        message,
                    });
                }
            }
        }
    }
    Ok(result)
}

async fn detail_handler(
    State(state): State<Shared>,
    _user: AuthUser,
    Path(source_id): Path<i64>,
    Query(query): Query<DetailQuery>,
) -> Result<Json<CatalogDetail>, AppError> {
    let id = validate_text(&query.id, "Media ID", 256)?;
    let (source, slot) = fallback::open_detail(&state, source_id, id)?;
    let item = detail_item(&state, &source, id).await?;
    let detail = catalog_detail(source, &item)?;
    fallback::remember_detail(&state, slot, &item)?;
    Ok(Json(detail))
}

pub async fn detail(state: &Shared, source_id: i64, id: &str) -> Result<CatalogDetail, AppError> {
    let id = validate_text(id, "Media ID", 256)?;
    let source = visible_source(state, source_id)?;
    let item = detail_item(state, &source, id).await?;
    catalog_detail(source, &item)
}

fn catalog_detail(source: Source, item: &Value) -> Result<CatalogDetail, AppError> {
    let summary = summary(&source, item).ok_or_else(|| {
        AppError(
            StatusCode::BAD_GATEWAY,
            "Source returned incomplete media metadata".into(),
        )
    })?;
    Ok(CatalogDetail {
        id: summary.id,
        source_id: source.id,
        source_name: source.name,
        title: summary.title,
        cover: summary.cover,
        description: text(item, "vod_content"),
        year: summary.year,
        remark: summary.remark,
        groups: play_groups(
            item["vod_play_from"].as_str().unwrap_or_default(),
            item["vod_play_url"].as_str().unwrap_or_default(),
        ),
    })
}

async fn detail_item(state: &Shared, source: &Source, id: &str) -> Result<Value, AppError> {
    let page = fetch_items(state, source, &[("ac", "detail"), ("ids", id)])
        .await
        .map_err(|message| AppError(StatusCode::BAD_GATEWAY, message))?;
    page.items
        .into_iter()
        .find(|item| text(item, "vod_id") == id)
        .ok_or_else(|| AppError::not_found("Media not found at this source"))
}

fn text(item: &Value, field: &str) -> String {
    match item.get(field) {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Number(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn media_kind(type_name: &str) -> &'static str {
    let name = type_name.trim();
    if name.contains("儿童")
        || name.contains("少儿")
        || name.contains("亲子")
        || name.contains("宝宝")
        || name.contains("幼儿")
        || name.contains("早教")
        || name.contains("益智")
        || name.contains("童话")
        || name.contains("儿歌")
    {
        "child"
    } else if name.contains("解说")
        || name.contains("纪录片")
        || name.contains("记录片")
        || name.contains("预告片")
    {
        "other"
    } else if name.contains("电影") {
        "movie"
    } else if name.contains("动漫") || name.contains("动画") || name.contains("漫剧") {
        "anime"
    } else if name.ends_with('片') {
        "movie"
    } else if name.contains("综艺") || name.contains("晚会") || name.contains("脱口秀") {
        "variety"
    } else if name.contains('剧') {
        "series"
    } else {
        "other"
    }
}

fn summary(source: &Source, item: &Value) -> Option<CatalogSummary> {
    let id = text(item, "vod_id");
    let title = text(item, "vod_name");
    if id.is_empty() || title.trim().is_empty() {
        return None;
    }
    let cover = text(item, "vod_pic");
    let cover = Url::parse(&source.url)
        .ok()
        .and_then(|base| base.join(cover.trim()).ok())
        .filter(|url| !cover.trim().is_empty() && http_url(url.as_str()).is_ok())
        .map(|url| url.to_string())
        .unwrap_or_default();
    Some(CatalogSummary {
        id,
        source_id: source.id,
        source_name: source.name.clone(),
        health_score: source.health_score,
        source_priority: source.priority,
        title,
        cover,
        year: text(item, "vod_year"),
        remark: text(item, "vod_remarks"),
        kind: media_kind(&text(item, "type_name")).to_owned(),
        lang: text(item, "vod_lang"),
        area: text(item, "vod_area"),
    })
}

fn play_groups(names: &str, playlists: &str) -> Vec<PlayGroup> {
    let names: Vec<_> = names.split("$$$").collect();
    playlists
        .split("$$$")
        .enumerate()
        .filter_map(|(index, playlist)| {
            let episodes = playlist
                .split('#')
                .filter_map(|episode| {
                    let episode = episode.trim();
                    let (name, raw_url) = episode.split_once('$').unwrap_or(("", episode));
                    let url = http_url(raw_url.trim()).ok()?;
                    Some(Episode {
                        name: if name.trim().is_empty() {
                            "播放".to_string()
                        } else {
                            name.trim().to_string()
                        },
                        direct: direct_media_path(url.path()),
                        url: url.into(),
                    })
                })
                .collect::<Vec<_>>();
            if episodes.is_empty() {
                return None;
            }
            let name = names.get(index).map(|value| value.trim()).unwrap_or("");
            Some(PlayGroup {
                name: if name.is_empty() {
                    format!("线路 {}", index + 1)
                } else {
                    name.to_string()
                },
                direct: episodes.iter().all(|episode| episode.direct),
                episodes,
            })
        })
        .collect()
}

/// Known media containers that browsers, TV clients, or the download worker
/// can fetch directly. CMS share pages typically need a configured parser.
pub fn is_direct_media_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| direct_media_path(url.path()))
}

fn direct_media_path(path: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "m3u8", "mp4", "m4v", "webm", "mkv", "mov", "ts", "flv", "avi", "wmv",
    ];
    path.rsplit_once('.').is_some_and(|(_, extension)| {
        EXTENSIONS
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    })
}

#[derive(Debug)]
struct CmsPage {
    items: Vec<Value>,
    page_count: u64,
    total: u64,
}

fn unsigned(value: Option<&Value>) -> Option<u64> {
    value.and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

async fn fetch_items(
    state: &Shared,
    source: &Source,
    query: &[(&str, &str)],
) -> Result<CmsPage, String> {
    let value = fetch_cms_value(state, source, query).await?;
    let items = value
        .get("list")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "Source JSON does not contain a CMS list".to_owned())?;
    Ok(CmsPage {
        page_count: unsigned(value.get("pagecount")).unwrap_or(u64::from(!items.is_empty())),
        total: unsigned(value.get("total")).unwrap_or(items.len() as u64),
        items,
    })
}

pub async fn cms_query(
    state: &Shared,
    source_id: i64,
    parameters: &[(&str, &str)],
) -> Result<Value, AppError> {
    for (key, value) in parameters {
        if !["ac", "t", "wd", "ids", "pg"].contains(key) {
            return Err(AppError::bad_request("Unsupported CMS query parameter"));
        }
        validate_text(value, "CMS query value", 256)?;
        if *key == "ac" && !["list", "detail"].contains(value) {
            return Err(AppError::bad_request("Unsupported CMS action"));
        }
        if *key == "pg"
            && !value
                .parse::<u32>()
                .is_ok_and(|page| (1..=10_000).contains(&page))
        {
            return Err(AppError::bad_request("Page must be between 1 and 10000"));
        }
    }
    let source = visible_source(state, source_id)?;
    if !source.searchable
        && (parameters.iter().any(|(key, _)| *key == "wd")
            || !parameters.iter().any(|(key, _)| *key == "ids"))
    {
        return Err(AppError::not_found("Source search is disabled"));
    }
    fetch_cms_value(state, &source, parameters)
        .await
        .map_err(|message| AppError(StatusCode::BAD_GATEWAY, message))
}

fn visible_source(state: &Shared, id: i64) -> Result<Source, AppError> {
    let conn = state.conn()?;
    let source = source_by_id(&conn, id)?;
    ensure_source_visible(&conn, &source)?;
    Ok(source)
}

fn record_health(
    state: &Shared,
    source: &Source,
    ok: bool,
    probe_latency: Option<i64>,
) -> Result<(), AppError> {
    // Give the latest observation half the score. A failure halves the score immediately,
    // while fast successes can restore it even after a long outage.
    state.conn()?.execute(
        "UPDATE catalog_sources SET fail_streak=CASE WHEN ?1 THEN 0 ELSE fail_streak+1 END,
        cooldown_until=CASE WHEN ?1 THEN 0 WHEN fail_streak>=2 THEN ?2+120 ELSE 0 END,
        last_latency_ms=COALESCE(?3,last_latency_ms),
        last_probe_at=CASE WHEN ?3 IS NULL THEN last_probe_at ELSE ?2 END,
        last_probe_ok=CASE WHEN ?3 IS NULL THEN last_probe_ok ELSE ?1 END,
        total_probes=total_probes+(?3 IS NOT NULL),
        successful_probes=successful_probes+(?1 AND ?3 IS NOT NULL),
        health_score=CASE WHEN ?1 THEN
          (health_score + 100 - MIN(30,COALESCE(?3,last_latency_ms,0)/250) + 1)/2
          ELSE health_score/2 END
        WHERE id=?4 AND url=?5",
        params![ok, crate::now(), probe_latency, source.id, source.url],
    )?;
    Ok(())
}

async fn fetch_cms_value(
    state: &Shared,
    source: &Source,
    query: &[(&str, &str)],
) -> Result<Value, String> {
    let available: bool = state.conn().map_err(|_| "Could not open source database")?.query_row(
        "SELECT EXISTS(SELECT 1 FROM catalog_sources WHERE id=? AND url=? AND cooldown_until<=?)",
        params![source.id,source.url,crate::now()], |row| row.get(0),
    ).map_err(|_| "Could not read source health")?;
    if !available {
        return Err("Source is cooling down or unavailable".to_owned());
    }
    let result = request_cms(state, source, query).await;
    record_health(state, source, result.is_ok(), None)
        .map_err(|_| "Could not update source health")?;
    result
}

async fn request_cms(
    state: &Shared,
    source: &Source,
    query: &[(&str, &str)],
) -> Result<Value, String> {
    let mut url = http_url(&source.url).map_err(|_| "Source URL is invalid".to_owned())?;
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !["ac", "wd", "ids", "pg", "t"].contains(&key.as_ref()))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.extend(
        query
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    url.query_pairs_mut().clear().extend_pairs(pairs);
    let timeout = Duration::from_secs(source.timeout_secs.clamp(3, 60));
    let fetch = async {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in [
            (reqwest::header::USER_AGENT, &source.custom_ua),
            (reqwest::header::REFERER, &source.custom_referer),
            (reqwest::header::COOKIE, &source.custom_cookie),
        ] {
            if !value.is_empty() {
                let mut value = reqwest::header::HeaderValue::from_str(value)
                    .map_err(|_| "Invalid source header")?;
                value.set_sensitive(true);
                headers.insert(name, value);
            }
        }
        let custom_client;
        let client = if headers.is_empty() && source.proxy_url.is_empty() {
            &state.client
        } else {
            // Configured headers belong only to the administrator-selected origin.
            let mut builder = reqwest::Client::builder().referer(false).redirect(
                reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() < 5
                        && attempt
                            .previous()
                            .first()
                            .is_some_and(|url| url.origin() == attempt.url().origin())
                    {
                        attempt.follow()
                    } else {
                        attempt.stop()
                    }
                }),
            );
            if !source.proxy_url.is_empty() {
                builder = builder.proxy(
                    reqwest::Proxy::all(&source.proxy_url).map_err(|_| "Invalid source proxy")?,
                );
            }
            custom_client = builder
                .build()
                .map_err(|_| "Could not configure source client")?;
            &custom_client
        };
        let response = client
            .get(url)
            .headers(headers)
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| "Source request failed".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "Source returned HTTP {}",
                response.status().as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err("Source response exceeds 8 MiB".to_owned());
        }
        let mut bytes = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| "Could not read source response".to_owned())?;
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err("Source response exceeds 8 MiB".to_owned());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| "Source did not return valid CMS JSON".to_owned())?;
        if value.get("code").is_some_and(|code| {
            code.as_i64().is_some_and(|code| code != 1)
                || code.as_str().is_some_and(|code| code != "1")
        }) {
            return Err("Source reported an unsuccessful CMS response".to_owned());
        }
        let categories_only =
            query.contains(&("ac", "list")) && value.get("class").is_some_and(Value::is_array);
        if !value.get("list").is_some_and(Value::is_array) && !categories_only {
            return Err("Source JSON does not contain the requested CMS list".to_owned());
        }
        Ok(value)
    };
    tokio::time::timeout(timeout, fetch)
        .await
        .map_err(|_| format!("Source timed out after {} seconds", timeout.as_secs()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use axum::routing::get;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn state() -> (TempDir, Shared) {
        let temp = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            playback_grants: Default::default(),
            download_runtime: Default::default(),
            media_dir: temp.path().to_path_buf(),
            download_dir: temp.path().to_path_buf(),
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        (temp, state)
    }

    fn admin() -> AuthUser {
        AuthUser {
            id: 1,
            username: "admin".into(),
            role: "admin".into(),
        }
    }
    #[test]
    fn media_kind_follows_cms_type_names() {
        assert_eq!(media_kind("国产剧"), "series");
        assert_eq!(media_kind("爽文短剧"), "series");
        assert_eq!(media_kind("剧情片"), "movie");
        assert_eq!(media_kind("动漫电影"), "movie");
        assert_eq!(media_kind("国产动漫"), "anime");
        assert_eq!(media_kind("动画片"), "anime");
        assert_eq!(media_kind("儿童儿歌"), "child");
        assert_eq!(media_kind("少儿动画"), "child");
        assert_eq!(media_kind("儿童电影"), "child");
        assert_eq!(media_kind("纪录片"), "other");
        assert_eq!(media_kind("电影解说"), "other");
        assert_eq!(media_kind("大陆综艺"), "variety");
        assert_eq!(media_kind("篮球"), "other");
        assert_eq!(media_kind(""), "other");
    }

    async fn fixture() -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/cms", get(|Query(query): Query<std::collections::HashMap<String,String>>| async move {
                assert_eq!(query.get("key").map(String::as_str), Some("retained"));
                assert_eq!(query.get("ac").map(String::as_str), Some("detail"));
                if let Some(word) = query.get("wd") {
                    assert_eq!(word, "中文 & A+B");
                }
                let type_id = query.get("t").map(String::as_str).unwrap_or("13");
                let (vod_id, title, type_name) = match type_id {
                    "5" => (51, "电影甲", "动作片"),
                    "6" => (61, "电影乙", "剧情片"),
                    "13" => (42, "故事 <script>", "国产剧"),
                    other => panic!("unexpected type {other}"),
                };
                if let Some(year) = query.get("year") {
                    assert_eq!(year, "2026");
                }
                Json(json!({"code":1,"pagecount":"7","total":70,"list":[{
                    "vod_id":vod_id,"vod_name":title,"vod_pic":"/poster.jpg",
                    "vod_year":2026,"vod_remarks":"更新至2集","vod_content":"<p>简介</p>",
                    "type_id":type_id.parse::<u16>().unwrap(),"type_name":type_name,
                    "vod_play_from":"HLS$$$文件$$$无效",
                    "vod_play_url":"第1集$https://media.example/1.m3u8?x=1&y=2#第2集$https://media.example/2.m3u8$$$电影$https://media.example/a.mp4$$$伪协议$javascript:alert(1)"
                }]}))
            }))
            .route("/bad", get(|| async { (StatusCode::SERVICE_UNAVAILABLE,"offline") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn source_crud_requires_admin_validates_and_preserves_enabled() {
        let (_temp, state) = state();
        let viewer = AuthUser {
            id: 2,
            username: "viewer".into(),
            role: "user".into(),
        };
        let input = || SourceInput {
            name: "测试".into(),
            url: "https://cms.example/api.php".into(),
            enabled: true,
            adult: false,
            ..Default::default()
        };
        assert_eq!(
            create_source(State(state.clone()), viewer, Json(input()))
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        let (_, Json(source)) = create_source(State(state.clone()), admin(), Json(input()))
            .await
            .unwrap();
        assert!(source.enabled);
        assert_eq!(
            create_source(State(state.clone()), admin(), Json(input()))
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let Json(updated) = update_source(
            State(state.clone()),
            admin(),
            Path(source.id),
            Json(SourceInput {
                enabled: false,
                ..input()
            }),
        )
        .await
        .unwrap();
        assert!(!updated.enabled);
        assert!(!sources(&state).unwrap()[0].enabled);
        assert!(
            validate_source(&SourceInput {
                url: "file:///etc/passwd".into(),
                ..input()
            })
            .is_err()
        );
        assert!(
            validate_source(&SourceInput {
                url: "https://user:secret@cms.example/api".into(),
                ..input()
            })
            .is_err()
        );
        delete_source(State(state.clone()), admin(), Path(source.id))
            .await
            .unwrap();
        assert!(sources(&state).unwrap().is_empty());
    }

    #[tokio::test]
    async fn adult_sources_are_hidden_until_an_admin_enables_them() {
        let (_temp, state) = state();
        let (base, task) = fixture().await;
        state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO catalog_sources(name,url,enabled,adult) VALUES
                 ('normal',?1,1,0),('adult',?2,1,1)",
                [
                    format!("{base}/cms?key=retained&normal=1"),
                    format!("{base}/cms?key=retained&adult=1"),
                ],
            )
            .unwrap();
        let viewer = AuthUser {
            id: 2,
            username: "viewer".into(),
            role: "user".into(),
        };
        assert_eq!(active_sources(&state).unwrap().len(), 1);
        let Json(list) = list_sources(State(state.clone()), viewer.clone())
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "normal");
        assert_eq!(
            settings_handler(State(state.clone()), viewer.clone())
                .await
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            save_settings_handler(
                State(state.clone()),
                viewer,
                Json(CatalogSettingsInput {
                    adult_sources_enabled: true
                })
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::FORBIDDEN
        );
        let Json(settings) = save_settings_handler(
            State(state.clone()),
            admin(),
            Json(CatalogSettingsInput {
                adult_sources_enabled: true,
            }),
        )
        .await
        .unwrap();
        assert!(settings.adult_sources_enabled);
        assert_eq!(active_sources(&state).unwrap().len(), 2);
        task.abort();
    }

    #[tokio::test]
    async fn search_retains_query_encoding_and_returns_partial_success() {
        let (_temp, state) = state();
        let (base, task) = fixture().await;
        for (name, path, enabled) in [
            ("good", "/cms?key=retained&wd=old", true),
            ("bad", "/bad", true),
            ("disabled", "/missing", false),
        ] {
            state
                .conn()
                .unwrap()
                .execute(
                    "INSERT INTO catalog_sources(name,url,enabled) VALUES(?,?,?)",
                    params![name, format!("{base}{path}"), enabled],
                )
                .unwrap();
        }
        let result = search(&state, "中文 & A+B").await.unwrap();
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].id, "42");
        assert_eq!(result.results[0].year, "2026");
        assert_eq!(result.results[0].source_name, "good");
        assert_eq!(result.results[0].kind, "series");
        assert_eq!(result.results[0].cover, format!("{base}/poster.jpg"));
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].source_id, 2);
        assert!(result.errors[0].message.contains("503"));
        assert!(search(&state, " ").await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn detail_parses_episode_groups_and_rejects_unsupported_urls() {
        let (_temp, state) = state();
        let (base, task) = fixture().await;
        state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO catalog_sources(name,url) VALUES('cms',?)",
                [format!("{base}/cms?key=retained")],
            )
            .unwrap();
        let item = detail(&state, 1, "42").await.unwrap();
        assert_eq!(item.title, "故事 <script>");
        assert_eq!(item.description, "<p>简介</p>");
        assert_eq!(item.groups.len(), 2);
        assert_eq!(item.groups[0].name, "HLS");
        assert_eq!(item.groups[0].episodes.len(), 2);
        assert!(item.groups[0].direct);
        assert_eq!(
            item.groups[0].episodes[0].url,
            "https://media.example/1.m3u8?x=1&y=2"
        );
        assert_eq!(item.groups[1].episodes[0].name, "电影");
        assert_eq!(
            detail(&state, 1, "999").await.unwrap_err().0,
            StatusCode::NOT_FOUND
        );
        assert!(detail(&state, 1, "\n").await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn bounded_response_and_invalid_json_are_reported_without_source_secrets() {
        let (_temp, state) = state();
        let app = Router::new()
            .route(
                "/oversize",
                get(|| async { vec![b'x'; MAX_RESPONSE_BYTES + 1] }),
            )
            .route("/html", get(|| async { "<html>not a CMS API</html>" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        for path in ["/oversize", "/html"] {
            let source = {
                let conn = state.conn().unwrap();
                conn.execute(
                    "INSERT INTO catalog_sources(name,url) VALUES ('test',?)",
                    [format!("{base}{path}?key=private")],
                )
                .unwrap();
                source_by_id(&conn, conn.last_insert_rowid()).unwrap()
            };
            let error = fetch_items(&state, &source, &[("ac", "detail")])
                .await
                .unwrap_err();
            assert!(!error.contains("private"));
            assert!(error.contains("8 MiB") || error.contains("valid CMS JSON"));
        }
        task.abort();
    }

    #[test]
    fn bare_urls_and_missing_group_names_are_usable() {
        let groups = play_groups(
            "",
            "https://media.example/movie.mp4#missing$file:///tmp/secret",
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "线路 1");
        assert_eq!(groups[0].episodes.len(), 1);
        assert_eq!(groups[0].episodes[0].name, "播放");
    }

    #[test]
    fn play_groups_mark_parser_pages_as_non_direct() {
        let groups = play_groups(
            "share,ffm3u8",
            "第1集$https://media.example/share/abc$$$第1集$https://media.example/1.m3u8?token=1",
        );
        assert_eq!(groups.len(), 2);
        assert!(!groups[0].direct);
        assert!(groups[1].direct);
    }

    #[tokio::test]
    async fn source_used_by_a_subscription_is_retained_with_a_conflict() {
        let (_temp, state) = state();
        state.conn().unwrap().execute_batch(
            "PRAGMA foreign_keys=ON;
             INSERT INTO catalog_sources(id,name,url) VALUES(1,'source','https://source.example/api');
             CREATE TABLE active_subscription(source_id INTEGER REFERENCES catalog_sources(id) ON DELETE RESTRICT);
             INSERT INTO active_subscription VALUES(1);"
        ).unwrap();
        let error = delete_source(State(state.clone()), admin(), Path(1))
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(sources(&state).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn discover_lists_latest_titles_and_page_metadata() {
        let (_temp, state) = state();
        assert!(
            discover(&state, None, 1, None, None, None)
                .await
                .unwrap()
                .results
                .is_empty()
        );
        let (base, task) = fixture().await;
        state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO catalog_sources(name,url) VALUES('cms',?)",
                [format!("{base}/cms?key=retained")],
            )
            .unwrap();
        let page = discover(&state, None, 2, None, None, None).await.unwrap();
        assert_eq!(page.source_id, None);
        assert_eq!(page.page, 2);
        assert_eq!(page.page_count, 7);
        assert_eq!(page.total, 70);
        assert_eq!(page.results[0].title, "故事 <script>");
        assert!(discover(&state, None, 0, None, None, None).await.is_err());
        assert!(
            discover(&state, Some(999), 1, None, None, None)
                .await
                .is_err()
        );
        let raw = cms_query(&state, 1, &[("ac", "detail")]).await.unwrap();
        assert_eq!(raw["pagecount"], "7");
        assert!(
            cms_query(&state, 1, &[("url", "https://unconfigured.example")])
                .await
                .is_err()
        );
        task.abort();
    }

    #[tokio::test]
    async fn discover_aggregates_sources_and_applies_filters() {
        let (_temp, state) = state();
        let (base, task) = fixture().await;
        {
            let conn = state.conn().unwrap();
            for side in ["a", "b"] {
                conn.execute(
                    "INSERT INTO catalog_sources(name,url) VALUES(?1,?2)",
                    params![
                        format!("CMS {side}"),
                        format!("{base}/cms?key=retained&side={side}")
                    ],
                )
                .unwrap();
            }
            conn.execute_batch(
                "INSERT INTO catalog_types(source_id,type_id,type_name)
                 SELECT id,5,'动作片' FROM catalog_sources;
                 INSERT INTO catalog_types(source_id,type_id,type_name)
                 SELECT id,6,'剧情片' FROM catalog_sources;
                 INSERT INTO catalog_types(source_id,type_id,type_name)
                 SELECT id,13,'国产剧' FROM catalog_sources;",
            )
            .unwrap();
        }
        let all = discover(&state, None, 1, None, None, None).await.unwrap();
        assert_eq!(all.results.len(), 2);
        assert_eq!(all.total, 140);
        assert_eq!(all.page_count, 7);
        let filtered = discover(&state, None, 1, None, Some("国产剧"), Some("2026"))
            .await
            .unwrap();
        assert_eq!(filtered.results.len(), 2);
        assert_eq!(filtered.total, 140);
        let ids: Vec<i64> = state
            .conn()
            .unwrap()
            .prepare("SELECT id FROM catalog_sources ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let one = discover(&state, Some(ids[0]), 1, None, None, None)
            .await
            .unwrap();
        assert_eq!(one.results.len(), 1);
        assert_eq!(one.source_id, Some(ids[0]));
        assert_eq!(one.total, 70);
        let movies = discover(&state, None, 1, Some("movie"), None, None)
            .await
            .unwrap();
        assert_eq!(movies.results.len(), 4);
        assert_eq!(movies.total, 280);
        assert!(movies.results.iter().all(|item| item.kind == "movie"));
        let series = discover(&state, None, 1, Some("series"), None, None)
            .await
            .unwrap();
        assert_eq!(series.results.len(), 2);
        assert!(series.results.iter().all(|item| item.kind == "series"));
        assert!(
            discover(&state, None, 1, Some("bogus"), None, None)
                .await
                .is_err()
        );
        assert!(
            discover(&state, Some(ids[0]), 1, None, Some("不存在"), None)
                .await
                .is_err()
        );
        assert!(
            discover(&state, None, 1, None, None, Some("20x0"))
                .await
                .is_err()
        );
        task.abort();
    }
}
