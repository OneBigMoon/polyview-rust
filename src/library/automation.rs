use super::{ScannedFile, check_media, collect_files, is_video, scan_library};
use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    now, organizer,
    scraper::{self, Lookup, Prepared},
    tracking,
};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path as FsPath,
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

pub(super) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS library_automation (
        media_id INTEGER PRIMARY KEY REFERENCES library_media(id) ON DELETE CASCADE,
        size INTEGER NOT NULL, status TEXT NOT NULL DEFAULT 'queued',
        hint TEXT NOT NULL DEFAULT '{}', manual INTEGER NOT NULL DEFAULT 0,
        prepared TEXT, organized INTEGER NOT NULL DEFAULT 0, scraped INTEGER NOT NULL DEFAULT 0,
        notified INTEGER NOT NULL DEFAULT 0, external INTEGER NOT NULL DEFAULT 0,
        attempts INTEGER NOT NULL DEFAULT 0, retry_at INTEGER NOT NULL DEFAULT 0, error TEXT NOT NULL DEFAULT ''
    ); CREATE INDEX IF NOT EXISTS library_automation_due ON library_automation(status,retry_at);")?;
    Ok(())
}

pub(super) fn enqueue(conn: &Connection, id: i64, size: i64) -> Result<(), AppError> {
    conn.execute(
        "INSERT INTO library_automation(media_id,size) VALUES(?1,?2)
        ON CONFLICT(media_id) DO UPDATE SET size=excluded.size,status='queued',prepared=NULL,
        organized=0,scraped=0,notified=0,attempts=0,retry_at=0,error=''
        WHERE size<>excluded.size AND status<>'running'",
        params![id, size],
    )?;
    Ok(())
}

fn external_root<'a>(
    path: &FsPath,
    media: &'a FsPath,
    downloads: &'a FsPath,
) -> Result<(&'a FsPath, &'static str), AppError> {
    if path.starts_with(media) {
        Ok((media, "media"))
    } else if path.starts_with(downloads) {
        Ok((downloads, "download"))
    } else {
        Err(AppError::bad_request(
            "Completed file is outside the media directories",
        ))
    }
}

fn mark_organized_source(
    conn: &Connection,
    root: &str,
    path: &str,
    size: i64,
) -> Result<bool, AppError> {
    Ok(conn.execute(
        "UPDATE library_automation SET external=1 WHERE media_id IN (
        SELECT m.id FROM organizer_history h JOIN library_media m ON m.id=h.media_id
        WHERE h.source_root=? AND h.source_path=? AND h.status='completed' AND m.available=1
          AND m.root='media' AND m.relative_path=h.target_path AND m.size=?)",
        params![root, path, size],
    )? > 0)
}

pub(crate) fn acknowledge_organized_external(
    state: &Shared,
    path: &FsPath,
    size: u64,
) -> Result<bool, AppError> {
    let media = fs::canonicalize(&state.media_dir)?;
    let downloads = fs::canonicalize(&state.download_dir)?;
    let (root, label) = external_root(path, &media, &downloads)?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AppError::bad_request("Invalid completion path"))?;
    if !relative
        .components()
        .all(|part| matches!(part, std::path::Component::Normal(_)))
    {
        return Ok(false);
    }
    let Some(relative) = relative.to_str() else {
        return Ok(false);
    };
    let Ok(size) = i64::try_from(size) else {
        return Ok(false);
    };
    mark_organized_source(&*state.conn()?, label, relative, size)
}

/// Called after the remote client reports completion. Commit the file identities and work
/// together so a replayed external event does not depend on the client's task history.
/// Blocking filesystem walk; invoke from spawn_blocking.
pub(crate) fn index_external(state: &Shared, path: &FsPath) -> Result<usize, AppError> {
    let _organizer_scan = organizer::scan_guard(state)?;
    let path = fs::canonicalize(path)?;
    let media = fs::canonicalize(&state.media_dir)?;
    let downloads = fs::canonicalize(&state.download_dir)?;
    let (root, label) = external_root(&path, &media, &downloads)?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| AppError::bad_request("Invalid completion path"))?;
    if relative
        .components()
        .any(|part| part.as_os_str().to_string_lossy().starts_with('.'))
    {
        return Err(AppError::bad_request("Hidden media is not indexed"));
    }
    let mut files = Vec::new();
    let metadata = fs::metadata(&path)?;
    if metadata.is_dir() {
        collect_files(root, &path, label, &mut files)?;
    } else if metadata.is_file() && is_video(&path) {
        files.push(ScannedFile {
            relative_path: relative
                .to_str()
                .ok_or_else(|| AppError::bad_request("Invalid filename"))?
                .to_owned(),
            root: label,
            size: i64::try_from(metadata.len())
                .map_err(|_| AppError::bad_request("Media file is too large"))?,
        });
    } else {
        return Err(AppError::bad_request("Unsupported completed file"));
    }
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let mut count = 0;
    for file in files {
        if let Some(id) = file.index(&tx)? {
            tx.execute(
                "UPDATE library_automation SET external=1 WHERE media_id=? AND external=0",
                [id],
            )?;
            count += 1;
        } else if mark_organized_source(&tx, file.root, &file.relative_path, file.size)? {
            count += 1;
        }
    }
    tx.commit()?;
    Ok(count)
}

