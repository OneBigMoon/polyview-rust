mod config;
mod protocol;
mod worker;

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post, put},
};
use config::{Account, Provider};
use futures_util::{StreamExt, stream};
use protocol::{Action, Added, Session};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

pub(crate) use config::{Backup, export_config, restore_config};
pub use worker::start_worker;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS downloaders (
        id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, provider TEXT NOT NULL, endpoint TEXT NOT NULL,
        username TEXT NOT NULL,password TEXT NOT NULL,remote_root TEXT NOT NULL,local_root TEXT NOT NULL,
        members_allowed INTEGER NOT NULL DEFAULT 0, revision TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS external_tasks (
        id INTEGER PRIMARY KEY AUTOINCREMENT, downloader_id INTEGER NOT NULL REFERENCES downloaders ON DELETE CASCADE,
        hash TEXT NOT NULL,owner_id INTEGER REFERENCES users ON DELETE SET NULL, name TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL DEFAULT 'queued',progress REAL NOT NULL DEFAULT 0,files TEXT NOT NULL DEFAULT '[]',
        UNIQUE(downloader_id,hash));
        CREATE TABLE IF NOT EXISTS external_additions (
        tag TEXT PRIMARY KEY,downloader_id INTEGER NOT NULL REFERENCES downloaders ON DELETE CASCADE,
        owner_id INTEGER NOT NULL REFERENCES users ON DELETE CASCADE,created_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS external_additions_downloader ON external_additions(downloader_id);
        CREATE TABLE IF NOT EXISTS external_completions (
        id INTEGER PRIMARY KEY AUTOINCREMENT,downloader_id INTEGER NOT NULL REFERENCES downloaders ON DELETE CASCADE,
        hash TEXT NOT NULL,directory TEXT NOT NULL,file TEXT NOT NULL,size INTEGER NOT NULL,
        done INTEGER NOT NULL DEFAULT 0,attempts INTEGER NOT NULL DEFAULT 0,retry_at INTEGER NOT NULL DEFAULT 0,last_error TEXT,
        UNIQUE(downloader_id,hash,directory,file));
        CREATE INDEX IF NOT EXISTS external_completions_due ON external_completions(done,retry_at);")?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/api/downloaders",
            get(config::accounts).post(config::create),
        )
        .route(
            "/api/downloaders/{id}",
            put(config::update).delete(config::remove),
        )
        .route("/api/downloaders/choices", get(config::choices))
        .route("/api/external-tasks", get(tasks).post(add))
        .route("/api/external-tasks/{id}", delete(remove_task))
        .route("/api/external-tasks/{id}/pause", post(pause))
        .route("/api/external-tasks/{id}/resume", post(resume))
}

#[derive(Serialize)]
struct File {
    name: String,
    size: u64,
    #[serde(skip)]
    ready: bool,
}
struct RemoteTask {
    hash: String,
    name: String,
    status: &'static str,
    progress: f64,
    directory: String,
    tags: String,
    files: Vec<File>,
}

fn missing() -> AppError {
    AppError::not_found("Downloader or task is unavailable")
}
fn conflict() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "Torrent already exists on this downloader".into(),
    )
}

// Serialize a daemon's snapshot, ownership assignment and controls across concurrent requests.
// A fixed lock table has bounded memory even as administrators replace accounts.
static LOCKS: [tokio::sync::Mutex<()>; 32] = [const { tokio::sync::Mutex::const_new(()) }; 32];
async fn account_lock(id: i64) -> tokio::sync::MutexGuard<'static, ()> {
    LOCKS[id as usize % LOCKS.len()].lock().await
}

fn current(conn: &Connection, id: i64, revision: &str) -> Result<(), AppError> {
    if !conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM downloaders WHERE id=? AND revision=?)",
        params![id, revision],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(missing());
    }
    Ok(())
}

fn permitted(account: &Account, user: &AuthUser) -> Result<(), AppError> {
    if user.role == "admin" || account.members_allowed {
        Ok(())
    } else {
        Err(missing())
    }
}

