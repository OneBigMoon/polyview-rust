use crate::{AppError, Shared, auth::AuthUser, catalog::adult_sources_enabled, network, now};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{delete, get, put},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS online_progress (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE CASCADE,
        media_id TEXT NOT NULL, title TEXT NOT NULL, cover TEXT NOT NULL DEFAULT '',
        episode_index INTEGER NOT NULL DEFAULT 0, episode_name TEXT NOT NULL DEFAULT '',
        remote_url TEXT NOT NULL DEFAULT '', position REAL NOT NULL DEFAULT 0,
        duration REAL NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL,
        PRIMARY KEY(user_id,source_id,media_id));
        CREATE INDEX IF NOT EXISTS online_progress_updated ON online_progress(user_id,updated_at DESC);
        CREATE TABLE IF NOT EXISTS catalog_favorites (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE CASCADE,
        media_id TEXT NOT NULL, title TEXT NOT NULL, cover TEXT NOT NULL DEFAULT '',
        year TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL DEFAULT 'other', remark TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL, PRIMARY KEY(user_id,source_id,media_id));
        CREATE TABLE IF NOT EXISTS channel_favorites (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
        created_at INTEGER NOT NULL, PRIMARY KEY(user_id,channel_id));
        CREATE TABLE IF NOT EXISTS search_history (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        keyword TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(user_id,keyword));")?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/progress", get(progress).put(save_progress))
        .route(
            "/api/progress/online/{source_id}/{media_id}",
            delete(remove_progress),
        )
        .route("/api/progress/local/{id}", delete(reset_local))
        .route("/api/favorites", get(favorites).post(add_favorite))
        .route(
            "/api/favorites/{source_id}/{media_id}",
            delete(remove_favorite),
        )
        .route("/api/channels/{id}/favorite", put(channel_favorite))
        .route(
            "/api/search/history",
            get(history).post(record_history).delete(delete_history),
        )
}

