use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    now,
};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::Path as FsPath};
use tower_http::services::ServeFile;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS library_series (
            id INTEGER PRIMARY KEY, title TEXT NOT NULL UNIQUE
         );
         CREATE TABLE IF NOT EXISTS library_media (
            id INTEGER PRIMARY KEY, title TEXT NOT NULL,
            relative_path TEXT NOT NULL, root TEXT NOT NULL,
            kind TEXT NOT NULL, series_id INTEGER REFERENCES library_series(id),
            season INTEGER, episode INTEGER, size INTEGER NOT NULL,
            available INTEGER NOT NULL DEFAULT 1,
            UNIQUE(root, relative_path)
         );
         CREATE TABLE IF NOT EXISTS library_user_media (
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            media_id INTEGER NOT NULL REFERENCES library_media(id) ON DELETE CASCADE,
            favorite INTEGER NOT NULL DEFAULT 0,
            position REAL NOT NULL DEFAULT 0, duration REAL NOT NULL DEFAULT 0,
            watched INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL,
            PRIMARY KEY(user_id, media_id)
         );
         CREATE TABLE IF NOT EXISTS library_subscriptions (
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            series_id INTEGER NOT NULL REFERENCES library_series(id) ON DELETE CASCADE,
            PRIMARY KEY(user_id, series_id)
         );
         CREATE INDEX IF NOT EXISTS library_media_series ON library_media(series_id);",
    )?;
    for column in ["cover", "year", "description"] {
        crate::ensure_column(conn, "library_media", column, "TEXT NOT NULL DEFAULT ''")?;
    }
    crate::ensure_column(conn, "library_user_media", "favorite_at", "INTEGER")?;
    conn.execute("UPDATE library_user_media SET favorite_at=updated_at WHERE favorite=1 AND favorite_at IS NULL", [])?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/library", get(list))
        .route("/api/library/scan", post(scan))
        .route("/api/library/{id}/stream", get(stream).head(stream))
        .route("/api/library/{id}/favorite", put(favorite))
        .route("/api/library/{id}/progress", put(progress))
        .route("/api/series", get(series))
        .route("/api/series/{id}/subscription", put(subscription))
}

#[derive(Debug, Serialize)]
pub struct MediaItem {
    pub id: i64,
    pub title: String,
    pub relative_path: String,
    pub kind: String,
    pub series: Option<String>,
    pub series_id: Option<i64>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub size: i64,
    pub favorite: bool,
    pub position: f64,
    pub duration: f64,
    pub watched: bool,
    pub cover: String,
    pub year: String,
    pub description: String,
}

#[derive(Default, Deserialize)]
struct Search {
    q: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    user: AuthUser,
    Query(search): Query<Search>,
) -> Result<Json<Vec<MediaItem>>, AppError> {
    let conn = state.conn()?;
    let query = search.q.unwrap_or_default();
    let mut stmt = conn.prepare(
        "SELECT m.id,m.title,m.relative_path,m.kind,s.title,m.series_id,m.season,m.episode,m.size,
                COALESCE(u.favorite,0),COALESCE(u.position,0),COALESCE(u.duration,0),COALESCE(u.watched,0),
                m.cover,m.year,m.description
         FROM library_media m LEFT JOIN library_series s ON s.id=m.series_id
         LEFT JOIN library_user_media u ON u.media_id=m.id AND u.user_id=?1
         WHERE m.available=1 AND (?2='' OR instr(lower(m.title || ' ' || COALESCE(s.title,'')),lower(?2))>0)
         ORDER BY s.title COLLATE NOCASE,m.season,m.episode,m.title COLLATE NOCASE"
    )?;
    let items = stmt
        .query_map(params![user.id, query], media_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(items))
}

