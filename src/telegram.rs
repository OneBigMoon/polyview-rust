pub(crate) mod browse;
pub mod login;
pub(crate) mod media;
pub mod native;
pub(crate) mod stream;
pub mod subscriptions;

use crate::{
    AppError, Shared,
    auth::{AuthUser, current_user, require_admin},
    download_settings::{DownloadSettings, download_settings},
};
use axum::{
    Extension, Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    routing::get,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{io::ErrorKind, path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{process::Command, sync::Mutex, time::timeout};
use uuid::Uuid;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS telegram_accounts (
            user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            account_key TEXT UNIQUE NOT NULL, attempt TEXT NOT NULL,
            root TEXT NOT NULL, namespace TEXT NOT NULL,
            phase TEXT NOT NULL CHECK(phase IN ('idle','waiting','verified','failed'))
        );",
    )?;
    login::init_schema(conn)?;
    browse::init_schema(conn)?;
    subscriptions::init_schema(conn)?;
    Ok(())
}

// Bound the number of simultaneous clients and serialize checks for one user.
// The SQLite connection is released before waiting for either the lock or tdl.
type ProbeLocks = Arc<[Mutex<()>; 16]>;

pub fn router() -> Router<Shared> {
    let locks: ProbeLocks = Arc::new(std::array::from_fn(|_| Mutex::new(())));
    Router::new()
        .route(
            "/api/telegram/login",
            get(session).post(session).delete(session),
        )
        .route("/api/telegram/login/requests", get(requests))
        .layer(Extension(locks))
        .merge(media::router())
        .merge(login::router())
        .merge(browse::router())
        .merge(stream::router())
        .merge(subscriptions::router())
}

fn signed_in(conn: &Connection, headers: &HeaderMap) -> Result<AuthUser, AppError> {
    current_user(conn, headers)?
        .ok_or_else(|| AppError(StatusCode::UNAUTHORIZED, "Please sign in".into()))
}

#[derive(Clone)]
struct Scope {
    account_key: String,
    attempt: String,
    phase: String,
}

struct Location {
    root: String,
    namespace: String,
}

impl Location {
    fn new(settings: &DownloadSettings, user_id: i64) -> Result<Self, AppError> {
        let root = std::path::absolute(&settings.telegram_directory)?;
        Ok(Self {
            root: root
                .into_os_string()
                .into_string()
                .map_err(|_| AppError::internal("Invalid Telegram storage path"))?,
            namespace: format!("{}_u{}", settings.telegram_namespace, user_id),
        })
    }

    fn scope(&self, conn: &Connection, user_id: i64) -> Result<Option<Scope>, AppError> {
        Ok(conn.query_row(
            "SELECT account_key,attempt,phase FROM telegram_accounts WHERE user_id=? AND root=? AND namespace=? AND phase!='idle'",
            params![user_id, self.root, self.namespace],
            |row| Ok(Scope { account_key: row.get(0)?, attempt: row.get(1)?, phase: row.get(2)? }),
        ).optional()?)
    }

    fn directory(&self, scope: &Scope) -> std::path::PathBuf {
        Path::new(&self.root)
            .join(&scope.account_key)
            .join(&scope.attempt)
    }

    fn storage(&self, scope: &Scope) -> String {
        let path = self.directory(scope);
        let path = path.to_string_lossy();
        // tdl's --storage is a pflag StringToString CSV record.
        if path.contains([',', '"']) {
            format!("type=bolt,\"path={}\"", path.replace('"', "\"\""))
        } else {
            format!("type=bolt,path={path}")
        }
    }

    fn command(&self, settings: &DownloadSettings, scope: &Scope) -> Command {
        let mut cmd = Command::new(&settings.tdl_path);
        cmd.arg("-n")
            .arg(&self.namespace)
            .arg("--storage")
            .arg(self.storage(scope));
        if !settings.telegram_proxy.is_empty() {
            cmd.arg("--proxy").arg(&settings.telegram_proxy);
        }
        cmd
    }
}

/// Resolve only a verified association for the current account and settings.
/// The caller holds the same connection lock when resolving the job owner.
pub(crate) fn tdl_command(
    conn: &Connection,
    settings: &DownloadSettings,
    user_id: i64,
) -> Result<Command, AppError> {
    let location = Location::new(settings, user_id)?;
    let scope = location
        .scope(conn, user_id)?
        .filter(|s| s.phase == "verified")
        .ok_or_else(|| {
            AppError(
                StatusCode::CONFLICT,
                "请先完成当前账号的 Telegram 主机授权".into(),
            )
        })?;
    Ok(location.command(settings, &scope))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum LoginState {
    Unconfigured,
    Idle,
    Waiting,
    Authorized,
    Error,
}

#[derive(Serialize)]
struct Status {
    state: LoginState,
    message: &'static str,
}

fn status(state: LoginState) -> Json<Status> {
    let message = match state {
        LoginState::Unconfigured => "主机未安装或未配置 tdl，请联系管理员。",
        LoginState::Idle => "可申请关联此账号，然后在可信主机终端完成 Telegram 授权。",
        LoginState::Waiting => {
            "请联系主机管理员。在下载页的主机授权请求中找到你的账号，由管理员在可信主机终端运行对应命令；仅在该终端完成 Telegram 登录，再检查状态。"
        }
        LoginState::Authorized => "当前账号已通过 Telegram 授权验证。",
        LoginState::Error => "无法验证 Telegram 授权，请检查主机连接和 tdl 配置后重试。",
    };
    Json(Status { state, message })
}

enum Outcome {
    Success,
    Failed,
    Missing,
    Error,
}

async fn run_quietly(command: &mut Command, limit: Duration) -> Outcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == ErrorKind::NotFound => return Outcome::Missing,
        Err(_) => return Outcome::Error,
    };
    match timeout(limit, child.wait()).await {
        Ok(Ok(exit)) if exit.success() => Outcome::Success,
        Ok(Ok(_)) => Outcome::Failed,
        _ => Outcome::Error,
    }
}

