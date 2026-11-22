use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
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
use std::{future::Future, path::PathBuf, time::Duration};
use tokio::{io::AsyncWriteExt, task::JoinHandle};
use url::Url;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channels (
            id INTEGER PRIMARY KEY, name TEXT NOT NULL, group_name TEXT NOT NULL,
            logo TEXT NOT NULL, url TEXT NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS downloads (
            id INTEGER PRIMARY KEY, user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            title TEXT NOT NULL, url TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
            bytes_written INTEGER NOT NULL DEFAULT 0, total_bytes INTEGER, error TEXT,
            relative_path TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS downloads_user ON downloads(user_id, id);",
    )?;
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
}

#[derive(Debug, Serialize)]
struct Channel {
    id: i64,
    name: String,
    group_name: String,
    logo: String,
    url: String,
}

async fn list_channels(
    State(state): State<Shared>,
    _user: AuthUser,
) -> Result<Json<Vec<Channel>>, AppError> {
    let conn = state.conn()?;
    let mut query = conn
        .prepare("SELECT id,name,group_name,logo,url FROM channels ORDER BY group_name,name,id")?;
    let rows = query
        .query_map([], |row| {
            Ok(Channel {
                id: row.get(0)?,
                name: row.get(1)?,
                group_name: row.get(2)?,
                logo: row.get(3)?,
                url: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(rows))
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
    for channel in &channels {
        tx.execute("INSERT INTO channels(name,group_name,logo,url) VALUES (?1,?2,?3,?4)
            ON CONFLICT(url) DO UPDATE SET name=excluded.name,group_name=excluded.group_name,logo=excluded.logo",
            params![channel.name, channel.group_name, channel.logo, channel.url])?;
    }
    tx.commit()?;
    Ok(Json(json!({"imported": channels.len()})))
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

fn http_url(raw: &str) -> Result<Url, AppError> {
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

fn attribute(info: &str, key: &str) -> String {
    let pattern = format!("{key}=\"");
    if let Some(start) = info.find(&pattern) {
        return info[start + pattern.len()..]
            .split('"')
            .next()
            .unwrap_or_default()
            .to_owned();
    }
    String::new()
}

fn parse_playlist(text: &str) -> Result<Vec<Channel>, AppError> {
    let mut channels = Vec::new();
    let mut pending: Option<(String, String, String)> = None;
    let mut saw_header = false;
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXTM3U") {
            saw_header = true;
            continue;
        }
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            if pending.is_some() {
                return Err(AppError::bad_request(format!(
                    "Missing channel URL near line {}",
                    number + 1
                )));
            }
            let mut quoted = false;
            let comma = info.char_indices().find_map(|(i, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                (c == ',' && !quoted).then_some(i)
            });
            let (attributes, name) = match comma {
                Some(i) => (&info[..i], info[i + 1..].trim().to_owned()),
                None => {
                    return Err(AppError::bad_request(format!(
                        "Invalid channel metadata on line {}",
                        number + 1
                    )));
                }
            };
            let name = if name.is_empty() {
                attribute(attributes, "tvg-name")
            } else {
                name
            };
            if name.is_empty() || name.len() > 300 {
                return Err(AppError::bad_request(
                    "Every channel needs a name of at most 300 characters",
                ));
            }
            let logo = attribute(attributes, "tvg-logo");
            if !logo.is_empty() {
                http_url(&logo)?;
            }
            pending = Some((name, attribute(attributes, "group-title"), logo));
        } else if !line.starts_with('#') {
            let (name, group_name, logo) = pending.take().ok_or_else(|| {
                AppError::bad_request(format!(
                    "Missing EXTINF metadata before line {}",
                    number + 1
                ))
            })?;
            let url = http_url(line)?.to_string();
            if !channels.iter().any(|channel: &Channel| channel.url == url) {
                channels.push(Channel {
                    id: 0,
                    name,
                    group_name,
                    logo,
                    url,
                });
            }
            if channels.len() > 10_000 {
                return Err(AppError::bad_request(
                    "Import at most 10,000 channels at a time",
                ));
            }
        }
    }
    if !saw_header || pending.is_some() || channels.is_empty() {
        return Err(AppError::bad_request(
            "Provide a complete M3U playlist with #EXTM3U, #EXTINF and HTTP(S) channel URLs",
        ));
    }
    Ok(channels)
}

#[derive(Debug, Serialize)]
struct Download {
    id: i64,
    title: String,
    url: String,
    status: String,
    bytes_written: i64,
    total_bytes: Option<i64>,
    error: Option<String>,
    relative_path: String,
}

fn download_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Download> {
    Ok(Download {
        id: row.get(0)?,
        title: row.get(1)?,
        url: row.get(2)?,
        status: row.get(3)?,
        bytes_written: row.get(4)?,
        total_bytes: row.get(5)?,
        error: row.get(6)?,
        relative_path: row.get(7)?,
    })
}

const DOWNLOAD_COLUMNS: &str = "id,title,url,status,bytes_written,total_bytes,error,relative_path";

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

#[derive(Deserialize)]
struct DownloadInput {
    url: String,
    title: String,
}

fn download_extension(url: &Url) -> Result<String, AppError> {
    let extension = url
        .path()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
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
    let conn = state.conn()?;
    let id = enqueue_in(&conn, &state, user.id, &input.title, &input.url)?;
    let result = conn.query_row(
        &format!("SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE id=?1"),
        [id],
        download_row,
    )?;
    Ok((StatusCode::CREATED, Json(result)))
}

pub fn enqueue_in(
    conn: &Connection,
    state: &Shared,
    user_id: i64,
    title: &str,
    raw_url: &str,
) -> Result<i64, AppError> {
    let url = http_url(raw_url.trim())?;
    let extension = download_extension(&url)?;
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(AppError::bad_request(
            "Give the download a title of 1 to 200 characters",
        ));
    }
    let filename = format!(
        "{}-{}.{}",
        safe_title(title),
        uuid::Uuid::new_v4(),
        extension
    );
    let relative_path = state
        .download_dir
        .join(filename)
        .strip_prefix(&state.media_dir)
        .map_err(|_| AppError::internal("Download directory must be inside the media directory"))?
        .to_string_lossy()
        .into_owned();
    conn.execute(
        "INSERT INTO downloads(user_id,title,url,relative_path,created_at) VALUES (?1,?2,?3,?4,?5)",
        params![user_id, title, url.to_string(), relative_path, now()],
    )?;
    Ok(conn.last_insert_rowid())
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
    let conn = state.conn()?;
    let status: String = conn
        .query_row(
            "SELECT status FROM downloads WHERE id=?1 AND user_id=?2",
            params![id, user.id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Download not found"))?;
    if status == "completed" {
        return Err(AppError::bad_request(
            "A completed download cannot be cancelled",
        ));
    }
    // Keep the status check in the write so a completion racing this request wins.
    let changed = conn.execute("UPDATE downloads SET status='cancelled',error=NULL WHERE id=?1 AND user_id=?2 AND status IN ('queued','running','failed')", params![id, user.id])?;
    if changed == 0 {
        return Err(AppError::bad_request(
            "A completed download cannot be cancelled",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn retry_download(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    let status: String = conn
        .query_row(
            "SELECT status FROM downloads WHERE id=?1 AND user_id=?2",
            params![id, user.id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Download not found"))?;
    if !matches!(status.as_str(), "failed" | "cancelled") {
        return Err(AppError::bad_request(
            "Only failed or cancelled downloads can be retried",
        ));
    }
    conn.execute("UPDATE downloads SET status='queued',bytes_written=0,total_bytes=NULL,error=NULL WHERE id=?1 AND user_id=?2", params![id, user.id])?;
    Ok(Json(json!({"status":"queued"})))
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = recover_jobs(&state) {
            tracing::error!(error = %error.1, "Could not recover download queue");
            return;
        }
        loop {
            match claim_job(&state) {
                Ok(Some(job)) => {
                    let result = transfer(&state, &job).await;
                    let part = state
                        .media_dir
                        .join(&job.relative_path)
                        .with_extension("part");
                    if let Err(error) = tokio::fs::remove_file(part).await
                        && error.kind() != std::io::ErrorKind::NotFound
                    {
                        tracing::warn!(%error, download_id = job.id, "Could not remove partial download");
                    }
                    if let Err(error) = result {
                        tracing::warn!(download_id = job.id, %error, "Download failed");
                        if let Ok(conn) = state.conn() {
                            let _ = conn.execute("UPDATE downloads SET status='failed',error=?1 WHERE id=?2 AND status='running'", params![error, job.id]);
                        }
                    } else if result == Ok(true) {
                        let scan_state = state.clone();
                        match tokio::task::spawn_blocking(move || {
                            crate::library::scan_library(&scan_state)
                        })
                        .await
                        {
                            Ok(Ok(_)) => {}
                            Ok(Err(error)) => {
                                tracing::warn!(error = %error.1, "Downloaded video was saved, but library scan failed")
                            }
                            Err(error) => {
                                tracing::warn!(%error, "Downloaded video was saved, but library scan failed")
                            }
                        }
                    }
                }
                Ok(None) => tokio::time::sleep(Duration::from_secs(1)).await,
                Err(error) => {
                    tracing::error!(error = %error.1, "Download queue failed");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    })
}

fn recover_jobs(state: &Shared) -> Result<(), AppError> {
    let interrupted = {
        let conn = state.conn()?;
        let mut query =
            conn.prepare("SELECT id,relative_path FROM downloads WHERE status='running'")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (id, relative) in interrupted {
        if let Ok((part, final_path)) = download_paths(state, &relative)
            && let Some(size) = published_download_size(&part, &final_path)
        {
            state.conn()?.execute("UPDATE downloads SET status='completed',bytes_written=?1,total_bytes=?1,error=NULL WHERE id=?2 AND status='running'", params![size,id])?;
            if let Err(error) = std::fs::remove_file(part) {
                tracing::warn!(%error, download_id=id, "Could not remove recovered partial download");
            }
        }
    }
    state.conn()?.execute("UPDATE downloads SET status='queued',bytes_written=0,total_bytes=NULL,error=NULL WHERE status='running'", [])?;
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
    let job = conn
        .query_row(
            &format!(
                "SELECT {DOWNLOAD_COLUMNS} FROM downloads WHERE status='queued' ORDER BY id LIMIT 1"
            ),
            [],
            download_row,
        )
        .optional()?;
    if let Some(ref job) = job {
        conn.execute("UPDATE downloads SET status='running',bytes_written=0,total_bytes=NULL,error=NULL WHERE id=?1 AND status='queued'", [job.id])?;
    }
    Ok(job)
}

fn is_running(state: &Shared, id: i64) -> Result<bool, String> {
    let conn = state.conn().map_err(|e| e.1)?;
    let status: Option<String> = conn
        .query_row("SELECT status FROM downloads WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(status.as_deref() == Some("running"))
}

async fn while_running<T>(
    state: &Shared,
    id: i64,
    future: impl Future<Output = T>,
) -> Result<Option<T>, String> {
    let timeout = tokio::time::timeout(Duration::from_secs(30), future);
    tokio::pin!(timeout);
    loop {
        tokio::select! {
            result = &mut timeout => return result.map(Some).map_err(|_| "Source stopped responding for 30 seconds".to_owned()),
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if !is_running(state, id)? { return Ok(None); }
            }
        }
    }
}

fn download_paths(state: &Shared, relative: &str) -> Result<(PathBuf, PathBuf), String> {
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
    Ok((final_path.with_extension("part"), final_path))
}

async fn transfer(state: &Shared, job: &Download) -> Result<bool, String> {
    let (part_path, final_path) = download_paths(state, &job.relative_path)?;
    if !is_running(state, job.id)? {
        return Ok(false);
    }
    if Url::parse(&job.url).is_ok_and(|url| url.path().to_ascii_lowercase().ends_with(".m3u8")) {
        return transfer_hls(state, job, &part_path, &final_path).await;
    }
    let response = match while_running(state, job.id, state.client.get(&job.url).send()).await? {
        Some(response) => {
            response.map_err(|_| "Could not connect to the download source".to_owned())?
        }
        None => return Ok(false),
    };
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
            "UPDATE downloads SET total_bytes=?1 WHERE id=?2 AND status='running'",
            params![total_bytes, job.id],
        )
        .map_err(|e| e.to_string())?;
    match tokio::fs::remove_file(&part_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Cannot clean interrupted download: {error}")),
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&part_path)
        .await
        .map_err(|e| format!("Cannot create download file: {e}"))?;
    let mut stream = response.bytes_stream();
    let mut written: i64 = 0;
    let mut last_saved = std::time::Instant::now();
    loop {
        let chunk = match while_running(state, job.id, stream.next()).await? {
            None => return Ok(false),
            Some(None) => break,
            Some(Some(result)) => result
                .map_err(|_| "Connection ended before the video finished downloading".to_owned())?,
        };
        if !is_running(state, job.id)? {
            return Ok(false);
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Cannot write download file: {e}"))?;
        written = written
            .checked_add(chunk.len() as i64)
            .ok_or("Download is too large")?;
        if last_saved.elapsed() >= Duration::from_millis(500) {
            state
                .conn()
                .map_err(|e| e.1)?
                .execute(
                    "UPDATE downloads SET bytes_written=?1 WHERE id=?2 AND status='running'",
                    params![written, job.id],
                )
                .map_err(|e| e.to_string())?;
            last_saved = std::time::Instant::now();
        }
    }
    if written == 0 {
        return Err("Source returned an empty video".to_owned());
    }
    if total_bytes.is_some_and(|total| written != total) {
        return Err("Source sent an incomplete video".to_owned());
    }
    file.flush().await.map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())?;
    drop(file);
    publish_download(state, job.id, written, &part_path, &final_path)
}

async fn transfer_hls(
    state: &Shared,
    job: &Download,
    part_path: &std::path::Path,
    final_path: &std::path::Path,
) -> Result<bool, String> {
    match tokio::fs::remove_file(part_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Cannot clean interrupted download: {error}")),
    }
    let mut child = tokio::process::Command::new("ffmpeg")
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
        .arg(&job.url)
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "HLS downloads require FFmpeg installed on the server".to_string())?;
    loop {
        if !is_running(state, job.id)? {
            let _ = child.kill().await;
            return Ok(false);
        }
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            if !status.success() {
                return Err("FFmpeg could not download this HLS stream; check the source, permissions, and available disk space".into());
            }
            break;
        }
        if let Ok(metadata) = tokio::fs::metadata(part_path).await {
            state
                .conn()
                .map_err(|e| e.1)?
                .execute(
                    "UPDATE downloads SET bytes_written=? WHERE id=? AND status='running'",
                    params![metadata.len().min(i64::MAX as u64) as i64, job.id],
                )
                .map_err(|e| e.to_string())?;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let file = tokio::fs::File::open(part_path)
        .await
        .map_err(|error| error.to_string())?;
    file.sync_all().await.map_err(|error| error.to_string())?;
    let size = i64::try_from(
        file.metadata()
            .await
            .map_err(|error| error.to_string())?
            .len(),
    )
    .map_err(|_| "Video is too large".to_string())?;
    drop(file);
    if size == 0 {
        return Err("Source returned an empty video".into());
    }
    publish_download(state, job.id, size, part_path, final_path)
}

fn publish_download(
    state: &Shared,
    id: i64,
    written: i64,
    part_path: &std::path::Path,
    final_path: &std::path::Path,
) -> Result<bool, String> {
    let conn = state.conn().map_err(|e| e.1)?;
    let status: Option<String> = conn
        .query_row("SELECT status FROM downloads WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    if status.as_deref() != Some("running") {
        return Ok(false);
    }
    // Publishing with a hard link is atomic and fails if an existing media file occupies the name.
    std::fs::hard_link(part_path, final_path)
        .map_err(|e| format!("Cannot publish completed video: {e}"))?;
    match conn.execute("UPDATE downloads SET status='completed',bytes_written=?1,total_bytes=?1,error=NULL WHERE id=?2 AND status='running'", params![written, id]) {
        Ok(0) => { let _ = std::fs::remove_file(final_path); return Ok(false); }
        Ok(_) => {}
        Err(error) => {
            let _ = std::fs::remove_file(final_path);
            return Err(error.to_string());
        }
    }
    Ok(true)
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
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
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
        assert!(download_paths(&state, "downloads/../../outside.mp4").is_err());
        assert!(download_paths(&state, "downloads/a.mp4").is_ok());
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
        assert_eq!(claim_job(&state).unwrap().unwrap().id, job.id);
        assert!(is_running(&state, job.id).unwrap());
        cancel_download(State(state.clone()), user(1), Path(job.id))
            .await
            .unwrap();
        assert!(!is_running(&state, job.id).unwrap());
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
            }),
        )
        .await
        .unwrap();
        let job = claim_job(&state).unwrap().unwrap();
        transfer(&state, &job).await.unwrap();
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
            transfer(&state, &job)
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
            }),
        )
        .await
        .unwrap();
        let final_path = state.media_dir.join(&job.relative_path);
        let part = final_path.with_extension("part");
        let worker = start_worker(state.clone());
        wait_for(|| std::fs::metadata(&part).is_ok_and(|meta| meta.len() > 0)).await;
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
        let (_, Json(job)) = create_download(
            State(state.clone()),
            user(2),
            Json(DownloadInput {
                url: "https://example.com/video.mp4".into(),
                title: "Video".into(),
            }),
        )
        .await
        .unwrap();
        claim_job(&state).unwrap();
        state
            .conn()
            .unwrap()
            .execute("DELETE FROM users WHERE id=2", [])
            .unwrap();
        assert!(!is_running(&state, job.id).unwrap());
        assert!(!transfer(&state, &job).await.unwrap());
        assert!(
            list_downloads(State(state), user(2))
                .await
                .unwrap()
                .0
                .is_empty()
        );
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
            }),
        )
        .await
        .unwrap();
        claim_job(&state).unwrap();
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
        let Json(jobs) = list_downloads(State(state), user(1)).await.unwrap();
        assert_eq!(jobs[0].status, "queued");
    }
}