pub(crate) fn media_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaItem> {
    Ok(MediaItem {
        id: row.get(0)?,
        title: row.get(1)?,
        relative_path: row.get(2)?,
        kind: row.get(3)?,
        series: row.get(4)?,
        series_id: row.get(5)?,
        season: row.get(6)?,
        episode: row.get(7)?,
        size: row.get(8)?,
        favorite: row.get(9)?,
        position: row.get(10)?,
        duration: row.get(11)?,
        watched: row.get(12)?,
        cover: row.get(13)?,
        year: row.get(14)?,
        description: row.get(15)?,
    })
}

#[derive(Debug, Serialize)]
pub struct ScanResult {
    pub added: usize,
    pub removed: usize,
    pub total: usize,
}

#[derive(Debug)]
struct ScannedFile {
    relative_path: String,
    root: &'static str,
    size: i64,
}

impl ScannedFile {
    fn index(&self, conn: &Connection) -> Result<(), AppError> {
        let relative = FsPath::new(&self.relative_path);
        let stem = relative
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let (series, season, episode) = episode_info(stem, relative);
        let series_id = if let Some(title) = series {
            conn.execute(
                "INSERT INTO library_series(title) VALUES(?) ON CONFLICT(title) DO NOTHING",
                [&title],
            )?;
            Some(conn.query_row(
                "SELECT id FROM library_series WHERE title=?",
                [&title],
                |r| r.get::<_, i64>(0),
            )?)
        } else {
            None
        };
        let kind = if series_id.is_some() {
            "episode"
        } else {
            "movie"
        };
        conn.execute(
            "INSERT INTO library_media(title,relative_path,root,kind,series_id,season,episode,size,available)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1)
             ON CONFLICT(root,relative_path) DO UPDATE SET title=excluded.title,kind=excluded.kind,
             series_id=excluded.series_id,season=excluded.season,episode=excluded.episode,size=excluded.size,available=1",
            params![clean_title(stem), self.relative_path, self.root, kind, series_id, season, episode, self.size],
        )?;
        Ok(())
    }
}

/// Queue cleanup may delete a completed job immediately. Commit its durable media row
/// and catalog metadata in the same transaction as the terminal queue state.
pub(crate) fn complete_download(
    conn: &mut Connection,
    id: i64,
    size: i64,
) -> Result<bool, AppError> {
    let tx = conn.transaction()?;
    let path = tx
        .query_row(
            "UPDATE downloads SET status='completed',bytes_written=?2,total_bytes=?2,error=NULL
         WHERE id=?1 AND status='running' RETURNING relative_path",
            params![id, size],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(relative_path) = path else {
        return Ok(false);
    };
    // Queue paths are relative to media_dir, including the download subdirectory.
    let file = ScannedFile {
        relative_path,
        root: "media",
        size,
    };
    file.index(&tx)?;
    tx.execute(
        "UPDATE library_media SET (cover,year,description)=(SELECT cover,year,description FROM downloads WHERE id=?1)
         WHERE root='media' AND relative_path=?2",
        params![id, file.relative_path],
    )?;
    tx.commit()?;
    Ok(true)
}

async fn scan(State(state): State<Shared>, user: AuthUser) -> Result<Json<ScanResult>, AppError> {
    require_admin(&user)?;
    tokio::task::spawn_blocking(move || scan_library(&state))
        .await
        .map_err(|error| AppError::internal(error.to_string()))?
        .map(Json)
}

/// Blocking: callers on the async runtime must use spawn_blocking.
pub fn scan_library(state: &Shared) -> Result<ScanResult, AppError> {
    let media_root = fs::canonicalize(&state.media_dir)?;
    let download_root = fs::canonicalize(&state.download_dir)?;
    let mut missing: HashSet<(String, String)> = {
        let conn = state.conn()?;
        let mut statement =
            conn.prepare("SELECT root,relative_path FROM library_media WHERE available=1")?;
        statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut files = Vec::new();
    collect_files(&media_root, &media_root, "media", &mut files)?;
    if !download_root.starts_with(&media_root) {
        collect_files(&download_root, &download_root, "download", &mut files)?;
    }

    // Only reconcile after the complete walk succeeds; an unreadable mount must not clear the catalog.
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let mut added = 0;
    let total = files.len();
    // Only mark files from the pre-walk snapshot as missing. A queue worker may
    // have indexed a new file while this scan was walking a different directory.
    for file in files {
        file.index(&tx)?;
        if !missing.remove(&(file.root.to_owned(), file.relative_path)) {
            added += 1;
        }
    }
    let mut removed = 0;
    for (root, path) in missing {
        removed += tx.execute(
            "UPDATE library_media SET available=0 WHERE root=? AND relative_path=? AND available=1",
            params![root, path],
        )?;
    }
    tx.commit()?;
    Ok(ScanResult {
        added,
        removed,
        total,
    })
}

fn collect_files(
    root: &FsPath,
    directory: &FsPath,
    source: &'static str,
    files: &mut Vec<ScannedFile>,
) -> Result<(), AppError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            collect_files(root, &path, source, files)?;
        } else if file_type.is_file() && is_video(&path) {
            let canonical = fs::canonicalize(&path)?;
            if !canonical.starts_with(root) {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|e| AppError::internal(e.to_string()))?;
            let relative_path = relative
                .to_str()
                .ok_or_else(|| AppError::bad_request("Media filenames must be valid UTF-8"))?
                .to_string();
            let size = i64::try_from(entry.metadata()?.len())
                .map_err(|_| AppError::bad_request("Media file is too large"))?;
            files.push(ScannedFile {
                relative_path,
                root: source,
                size,
            });
        }
    }
    Ok(())
}

