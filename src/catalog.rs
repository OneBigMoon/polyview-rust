use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get},
};
use futures_util::{StreamExt, stream};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use url::Url;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalog_sources (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            url TEXT NOT NULL UNIQUE,
            enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1))
        );",
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS catalog_types (
            source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE CASCADE,
            type_id INTEGER NOT NULL,
            type_name TEXT NOT NULL,
            PRIMARY KEY(source_id, type_id)
        );
        CREATE INDEX IF NOT EXISTS catalog_types_name ON catalog_types(type_name);",
    )?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/sources", get(list_sources).post(create_source))
        .route(
            "/api/sources/{id}",
            delete(delete_source).put(update_source),
        )
        .route("/api/catalog/search", get(search_handler))
        .route("/api/catalog/discover", get(discover_handler))
        .route("/api/catalog/categories", get(categories_handler))
        .route("/api/catalog/{source_id}/detail", get(detail_handler))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub enabled: bool,
}

fn source_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Source> {
    Ok(Source {
        id: row.get(0)?,
        name: row.get(1)?,
        url: row.get(2)?,
        enabled: row.get(3)?,
    })
}

pub fn sources(state: &Shared) -> Result<Vec<Source>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare("SELECT id,name,url,enabled FROM catalog_sources ORDER BY id")?;
    Ok(stmt.query_map([], source_row)?.collect::<Result<_, _>>()?)
}

async fn list_sources(
    State(state): State<Shared>,
    _user: AuthUser,
) -> Result<Json<Vec<Source>>, AppError> {
    sources(&state).map(Json)
}

#[derive(Deserialize)]
struct SourceInput {
    name: String,
    url: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
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

fn validate_source(input: &SourceInput) -> Result<(String, String), AppError> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(AppError::bad_request(
            "Source name must contain 1–100 characters",
        ));
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
    let (name, url) = validate_source(&input)?;
    let conn = state.conn()?;
    conn.execute(
        "INSERT INTO catalog_sources(name,url,enabled) VALUES(?,?,?)",
        params![name, url, input.enabled],
    )
    .map_err(source_write_error)?;
    Ok((
        StatusCode::CREATED,
        Json(Source {
            id: conn.last_insert_rowid(),
            name,
            url,
            enabled: input.enabled,
        }),
    ))
}

async fn update_source(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<SourceInput>,
) -> Result<Json<Source>, AppError> {
    require_admin(&user)?;
    let (name, url) = validate_source(&input)?;
    if state
        .conn()?
        .execute(
            "UPDATE catalog_sources SET name=?,url=?,enabled=? WHERE id=?",
            params![name, url, input.enabled, id],
        )
        .map_err(source_write_error)?
        == 0
    {
        return Err(AppError::not_found("Source not found"));
    }
    Ok(Json(Source {
        id,
        name,
        url,
        enabled: input.enabled,
    }))
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
    pub title: String,
    pub cover: String,
    pub year: String,
    pub remark: String,
    pub kind: String,
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

pub async fn search(state: &Shared, query: &str) -> Result<SearchResponse, AppError> {
    let query = validate_text(query, "Search query", 100)?;
    let active = sources(state)?.into_iter().filter(|source| source.enabled);
    let mut responses = stream::iter(active.map(|source| async move {
        let result = fetch_items(state, &source, &[("ac", "detail"), ("wd", query)]).await;
        (source, result)
    }))
    .buffer_unordered(6);
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
            Ok(page) => response.results.extend(
                page.items
                    .iter()
                    .filter_map(|item| summary(&source, item))
                    .take(1000),
            ),
            Err(message) => response.errors.push(SourceError {
                source_id: source.id,
                message,
            }),
        }
    }
    response
        .results
        .sort_by(|a, b| (a.source_id, &a.title, &a.id).cmp(&(b.source_id, &b.title, &b.id)));
    response.errors.sort_by_key(|error| error.source_id);
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

