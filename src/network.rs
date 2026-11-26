mod playlist;
mod resolved;

pub(crate) use playlist::{
    live_headers, live_url, parse_playlist, validate_catchup, validate_channel_metadata,
};
pub(crate) use resolved::{PreparedDownload, prepare_catalog_download};

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    download_settings::{DownloadSettings, download_settings},
    now,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use futures_util::StreamExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{Notify, Semaphore, watch},
    task::{JoinHandle, JoinSet},
};
use url::Url;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channels (
            id INTEGER PRIMARY KEY, name TEXT NOT NULL, group_name TEXT NOT NULL,
            logo TEXT NOT NULL, url TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS channel_sources (
            id INTEGER PRIMARY KEY,
            channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            url TEXT NOT NULL, label TEXT NOT NULL DEFAULT '',
            position INTEGER NOT NULL DEFAULT 0,
            UNIQUE(channel_id, url)
        );
        CREATE INDEX IF NOT EXISTS channel_sources_channel
            ON channel_sources(channel_id, position);
        INSERT OR IGNORE INTO channel_sources(channel_id, url, label, position)
            SELECT id, url, '', 0 FROM channels c
            WHERE NOT EXISTS (SELECT 1 FROM channel_sources s WHERE s.channel_id=c.id);
        CREATE TABLE IF NOT EXISTS downloads (
            id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            title TEXT NOT NULL, url TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
            bytes_written INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER, error TEXT,
            relative_path TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS downloads_user ON downloads(user_id, id);",
    )?;
    crate::ensure_column(conn, "channels", "tvg_id", "TEXT NOT NULL DEFAULT ''")?;
    for (name, definition) in [
        ("headers_json", "TEXT NOT NULL DEFAULT '{}'"),
        ("catchup_mode", "TEXT NOT NULL DEFAULT ''"),
        ("catchup_source", "TEXT NOT NULL DEFAULT ''"),
        ("latency_ms", "INTEGER"),
        ("last_ok_at", "INTEGER"),
        ("fail_count", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        crate::ensure_column(conn, "channel_sources", name, definition)?;
    }
    for (name, definition) in [
        ("kind", "TEXT NOT NULL DEFAULT 'direct'"),
        ("attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("cover", "TEXT NOT NULL DEFAULT ''"),
        ("year", "TEXT NOT NULL DEFAULT ''"),
        ("description", "TEXT NOT NULL DEFAULT ''"),
        ("attempt_token", "TEXT NOT NULL DEFAULT ''"),
        ("retry_at", "INTEGER NOT NULL DEFAULT 0"),
        ("resolved_media", "TEXT"),
        ("bytes_per_second", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        crate::ensure_column(conn, "downloads", name, definition)?;
    }
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/channels", get(list_channels))
        .route("/api/channels/import", post(import_channels))
        .route("/api/channels/{id}", delete(delete_channel))
        .route("/api/downloads", get(list_downloads).post(create_download))
        .route("/api/downloads/{id}", delete(cancel_download))
        .route("/api/downloads/{id}/retry", post(retry_download))
        .route("/api/downloads/{id}/pause", post(pause_download))
        .route("/api/downloads/{id}/resume", post(resume_download))
        .route("/api/downloads/completed", delete(clear_downloads))
}

#[derive(Debug, Serialize)]
struct ChannelSource {
    id: Option<i64>,
    stream_url: Option<String>,
    hls: bool,
    catchup: bool,
    catchup_hls: Option<bool>,
    latency_ms: Option<i64>,
    last_ok_at: Option<i64>,
    fail_count: i64,
    url: String,
    label: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct Channel {
    id: i64,
    name: String,
    group_name: String,
    logo: String,
    url: String,
    tvg_id: String,
    catchup: bool,
    pub favorite: bool,
    sources: Vec<ChannelSource>,
}

#[derive(Debug)]
pub(crate) struct PlaylistChannel {
    name: String,
    group_name: String,
    logo: String,
    url: String,
    tvg_id: String,
    headers_json: String,
    catchup_mode: String,
    catchup_source: String,
}

async fn list_channels(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Channel>>, AppError> {
    let conn = state.conn()?;
    Ok(Json(channels_for_user(&conn, &user, false)?))
}

pub(crate) fn channels_for_user(
    conn: &Connection,
    user: &AuthUser,
    favorites_only: bool,
) -> Result<Vec<Channel>, AppError> {
    let mut query = conn.prepare(
        "SELECT c.id,c.name,c.group_name,c.logo!='',c.url,f.channel_id IS NOT NULL,c.tvg_id
        FROM channels c LEFT JOIN channel_favorites f ON f.channel_id=c.id AND f.user_id=?1
        WHERE NOT ?2 OR f.channel_id IS NOT NULL
        ORDER BY CASE WHEN ?2 THEN f.created_at END DESC,c.group_name,c.name,c.id",
    )?;
    let mut channels = query
        .query_map(params![user.id, favorites_only], |row| {
            Ok(Channel {
                id: row.get(0)?,
                name: row.get(1)?,
                group_name: row.get(2)?,
                logo: if row.get::<_, bool>(3)? {
                    format!("/api/channels/{}/logo", row.get::<_, i64>(0)?)
                } else {
                    String::new()
                },
                url: row.get(4)?,
                favorite: row.get(5)?,
                tvg_id: row.get(6)?,
                catchup: false,
                sources: Vec::new(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut sources = conn.prepare(
        "SELECT id,url,label,headers_json!='{}',catchup_source,latency_ms,last_ok_at,fail_count
         FROM channel_sources WHERE channel_id=?1 ORDER BY position,id",
    )?;
    for channel in &mut channels {
        channel.sources = sources
            .query_map([channel.id], |row| {
                let id: i64 = row.get(0)?;
                let upstream: String = row.get(1)?;
                let has_headers: bool = row.get(3)?;
                let template: String = row.get(4)?;
                let stream_url = format!("/api/channels/lines/{id}/stream");
                let hls = playlist::is_hls(&upstream);
                let url = if user.role == "admin" && !has_headers && !upstream.contains(['?', '#'])
                {
                    upstream
                } else {
                    stream_url.clone()
                };
                Ok(ChannelSource {
                    id: Some(id),
                    stream_url: Some(stream_url),
                    hls,
                    catchup: !template.is_empty(),
                    catchup_hls: playlist::catchup_hls(&template),
                    latency_ms: row.get(5)?,
                    last_ok_at: row.get(6)?,
                    fail_count: row.get(7)?,
                    url,
                    label: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(first) = channel.sources.first() {
            channel.url.clone_from(&first.url);
            channel.catchup = channel.sources.iter().any(|line| line.catchup);
        } else {
            // SQL-only legacy entries can lack a line until the next startup backfill.
            let hls = playlist::is_hls(&channel.url);
            if user.role != "admin" || channel.url.contains(['?', '#']) {
                channel.url.clear();
            }
            channel.sources.push(ChannelSource {
                id: None,
                stream_url: None,
                hls,
                catchup: false,
                catchup_hls: None,
                latency_ms: None,
                last_ok_at: None,
                fail_count: 0,
                url: channel.url.clone(),
                label: String::new(),
            });
        }
    }
    Ok(channels)
}

#[derive(Deserialize)]
struct PlaylistInput {
    playlist: String,
}

async fn import_channels(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<PlaylistInput>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let channels = parse_playlist(&input.playlist)?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let counts = merge_playlist(&tx, &channels)?;
    tx.commit()?;
    Ok(Json(
        json!({"imported": channels.len(), "new_channels": counts.added}),
    ))
}

pub(crate) struct MergeCounts {
    pub channels: usize,
    pub added: usize,
}

pub(crate) fn merge_playlist(
    tx: &Connection,
    channels: &[PlaylistChannel],
) -> Result<MergeCounts, AppError> {
    if crate::settings::get_setting_json(tx, crate::settings::MERGE_SIMILAR_KEY)? {
        return merge_similar_playlist(tx, channels);
    }
    let mut groups: BTreeMap<(&str, &str), Vec<&PlaylistChannel>> = BTreeMap::new();
    for channel in channels {
        groups
            .entry((&channel.name, &channel.group_name))
            .or_default()
            .push(channel);
    }
    let mut counts = MergeCounts {
        channels: groups.len(),
        added: 0,
    };
    for ((name, group_name), entries) in groups {
        let first = entries[0];
        let mut id = tx
            .query_row(
                "SELECT id FROM channels WHERE name=?1 AND group_name=?2 ORDER BY id LIMIT 1",
                params![name, group_name],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if id.is_none() {
            for entry in &entries {
                if let Some(found) = tx
                    .query_row(
                        "SELECT id FROM channels WHERE url=?1",
                        [&entry.url],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?
                {
                    id = Some(found);
                    break;
                }
            }
        }
        save_channel(tx, id, &entries, &first.url, false)?;
        counts.added += usize::from(id.is_none());
    }
    Ok(counts)
}

// Unknown local stations can share a generic name (e.g. 新闻综合) across regions.
// Only known public stations have an identity independent of the playlist group.
fn similar_key(name: &str, group: &str) -> String {
    let mut key = crate::epg::canonical(name);
    if !crate::epg::public_channel_key(&key) {
        if key.is_empty() {
            key.push_str(name);
        }
        key.push('\0');
        key.push_str(group);
    }
    key
}

#[derive(Default)]
struct VariantGroup<'a> {
    entries: Vec<&'a PlaylistChannel>,
    ids: Vec<i64>,
}

fn merge_similar_playlist(
    tx: &Connection,
    channels: &[PlaylistChannel],
) -> Result<MergeCounts, AppError> {
    let mut groups = BTreeMap::<String, VariantGroup>::new();
    for line in channels {
        groups
            .entry(similar_key(&line.name, &line.group_name))
            .or_default()
            .entries
            .push(line);
    }
    {
        let mut query = tx.prepare("SELECT id,name,group_name FROM channels ORDER BY id")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let name = row
                .get_ref(1)?
                .as_str()
                .map_err(|_| AppError::internal("Invalid stored live channel name"))?;
            let group = row
                .get_ref(2)?
                .as_str()
                .map_err(|_| AppError::internal("Invalid stored live channel group"))?;
            if let Some(variants) = groups.get_mut(&similar_key(name, group)) {
                variants.ids.push(row.get(0)?);
            }
        }
    }
    let mut counts = MergeCounts {
        channels: 0,
        added: 0,
    };
    for VariantGroup { entries, ids } in groups.into_values() {
        let Some((&first, others)) = ids.split_first() else {
            // A shared alternate can belong to multiple channels, but a legacy
            // primary URL is unique. Never rename its unrelated existing owner.
            let mut taken = tx.prepare_cached("SELECT id FROM channels WHERE url=?")?;
            let mut primary = None;
            let mut owners = BTreeMap::<i64, Vec<&PlaylistChannel>>::new();
            for line in &entries {
                if let Some(id) = taken.query_row([&line.url], |r| r.get(0)).optional()? {
                    owners.entry(id).or_default().push(line);
                } else {
                    primary = Some(&line.url);
                    break;
                }
            }
            if let Some(url) = primary {
                save_channel(tx, None, &entries, url, true)?;
                counts.added += 1;
                counts.channels += 1;
            } else {
                // A feed renamed an existing primary URL. Updating its own line
                // retains the established channel identity and favorites.
                counts.channels += owners.len();
                for (id, lines) in owners {
                    save_channel(tx, Some(id), &lines, &lines[0].url, true)?;
                }
            }
            continue;
        };
        let mut owners = vec![first];
        for &other in others {
            if !combine_channels(tx, first, other)? {
                owners.push(other);
            }
        }
        if owners.len() == 1 {
            save_channel(tx, Some(first), &entries, &entries[0].url, true)?;
            counts.channels += 1;
            continue;
        }
        // Historic channels with incompatible versions of the same URL retain
        // their own lines. Prefer the named owner, then the exact line identity.
        let mut by_owner: BTreeMap<i64, Vec<&PlaylistChannel>> = BTreeMap::new();
        let mut score = tx.prepare_cached(
            "SELECT (c.name=?2 AND c.group_name=?3)*4 + (s.id IS NOT NULL) +
             COALESCE((s.headers_json=?5 AND s.catchup_mode=?6 AND s.catchup_source=?7)*2,0)
             FROM channels c LEFT JOIN channel_sources s ON s.channel_id=c.id AND s.url=?4 WHERE c.id=?1",
        )?;
        for entry in entries {
            let mut best = (first, -1);
            for &owner in &owners {
                let rank: i64 = score.query_row(
                    params![
                        owner,
                        entry.name,
                        entry.group_name,
                        entry.url,
                        entry.headers_json,
                        entry.catchup_mode,
                        entry.catchup_source
                    ],
                    |r| r.get(0),
                )?;
                if rank > best.1 {
                    best = (owner, rank);
                }
            }
            by_owner.entry(best.0).or_default().push(entry);
        }
        counts.channels += by_owner.len();
        for (owner, lines) in by_owner {
            save_channel(tx, Some(owner), &lines, &lines[0].url, true)?;
        }
    }
    Ok(counts)
}

fn combine_channels(tx: &Connection, keep: i64, other: i64) -> Result<bool, AppError> {
    // SQL-only legacy channels can lack their initial line until startup.
    tx.execute(
        "INSERT INTO channel_sources(channel_id,url) SELECT id,url FROM channels c
         WHERE id IN (?1,?2) AND NOT EXISTS (SELECT 1 FROM channel_sources WHERE channel_id=c.id)",
        params![keep, other],
    )?;
    let conflict: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM channel_sources a JOIN channel_sources b ON a.url=b.url
         WHERE a.channel_id=?1 AND b.channel_id=?2 AND (a.headers_json!=b.headers_json OR
         a.catchup_mode!=b.catchup_mode OR a.catchup_source!=b.catchup_source OR a.label!=b.label OR
         a.latency_ms IS NOT b.latency_ms OR a.last_ok_at IS NOT b.last_ok_at OR a.fail_count!=b.fail_count))
         OR EXISTS(SELECT 1 FROM epg_programs a JOIN epg_programs b ON a.start=b.start AND a.title=b.title
         WHERE a.channel_id=?1 AND b.channel_id=?2 AND (a.stop!=b.stop OR a.description!=b.description))",
        params![keep, other], |r| r.get(0),
    )?;
    if conflict {
        return Ok(false);
    }
    tx.execute(
        "UPDATE OR IGNORE channel_sources SET channel_id=?1,
         position=position+(SELECT COALESCE(MAX(position),-1)+1 FROM channel_sources WHERE channel_id=?1)
         WHERE channel_id=?2", params![keep, other],
    )?;
    tx.execute(
        "UPDATE OR IGNORE channel_favorites SET channel_id=?1 WHERE channel_id=?2",
        params![keep, other],
    )?;
    tx.execute(
        "UPDATE OR IGNORE epg_programs SET channel_id=?1 WHERE channel_id=?2",
        params![keep, other],
    )?;
    tx.execute(
        "UPDATE channels AS a SET logo=COALESCE(NULLIF(a.logo,''),b.logo),
         tvg_id=COALESCE(NULLIF(a.tvg_id,''),b.tvg_id) FROM channels b WHERE a.id=?1 AND b.id=?2",
        params![keep, other],
    )?;
    tx.execute("DELETE FROM channels WHERE id=?", [other])?;
    Ok(true)
}

fn save_channel(
    tx: &Connection,
    id: Option<i64>,
    entries: &[&PlaylistChannel],
    primary_url: &str,
    preserve_identity: bool,
) -> Result<(), AppError> {
    let first = entries[0];
    let logo = entries.iter().find(|e| !e.logo.is_empty()).map(|e| &e.logo);
    let tvg_id = entries
        .iter()
        .find(|e| !e.tvg_id.is_empty())
        .map(|e| &e.tvg_id);
    let id = if let Some(id) = id {
        tx.execute(
            "UPDATE channels SET name=CASE WHEN ?6 THEN name ELSE ?1 END,
             group_name=CASE WHEN ?6 THEN group_name ELSE ?2 END,
             logo=CASE WHEN ?6 AND logo!='' THEN logo ELSE COALESCE(?3,logo) END,
             tvg_id=CASE WHEN ?6 AND tvg_id!='' THEN tvg_id ELSE COALESCE(?4,tvg_id) END WHERE id=?5",
            params![first.name, first.group_name, logo, tvg_id, id, preserve_identity],
        )?;
        id
    } else {
        tx.execute(
            "INSERT INTO channels(name,group_name,logo,tvg_id,url) VALUES (?1,?2,COALESCE(?3,''),COALESCE(?4,''),?5)",
            params![first.name, first.group_name, logo, tvg_id, primary_url],
        )?;
        tx.last_insert_rowid()
    };
    let next_position: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position),-1)+1 FROM channel_sources WHERE channel_id=?1",
        [id],
        |row| row.get(0),
    )?;
    let mut upsert = tx.prepare_cached(
        "INSERT INTO channel_sources(channel_id,url,position,headers_json,catchup_mode,catchup_source)
         VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(channel_id,url) DO UPDATE SET
         latency_ms=CASE WHEN headers_json=excluded.headers_json THEN latency_ms END,
         last_ok_at=CASE WHEN headers_json=excluded.headers_json THEN last_ok_at END,
         fail_count=CASE WHEN headers_json=excluded.headers_json THEN fail_count ELSE 0 END,
         headers_json=excluded.headers_json,catchup_mode=excluded.catchup_mode,
         catchup_source=excluded.catchup_source
         WHERE headers_json!=excluded.headers_json OR catchup_mode!=excluded.catchup_mode
            OR catchup_source!=excluded.catchup_source",
    )?;
    for (offset, entry) in entries.iter().enumerate() {
        upsert.execute(params![
            id,
            entry.url,
            next_position + offset as i64,
            entry.headers_json,
            entry.catchup_mode,
            entry.catchup_source
        ])?;
    }
    Ok(())
}

async fn delete_channel(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    if state
        .conn()?
        .execute("DELETE FROM channels WHERE id=?1", [id])?
        == 0
    {
        return Err(AppError::not_found("Channel not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn http_url(raw: &str) -> Result<Url, AppError> {
    let parsed =
        Url::parse(raw).map_err(|_| AppError::bad_request("Enter a valid HTTP or HTTPS URL"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(AppError::bad_request(
            "Only HTTP and HTTPS URLs without embedded credentials are supported",
        ));
    }
    Ok(parsed)
}

#[derive(Debug, Serialize)]
pub(crate) struct Download {
    id: i64,
    title: String,
    #[serde(serialize_with = "public_download_url")]
    url: String,
    status: String,
    bytes_written: i64,
    total_bytes: Option<i64>,
    bytes_per_second: i64,
    percent: Option<f64>,
    error: Option<String>,
    relative_path: String,
    kind: String,
    attempts: i64,
    #[serde(flatten)]
    metadata: DownloadMetadata,
    #[serde(skip)]
    user_id: i64,
    #[serde(skip)]
    attempt_token: String,
}

fn public_download_url<S: serde::Serializer>(url: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(url.split(['?', '#']).next().unwrap_or_default())
}

fn download_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Download> {
    let written: i64 = row.get(4)?;
    let total: Option<i64> = row.get(5)?;
    Ok(Download {
        id: row.get(0)?,
        title: row.get(1)?,
        url: row.get(2)?,
        status: row.get(3)?,
        bytes_written: written,
        total_bytes: total,
        bytes_per_second: row.get(15)?,
        percent: total
            .filter(|n| *n > 0)
            .map(|n| (100.0 * written as f64 / n as f64).clamp(0.0, 100.0)),
        error: row.get(6)?,
        relative_path: row.get(7)?,
        kind: row.get(8)?,
        attempts: row.get(9)?,
        metadata: DownloadMetadata {
            cover: row.get(10)?,
            year: row.get(11)?,
            description: row.get(12)?,
        },
        user_id: row.get(13)?,
        attempt_token: row.get(14)?,
    })
}

const DOWNLOAD_COLUMNS: &str = "id,title,url,status,bytes_written,total_bytes,error,relative_path,kind,attempts,cover,year,description,user_id,attempt_token,CASE WHEN status='running' THEN bytes_per_second ELSE 0 END";

pub(crate) fn download_by_id(
    conn: &Connection,
    id: i64,
    user_id: i64,
) -> Result<Download, AppError> {
    conn.query_row(
        &format!("SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE id=?1 AND user_id=?2"),
        params![id, user_id],
        download_row,
    )
    .optional()?
    .ok_or_else(|| AppError::not_found("Download not found"))
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct DownloadMetadata {
    pub cover: String,
    pub year: String,
    pub description: String,
}

async fn list_downloads(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Download>>, AppError> {
    let conn = state.conn()?;
    let mut query = conn.prepare(&format!(
        "SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE user_id=?1 ORDER BY id DESC"
    ))?;
    let rows = query
        .query_map([user.id], download_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(rows))
}

#[derive(Default, Deserialize)]
struct DownloadInput {
    url: String,
    title: String,
    #[serde(default)]
    flag: String,
    direct: Option<bool>,
    #[serde(flatten)]
    metadata: DownloadMetadata,
}

fn download_extension(url: &Url) -> Result<String, AppError> {
    let Some((_, extension)) = url
        .path()
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
    else {
        return Ok("mp4".into());
    };
    let extension = extension.to_ascii_lowercase();
    if extension == "m3u8" {
        return Ok("mp4".into());
    }
    if !matches!(
        extension.as_str(),
        "mp4" | "m4v" | "webm" | "mkv" | "avi" | "mov" | "ts" | "mpeg" | "mpg" | "ogv"
    ) {
        return Err(AppError::bad_request(
            "Downloads need an HTTP(S) video file or .m3u8 playlist URL",
        ));
    }
    Ok(extension)
}

async fn create_download(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<DownloadInput>,
) -> Result<(StatusCode, Json<Download>), AppError> {
    let direct = input.direct.unwrap_or_else(|| {
        input.flag.is_empty() || crate::catalog::is_direct_media_url(&input.url)
    });
    let media = prepare_catalog_download(&state, &input.url, &input.flag, direct).await?;
    let conn = state.conn()?;
    let id = enqueue_prepared(
        &conn,
        &state,
        user.id,
        &input.title,
        &media,
        &input.metadata,
    )?;
    let result = download_by_id(&conn, id, user.id)?;
    Ok((StatusCode::CREATED, Json(result)))
}

pub fn enqueue_in(
    conn: &Connection,
    state: &Shared,
    user_id: i64,
    title: &str,
    raw_url: &str,
) -> Result<i64, AppError> {
    enqueue_prepared(
        conn,
        state,
        user_id,
        title,
        &PreparedDownload::direct(raw_url)?,
        &DownloadMetadata::default(),
    )
}

pub(crate) fn enqueue_prepared(
    conn: &Connection,
    state: &Shared,
    user_id: i64,
    title: &str,
    media: &PreparedDownload,
    metadata: &DownloadMetadata,
) -> Result<i64, AppError> {
    insert_download(
        conn,
        state,
        user_id,
        title,
        DownloadSource::Media(media, metadata),
    )
}

pub(crate) fn enqueue_telegram(
    conn: &Connection,
    state: &Shared,
    user_id: i64,
    title: &str,
    url: &str,
) -> Result<i64, AppError> {
    insert_download(conn, state, user_id, title, DownloadSource::Telegram(url))
}

enum DownloadSource<'a> {
    Media(&'a PreparedDownload, &'a DownloadMetadata),
    Telegram(&'a str),
}

fn insert_download(
    conn: &Connection,
    state: &Shared,
    user_id: i64,
    title: &str,
    source: DownloadSource<'_>,
) -> Result<i64, AppError> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(AppError::bad_request(
            "Give the download a title of 1 to 200 characters",
        ));
    }
    let empty = DownloadMetadata::default();
    let (url, extension, resolved, kind, metadata) = match source {
        DownloadSource::Media(media, metadata) => (
            media.original.as_str(),
            media.extension.as_str(),
            media.resolved.as_deref(),
            "direct",
            metadata,
        ),
        DownloadSource::Telegram(url) => (url, "mp4", None, "telegram", &empty),
    };
    let relative_path = unique_relative_path(conn, state, title, extension)?;
    conn.execute(
        "INSERT INTO downloads(user_id,title,url,relative_path,created_at,kind,cover,year,description,resolved_media) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![user_id, title, url, relative_path, now(), kind, metadata.cover, metadata.year, metadata.description, resolved],
    )?;
    state.download_runtime.wake();
    Ok(conn.last_insert_rowid())
}

fn unique_relative_path(
    conn: &Connection,
    state: &Shared,
    title: &str,
    extension: &str,
) -> Result<String, AppError> {
    let base = safe_title(title);
    for index in 1..=10_000 {
        let filename = if index == 1 {
            format!("{base}.{extension}")
        } else {
            format!("{base} ({index}).{extension}")
        };
        let relative_path = state
            .download_dir
            .join(&filename)
            .strip_prefix(&state.media_dir)
            .map_err(|_| {
                AppError::internal("Download directory must be inside the media directory")
            })?
            .to_string_lossy()
            .into_owned();
        let reserved: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM downloads WHERE relative_path=?1)
                 OR EXISTS(SELECT 1 FROM organizer_history WHERE status!='rolled_back'
                    AND ((source_root='media' AND source_path=?1) OR target_path=?1))",
            [&relative_path],
            |row| row.get(0),
        )?;
        if !reserved && !state.download_dir.join(&filename).exists() {
            return Ok(relative_path);
        }
    }
    Err(AppError::internal(
        "Could not allocate a unique download filename",
    ))
}

fn safe_title(title: &str) -> String {
    let mut result: String = title
        .chars()
        .take(80)
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    result = result.trim().to_owned();
    while result.len() > 160 {
        result.pop();
    }
    if result.is_empty() {
        "video".to_owned()
    } else {
        result
    }
}

async fn cancel_download(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    transition(
        &state,
        user.id,
        id,
        &["running", "queued", "paused", "failed"],
        "cancelled",
        false,
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn retry_download(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    transition(
        &state,
        user.id,
        id,
        &["failed", "cancelled"],
        "queued",
        true,
    )?;
    Ok(Json(json!({"status":"queued"})))
}

async fn pause_download(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    transition(&state, user.id, id, &["running", "queued"], "paused", false)?;
    Ok(Json(json!({"status":"paused"})))
}

async fn resume_download(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    transition(&state, user.id, id, &["paused"], "queued", false)?;
    Ok(Json(json!({"status":"queued"})))
}

fn transition(
    state: &Shared,
    user_id: i64,
    id: i64,
    from: &[&str],
    to: &str,
    reset_attempts: bool,
) -> Result<(), AppError> {
    let conn = state.conn()?;
    let (status, token) = conn
        .query_row(
            "SELECT status,attempt_token FROM downloads WHERE id=?1 AND user_id=?2",
            params![id, user_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Download not found"))?;
    if !from.contains(&status.as_str()) {
        return Err(AppError::bad_request(format!(
            "Cannot change a {status} download to {to}"
        )));
    }
    conn.execute(
        "UPDATE downloads SET status=?1,error=NULL,retry_at=0,
        bytes_written=CASE WHEN ?1='queued' THEN 0 ELSE bytes_written END,
        total_bytes=CASE WHEN ?1='queued' THEN NULL ELSE total_bytes END,
        attempts=CASE WHEN ?2 THEN 0 ELSE attempts END WHERE id=?3 AND user_id=?4",
        params![to, reset_attempts, id, user_id],
    )?;
    if let Some(cancel) = state.download_runtime.active.lock().unwrap().get(&token) {
        cancel.send_replace(true);
    }
    state.download_runtime.wake();
    Ok(())
}

async fn clear_downloads(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.conn()?;
    let removed = crate::tracking::clear_download_history(&mut conn, user.id)?;
    Ok(Json(json!({"removed":removed})))
}

#[derive(Default)]
pub struct DownloadRuntime {
    active: Mutex<HashMap<String, watch::Sender<bool>>>,
    notify: Notify,
    probe: tokio::sync::Mutex<Option<(String, Instant, bool)>>,
    scan: tokio::sync::Mutex<()>,
    pub(crate) library_worker: tokio::sync::Mutex<()>,
}

impl DownloadRuntime {
    pub fn wake(&self) {
        self.notify.notify_one();
    }
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = recover_jobs(&state) {
            tracing::error!(error = %error.1, "Could not recover download queue");
            return;
        }
        let slots = Arc::new(Semaphore::new(4));
        let mut tasks = JoinSet::new();
        loop {
            if let Err(error) = schedule(&state, &slots, &mut tasks) {
                tracing::error!(error = %error.1, "Could not schedule downloads");
            }
            tokio::select! {
                result = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = result { tracing::error!(%error, "Download worker stopped"); }
                }
                _ = state.download_runtime.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    })
}

fn schedule(
    state: &Shared,
    slots: &Arc<Semaphore>,
    tasks: &mut JoinSet<()>,
) -> Result<(), AppError> {
    let settings = {
        let conn = state.conn()?;
        download_settings(&conn)?
    };
    // Lowering concurrency drains outstanding permits without interrupting those transfers.
    while settings.enabled && 4 - slots.available_permits() < usize::from(settings.concurrency) {
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            break;
        };
        let Some(job) = claim_job(state)? else { break };
        let state = state.clone();
        let settings = settings.clone();
        tasks.spawn(async move {
            let _permit = permit;
            match transfer(&state, &job, &settings).await {
                Err(error) => {
                    if let Err(error) = record_failure(&state, &job, &error, settings.max_attempts) {
                        tracing::error!(error = %error.1, download_id=job.id, "Could not record download failure");
                    }
                }
                Ok(true) => {
                    // Publication already indexed the file and queued automation atomically.
                    // The automation consumer sends the notice after its final phase.
                    let automated = state.conn().and_then(|conn| crate::library::automation_enabled(&conn));
                    if matches!(automated, Ok(false)) {
                        let _scan = state.download_runtime.scan.lock().await;
                        let scan_state = state.clone();
                        match tokio::task::spawn_blocking(move || crate::library::scan_library(&scan_state)).await {
                            Ok(Ok(_)) => crate::notify::send_in_background(&state, "下载已入库", job.title),
                            Ok(Err(error)) => tracing::warn!(error = %error.1, "Could not scan completed download"),
                            Err(error) => tracing::warn!(%error, "Could not scan completed download"),
                        }
                    }
                }
                Ok(false) => {}
            }
        });
    }
    Ok(())
}

fn record_failure(
    state: &Shared,
    job: &Download,
    error: &str,
    max_attempts: u8,
) -> Result<(), AppError> {
    let retry = job.attempts < i64::from(max_attempts);
    state.conn()?.execute("UPDATE downloads SET status=?1,error=?2,retry_at=CAST(unixepoch('subsec')*1000 AS INTEGER)+?3
        WHERE id=?4 AND attempt_token=?5 AND status='running'",
        params![if retry { "queued" } else { "failed" }, error, job.attempts.min(8)*250, job.id, job.attempt_token])?;
    Ok(())
}

fn recover_jobs(state: &Shared) -> Result<(), AppError> {
    let mut conn = state.conn()?;
    let interrupted = {
        let mut query = conn.prepare(&format!(
            "SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE status='running'"
        ))?;
        query
            .query_map([], download_row)?
            .collect::<Result<Vec<_>, _>>()?
    };
    for job in interrupted {
        if let Ok(final_path) = download_path(state, &job.relative_path) {
            // Pre-M6 jobs staged a sibling .part file. New jobs stage inside their attempt directory.
            let stage = attempt_directory(state, &job);
            let part = if job.attempt_token.is_empty() {
                Some(final_path.with_extension("part"))
            } else {
                stage.as_ref().map(|path| path.join("video.part"))
            };
            if let Some(part) = part
                && let Some(size) = published_download_size(&part, &final_path)
            {
                crate::library::complete_download(&mut conn, job.id, size)?;
                if stage.is_none() {
                    let _ = std::fs::remove_file(part);
                }
            }
        }
    }
    // A crash between a committed pause/completion and TempDir::drop may leave a
    // private stage behind, even if the owner already cleared that job's history.
    if std::fs::canonicalize(&state.download_dir)? != state.download_dir {
        return Err(AppError::internal(
            "Download directory was moved or replaced",
        ));
    }
    for entry in std::fs::read_dir(&state.download_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((id, token)) = name
            .to_str()
            .and_then(|s| s.strip_prefix(".polyview-job-"))
            .and_then(|s| s.split_once('-'))
        else {
            continue;
        };
        if id.parse::<i64>().is_ok()
            && uuid::Uuid::parse_str(token).is_ok()
            && entry.file_type()?.is_dir()
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::warn!(%error, "Could not remove interrupted download directory");
        }
    }
    conn.execute("UPDATE downloads SET status='queued',bytes_written=0,total_bytes=NULL,error=NULL,retry_at=0 WHERE status='running'", [])?;
    Ok(())
}

fn published_download_size(part: &std::path::Path, final_path: &std::path::Path) -> Option<i64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let partial = std::fs::symlink_metadata(part).ok()?;
        let complete = std::fs::symlink_metadata(final_path).ok()?;
        if partial.is_file()
            && complete.is_file()
            && complete.len() > 0
            && partial.dev() == complete.dev()
            && partial.ino() == complete.ino()
        {
            return i64::try_from(complete.len()).ok();
        }
    }
    #[cfg(not(unix))]
    let _ = (part, final_path);
    None
}

fn claim_job(state: &Shared) -> Result<Option<Download>, AppError> {
    let conn = state.conn()?;
    Ok(conn.query_row(&format!("UPDATE downloads SET status='running', attempts=attempts+1, attempt_token=?1,
        bytes_written=0,total_bytes=NULL,bytes_per_second=0,error=NULL WHERE id=(SELECT id FROM downloads WHERE status='queued'
        AND retry_at <= CAST(unixepoch('subsec')*1000 AS INTEGER) ORDER BY id LIMIT 1) RETURNING {DOWNLOAD_COLUMNS}"),
        [uuid::Uuid::new_v4().to_string()], download_row).optional()?)
}

fn is_running(conn: &Connection, job: &Download) -> Result<bool, String> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM downloads WHERE id=?1 AND attempt_token=?2 AND user_id=?3 AND status='running')",
        params![job.id, job.attempt_token, job.user_id], |row| row.get(0)).map_err(|e| e.to_string())
}

fn attempt_directory(state: &Shared, job: &Download) -> Option<PathBuf> {
    let token = uuid::Uuid::parse_str(&job.attempt_token).ok()?;
    Some(
        state
            .download_dir
            .join(format!(".polyview-job-{}-{token}", job.id)),
    )
}

struct ActiveAttempt<'a> {
    runtime: &'a DownloadRuntime,
    token: &'a str,
}

impl Drop for ActiveAttempt<'_> {
    fn drop(&mut self) {
        self.runtime.active.lock().unwrap().remove(self.token);
    }
}

fn download_path(state: &Shared, relative: &str) -> Result<PathBuf, String> {
    let final_path = state.media_dir.join(relative);
    if !final_path.starts_with(&state.download_dir)
        || final_path.parent() != Some(state.download_dir.as_path())
    {
        return Err("Download path is outside the configured download directory".to_owned());
    }
    let canonical = std::fs::canonicalize(&state.download_dir).map_err(|e| e.to_string())?;
    if canonical != state.download_dir {
        return Err("Download directory was moved or replaced".to_owned());
    }
    Ok(final_path)
}

async fn transfer(
    state: &Shared,
    job: &Download,
    settings: &DownloadSettings,
) -> Result<bool, String> {
    let runtime = &state.download_runtime;
    let (cancel, mut cancelled) = watch::channel(false);
    runtime
        .active
        .lock()
        .unwrap()
        .insert(job.attempt_token.clone(), cancel);
    let _active = ActiveAttempt {
        runtime,
        token: &job.attempt_token,
    };
    let running = || {
        let conn = state.conn().map_err(|e| e.1)?;
        is_running(&conn, job)
    };
    if !running()? {
        return Ok(false);
    }
    let future = transfer_attempt(state, job, settings);
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            _ = cancelled.changed() => return Ok(false),
            result = &mut future => return result,
            // Account removal cascades through SQLite without calling the job endpoints.
            _ = tokio::time::sleep(Duration::from_secs(1)) => if !running()? { return Ok(false) },
        }
    }
}

async fn transfer_attempt(
    state: &Shared,
    job: &Download,
    settings: &DownloadSettings,
) -> Result<bool, String> {
    download_path(state, &job.relative_path)?;
    let stage = attempt_directory(state, job).ok_or("Invalid download attempt token")?;
    let mut builder = tempfile::Builder::new();
    builder
        .prefix(stage.file_name().ok_or("Invalid download attempt path")?)
        .rand_bytes(0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let work = builder
        .tempdir_in(&state.download_dir)
        .map_err(|e| format!("Cannot create download directory: {e}"))?;
    let part = work.path().join("video.part");
    let mut extension = None;
    if job.kind == "telegram" {
        let mut last_saved = Instant::now();
        let mut last_bytes = 0;
        let video =
            crate::telegram::media::download(
                state,
                job.user_id,
                settings,
                &job.url,
                work.path(),
                |written, total| {
                    let elapsed = last_saved.elapsed();
                    if last_bytes > 0
                        && elapsed < Duration::from_millis(250)
                        && Some(written) != total
                    {
                        return Ok(());
                    }
                    let rate = (written.saturating_sub(last_bytes) as f64
                        / elapsed.as_secs_f64().max(0.001)) as i64;
                    state.conn().map_err(|e| e.1)?.execute(
                    "UPDATE downloads SET bytes_written=?1,total_bytes=?2,bytes_per_second=?3
                     WHERE id=?4 AND attempt_token=?5 AND status='running'",
                    params![written, total, rate, job.id, job.attempt_token],
                ).map_err(|e| e.to_string())?;
                    last_saved = Instant::now();
                    last_bytes = written;
                    Ok(())
                },
            )
            .await?;
        extension = Some(
            video
                .extension()
                .and_then(|s| s.to_str())
                .ok_or("Telegram video has no extension")?
                .to_ascii_lowercase(),
        );
        tokio::fs::rename(video, &part)
            .await
            .map_err(|e| e.to_string())?;
    } else {
        let (url, private) = resolved::transfer_source(state, job)?;
        if url.path().to_ascii_lowercase().ends_with(".m3u8") {
            let relay = crate::playback::DownloadRelay::start(url, private.unwrap_or_default())
                .await
                .map_err(|e| e.1)?;
            transfer_hls(state, job, settings, work.path(), &part, &relay.url).await?;
        } else {
            let response = if let Some(headers) = &private {
                crate::playback::fetch_media(url, Some(headers), &Default::default())
                    .await
                    .map_err(|e| e.1)?
                    .0
            } else {
                tokio::time::timeout(Duration::from_secs(30), state.client.get(url).send())
                    .await
                    .map_err(|_| "Source stopped responding for 30 seconds")?
                    .map_err(|_| "Could not connect to the download source")?
            };
            transfer_direct(state, job, &part, response).await?;
        }
    }
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .await
        .map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    let size = i64::try_from(file.metadata().await.map_err(|e| e.to_string())?.len())
        .map_err(|_| "Video is too large")?;
    if size == 0 {
        return Err("Source returned an empty video".into());
    }
    drop(file);
    publish_download(state, job, size, &part, extension.as_deref())
}

async fn transfer_direct(
    state: &Shared,
    job: &Download,
    part: &FsPath,
    response: reqwest::Response,
) -> Result<(), String> {
    let response = response.error_for_status().map_err(|e| {
        format!(
            "Source returned HTTP {}",
            e.status().map_or(0, |s| s.as_u16())
        )
    })?;
    if let Some(content_type) = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let content_type = content_type.to_ascii_lowercase();
        if content_type.contains("text/html")
            || content_type.contains("mpegurl")
            || content_type.contains("application/json")
        {
            return Err(
                "Source returned a page or playlist instead of a direct video file".to_owned(),
            );
        }
    }
    let total_bytes = response
        .content_length()
        .and_then(|n| i64::try_from(n).ok());
    state
        .conn()
        .map_err(|e| e.1)?
        .execute(
            "UPDATE downloads SET total_bytes=?1 WHERE id=?2 AND attempt_token=?3 AND status='running'",
            params![total_bytes, job.id, job.attempt_token],
        )
        .map_err(|e| e.to_string())?;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(part)
        .await
        .map_err(|e| format!("Cannot create download file: {e}"))?;
    let mut stream = response.bytes_stream();
    let mut written: i64 = 0;
    let mut last_saved = std::time::Instant::now();
    loop {
        let Some(chunk) = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .map_err(|_| "Source stopped responding for 30 seconds")?
        else {
            break;
        };
        let chunk = chunk.map_err(|_| "Connection ended before the video finished downloading")?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Cannot write download file: {e}"))?;
        written = written
            .checked_add(chunk.len() as i64)
            .ok_or("Download is too large")?;
        if last_saved.elapsed() >= Duration::from_millis(500) {
            update_progress(state, job, written)?;
            last_saved = std::time::Instant::now();
        }
    }
    if written == 0 {
        return Err("Source returned an empty video".to_owned());
    }
    if total_bytes.is_some_and(|total| written != total) {
        return Err("Source sent an incomplete video".to_owned());
    }
    file.flush().await.map_err(|e| e.to_string())
}

fn update_progress(state: &Shared, job: &Download, written: i64) -> Result<(), String> {
    state.conn().map_err(|e| e.1)?.execute("UPDATE downloads SET bytes_written=?1 WHERE id=?2 AND attempt_token=?3 AND status='running'",
        params![written, job.id, job.attempt_token]).map_err(|e| e.to_string())?;
    Ok(())
}

fn command(binary: &str) -> Command {
    let mut command = Command::new(binary);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn preferred_available(runtime: &DownloadRuntime, binary: &str) -> bool {
    let mut cached = runtime.probe.lock().await;
    if let Some((path, checked, available)) = &*cached
        && path == binary
        && checked.elapsed() < Duration::from_secs(60)
    {
        return *available;
    }
    let available = matches!(tokio::time::timeout(Duration::from_secs(3), command(binary).arg("--version").status()).await, Ok(Ok(status)) if status.success());
    *cached = Some((binary.to_owned(), Instant::now(), available));
    available
}

async fn transfer_hls(
    state: &Shared,
    job: &Download,
    settings: &DownloadSettings,
    work: &FsPath,
    part_path: &FsPath,
    relay_url: &str,
) -> Result<(), String> {
    if preferred_available(&state.download_runtime, &settings.n_m3u8dl_path).await {
        let mut preferred = command(&settings.n_m3u8dl_path);
        preferred
            .arg(relay_url)
            .arg("--save-dir")
            .arg(work)
            .arg("--tmp-dir")
            .arg(work.join("segments"))
            .args([
                "--save-name",
                "video",
                "--auto-select",
                "--mux-after-done",
                "format=mp4:muxer=ffmpeg",
            ]);
        if settings.ffmpeg_path != "ffmpeg" {
            preferred
                .arg("--ffmpeg-binary-path")
                .arg(&settings.ffmpeg_path);
        }
        if preferred.status().await.is_ok_and(|s| s.success()) {
            for name in ["video.MUX.mp4", "video.mp4"] {
                let candidate = work.join(name);
                if tokio::fs::symlink_metadata(&candidate)
                    .await
                    .is_ok_and(|m| m.is_file() && m.len() > 0)
                {
                    tokio::fs::rename(candidate, part_path)
                        .await
                        .map_err(|e| e.to_string())?;
                    return Ok(());
                }
            }
        }
    }
    let mut child = command(&settings.ffmpeg_path)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-n",
            "-protocol_whitelist",
            "http,https,tcp,tls,crypto",
            "-rw_timeout",
            "30000000",
            "-user_agent",
            "Mozilla/5.0",
            "-i",
        ])
        .arg(relay_url)
        .args([
            "-map",
            "0:v?",
            "-map",
            "0:a?",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-f",
            "mp4",
        ])
        .arg(part_path)
        .spawn()
        .map_err(|_| "HLS downloads require FFmpeg installed on the server".to_string())?;
    loop {
        tokio::select! {
            result = child.wait() => {
                if !result.map_err(|e| e.to_string())?.success() {
                    return Err("FFmpeg could not download this HLS stream; check the source, permissions, and available disk space".into());
                }
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                if let Ok(metadata) = tokio::fs::metadata(part_path).await {
                    update_progress(state, job, i64::try_from(metadata.len()).map_err(|_| "Video is too large")?)?;
                }
            }
        }
    }
}

fn publish_download(
    state: &Shared,
    job: &Download,
    written: i64,
    part_path: &FsPath,
    extension: Option<&str>,
) -> Result<bool, String> {
    let mut conn = state.conn().map_err(|e| e.1)?;
    if !is_running(&conn, job)? {
        return Ok(false);
    }
    let mut final_path = download_path(state, &job.relative_path)?;
    if let Some(extension) = extension
        && final_path.extension().and_then(|s| s.to_str()) != Some(extension)
    {
        let relative =
            unique_relative_path(&conn, state, &job.title, extension).map_err(|e| e.1)?;
        final_path = download_path(state, &relative)?;
        conn.execute(
            "UPDATE downloads SET relative_path=?1 WHERE id=?2 AND attempt_token=?3",
            params![relative, job.id, job.attempt_token],
        )
        .map_err(|e| e.to_string())?;
    }
    // Publishing with a hard link is atomic and fails if an existing media file occupies the name.
    std::fs::hard_link(part_path, &final_path)
        .map_err(|e| format!("Cannot publish completed video: {e}"))?;
    match crate::library::complete_download(&mut conn, job.id, written) {
        Ok(true) => Ok(true),
        result => {
            let _ = std::fs::remove_file(final_path);
            result.map_err(|e| e.1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn state() -> (TempDir, Shared) {
        let temp = tempfile::tempdir().unwrap();
        let media_dir = temp.path().canonicalize().unwrap();
        let download_dir = media_dir.join("downloads");
        std::fs::create_dir(&download_dir).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1),(2);",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        crate::library::init_schema(&conn).unwrap();
        crate::catalog::init_schema(&conn).unwrap();
        crate::user_data::init_schema(&conn).unwrap();
        crate::settings::init_schema(&conn).unwrap();
        crate::tracking::init_schema(&conn).unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            epg_refresh: Default::default(),
            playback_grants: Default::default(),
            telegram_native: Default::default(),
            cloud_fixture: None,
            download_runtime: Default::default(),
            media_dir,
            download_dir,
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        (temp, state)
    }

    fn user(id: i64) -> AuthUser {
        AuthUser {
            id,
            username: format!("user{id}"),
            role: "user".to_owned(),
        }
    }

    #[test]
    fn playlist_handles_metadata_commas_duplicates_and_crlf() {
        let text = "\u{feff}#EXTM3U\r\n#EXTINF:-1 tvg-logo=\"https://example.com/logo.png\" group-title=\"News, local\",Channel one\r\nhttp://192.168.1.2/live.m3u8\r\n#EXTINF:-1,Duplicate\r\nhttp://192.168.1.2/live.m3u8\r\n";
        let channels = parse_playlist(text).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].group_name, "News, local");
        assert_eq!(channels[0].name, "Channel one");
        assert!(parse_playlist("#EXTM3U\n#EXTINF:-1,No URL").is_err());
        assert!(parse_playlist("#EXTM3U\n#EXTINF:-1,Private\nfile:///etc/passwd").is_err());
        assert!(parse_playlist("#EXTM3U\nhttps://example.com/a.mp4").is_err());
    }

    #[tokio::test]
    async fn channel_import_keeps_multiple_sources_for_one_name() {
        let (_temp, state) = state();
        let admin = AuthUser {
            id: 1,
            username: "admin".into(),
            role: "admin".into(),
        };
        let playlist = "#EXTM3U\n#EXTINF:-1 group-title=\"央视\",CCTV-1\nhttps://a.example/1.m3u8\n#EXTINF:-1 group-title=\"央视\",CCTV-1\nhttps://b.example/1.m3u8\n#EXTINF:-1 group-title=\"央视\",CCTV-2\nhttps://c.example/2.m3u8\n";
        for _ in 0..2 {
            let _ = import_channels(
                State(state.clone()),
                admin.clone(),
                Json(PlaylistInput {
                    playlist: playlist.into(),
                }),
            )
            .await
            .unwrap();
        }
        let Json(channels) = list_channels(State(state.clone()), admin).await.unwrap();
        assert_eq!(channels.len(), 2);
        let cctv1 = channels
            .iter()
            .find(|channel| channel.name == "CCTV-1")
            .unwrap();
        assert_eq!(cctv1.sources.len(), 2);
        assert_eq!(cctv1.sources[0].url, "https://a.example/1.m3u8");
        assert_eq!(cctv1.sources[1].url, "https://b.example/1.m3u8");
    }

    #[tokio::test]
    async fn channel_import_merges_same_name_when_one_url_already_exists() {
        let (_temp, state) = state();
        let admin = AuthUser {
            id: 1,
            username: "admin".into(),
            role: "admin".into(),
        };
        let import = |playlist: &'static str| {
            import_channels(
                State(state.clone()),
                admin.clone(),
                Json(PlaylistInput {
                    playlist: playlist.into(),
                }),
            )
        };
        let _ = import("#EXTM3U\n#EXTINF:-1 group-title=\"旧\",旧频道\nhttps://a.example/1.m3u8\n")
            .await
            .unwrap();
        let _ = import("#EXTM3U\n#EXTINF:-1 group-title=\"央视\",CCTV-1\nhttps://b.example/1.m3u8\n#EXTINF:-1 group-title=\"央视\",CCTV-1\nhttps://a.example/1.m3u8\n")
            .await
            .unwrap();
        let Json(channels) = list_channels(State(state), admin).await.unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "CCTV-1");
        assert_eq!(channels[0].sources.len(), 2);
    }

    #[test]
    fn download_urls_and_filenames_cannot_escape() {
        assert!(http_url("file:///etc/passwd").is_err());
        assert!(http_url("https://user:password@example.com/video.mp4").is_err());
        assert_eq!(
            download_extension(&http_url("https://example.com/a.m3u8").unwrap()).unwrap(),
            "mp4"
        );
        assert_eq!(
            download_extension(&http_url("https://example.com/a.MP4?token=abc").unwrap()).unwrap(),
            "mp4"
        );
        assert!(!safe_title("../../evil\\file.mp4").contains('/'));
        assert!(safe_title(&"影".repeat(80)).len() <= 160);
        let (_temp, state) = state();
        assert!(download_path(&state, "downloads/../../outside.mp4").is_err());
        assert!(download_path(&state, "downloads/a.mp4").is_ok());
    }

    #[test]
    fn downloads_get_clean_unique_filenames() {
        let (_temp, state) = state();
        let conn = state.conn().unwrap();
        let first = enqueue_in(&conn, &state, 1, "第01集", "https://example.com/a.m3u8").unwrap();
        let second = enqueue_in(&conn, &state, 1, "第01集", "https://example.com/b.m3u8").unwrap();
        std::fs::write(state.download_dir.join("已存在.mp4"), b"").unwrap();
        let third = enqueue_in(&conn, &state, 1, "已存在", "https://example.com/c.mp4").unwrap();
        let path = |id: i64| -> String {
            conn.query_row(
                "SELECT relative_path FROM downloads WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(path(first), "downloads/第01集.mp4");
        assert_eq!(path(second), "downloads/第01集 (2).mp4");
        assert_eq!(path(third), "downloads/已存在 (2).mp4");
    }

    #[tokio::test]
    async fn download_ownership_cancellation_retry_and_recovery() {
        let (_temp, state) = state();
        let (_, Json(job)) = create_download(
            State(state.clone()),
            user(1),
            Json(DownloadInput {
                url: "https://example.com/a.mp4".into(),
                title: "Episode 1".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            list_downloads(State(state.clone()), user(2))
                .await
                .unwrap()
                .0
                .len(),
            0
        );
        assert_eq!(
            cancel_download(State(state.clone()), user(2), Path(job.id))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let attempt = claim_job(&state).unwrap().unwrap();
        assert_eq!(attempt.id, job.id);
        assert!(is_running(&state.conn().unwrap(), &attempt).unwrap());
        cancel_download(State(state.clone()), user(1), Path(job.id))
            .await
            .unwrap();
        assert!(!is_running(&state.conn().unwrap(), &attempt).unwrap());
        let _ = retry_download(State(state.clone()), user(1), Path(job.id))
            .await
            .unwrap();
        claim_job(&state).unwrap();
        recover_jobs(&state).unwrap();
        assert_eq!(
            list_downloads(State(state.clone()), user(1))
                .await
                .unwrap()
                .0[0]
                .status,
            "queued"
        );
    }

    #[tokio::test]
    async fn transfer_downloads_bytes_and_does_not_overwrite() {
        let (_temp, state) = state();
        let server = axum::Router::new().route("/video.mp4", get(|| async { "test video bytes" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let (_, Json(created)) = create_download(
            State(state.clone()),
            user(1),
            Json(DownloadInput {
                url: format!("http://{address}/video.mp4"),
                title: "Video".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let job = claim_job(&state).unwrap().unwrap();
        transfer(&state, &job, &DownloadSettings::default())
            .await
            .unwrap();
        let final_path = state.media_dir.join(&created.relative_path);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"test video bytes");
        assert_eq!(
            list_downloads(State(state.clone()), user(1))
                .await
                .unwrap()
                .0[0]
                .status,
            "completed"
        );
        state
            .conn()
            .unwrap()
            .execute(
                "UPDATE downloads SET status='running' WHERE id=?1",
                [job.id],
            )
            .unwrap();
        assert!(
            transfer(&state, &job, &DownloadSettings::default())
                .await
                .unwrap_err()
                .contains("Cannot publish")
        );
        assert_eq!(std::fs::read(&final_path).unwrap(), b"test video bytes");
        server_task.abort();
    }

    async fn wait_for(check: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !check() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("worker did not reach the expected state");
    }

    #[tokio::test]
    async fn worker_cancels_hanging_transfer_retries_and_adds_completed_video_to_library() {
        use axum::body::{Body, Bytes};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_temp, state) = state();
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = attempts.clone();
        let server = axum::Router::new().route(
            "/video.mp4",
            get(move || {
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                async move {
                    let body = if attempt == 0 {
                        Body::from_stream(
                            futures_util::stream::iter([Ok::<_, std::io::Error>(
                                Bytes::from_static(b"partial"),
                            )])
                            .chain(futures_util::stream::pending()),
                        )
                    } else {
                        Body::from("complete video")
                    };
                    axum::http::Response::builder()
                        .header("content-type", "video/mp4")
                        .body(body)
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let (_, Json(job)) = create_download(
            State(state.clone()),
            user(1),
            Json(DownloadInput {
                url: format!("http://{address}/video.mp4"),
                title: "Downloaded video".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let final_path = state.media_dir.join(&job.relative_path);
        let worker = start_worker(state.clone());
        let partial_path = || {
            let conn = state.conn().unwrap();
            attempt_directory(&state, &download_by_id(&conn, job.id, 1).unwrap())
                .map(|dir| dir.join("video.part"))
        };
        wait_for(|| {
            partial_path().is_some_and(|part| std::fs::metadata(part).is_ok_and(|m| m.len() > 0))
        })
        .await;
        let part = partial_path().unwrap();
        assert!(!final_path.exists());
        cancel_download(State(state.clone()), user(1), Path(job.id))
            .await
            .unwrap();
        // Retry immediately, while the first transfer is still waiting for more bytes.
        let _ = retry_download(State(state.clone()), user(1), Path(job.id))
            .await
            .unwrap();
        wait_for(|| {
            let conn = state.conn().unwrap();
            conn.query_row(
                "SELECT status='completed' FROM downloads WHERE id=?1",
                [job.id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
                && !part.exists()
                && conn
                    .query_row(
                        "SELECT COUNT(*) FROM library_media WHERE available=1",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap()
                    == 1
        })
        .await;
        assert_eq!(std::fs::read(&final_path).unwrap(), b"complete video");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        worker.abort();
        server_task.abort();
    }

    #[tokio::test]
    async fn deleting_an_account_removes_its_jobs_and_stops_active_transfer() {
        let (_temp, state) = state();
        let (_, Json(created)) = create_download(
            State(state.clone()),
            user(2),
            Json(DownloadInput {
                url: "https://example.com/video.mp4".into(),
                title: "Video".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        let job = claim_job(&state).unwrap().unwrap();
        assert_eq!(job.id, created.id);
        state
            .conn()
            .unwrap()
            .execute("DELETE FROM users WHERE id=2", [])
            .unwrap();
        assert!(!is_running(&state.conn().unwrap(), &job).unwrap());
        assert!(
            !transfer(&state, &job, &DownloadSettings::default())
                .await
                .unwrap()
        );
        assert!(
            list_downloads(State(state), user(2))
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }

    fn queue(state: &Shared, owner: i64, title: &str, url: &str) -> i64 {
        let conn = state.conn().unwrap();
        enqueue_in(&conn, state, owner, title, url).unwrap()
    }

    fn job_for(state: &Shared, id: i64, owner: i64) -> Download {
        download_by_id(&state.conn().unwrap(), id, owner).unwrap()
    }

    fn configure(state: &Shared, settings: &DownloadSettings) {
        crate::settings::set_setting_json(&state.conn().unwrap(), "downloads", settings).unwrap();
        state.download_runtime.wake();
    }

    #[tokio::test]
    async fn concurrent_pause_resume_and_cleanup_are_scoped_to_the_owner() {
        use axum::routing::get;
        use tokio::sync::{mpsc, oneshot};
        let (_temp, state) = state();
        let (requests, mut pending) = mpsc::unbounded_channel();
        let router = Router::new().route(
            "/{video}",
            get(move |Path(video): Path<String>| {
                let requests = requests.clone();
                async move {
                    let (reply, wait) = oneshot::channel::<String>();
                    requests.send((video, reply)).unwrap();
                    wait.await.unwrap_or_default()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut settings = DownloadSettings {
            enabled: false,
            concurrency: 2,
            ..Default::default()
        };
        configure(&state, &settings);
        let first = queue(&state, 1, "First", &format!("{base}/first.mp4"));
        let second = queue(&state, 2, "Second", &format!("{base}/second.mp4"));
        let third = queue(&state, 1, "Third", &format!("{base}/third.mp4"));
        let worker = start_worker(state.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pending.recv())
                .await
                .is_err()
        );
        settings.enabled = true;
        configure(&state, &settings);
        let mut replies = HashMap::new();
        for _ in 0..2 {
            let (video, sender) = tokio::time::timeout(Duration::from_secs(5), pending.recv())
                .await
                .unwrap()
                .unwrap();
            replies.insert(video, sender);
        }
        assert_eq!(job_for(&state, first, 1).status, "running");
        assert_eq!(job_for(&state, second, 2).status, "running");
        assert_eq!(job_for(&state, third, 1).status, "queued");
        assert_eq!(
            pause_download(State(state.clone()), user(2), Path(first))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let old = job_for(&state, first, 1);
        let _ = pause_download(State(state.clone()), user(1), Path(first))
            .await
            .unwrap();
        let (video, third_reply) = tokio::time::timeout(Duration::from_secs(3), pending.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(video, "third.mp4");
        assert_eq!(job_for(&state, first, 1).status, "paused");
        assert!(!attempt_directory(&state, &old).unwrap().exists());
        assert_eq!(
            resume_download(State(state.clone()), user(2), Path(first))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let _ = resume_download(State(state.clone()), user(1), Path(first))
            .await
            .unwrap();
        assert_eq!(job_for(&state, first, 1).status, "queued");
        replies
            .remove("second.mp4")
            .unwrap()
            .send("second video".into())
            .unwrap();
        let (video, resumed) = tokio::time::timeout(Duration::from_secs(3), pending.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(video, "first.mp4");
        let fresh = job_for(&state, first, 1);
        assert_ne!(fresh.attempt_token, old.attempt_token);
        assert!(!is_running(&state.conn().unwrap(), &old).unwrap());
        record_failure(&state, &old, "late fault", 1).unwrap();
        update_progress(&state, &old, 9999).unwrap();
        assert!(!publish_download(&state, &old, 1, FsPath::new("stale.part"), None).unwrap());
        assert_eq!(job_for(&state, first, 1).status, "running");
        assert_eq!(job_for(&state, first, 1).bytes_written, 0);
        let _ = replies
            .remove("first.mp4")
            .unwrap()
            .send("discarded video".into());
        cancel_download(State(state.clone()), user(1), Path(third))
            .await
            .unwrap();
        drop(third_reply);
        resumed.send("resumed video".into()).unwrap();
        wait_for(|| job_for(&state, first, 1).status == "completed").await;
        assert_eq!(
            clear_downloads(State(state.clone()), user(1))
                .await
                .unwrap()
                .0["removed"],
            2
        );
        assert_eq!(job_for(&state, second, 2).status, "completed");
        assert!(
            list_downloads(State(state.clone()), user(1))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::read(state.media_dir.join(fresh.relative_path)).unwrap(),
            b"resumed video"
        );
        worker.abort();
        server.abort();
    }

    #[tokio::test]
    async fn transient_failures_retry_and_terminal_failures_stop_at_the_configured_limit() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_temp, state) = state();
        configure(
            &state,
            &DownloadSettings {
                max_attempts: 2,
                concurrency: 2,
                ..Default::default()
            },
        );
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let router = Router::new()
            .route(
                "/temporary.mp4",
                get(move || {
                    let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
                    async move {
                        if first {
                            (StatusCode::BAD_GATEWAY, "")
                        } else {
                            (StatusCode::OK, "video")
                        }
                    }
                }),
            )
            .route("/bad.mp4", get(|| async { StatusCode::BAD_GATEWAY }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let good = queue(&state, 1, "Good", &format!("{base}/temporary.mp4"));
        let bad = queue(&state, 1, "Bad", &format!("{base}/bad.mp4"));
        let worker = start_worker(state.clone());
        wait_for(|| {
            job_for(&state, good, 1).status == "completed"
                && job_for(&state, bad, 1).status == "failed"
        })
        .await;
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(job_for(&state, good, 1).attempts, 2);
        assert_eq!(job_for(&state, bad, 1).attempts, 2);
        assert!(job_for(&state, bad, 1).error.unwrap().contains("502"));
        assert!(!state.download_dir.join("Bad.mp4").exists());
        worker.abort();
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preferred_hls_probe_is_cached_and_failure_or_unavailability_uses_configured_ffmpeg() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, state) = state();
        let stub = |name: &str, script: &str| {
            let path = temp.path().join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path.to_str().unwrap().to_owned()
        };
        let primary = stub(
            "preferred tool",
            r#"#!/bin/sh
set -eu
if [ "$1" = '--version' ]; then printf 'probe\n' >> "$0.log"; exit 0; fi
url=$1
shift
while [ "$#" -gt 0 ]; do
  case "$1" in
    --save-dir) dir=$2; shift;;
    --save-name) name=$2; shift;;
  esac
  shift
done
printf '%s\n' "$url" >> "$0.log"
if [ -f "$0.fail" ]; then printf partial > "$dir/$name.MUX.mp4"; exit 1; fi
printf preferred > "$dir/$name.MUX.mp4"
"#,
        );
        let fallback = stub(
            "fallback tool",
            r#"#!/bin/sh
set -eu
for last in "$@"; do :; done
printf 'fallback\n' >> "$0.log"
printf fallback > "$last"
"#,
        );
        let mut settings = DownloadSettings {
            n_m3u8dl_path: primary.clone(),
            ffmpeg_path: fallback.clone(),
            ..Default::default()
        };
        configure(&state, &settings);
        let worker = start_worker(state.clone());
        for (title, expected) in [
            ("priority", "preferred"),
            ("failed", "fallback"),
            ("unavailable", "fallback"),
        ] {
            if title == "failed" {
                std::fs::write(format!("{primary}.fail"), "").unwrap();
            }
            if title == "unavailable" {
                settings.n_m3u8dl_path = temp.path().join("missing").to_str().unwrap().to_owned();
                configure(&state, &settings);
            }
            let id = queue(
                &state,
                1,
                title,
                &format!("http://127.0.0.1/{title}.m3u8?token=secret"),
            );
            wait_for(|| job_for(&state, id, 1).status == "completed").await;
            let job = job_for(&state, id, 1);
            assert_eq!(job.attempts, 1);
            assert_eq!(
                std::fs::read_to_string(state.media_dir.join(job.relative_path)).unwrap(),
                expected
            );
        }
        let calls = std::fs::read_to_string(format!("{primary}.log")).unwrap();
        let mut calls = calls.lines();
        assert_eq!(calls.next(), Some("probe"));
        for _ in 0..2 {
            let call = calls.next().unwrap();
            assert!(
                call.starts_with("http://127.0.0.1:") && call.contains("/api/playback/grants/")
            );
            assert!(call.ends_with(".m3u8"));
            assert!(!call.contains("secret"));
        }
        assert_eq!(calls.next(), None);
        assert_eq!(
            std::fs::read_to_string(format!("{fallback}.log")).unwrap(),
            "fallback\nfallback\n"
        );
        worker.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_recognizes_published_hard_link_and_preserves_different_files() {
        let (_temp, state) = state();
        let (_, Json(job)) = create_download(
            State(state.clone()),
            user(1),
            Json(DownloadInput {
                url: "https://example.com/video.mp4".into(),
                title: "Video".into(),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        // A database migrated from the previous worker has no attempt token.
        state
            .conn()
            .unwrap()
            .execute(
                "UPDATE downloads SET status='running' WHERE id=?1",
                [job.id],
            )
            .unwrap();
        let final_path = state.media_dir.join(&job.relative_path);
        let part = final_path.with_extension("part");
        std::fs::write(&part, b"complete video").unwrap();
        std::fs::hard_link(&part, &final_path).unwrap();
        recover_jobs(&state).unwrap();
        assert!(!part.exists());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"complete video");
        let Json(jobs) = list_downloads(State(state.clone()), user(1)).await.unwrap();
        assert_eq!(jobs[0].status, "completed");
        assert_eq!(jobs[0].bytes_written, 14);
        std::fs::write(&part, b"unrelated partial").unwrap();
        state
            .conn()
            .unwrap()
            .execute(
                "UPDATE downloads SET status='running' WHERE id=?1",
                [job.id],
            )
            .unwrap();
        recover_jobs(&state).unwrap();
        assert_eq!(std::fs::read(&final_path).unwrap(), b"complete video");
        assert_eq!(std::fs::read(&part).unwrap(), b"unrelated partial");
        let Json(jobs) = list_downloads(State(state), user(1)).await.unwrap();
        assert_eq!(jobs[0].status, "queued");
    }
}