#[derive(Clone, Copy)]
struct Features {
    organize: bool,
    scrape: bool,
    refresh: bool,
}

impl Features {
    fn read(conn: &Connection) -> Result<Self, AppError> {
        Ok(Self {
            organize: organizer::settings(conn)?.auto_organize,
            scrape: scraper::enabled(conn)?,
            refresh: tracking::automation::refresh_enabled(conn)?,
        })
    }

    fn discovery(self) -> bool {
        self.organize || self.scrape || self.refresh
    }
}

pub(crate) fn enabled(conn: &Connection) -> Result<bool, AppError> {
    Ok(Features::read(conn)?.discovery())
}

struct Job {
    id: i64,
    path: String,
    title: String,
    hint: Lookup,
    prepared: Option<Prepared>,
    organize: bool,
    scrape: bool,
    attempts: i64,
}

fn claim(state: &Shared, features: Features) -> Result<Option<Job>, AppError> {
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let row = tx.query_row("SELECT a.media_id,m.relative_path,COALESCE(json_extract(md.metadata_json,'$.title'),m.title),a.hint,a.prepared,
        (?1 AND NOT a.organized),((?2 OR a.manual) AND NOT a.scraped),a.attempts
        FROM library_automation a JOIN library_media m ON m.id=a.media_id
        LEFT JOIN media_metadata md ON md.media_id=m.id
        WHERE m.available=1 AND a.status<>'running' AND a.retry_at<=?4
        AND ((?1 AND NOT a.organized) OR ((?2 OR a.manual) AND NOT a.scraped) OR a.prepared IS NOT NULL
          OR ((?3 OR a.external OR a.organized OR a.scraped) AND NOT a.notified))
        ORDER BY a.manual DESC,a.retry_at,a.media_id LIMIT 1", params![features.organize,features.scrape,features.refresh,now()], |row| {
        Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get::<_,String>(3)?,row.get::<_,Option<String>>(4)?,row.get(5)?,row.get(6)?,row.get(7)?))
    }).optional()?;
    let Some((id, path, title, hint, prepared, organize, scrape, attempts)) = row else {
        return Ok(None);
    };
    let hint = serde_json::from_str(&hint)
        .map_err(|_| AppError::internal("Invalid stored metadata lookup"))?;
    let prepared = prepared
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(|_| AppError::internal("Invalid stored metadata result"))?;
    tx.execute(
        "UPDATE library_automation SET status='running',attempts=attempts+1 WHERE media_id=?",
        [id],
    )?;
    tx.commit()?;
    Ok(Some(Job {
        id,
        path,
        title,
        hint,
        prepared,
        organize,
        scrape,
        attempts,
    }))
}

async fn process(state: &Shared, job: &mut Job) -> Result<(), AppError> {
    if job.scrape && job.prepared.is_none() {
        let mut hint = job.hint.clone();
        let detected = organizer::detect_filename(FsPath::new(&job.path));
        if hint.title.is_none() && !detected.title.is_empty() {
            hint.title = Some(detected.title);
        }
        if hint.year.is_none() {
            hint.year = detected.year.map(|n| n.to_string());
        }
        let prepared = scraper::lookup_media(state, job.id, hint).await?;
        let raw = serde_json::to_string(&prepared)
            .map_err(|_| AppError::internal("Cannot save metadata lookup"))?;
        state.conn()?.execute(
            "UPDATE library_automation SET prepared=? WHERE media_id=?",
            params![raw, job.id],
        )?;
        job.prepared = Some(prepared);
    }
    if job.organize {
        let cached = if job.prepared.is_none() {
            scraper::metadata(&*state.conn()?, job.id)?
        } else {
            None
        };
        let recognized = job
            .prepared
            .as_ref()
            .map(|p| &p.metadata)
            .or(cached.as_ref());
        let title = recognized
            .map(|m| m.title.as_str())
            .or(job.hint.title.as_deref());
        let year = recognized
            .map(|m| m.year.as_str())
            .or(job.hint.year.as_deref())
            .and_then(|s| s.parse().ok());
        organizer::on_completion(state, job.id, title, year).await?;
        if let Some(cached) = cached {
            scraper::write_cached_sidecars(state, job.id).await?;
            job.title = cached.title;
        }
        state.conn()?.execute(
            "UPDATE library_automation SET organized=1,notified=0 WHERE media_id=?",
            [job.id],
        )?;
    }
    if let Some(prepared) = job.prepared.take() {
        let metadata = scraper::persist_media(state, job.id, prepared).await?;
        job.title = metadata.title;
        state.conn()?.execute("UPDATE library_automation SET scraped=1,notified=0,prepared=NULL,manual=0 WHERE media_id=?", [job.id])?;
    }
    tracking::automation::postprocess_complete(state, job.id, &job.title).await?;
    state.conn()?.execute("UPDATE library_automation SET status='done',notified=1,attempts=0,retry_at=0,error='' WHERE media_id=?", [job.id])?;
    Ok(())
}