async fn session(
    State(state): State<Shared>,
    Extension(locks): Extension<ProbeLocks>,
    method: Method,
    headers: HeaderMap,
) -> Result<Json<Status>, AppError> {
    let user = signed_in(&*state.conn()?, &headers)?;
    let _probe = locks[user.id as usize % locks.len()].lock().await;
    let (settings, location, scope) = {
        let conn = state.conn()?;
        let user = signed_in(&conn, &headers)?;
        let settings = download_settings(&conn)?;
        let location = Location::new(&settings, user.id)?;
        if method == Method::DELETE {
            conn.execute(
                "UPDATE telegram_accounts SET phase='idle' WHERE user_id=? AND phase='waiting'",
                [user.id],
            )?;
        }
        let scope = location.scope(&conn, user.id)?;
        (settings, location, scope)
    };

    // --help avoids the default-storage initialization that tdl version performs.
    match run_quietly(
        Command::new(&settings.tdl_path).arg("--help"),
        Duration::from_secs(3),
    )
    .await
    {
        Outcome::Success => (),
        Outcome::Missing => return Ok(status(LoginState::Unconfigured)),
        _ => return Ok(status(LoginState::Error)),
    }
    let Some(scope) = scope else {
        if method != Method::POST {
            return Ok(status(LoginState::Idle));
        }
        let conn = state.conn()?;
        signed_in(&conn, &headers)?;
        conn.execute(
            "INSERT INTO telegram_accounts(user_id,account_key,attempt,root,namespace,phase) VALUES(?,?,?,?,?,'waiting')
             ON CONFLICT(user_id) DO UPDATE SET attempt=excluded.attempt,root=excluded.root,namespace=excluded.namespace,phase='waiting'",
            params![user.id, Uuid::new_v4().simple().to_string(), Uuid::new_v4().simple().to_string(), location.root, location.namespace],
        )?;
        return Ok(status(LoginState::Waiting));
    };

    // A fresh request does not create private files. The official host login
    // creates the directory under umask 077. Its presence only permits a probe.
    let outcome = match tokio::fs::metadata(location.directory(&scope)).await {
        Ok(meta) if meta.is_dir() => {
            run_quietly(
                location.command(&settings, &scope).args(["chat", "ls"]),
                Duration::from_secs(12),
            )
            .await
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Outcome::Failed,
        _ => Outcome::Error,
    };
    let conn = state.conn()?;
    signed_in(&conn, &headers)?;
    let (next, response) = match outcome {
        Outcome::Success => ("verified", LoginState::Authorized),
        Outcome::Missing => (scope.phase.as_str(), LoginState::Unconfigured),
        Outcome::Failed if scope.phase == "waiting" || method == Method::POST => {
            ("waiting", LoginState::Waiting)
        }
        _ if scope.phase == "waiting" => ("waiting", LoginState::Error),
        _ => ("failed", LoginState::Error),
    };
    let updated = conn.execute(
        "UPDATE telegram_accounts SET phase=? WHERE user_id=? AND account_key=? AND attempt=? AND phase=?",
        params![next, user.id, scope.account_key, scope.attempt, scope.phase],
    )?;
    Ok(status(if updated == 1 {
        response
    } else {
        LoginState::Idle
    }))
}

#[derive(Serialize)]
struct HostRequest {
    user_id: i64,
    username: String,
    command: String,
    uses_proxy: bool,
}

fn shell_word(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

async fn requests(
    State(state): State<Shared>,
    headers: HeaderMap,
) -> Result<Json<Vec<HostRequest>>, AppError> {
    let conn = state.conn()?;
    require_admin(&signed_in(&conn, &headers)?)?;
    let settings = download_settings(&conn)?;
    let root = Location::new(&settings, 0)?.root;
    let mut stmt = conn.prepare(
        "SELECT user_id,username,account_key,attempt,namespace FROM telegram_accounts JOIN users ON users.id=user_id
         WHERE phase='waiting' AND disabled=0 AND root=? AND namespace=(? || '_u' || user_id) ORDER BY user_id",
    )?;
    let executable = if Path::new(&settings.tdl_path).is_relative()
        && settings.tdl_path.contains(std::path::MAIN_SEPARATOR)
    {
        std::path::absolute(&settings.tdl_path)?
            .to_string_lossy()
            .into_owned()
    } else {
        settings.tdl_path.clone()
    };
    let viewer = std::env::current_exe()?;
    let rows = stmt.query_map(params![root, settings.telegram_namespace], |row| {
        let scope = Scope {
            account_key: row.get(2)?,
            attempt: row.get(3)?,
            phase: "waiting".into(),
        };
        let location = Location {
            root: root.clone(),
            namespace: row.get(4)?,
        };
        let uses_proxy = !settings.telegram_proxy.is_empty();
        let proxy = if uses_proxy {
            " --proxy \"${TDL_PROXY:?Set TDL_PROXY on this host}\""
        } else {
            ""
        };
        Ok(HostRequest {
            user_id: row.get(0)?,
            username: row.get(1)?,
            uses_proxy,
            command: format!(
                "(umask 077 && exec {} telegram-host-login {} -n {} --storage {}{} login -T qr)",
                shell_word(&viewer.to_string_lossy()),
                shell_word(&executable),
                shell_word(&location.namespace),
                shell_word(&location.storage(&scope)),
                proxy
            ),
        })
    })?;
    Ok(Json(rows.collect::<Result<_, _>>()?))
}

#[cfg(all(test, unix))]
mod tests;