fn visible(conn: &Connection, id: i64) -> Result<(), AppError> {
    if !conn.query_row("SELECT EXISTS(SELECT 1 FROM catalog_sources WHERE id=?1 AND enabled=1 AND (adult=0 OR ?2))", params![id, adult_sources_enabled(conn)?], |r| r.get::<_, bool>(0))? {
        return Err(AppError::not_found("Source not found or disabled"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct Lookup {
    source_id: Option<i64>,
    media_id: Option<String>,
}

#[derive(Serialize)]
struct Progress {
    entry_kind: String,
    source_id: Option<i64>,
    media_id: Option<String>,
    local_id: Option<i64>,
    title: String,
    cover: String,
    episode_index: i64,
    episode_name: String,
    remote_url: String,
    position: f64,
    duration: f64,
    updated_at: i64,
}

const ONLINE: &str = "SELECT 'online',p.source_id,p.media_id,NULL,p.title,p.cover,p.episode_index,p.episode_name,
    p.remote_url,p.position,p.duration,p.updated_at AS updated_at FROM online_progress p JOIN catalog_sources s ON s.id=p.source_id
    WHERE p.user_id=?1 AND s.enabled=1 AND (s.adult=0 OR ?2)";

async fn progress(
    State(state): State<Shared>,
    user: AuthUser,
    Query(find): Query<Lookup>,
) -> Result<Json<Vec<Progress>>, AppError> {
    let conn = state.conn()?;
    let sql = match (&find.source_id, &find.media_id) {
        (Some(id), Some(_)) => {
            visible(&conn, *id)?;
            format!("{ONLINE} AND p.source_id=?3 AND p.media_id=?4")
        }
        (None, None) => format!("{ONLINE} AND p.position>0 AND (p.position<p.duration*0.95 OR EXISTS(
            SELECT 1 FROM tracking_subscriptions t WHERE t.user_id=p.user_id AND t.source_id=p.source_id AND t.media_id=p.media_id))
            UNION ALL SELECT 'local',NULL,NULL,m.id,m.title,'',0,'','',u.position,u.duration,u.updated_at
            FROM library_user_media u JOIN library_media m ON m.id=u.media_id WHERE u.user_id=?1 AND m.available=1 AND u.position>0 AND u.watched=0
            ORDER BY updated_at DESC,2,3,4 LIMIT 24"),
        _ => return Err(AppError::bad_request("Provide both source_id and media_id")),
    };
    let mut stmt = conn.prepare(&sql)?;
    let adult = adult_sources_enabled(&conn)?;
    let mut rows = if let Some(id) = find.source_id {
        stmt.query(params![user.id, adult, id, find.media_id])?
    } else {
        stmt.query(params![user.id, adult])?
    };
    let mut result = Vec::new();
    while let Some(r) = rows.next()? {
        result.push(Progress {
            entry_kind: r.get(0)?,
            source_id: r.get(1)?,
            media_id: r.get(2)?,
            local_id: r.get(3)?,
            title: r.get(4)?,
            cover: r.get(5)?,
            episode_index: r.get(6)?,
            episode_name: r.get(7)?,
            remote_url: r.get(8)?,
            position: r.get(9)?,
            duration: r.get(10)?,
            updated_at: r.get(11)?,
        });
    }
    Ok(Json(result))
}

#[derive(Deserialize)]
struct ProgressInput {
    source_id: i64,
    media_id: String,
    title: String,
    #[serde(default)]
    cover: String,
    #[serde(default)]
    episode_index: i64,
    #[serde(default)]
    episode_name: String,
    #[serde(default)]
    remote_url: String,
    position: f64,
    duration: f64,
}

async fn save_progress(
    State(state): State<Shared>,
    user: AuthUser,
    Json(p): Json<ProgressInput>,
) -> Result<Json<Value>, AppError> {
    if p.position < 0.0
        || !p.position.is_finite()
        || p.duration <= 0.0
        || !p.duration.is_finite()
        || p.episode_index < 0
        || p.media_id.trim().is_empty()
        || p.title.trim().is_empty()
    {
        return Err(AppError::bad_request("Invalid playback progress"));
    }
    let conn = state.conn()?;
    visible(&conn, p.source_id)?;
    conn.execute("INSERT INTO online_progress(user_id,source_id,media_id,title,cover,episode_index,episode_name,remote_url,position,duration,updated_at)
        VALUES(?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(user_id,source_id,media_id) DO UPDATE SET title=excluded.title,cover=excluded.cover,
        episode_index=excluded.episode_index,episode_name=excluded.episode_name,remote_url=excluded.remote_url,
        position=excluded.position,duration=excluded.duration,updated_at=excluded.updated_at",params![user.id,p.source_id,p.media_id,p.title,p.cover,p.episode_index,p.episode_name,p.remote_url,p.position,p.duration,now()])?;
    Ok(Json(json!({"saved":true})))
}

async fn remove_progress(
    State(state): State<Shared>,
    user: AuthUser,
    Path((source, media)): Path<(i64, String)>,
) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    visible(&conn, source)?;
    conn.execute(
        "DELETE FROM online_progress WHERE user_id=? AND source_id=? AND media_id=?",
        params![user.id, source, media],
    )?;
    Ok(Json(json!({"removed":true})))
}

async fn reset_local(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    crate::library::check_media(&conn, id)?;
    conn.execute(
        "UPDATE library_user_media SET position=0,updated_at=? WHERE user_id=? AND media_id=?",
        params![now(), user.id, id],
    )?;
    Ok(Json(json!({"removed":true})))
}

#[derive(Deserialize)]
struct CatalogInput {
    source_id: i64,
    media_id: String,
    title: String,
    #[serde(default)]
    cover: String,
    #[serde(default)]
    year: String,
    kind: Option<String>,
    #[serde(default)]
    remark: String,
}

async fn add_favorite(
    State(state): State<Shared>,
    user: AuthUser,
    Json(f): Json<CatalogInput>,
) -> Result<Json<Value>, AppError> {
    if f.media_id.trim().is_empty() || f.title.trim().is_empty() {
        return Err(AppError::bad_request("Missing favorite title or media ID"));
    }
    let conn = state.conn()?;
    visible(&conn, f.source_id)?;
    conn.execute("INSERT INTO catalog_favorites(user_id,source_id,media_id,title,cover,year,kind,remark,created_at) VALUES(?,?,?,?,?,?,?,?,?)
        ON CONFLICT(user_id,source_id,media_id) DO UPDATE SET title=excluded.title,cover=excluded.cover,year=excluded.year,kind=excluded.kind,remark=excluded.remark",
        params![user.id,f.source_id,f.media_id,f.title,f.cover,f.year,f.kind.as_deref().unwrap_or("other"),f.remark,now()])?;
    Ok(Json(json!({"favorite":true})))
}

async fn remove_favorite(
    State(state): State<Shared>,
    user: AuthUser,
    Path((source, media)): Path<(i64, String)>,
) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    visible(&conn, source)?;
    conn.execute(
        "DELETE FROM catalog_favorites WHERE user_id=? AND source_id=? AND media_id=?",
        params![user.id, source, media],
    )?;
    Ok(Json(json!({"favorite":false})))
}

#[derive(Deserialize)]
struct Favorite {
    favorite: bool,
}

async fn channel_favorite(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(f): Json<Favorite>,
) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM channels WHERE id=?)",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(AppError::not_found("Channel not found"));
    }
    if f.favorite {
        conn.execute("INSERT INTO channel_favorites(user_id,channel_id,created_at) VALUES(?,?,?) ON CONFLICT DO NOTHING",params![user.id,id,now()])?;
    } else {
        conn.execute(
            "DELETE FROM channel_favorites WHERE user_id=? AND channel_id=?",
            params![user.id, id],
        )?;
    }
    Ok(Json(json!({"favorite":f.favorite})))
}