/// One consumer per application. Persist each phase before dispatching the next, including
/// identification before changing the filename used for provider lookup.
pub fn start_automation_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _consumer = state.download_runtime.library_worker.lock().await;
        if let Err(error) = state.conn().and_then(|conn| {
            Ok(conn.execute(
                "UPDATE library_automation SET status='queued' WHERE status='running'",
                [],
            )?)
        }) {
            tracing::error!(error=%error.1, "Library queue recovery failed");
            return;
        }
        let mut last_scan = None;
        loop {
            let result = async {
                let features = Features::read(&*state.conn()?)?;
                if features.discovery() && last_scan.is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(30)) {
                    last_scan = Some(Instant::now());
                    let scan_state = state.clone();
                    let scan = tokio::task::spawn_blocking(move || scan_library(&scan_state)).await;
                    if !matches!(scan, Ok(Ok(_))) { tracing::warn!("Library discovery did not complete"); }
                }
                let Some(mut job) = claim(&state,features)? else { return Ok(false) };
                if let Err(error) = process(&state,&mut job).await {
                    let delay = 5_i64 << job.attempts.clamp(0, 8);
                    state.conn()?.execute("UPDATE library_automation SET status='retry',retry_at=?,error=? WHERE media_id=?",
                        params![now()+delay,error.1,job.id])?;
                    tracing::warn!(media_id=job.id,"Library automation will retry");
                }
                Ok::<_,AppError>(true)
            }.await;
            if let Err(error) = &result {
                tracing::warn!(error=%error.1,"Library automation queue unavailable");
            }
            if !matches!(result, Ok(true)) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    })
}

fn queue_manual(
    state: &Shared,
    id: i64,
    hint: Option<Lookup>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    check_media(&tx, id)?;
    let size = tx.query_row("SELECT size FROM library_media WHERE id=?", [id], |r| {
        r.get(0)
    })?;
    enqueue(&tx, id, size)?;
    let hint = hint
        .map(|h| serde_json::to_string(&h))
        .transpose()
        .map_err(|_| AppError::bad_request("Invalid lookup"))?;
    let updated = tx.execute(
        "UPDATE library_automation SET hint=COALESCE(?1,hint),manual=1,prepared=NULL,scraped=0,
        attempts=0,retry_at=0,error='',status='queued' WHERE media_id=?2 AND status<>'running'",
        params![hint, id],
    )?;
    if updated == 0 {
        return Err(AppError(
            StatusCode::CONFLICT,
            "Media is currently being processed".into(),
        ));
    }
    tx.commit()?;
    Ok((StatusCode::ACCEPTED, Json(json!({"status":"queued"}))))
}

pub(super) async fn scrape(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    require_admin(&user)?;
    queue_manual(&state, id, None)
}

pub(super) async fn reidentify(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(mut hint): Json<Lookup>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    require_admin(&user)?;
    hint.title = hint
        .title
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    hint.year = hint.year.filter(|s| !s.is_empty());
    if (hint.title.is_none() && hint.tmdb_id.is_none())
        || hint.title.as_ref().is_some_and(|s| s.len() > 1024)
        || hint
            .year
            .as_ref()
            .is_some_and(|s| s.len() != 4 || !s.bytes().all(|b| b.is_ascii_digit()))
        || hint.tmdb_id.is_some_and(|id| id <= 0)
    {
        return Err(AppError::bad_request("Invalid metadata lookup"));
    }
    queue_manual(&state, id, Some(hint))
}