pub(crate) fn is_video(path: &FsPath) -> bool {
    path.extension()
        .and_then(|v| v.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "mp4"
                    | "m4v"
                    | "webm"
                    | "mkv"
                    | "mov"
                    | "avi"
                    | "ts"
                    | "m2ts"
                    | "ogv"
                    | "mpeg"
                    | "mpg"
            )
        })
}

fn clean_title(stem: &str) -> String {
    stem.replace(['.', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn episode_info(stem: &str, relative: &FsPath) -> (Option<String>, Option<i64>, Option<i64>) {
    let lower = stem.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    for start in 0..bytes.len() {
        if bytes[start] != b's' {
            continue;
        }
        let mut middle = start + 1;
        while middle < bytes.len() && bytes[middle].is_ascii_digit() {
            middle += 1;
        }
        if middle == start + 1 || middle >= bytes.len() || bytes[middle] != b'e' {
            continue;
        }
        let mut end = middle + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == middle + 1 {
            continue;
        }
        let Ok(season) = lower[start + 1..middle].parse::<i64>() else {
            continue;
        };
        let Ok(episode) = lower[middle + 1..end].parse::<i64>() else {
            continue;
        };
        let mut title = clean_title(stem[..start].trim_matches([' ', '.', '_', '-']));
        if title.is_empty() {
            let mut parent = relative.parent();
            if let Some(folder) = parent.and_then(|p| p.file_name()).and_then(|p| p.to_str()) {
                let folder_lower = folder.to_ascii_lowercase();
                if folder_lower.starts_with("season ")
                    || folder_lower.starts_with("season_")
                    || (folder_lower.starts_with('s')
                        && folder_lower[1..].chars().all(|c| c.is_ascii_digit()))
                {
                    parent = parent.and_then(|p| p.parent());
                }
            }
            title = parent
                .and_then(|p| p.file_name())
                .and_then(|p| p.to_str())
                .map(clean_title)
                .unwrap_or_default();
        }
        if title.is_empty() {
            title = "Unsorted series".into();
        }
        return (Some(title), Some(season), Some(episode));
    }
    // Also support the common `Series/Season 1/file.mkv` layout.
    if let Some(folder) = relative
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|p| p.to_str())
    {
        let folder = folder.to_ascii_lowercase();
        let season = folder
            .strip_prefix("season ")
            .or_else(|| folder.strip_prefix("season_"))
            .or_else(|| folder.strip_prefix('s'))
            .and_then(|number| number.parse::<i64>().ok());
        if let Some(season) = season {
            let title = relative
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .and_then(|p| p.to_str())
                .map(clean_title)
                .filter(|value| !value.is_empty());
            if title.is_some() {
                return (title, Some(season), None);
            }
        }
    }
    (None, None, None)
}

async fn stream(
    State(state): State<Shared>,
    _user: AuthUser,
    Path(id): Path<i64>,
    request: Request,
) -> Result<Response, AppError> {
    let (root, relative): (String, String) = state
        .conn()?
        .query_row(
            "SELECT root,relative_path FROM library_media WHERE id=?1 AND available=1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Media not found"))?;
    let base = match root.as_str() {
        "media" => &state.media_dir,
        "download" => &state.download_dir,
        _ => return Err(AppError::not_found("Media file is unavailable")),
    };
    let base = tokio::fs::canonicalize(base)
        .await
        .map_err(|_| AppError::not_found("Media directory is unavailable"))?;
    let path = tokio::fs::canonicalize(base.join(&relative))
        .await
        .map_err(|_| AppError::not_found("Media file is unavailable"))?;
    // macOS resolves /var through /private/var; base is canonicalized above.
    if !path.starts_with(&base) {
        return Err(AppError::not_found("Media file is unavailable"));
    }
    let response = ServeFile::new(path).try_call(request).await?;
    Ok(response.into_response())
}

pub(crate) fn check_media(conn: &Connection, id: i64) -> Result<(), AppError> {
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_media WHERE id=?1 AND available=1)",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(AppError::not_found("Media not found"));
    }
    Ok(())
}