async fn favorites(State(state): State<Shared>, user: AuthUser) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    let mut stmt=conn.prepare("SELECT f.source_id,f.media_id,f.title,f.cover,f.year,f.kind,f.remark,f.created_at,s.name FROM catalog_favorites f JOIN catalog_sources s ON s.id=f.source_id
        WHERE f.user_id=?1 AND s.enabled=1 AND (s.adult=0 OR ?2) ORDER BY f.created_at DESC,f.rowid DESC")?;
    let catalog=stmt.query_map(params![user.id,adult_sources_enabled(&conn)?],|r| Ok(json!({"source_id":r.get::<_,i64>(0)?,"media_id":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"cover":r.get::<_,String>(3)?,"year":r.get::<_,String>(4)?,"kind":r.get::<_,String>(5)?,"remark":r.get::<_,String>(6)?,"created_at":r.get::<_,i64>(7)?,"source_name":r.get::<_,String>(8)?})))?.collect::<Result<Vec<_>,_>>()?;
    let mut stmt=conn.prepare("SELECT m.id,m.title,m.relative_path,m.kind,s.title,m.series_id,m.season,m.episode,m.size,u.favorite,u.position,u.duration,u.watched
        FROM library_user_media u JOIN library_media m ON m.id=u.media_id LEFT JOIN library_series s ON s.id=m.series_id
        WHERE u.user_id=? AND u.favorite=1 AND m.available=1 ORDER BY COALESCE(u.favorite_at,u.updated_at) DESC,u.rowid DESC")?;
    let local = stmt
        .query_map([user.id], crate::library::media_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let channels = network::channels_for_user(&conn, user.id, true)?;
    Ok(Json(
        json!({"catalog":catalog,"local":local,"channels":channels}),
    ))
}

async fn history(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<String>>, AppError> {
    let conn = state.conn()?;
    let mut stmt=conn.prepare("SELECT keyword FROM search_history WHERE user_id=? ORDER BY updated_at DESC,rowid DESC LIMIT 20")?;
    Ok(Json(
        stmt.query_map([user.id], |r| r.get(0))?
            .collect::<Result<_, _>>()?,
    ))
}

#[derive(Deserialize)]
struct Keyword {
    keyword: String,
}

async fn record_history(
    State(state): State<Shared>,
    user: AuthUser,
    Json(k): Json<Keyword>,
) -> Result<Json<Value>, AppError> {
    let keyword = k.keyword.trim();
    if keyword.is_empty() {
        return Err(AppError::bad_request("Empty search keyword"));
    }
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    // A refreshed word receives a new rowid, breaking ties within the same second.
    tx.execute(
        "INSERT OR REPLACE INTO search_history(user_id,keyword,updated_at) VALUES(?,?,?)",
        params![user.id, keyword, now()],
    )?;
    tx.execute("DELETE FROM search_history WHERE rowid IN (SELECT rowid FROM search_history WHERE user_id=? ORDER BY updated_at DESC,rowid DESC LIMIT -1 OFFSET 20)",[user.id])?;
    tx.commit()?;
    Ok(Json(json!({"saved":true})))
}

#[derive(Deserialize)]
struct HistoryQuery {
    q: Option<String>,
}

async fn delete_history(
    State(state): State<Shared>,
    user: AuthUser,
    Query(search): Query<HistoryQuery>,
) -> Result<Json<Value>, AppError> {
    state.conn()?.execute(
        "DELETE FROM search_history WHERE user_id=?1 AND (?2 IS NULL OR keyword=?2)",
        params![user.id, search.q.as_deref().map(str::trim)],
    )?;
    Ok(Json(json!({"removed":true})))
}