async fn refresh_categories(state: &Shared) -> Result<(), AppError> {
    let active: Vec<Source> = sources(state)?
        .into_iter()
        .filter(|source| source.enabled)
        .collect();
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
    let mut statement =
        conn.prepare("SELECT DISTINCT type_name FROM catalog_types ORDER BY type_name")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut categories = BTreeSet::new();
    for row in rows {
        categories.insert(row?);
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
        && !matches!(value, "movie" | "series" | "variety" | "anime" | "other")
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
            "SELECT source_id,type_id FROM catalog_types WHERE type_name=?1 ORDER BY source_id,type_id",
        )?;
        let rows = statement.query_map([category], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (source_id, type_id) = row?;
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
    let active: Vec<Source> = sources(state)?
        .into_iter()
        .filter(|source| source.enabled && source_id.is_none_or(|id| source.id == id))
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
    .buffer_unordered(8);
    let mut fetched = Vec::new();
    while let Some((source, result)) = responses.next().await {
        if let Ok(listing) = &result
            && let Ok(conn) = state.conn()
        {
            let _ = remember_types(&conn, source.id, &listing.items);
        }
        fetched.push((source, result));
    }
    fetched.sort_by_key(|(source, _)| source.id);
    let mut seen = BTreeSet::new();
    let mut result = DiscoverResponse {
        results: Vec::new(),
        errors: Vec::new(),
        source_id,
        page,
        page_count: 0,
        total: 0,
    };
    for (source, fetch) in fetched {
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
    detail(&state, source_id, &query.id).await.map(Json)
}

pub async fn detail(state: &Shared, source_id: i64, id: &str) -> Result<CatalogDetail, AppError> {
    let id = validate_text(id, "Media ID", 256)?;
    let source = state
        .conn()?
        .query_row(
            "SELECT id,name,url,enabled FROM catalog_sources WHERE id=? AND enabled=1",
            [source_id],
            source_row,
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Source not found or disabled"))?;
    let page = fetch_items(state, &source, &[("ac", "detail"), ("ids", id)])
        .await
        .map_err(|message| AppError(StatusCode::BAD_GATEWAY, message))?;
    let item = page
        .items
        .iter()
        .find(|item| text(item, "vod_id") == id)
        .ok_or_else(|| AppError::not_found("Media not found at this source"))?;
    let summary = summary(&source, item).ok_or_else(|| {
        AppError(
            StatusCode::BAD_GATEWAY,
            "Source returned incomplete media metadata".into(),
        )
    })?;
    Ok(CatalogDetail {
        id: summary.id,
        source_id,
        source_name: source.name,
        title: summary.title,
        cover: summary.cover,
        description: text(item, "vod_content"),
        year: summary.year,
        remark: summary.remark,
        groups: play_groups(&text(item, "vod_play_from"), &text(item, "vod_play_url")),
    })
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
    if name.contains("解说")
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
        title,
        cover,
        year: text(item, "vod_year"),
        remark: text(item, "vod_remarks"),
        kind: media_kind(&text(item, "type_name")).to_owned(),
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
                        url: url.to_string(),
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
                direct: episodes
                    .iter()
                    .all(|episode| is_direct_media_url(&episode.url)),
                episodes,
            })
        })
        .collect()
}

/// Known media containers that browsers, TV clients, or the download worker
/// can fetch directly. CMS "share" or parser pages have no such suffix and
/// need a spider/parser PolyView does not run.
pub fn is_direct_media_url(url: &str) -> bool {
    const EXTENSIONS: &[&str] = &[
        "m3u8", "mp4", "m4v", "webm", "mkv", "mov", "ts", "flv", "avi", "wmv",
    ];
    Url::parse(url).ok().is_some_and(|url| {
        url.path()
            .rsplit_once('.')
            .map(|(_, extension)| {
                EXTENSIONS
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
            })
            .unwrap_or(false)
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
    let source = state
        .conn()?
        .query_row(
            "SELECT id,name,url,enabled FROM catalog_sources WHERE id=? AND enabled=1",
            [source_id],
            source_row,
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Source not found or disabled"))?;
    fetch_cms_value(state, &source, parameters)
        .await
        .map_err(|message| AppError(StatusCode::BAD_GATEWAY, message))
}

async fn fetch_cms_value(
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
    let fetch = async {
        let response = state
            .client
            .get(url)
            .timeout(REQUEST_TIMEOUT)
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
        if !value.get("list").is_some_and(Value::is_array)
            && !value.get("class").is_some_and(Value::is_array)
        {
            return Err("Source JSON does not contain CMS lists or categories".to_owned());
        }
        Ok(value)
    };
    tokio::time::timeout(REQUEST_TIMEOUT, fetch)
        .await
        .map_err(|_| "Source timed out after 15 seconds".to_owned())?
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
            let source = Source {
                id: 1,
                name: "test".into(),
                url: format!("{base}{path}?key=private"),
                enabled: true,
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