#[derive(Deserialize)]
struct FavoriteInput {
    favorite: bool,
}

async fn favorite(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<FavoriteInput>,
) -> Result<Json<serde_json::Value>, AppError> {
    let conn = state.conn()?;
    check_media(&conn, id)?;
    conn.execute(
        "INSERT INTO library_user_media(user_id,media_id,favorite,updated_at,favorite_at) VALUES(?1,?2,?3,?4,?4)
         ON CONFLICT(user_id,media_id) DO UPDATE SET favorite=excluded.favorite,
         favorite_at=CASE WHEN excluded.favorite=0 THEN NULL WHEN library_user_media.favorite=1 THEN library_user_media.favorite_at ELSE excluded.favorite_at END",
        params![user.id,id,input.favorite,now()],
    )?;
    Ok(Json(serde_json::json!({"favorite": input.favorite})))
}

#[derive(Deserialize)]
struct ProgressInput {
    position: f64,
    duration: f64,
    watched: bool,
}

async fn progress(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<ProgressInput>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !input.position.is_finite()
        || !input.duration.is_finite()
        || input.position < 0.0
        || input.duration < 0.0
        || input.position > input.duration
        || input.duration > 31_536_000.0
    {
        return Err(AppError::bad_request(
            "Progress must be between zero and the finite media duration",
        ));
    }
    let conn = state.conn()?;
    check_media(&conn, id)?;
    conn.execute(
        "INSERT INTO library_user_media(user_id,media_id,position,duration,watched,updated_at) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(user_id,media_id) DO UPDATE SET position=excluded.position,duration=excluded.duration,
         watched=excluded.watched,updated_at=excluded.updated_at",
        params![user.id,id,input.position,input.duration,input.watched,now()],
    )?;
    Ok(Json(serde_json::json!({"saved": true})))
}

#[derive(Serialize)]
struct SeriesItem {
    id: i64,
    title: String,
    total_episodes: i64,
    watched_episodes: i64,
    subscribed: bool,
}