fn account_ids(state: &Shared, admin: bool) -> Result<Vec<i64>, AppError> {
    let conn = state.conn()?;
    Ok(conn
        .prepare("SELECT id FROM downloaders WHERE ? OR members_allowed ORDER BY id")?
        .query_map([admin], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn save_snapshot(
    state: &Shared,
    account: &Account,
    revision: &str,
    tasks: &[RemoteTask],
) -> Result<(), AppError> {
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    current(&tx, account.id, revision)?;
    let existing: Vec<(i64, String)> = tx
        .prepare("SELECT id,hash FROM external_tasks WHERE downloader_id=?")?
        .query_map([account.id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let seen: HashSet<_> = tasks.iter().map(|t| t.hash.as_str()).collect();
    for (id, hash) in existing {
        if !seen.contains(hash.as_str()) {
            tx.execute("DELETE FROM external_tasks WHERE id=?", [id])?;
        }
    }
    for task in tasks {
        let mut owner: Option<i64> = None;
        let mut ambiguous = false;
        // Only an unpredictable tag issued for a POST by this server can establish ownership.
        // The UPDATE never changes an existing row's owner, even if a duplicate add applied a tag.
        for tag in task
            .tags
            .split(',')
            .map(str::trim)
            .filter(|t| t.starts_with("polyview-"))
        {
            if let Some(id) = tx.query_row("DELETE FROM external_additions WHERE downloader_id=? AND tag=? RETURNING owner_id",params![account.id,tag],|r|r.get(0)).optional()? {
                ambiguous |= owner.is_some_and(|previous| previous != id);
                owner = Some(id);
            }
        }
        if ambiguous {
            owner = None;
        }
        let files = serde_json::to_string(&task.files)
            .map_err(|_| AppError::internal("Invalid downloader files"))?;
        tx.execute("INSERT INTO external_tasks(downloader_id,hash,owner_id,name,status,progress,files) VALUES(?,?,?,?,?,?,?)
             ON CONFLICT(downloader_id,hash) DO UPDATE SET name=excluded.name,status=excluded.status,progress=excluded.progress,files=excluded.files",
             params![account.id,task.hash,owner,task.name,task.status,task.progress,files])?;
        if task.progress == 1.0 && task.status != "failed" && !account.remote_root.is_empty() {
            for file in task
                .files
                .iter()
                .filter(|f| f.ready && crate::library::is_video(std::path::Path::new(&f.name)))
            {
                tx.execute("INSERT INTO external_completions(downloader_id,hash,directory,file,size) VALUES(?,?,?,?,?) ON CONFLICT(downloader_id,hash,directory,file) DO NOTHING",params![account.id,task.hash,task.directory,file.name,file.size])?;
            }
        }
    }
    tx.execute(
        "DELETE FROM external_additions WHERE downloader_id=? AND created_at<?",
        params![account.id, crate::now() - 86400],
    )?;
    tx.commit()?;
    Ok(())
}

async fn refresh(state: &Shared, id: i64, user: Option<&AuthUser>) -> Result<(), AppError> {
    let _guard = account_lock(id).await;
    let (account, revision) = config::load(&*state.conn()?, id)?;
    if let Some(user) = user {
        permitted(&account, user)?;
    }
    let mut session = Session::open(&account).await?;
    let result = session.list().await;
    session.close().await;
    save_snapshot(state, &account, &revision, &result?)
}

async fn tasks(State(state): State<Shared>, user: AuthUser) -> Result<Json<Vec<Value>>, AppError> {
    let admin = user.role == "admin";
    let ids = account_ids(&state, admin)?;
    let account_count = ids.len();
    let results = stream::iter(ids)
        .map(|id| {
            let state = &state;
            let user = &user;
            async move { (id, refresh(state, id, Some(user)).await) }
        })
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
    let failed: HashSet<_> = results
        .iter()
        .filter(|(_, r)| r.is_err())
        .map(|(id, _)| *id)
        .collect();
    let conn = state.conn()?;
    let mut stmt = conn.prepare("SELECT t.id,d.id,t.name,t.status,t.progress,t.files FROM external_tasks t JOIN downloaders d ON d.id=t.downloader_id WHERE ? OR (d.members_allowed AND t.owner_id=?) ORDER BY t.id DESC")?;
    let rows = stmt.query_map(params![admin,user.id],|r| {
        let id: i64 = r.get(1)?;
        let status = if failed.contains(&id) { "unavailable".to_owned() } else { r.get(3)? };
        let files: Value = serde_json::from_str(&r.get::<_,String>(5)?).map_err(|_|rusqlite::Error::InvalidQuery)?;
        Ok(json!({"id":r.get::<_,i64>(0)?,"downloader_id":id,"name":r.get::<_,String>(2)?,"status":status,"progress":r.get::<_,f64>(4)?,"files":files}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() && account_count > 0 && failed.len() == account_count {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            "Cannot reach the configured downloader".into(),
        ));
    }
    Ok(Json(rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddInput {
    downloader_id: i64,
    url: String,
}

fn validate_url(raw: &str) -> Result<(), AppError> {
    if raw.len() > 16384 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(AppError::bad_request("Invalid torrent URL"));
    }
    if raw.starts_with("magnet:?") {
        let url = url::Url::parse(raw).map_err(|_| AppError::bad_request("Invalid magnet URI"))?;
        if url.query_pairs().any(|(k, v)| k == "xt" && valid_topic(&v)) {
            return Ok(());
        }
        return Err(AppError::bad_request("Magnet URI requires a torrent hash"));
    }
    crate::network::http_url(raw)?;
    Ok(())
}

fn valid_topic(topic: &str) -> bool {
    if let Some(hash) = topic.strip_prefix("urn:btih:") {
        return (hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            || (hash.len() == 32
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_alphabetic() || (b'2'..=b'7').contains(&b)));
    }
    topic
        .strip_prefix("urn:btmh:1220")
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()))
}

async fn add(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<AddInput>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    validate_url(&input.url)?;
    let id = input.downloader_id;
    let _guard = account_lock(id).await;
    let (account, revision) = config::load(&*state.conn()?, id)?;
    permitted(&account, &user)?;
    let mut session = Session::open(&account).await?;
    let tag = format!("polyview-{}", uuid::Uuid::new_v4().simple());
    let result = async {
        if account.provider == Provider::Qbittorrent {
            let before = session.list().await?;
            save_snapshot(&state,&account,&revision,&before)?;
            if state.conn()?.execute("INSERT INTO external_additions(tag,downloader_id,owner_id,created_at) SELECT ?,id,?,? FROM downloaders WHERE id=? AND revision=?",params![tag,user.id,crate::now(),id,revision])? == 0 {
                return Err(missing());
            }
        }
        match session.add(&input.url,&tag).await? {
            Added::Duplicate => Err(conflict()),
            Added::Known(hash) => {
                let conn = state.conn()?;
                current(&conn,id,&revision)?;
                conn.execute("INSERT INTO external_tasks(downloader_id,owner_id,hash) VALUES(?,?,?) ON CONFLICT(downloader_id,hash) DO UPDATE SET owner_id=excluded.owner_id",params![id,user.id,hash])?;
                Ok(())
            }
            Added::Tagged => Ok(()),
        }
    }.await;
    session.close().await;
    result?;
    Ok((StatusCode::CREATED, Json(json!({"accepted":true}))))
}

fn task_authority(conn: &Connection, id: i64, user: &AuthUser) -> Result<(i64, String), AppError> {
    conn.query_row("SELECT t.downloader_id,t.hash FROM external_tasks t JOIN downloaders d ON d.id=t.downloader_id WHERE t.id=? AND (? OR (d.members_allowed AND t.owner_id=?))",params![id,user.role=="admin",user.id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or_else(missing)
}

async fn control(
    state: Shared,
    user: AuthUser,
    id: i64,
    action: Action,
) -> Result<StatusCode, AppError> {
    let (account_id, _) = task_authority(&*state.conn()?, id, &user)?;
    let _guard = account_lock(account_id).await;
    let (account, revision, hash) = {
        let conn = state.conn()?;
        let (id, hash) = task_authority(&conn, id, &user)?;
        let (account, revision) = config::load(&conn, id)?;
        (account, revision, hash)
    };
    let mut session = Session::open(&account).await?;
    let result = session.control(&hash, action).await;
    session.close().await;
    result?;
    if action == Action::Remove {
        let conn = state.conn()?;
        current(&conn, account_id, &revision)?;
        conn.execute("DELETE FROM external_tasks WHERE id=?", [id])?;
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn pause(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    control(state, user, id, Action::Pause).await
}
async fn resume(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    control(state, user, id, Action::Resume).await
}
async fn remove_task(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    control(state, user, id, Action::Remove).await
}
