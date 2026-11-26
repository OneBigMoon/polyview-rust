//! Metadata lookup and sidecars are keyed by the existing library row, even after organization.
mod config;
mod files;
mod providers;

use crate::{AppError, Shared, auth::AuthUser};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::header,
    response::IntoResponse,
    routing::get,
};
pub use config::{Backup, enabled, export_config, restore_config};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lookup {
    pub title: Option<String>,
    pub year: Option<String>,
    pub tmdb_id: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Metadata {
    pub provider: String,
    pub provider_id: String,
    pub title: String,
    pub original_title: String,
    pub year: String,
    pub rating: Option<f64>,
    pub genres: Vec<String>,
    pub description: String,
    pub cover: String,
    pub backdrop: String,
    pub cast: Vec<Credit>,
    pub crew: Vec<Credit>,
    pub seasons: Vec<Season>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credit {
    pub name: String,
    pub role: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Season {
    pub number: i64,
    pub title: String,
    pub episodes: Vec<Episode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Episode {
    pub number: i64,
    pub title: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prepared {
    pub metadata: Metadata,
    poster_url: Option<String>,
    backdrop_url: Option<String>,
}

struct Media {
    root: String,
    relative: String,
    title: String,
    year: String,
    series_id: Option<i64>,
    season: Option<i64>,
    episode: Option<i64>,
}

impl Media {
    fn get(conn: &Connection, id: i64) -> Result<Self, AppError> {
        conn.query_row("SELECT m.root,m.relative_path,COALESCE(s.title,m.title),m.year,m.series_id,m.season,m.episode
            FROM library_media m LEFT JOIN library_series s ON m.series_id=s.id WHERE m.id=? AND m.available=1", [id], |row| Ok(Self {
            root: row.get(0)?, relative: row.get(1)?, title: row.get(2)?, year:row.get(3)?, series_id:row.get(4)?, season:row.get(5)?, episode:row.get(6)?,
        })).optional()?.ok_or_else(|| AppError::not_found("Media is unavailable"))
    }
}

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS media_metadata (
        media_id INTEGER PRIMARY KEY REFERENCES library_media(id) ON DELETE CASCADE, metadata_json TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS scraper_assets (id INTEGER PRIMARY KEY, digest BLOB NOT NULL UNIQUE, mime TEXT NOT NULL, data BLOB NOT NULL);
        CREATE TABLE IF NOT EXISTS media_art (media_id INTEGER NOT NULL REFERENCES library_media(id) ON DELETE CASCADE,
        kind TEXT NOT NULL, asset_id INTEGER NOT NULL REFERENCES scraper_assets(id), PRIMARY KEY(media_id,kind));
        CREATE INDEX IF NOT EXISTS media_art_asset ON media_art(asset_id);
        CREATE TABLE IF NOT EXISTS scraper_sidecars (root TEXT NOT NULL, path TEXT NOT NULL,
        owner TEXT NOT NULL, digest BLOB NOT NULL, pending BLOB, PRIMARY KEY(root,path));")?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/api/settings/scraper",
            get(config::read).put(config::update),
        )
        .route("/api/library/{id}/metadata", get(read_metadata))
        .route("/api/library/{id}/art/{kind}", get(read_art))
}

pub fn metadata(conn: &Connection, id: i64) -> Result<Option<Metadata>, AppError> {
    let raw: Option<String> = conn.query_row("SELECT metadata_json FROM media_metadata JOIN library_media ON media_id=id WHERE id=? AND available=1", [id], |row| row.get(0)).optional()?;
    raw.map(|s| {
        serde_json::from_str(&s).map_err(|_| AppError::internal("Invalid stored media metadata"))
    })
    .transpose()
}

pub fn artwork(conn: &Connection, id: i64, kind: &str) -> Result<(String, Vec<u8>), AppError> {
    conn.query_row(
        "SELECT a.mime,a.data FROM media_art m JOIN scraper_assets a ON a.id=m.asset_id
        JOIN library_media l ON l.id=m.media_id WHERE m.media_id=? AND m.kind=? AND l.available=1",
        params![id, kind],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| AppError::not_found("Artwork unavailable"))
}

async fn read_art(
    State(state): State<Shared>,
    _user: AuthUser,
    Path((id, kind)): Path<(i64, String)>,
) -> Result<impl IntoResponse, AppError> {
    let (mime, bytes) = artwork(&*state.conn()?, id, &kind)?;
    Ok(([(header::CONTENT_TYPE, mime)], bytes))
}

async fn read_metadata(
    State(state): State<Shared>,
    _user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Metadata>, AppError> {
    metadata(&*state.conn()?, id)?
        .map(Json)
        .ok_or_else(|| AppError::not_found("Metadata unavailable"))
}

pub async fn lookup_media(state: &Shared, id: i64, mut hint: Lookup) -> Result<Prepared, AppError> {
    let (media, config) = {
        let conn = state.conn()?;
        (Media::get(&conn, id)?, export_config(&conn)?)
    };
    let title = hint.title.take().unwrap_or(media.title);
    let year = hint.year.take().unwrap_or(media.year);
    if title.trim().is_empty()
        || title.len() > 1024
        || (!year.is_empty() && !(year.len() == 4 && year.bytes().all(|c| c.is_ascii_digit())))
        || hint.tmdb_id.is_some_and(|id| id <= 0)
    {
        return Err(AppError::bad_request("Invalid metadata lookup"));
    }
    providers::lookup(&config, title.trim(), &year, media.season, hint.tmdb_id).await
}

pub async fn persist_media(
    state: &Shared,
    id: i64,
    mut prepared: Prepared,
) -> Result<Metadata, AppError> {
    let config = export_config(&*state.conn()?)?;
    let client = config.client()?;
    let (poster, backdrop) = tokio::try_join!(
        providers::image(&client, prepared.poster_url.as_deref()),
        providers::image(&client, prepared.backdrop_url.as_deref())
    )?;
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = state.conn()?;
        let media = Media::get(&conn,id)?;
        let assets = [("poster",poster),("backdrop",backdrop)];
        files::write_sidecars(&state,&conn,&media,id,&prepared.metadata,&assets)?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM media_art WHERE media_id=?",[id])?;
        for (kind,asset) in assets {
            let Some(asset) = asset else { continue };
            // Shared artwork (typically a poster for an entire show) is stored once.
            let digest = ring::digest::digest(&ring::digest::SHA256,&asset.data);
            let existing:Option<i64> = tx.query_row("SELECT id FROM scraper_assets WHERE digest=?",[digest.as_ref()],|r|r.get(0)).optional()?;
            let asset_id = match existing {
                Some(asset_id) => asset_id,
                None => { tx.execute("INSERT INTO scraper_assets(digest,mime,data) VALUES(?,?,?)",params![digest.as_ref(),asset.mime,&asset.data])?; tx.last_insert_rowid() }
            };
            tx.execute("INSERT INTO media_art(media_id,kind,asset_id) VALUES(?,?,?)",params![id,kind,asset_id])?;
            let local = format!("/api/library/{id}/art/{kind}");
            if kind == "poster" { prepared.metadata.cover = local } else { prepared.metadata.backdrop = local }
        }
        tx.execute("DELETE FROM scraper_assets WHERE NOT EXISTS(SELECT 1 FROM media_art WHERE asset_id=scraper_assets.id)",[])?;
        let m = &prepared.metadata;
        let raw = serde_json::to_string(m).map_err(|_| AppError::internal("Could not serialize metadata"))?;
        tx.execute("INSERT INTO media_metadata(media_id,metadata_json) VALUES(?,?) ON CONFLICT(media_id) DO UPDATE SET metadata_json=excluded.metadata_json", params![id,raw])?;
        tx.execute("UPDATE library_media SET cover=?,year=?,description=? WHERE id=?", params![m.cover,m.year,m.description,id])?;
        tx.commit()?;
        Ok(prepared.metadata)
    }).await.map_err(|_| AppError::internal("Metadata persistence interrupted"))?
}

pub async fn write_cached_sidecars(state: &Shared, id: i64) -> Result<(), AppError> {
    let state = state.clone();
    tokio::task::spawn_blocking(move || {
        let conn = state.conn()?;
        let media = Media::get(&conn, id)?;
        let m = metadata(&conn, id)?.ok_or_else(|| AppError::not_found("Metadata unavailable"))?;
        let load = |kind, url: &str| -> Result<Option<providers::Image>, AppError> {
            if url.is_empty() {
                return Ok(None);
            }
            let (_, bytes) = artwork(&conn, id, kind)?;
            providers::decode_image(bytes).map(Some)
        };
        let assets = [
            ("poster", load("poster", &m.cover)?),
            ("backdrop", load("backdrop", &m.backdrop)?),
        ];
        files::write_sidecars(&state, &conn, &media, id, &m, &assets)
    })
    .await
    .map_err(|_| AppError::internal("Could not restore cached sidecars"))?
}

pub async fn scrape_media(
    state: &Shared,
    id: i64,
    hint: Lookup,
    force: bool,
) -> Result<Metadata, AppError> {
    if !force && let Some(m) = metadata(&*state.conn()?, id)? {
        return Ok(m);
    }
    let prepared = lookup_media(state, id, hint).await?;
    persist_media(state, id, prepared).await
}