async fn series(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<SeriesItem>>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(
        "SELECT s.id,s.title,COUNT(m.id),COALESCE(SUM(u.watched),0),
                EXISTS(SELECT 1 FROM library_subscriptions sub WHERE sub.series_id=s.id AND sub.user_id=?1)
         FROM library_series s JOIN library_media m ON m.series_id=s.id AND m.available=1
         LEFT JOIN library_user_media u ON u.media_id=m.id AND u.user_id=?1
         GROUP BY s.id ORDER BY s.title COLLATE NOCASE"
    )?;
    let items = stmt
        .query_map([user.id], |r| {
            Ok(SeriesItem {
                id: r.get(0)?,
                title: r.get(1)?,
                total_episodes: r.get(2)?,
                watched_episodes: r.get(3)?,
                subscribed: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(items))
}

#[derive(Deserialize)]
struct SubscriptionInput {
    subscribed: bool,
}

async fn subscription(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<SubscriptionInput>,
) -> Result<Json<serde_json::Value>, AppError> {
    let conn = state.conn()?;
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_series WHERE id=?1)",
        [id],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(AppError::not_found("Series not found"));
    }
    if input.subscribed {
        conn.execute("INSERT INTO library_subscriptions(user_id,series_id) VALUES(?1,?2) ON CONFLICT DO NOTHING", params![user.id,id])?;
    } else {
        conn.execute(
            "DELETE FROM library_subscriptions WHERE user_id=?1 AND series_id=?2",
            params![user.id, id],
        )?;
    }
    Ok(Json(serde_json::json!({"subscribed": input.subscribed})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use axum::{
        body::{Body, to_bytes},
        http::{Request as HttpRequest, StatusCode},
    };
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn state() -> (TempDir, Shared) {
        let temp = tempfile::tempdir().unwrap();
        let media = temp.path().join("media");
        fs::create_dir_all(media.join("downloads")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1),(2);").unwrap();
        init_schema(&conn).unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            playback_grants: Default::default(),
            telegram_native: Default::default(),
            download_runtime: Default::default(),
            media_dir: media.clone(),
            download_dir: media.join("downloads"),
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        (temp, state)
    }

    fn user(id: i64, admin: bool) -> AuthUser {
        AuthUser {
            id,
            username: format!("user{id}"),
            role: if admin { "admin" } else { "user" }.into(),
        }
    }

    #[test]
    fn detects_episodes_and_preserves_unicode_titles() {
        assert_eq!(
            episode_info("三体.S01E02.1080p", FsPath::new("三体.S01E02.1080p.mp4")),
            (Some("三体".into()), Some(1), Some(2))
        );
        assert_eq!(
            episode_info("s02e03", FsPath::new("Show/Season 02/s02e03.webm")),
            (Some("Show".into()), Some(2), Some(3))
        );
        assert_eq!(
            episode_info("episode", FsPath::new("Show/Season 02/episode.webm")),
            (Some("Show".into()), Some(2), None)
        );
        assert_eq!(
            episode_info("Movie.2025", FsPath::new("Movie.2025.mp4")),
            (None, None, None)
        );
    }

    #[tokio::test]
    async fn scan_reconciles_without_losing_user_state() {
        let (_temp, state) = state();
        let path = state.media_dir.join("Show.S01E01.mp4");
        fs::write(&path, b"0123456789").unwrap();
        fs::write(state.media_dir.join("ignored.txt"), b"text").unwrap();
        assert_eq!(scan_library(&state).unwrap().total, 1);
        let Json(items) = list(
            State(state.clone()),
            user(1, false),
            Query(Search::default()),
        )
        .await
        .unwrap();
        let id = items[0].id;
        let _ = favorite(
            State(state.clone()),
            user(1, false),
            Path(id),
            Json(FavoriteInput { favorite: true }),
        )
        .await
        .unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(scan_library(&state).unwrap().removed, 1);
        fs::write(&path, b"restored").unwrap();
        scan_library(&state).unwrap();
        let Json(items) = list(
            State(state.clone()),
            user(1, false),
            Query(Search::default()),
        )
        .await
        .unwrap();
        assert_eq!(items[0].id, id);
        assert!(items[0].favorite);
        let Json(other) = list(
            State(state.clone()),
            user(2, false),
            Query(Search::default()),
        )
        .await
        .unwrap();
        assert!(!other[0].favorite);
        assert!(scan(State(state.clone()), user(2, false)).await.is_err());
    }

    #[tokio::test]
    async fn progress_and_subscriptions_are_per_user_and_validated() {
        let (_temp, state) = state();
        fs::write(state.media_dir.join("Show.S01E02.mp4"), b"0123456789").unwrap();
        scan_library(&state).unwrap();
        let Json(items) = list(
            State(state.clone()),
            user(1, false),
            Query(Search::default()),
        )
        .await
        .unwrap();
        let id = items[0].id;
        let series_id = items[0].series_id.unwrap();
        assert!(
            progress(
                State(state.clone()),
                user(1, false),
                Path(id),
                Json(ProgressInput {
                    position: f64::NAN,
                    duration: 10.0,
                    watched: false
                })
            )
            .await
            .is_err()
        );
        assert!(
            progress(
                State(state.clone()),
                user(1, false),
                Path(id),
                Json(ProgressInput {
                    position: 11.0,
                    duration: 10.0,
                    watched: false
                })
            )
            .await
            .is_err()
        );
        let _ = progress(
            State(state.clone()),
            user(1, false),
            Path(id),
            Json(ProgressInput {
                position: 9.0,
                duration: 10.0,
                watched: true,
            }),
        )
        .await
        .unwrap();
        let _ = subscription(
            State(state.clone()),
            user(1, false),
            Path(series_id),
            Json(SubscriptionInput { subscribed: true }),
        )
        .await
        .unwrap();
        let Json(first) = series(State(state.clone()), user(1, false)).await.unwrap();
        let Json(second) = series(State(state.clone()), user(2, false)).await.unwrap();
        assert!(first[0].subscribed);
        assert_eq!(first[0].watched_episodes, 1);
        assert!(!second[0].subscribed);
        assert_eq!(second[0].watched_episodes, 0);
    }

    #[tokio::test]
    async fn streaming_supports_byte_ranges_and_head() {
        let (_temp, state) = state();
        fs::write(state.media_dir.join("Movie.mp4"), b"0123456789").unwrap();
        scan_library(&state).unwrap();
        let Json(items) = list(
            State(state.clone()),
            user(1, false),
            Query(Search::default()),
        )
        .await
        .unwrap();
        let id = items[0].id;
        let request = HttpRequest::builder()
            .uri(format!("/api/library/{id}/stream"))
            .header("range", "bytes=2-5")
            .body(Body::empty())
            .unwrap();
        let response = stream(State(state.clone()), user(1, false), Path(id), request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-range"], "bytes 2-5/10");
        assert_eq!(
            to_bytes(response.into_body(), 100).await.unwrap().as_ref(),
            b"2345"
        );
        let request = HttpRequest::builder()
            .method("HEAD")
            .uri(format!("/api/library/{id}/stream"))
            .body(Body::empty())
            .unwrap();
        let response = stream(State(state), user(1, false), Path(id), request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-length"], "10");
        assert!(
            to_bytes(response.into_body(), 100)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn scanning_and_streaming_reject_symlink_escapes() {
        use std::os::unix::fs::symlink;
        let (temp, state) = state();
        let outside = temp.path().join("private.mp4");
        fs::write(&outside, b"private").unwrap();
        symlink(&outside, state.media_dir.join("escaped.mp4")).unwrap();
        assert_eq!(scan_library(&state).unwrap().total, 0);
        let inside = state.media_dir.join("Movie.mp4");
        fs::write(&inside, b"public").unwrap();
        scan_library(&state).unwrap();
        fs::remove_file(&inside).unwrap();
        symlink(outside, &inside).unwrap();
        let request = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
        let result = stream(State(state), user(1, false), Path(1), request).await;
        assert!(matches!(result, Err(AppError(StatusCode::NOT_FOUND, _))));
    }

    #[test]
    fn a_failed_scan_keeps_the_previous_catalog() {
        let (_temp, state) = state();
        fs::write(state.media_dir.join("Movie.mp4"), b"video").unwrap();
        scan_library(&state).unwrap();
        fs::remove_dir_all(&state.download_dir).unwrap();
        assert!(scan_library(&state).is_err());
        let available: i64 = state
            .conn()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM library_media WHERE available=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(available, 1);
    }
}
