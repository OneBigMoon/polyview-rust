use super::{pan115, quark, tianyi};
use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    playback,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
    routing::{get, post, put},
};
use reqwest::{RequestBuilder, Url};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, io::Write, net::IpAddr, time::Duration};
use uuid::Uuid;

pub(crate) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cloud_accounts (
            id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, provider TEXT NOT NULL,
            default_dir TEXT NOT NULL, base_endpoint TEXT NOT NULL,
            members_allowed INTEGER NOT NULL DEFAULT 0, cookie TEXT NOT NULL DEFAULT '',
            token TEXT NOT NULL DEFAULT '', revision TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS cloud_playback_grants (
            grant TEXT PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES cloud_accounts(id) ON DELETE CASCADE,
            owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE, media_id TEXT NOT NULL
        );",
    )?;
    Ok(())
}

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/cloud/accounts", get(accounts).post(create))
        .route("/api/cloud/accounts/{id}", put(update).delete(remove))
        .route("/api/cloud/choices", get(choices))
        .route("/api/cloud/folders", get(folders))
        .route("/api/cloud/check", post(check))
        .route("/api/cloud/save", post(save))
        .route("/cloud/stream/{grant}", get(stream))
}

fn origin(provider: &str) -> Result<&'static str, AppError> {
    match provider {
        "quark" => Ok("https://drive-pc.quark.cn"),
        "115" => Ok("https://webapi.115.com"),
        "tianyi" => Ok("https://cloud.189.cn"),
        _ => Err(AppError::bad_request("Unsupported cloud provider")),
    }
}

fn root(provider: &str) -> &'static str {
    if provider == "tianyi" { "-11" } else { "0" }
}

fn validate_dir(dir: &str) -> Result<(), AppError> {
    if dir.is_empty()
        || dir.len() > 256
        || !dir
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err(AppError::bad_request("Invalid provider directory ID"));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountBackup {
    id: i64,
    name: String,
    provider: String,
    default_dir: String,
    base_endpoint: String,
    members_allowed: bool,
    cookie: String,
    token: String,
}

pub(crate) type Backup = Vec<AccountBackup>;

impl AccountBackup {
    fn row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            provider: row.get(2)?,
            default_dir: row.get(3)?,
            base_endpoint: row.get(4)?,
            members_allowed: row.get(5)?,
            cookie: row.get(6)?,
            token: row.get(7)?,
        })
    }

    fn validate(&self) -> Result<(), AppError> {
        if self.name.trim().is_empty()
            || self.name.len() > 160
            || self.name.chars().any(char::is_control)
        {
            return Err(AppError::bad_request("Invalid cloud account name"));
        }
        if self.base_endpoint != origin(&self.provider)? {
            return Err(AppError::bad_request(
                "Use the official API origin for this provider",
            ));
        }
        validate_dir(&self.default_dir)?;
        if self.cookie.is_empty() && self.token.is_empty() {
            return Err(AppError::bad_request(
                "A provider cookie or access token is required",
            ));
        }
        for value in [&self.cookie, &self.token] {
            if value.len() > 16_384 || !value.bytes().all(|c| (32..127).contains(&c)) {
                return Err(AppError::bad_request("Invalid provider credential"));
            }
        }
        Ok(())
    }

    fn store(&self, conn: &Connection) -> Result<i64, AppError> {
        self.validate()?;
        let id = conn.query_row(
            "INSERT INTO cloud_accounts(id,name,provider,default_dir,base_endpoint,members_allowed,cookie,token,revision)
             VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,provider=excluded.provider,
             default_dir=excluded.default_dir,base_endpoint=excluded.base_endpoint,members_allowed=excluded.members_allowed,
             cookie=excluded.cookie,token=excluded.token,revision=excluded.revision RETURNING id",
            params![(self.id > 0).then_some(self.id),self.name,self.provider,self.default_dir,self.base_endpoint,self.members_allowed,self.cookie,self.token,Uuid::new_v4().to_string()],
            |row| row.get(0),
        )?;
        Ok(id)
    }
}

const SELECT_ACCOUNT: &str = "SELECT id,name,provider,default_dir,base_endpoint,members_allowed,cookie,token,revision FROM cloud_accounts";

pub(crate) fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    let mut stmt = conn.prepare(&format!("{SELECT_ACCOUNT} ORDER BY id"))?;
    Ok(stmt
        .query_map([], AccountBackup::row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn restore_config(conn: &Connection, entries: Backup) -> Result<(), AppError> {
    let mut ids = HashSet::new();
    for entry in &entries {
        entry.validate()?;
        if entry.id <= 0 || !ids.insert(entry.id) {
            return Err(AppError::bad_request(
                "Invalid or repeated cloud account ID",
            ));
        }
    }
    conn.execute("DELETE FROM cloud_playback_grants", [])?;
    conn.execute("DELETE FROM cloud_accounts", [])?;
    for entry in entries {
        entry.store(conn)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountInput {
    name: String,
    provider: String,
    default_dir: Option<String>,
    base_endpoint: Option<String>,
    members_allowed: Option<bool>,
    cookie: Option<String>,
    token: Option<String>,
}

impl AccountInput {
    fn into_record(self, previous: Option<AccountBackup>) -> Result<AccountBackup, AppError> {
        let official = origin(&self.provider)?;
        if previous
            .as_ref()
            .is_some_and(|record| record.provider != self.provider)
        {
            return Err(AppError::bad_request(
                "Create a new account to change providers",
            ));
        }
        let mut record = previous.unwrap_or_else(|| AccountBackup {
            id: 0,
            name: String::new(),
            provider: self.provider.clone(),
            default_dir: root(&self.provider).into(),
            base_endpoint: official.into(),
            members_allowed: false,
            cookie: String::new(),
            token: String::new(),
        });
        record.name = self.name.trim().to_owned();
        if let Some(dir) = self.default_dir {
            record.default_dir = if dir.is_empty() {
                root(&self.provider).into()
            } else {
                dir
            };
        }
        if let Some(base) = self.base_endpoint {
            record.base_endpoint = if base.is_empty() {
                official.into()
            } else {
                base.strip_suffix('/').unwrap_or(&base).into()
            };
        }
        if let Some(allowed) = self.members_allowed {
            record.members_allowed = allowed;
        }
        if let Some(cookie) = self.cookie {
            record.cookie = cookie.trim().into();
        }
        if let Some(token) = self.token {
            record.token = token.trim().into();
        }
        Ok(record)
    }
}

#[derive(Serialize)]
pub(crate) struct AccountChoice {
    pub id: i64,
    pub name: String,
    pub provider: String,
    pub default_dir: String,
}

impl AccountChoice {
    fn row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            provider: row.get(2)?,
            default_dir: row.get(3)?,
        })
    }
}

#[derive(Serialize)]
struct AdminView {
    #[serde(flatten)]
    choice: AccountChoice,
    base_endpoint: String,
    members_allowed: bool,
    credential_configured: bool,
}

impl AdminView {
    fn row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            choice: AccountChoice::row(row)?,
            base_endpoint: row.get(4)?,
            members_allowed: row.get(5)?,
            credential_configured: row.get(6)?,
        })
    }
}

const SELECT_VIEW: &str = "SELECT id,name,provider,default_dir,base_endpoint,members_allowed,(cookie != '' OR token != '') FROM cloud_accounts";

async fn accounts(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<AdminView>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!("{SELECT_VIEW} ORDER BY id"))?;
    Ok(Json(
        stmt.query_map([], AdminView::row)?
            .collect::<rusqlite::Result<_>>()?,
    ))
}

fn missing() -> AppError {
    AppError::not_found("Cloud account is unavailable")
}

fn write_account(
    state: &Shared,
    user: &AuthUser,
    id: Option<i64>,
    body: Result<Json<AccountInput>, JsonRejection>,
) -> Result<AdminView, AppError> {
    require_admin(user)?;
    let Json(input) =
        body.map_err(|_| AppError::bad_request("Invalid cloud account configuration"))?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let previous = if let Some(id) = id {
        Some(
            tx.query_row(
                &format!("{SELECT_ACCOUNT} WHERE id=?"),
                [id],
                AccountBackup::row,
            )
            .optional()?
            .ok_or_else(missing)?,
        )
    } else {
        None
    };
    let record = input.into_record(previous)?;
    let saved = record.store(&tx)?;
    tx.execute(
        "DELETE FROM cloud_playback_grants WHERE account_id=?",
        [saved],
    )?;
    let view = tx.query_row(
        &format!("{SELECT_VIEW} WHERE id=?"),
        [saved],
        AdminView::row,
    )?;
    tx.commit()?;
    Ok(view)
}

async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    body: Result<Json<AccountInput>, JsonRejection>,
) -> Result<(StatusCode, Json<AdminView>), AppError> {
    Ok((
        StatusCode::CREATED,
        Json(write_account(&state, &user, None, body)?),
    ))
}

async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    body: Result<Json<AccountInput>, JsonRejection>,
) -> Result<Json<AdminView>, AppError> {
    Ok(Json(write_account(&state, &user, Some(id), body)?))
}

async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    if state
        .conn()?
        .execute("DELETE FROM cloud_accounts WHERE id=?", [id])?
        == 0
    {
        return Err(missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn choices_for(state: &Shared, user: &AuthUser) -> Result<Vec<AccountChoice>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare("SELECT a.id,a.name,a.provider,a.default_dir FROM cloud_accounts a JOIN users u ON u.id=? AND u.disabled=0 WHERE u.role='admin' OR a.members_allowed=1 ORDER BY a.id")?;
    Ok(stmt
        .query_map([user.id], AccountChoice::row)?
        .collect::<rusqlite::Result<_>>()?)
}

async fn choices(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<AccountChoice>>, AppError> {
    Ok(Json(choices_for(&state, &user)?))
}

pub(super) struct DriveAccount {
    config: AccountBackup,
    revision: String,
    fixture: Option<Url>,
}

impl DriveAccount {
    fn load(state: &Shared, user_id: i64, id: i64) -> Result<Self, AppError> {
        let (config, revision) = state.conn()?.query_row(
            &format!("{SELECT_ACCOUNT} WHERE id=? AND EXISTS (SELECT 1 FROM users WHERE id=? AND disabled=0 AND (role='admin' OR cloud_accounts.members_allowed=1))"),
            params![id,user_id], |row| Ok((AccountBackup::row(row)?, row.get(8)?)),
        ).optional()?.ok_or_else(missing)?;
        config.validate()?;
        Ok(Self {
            config,
            revision,
            fixture: state.cloud_fixture.clone(),
        })
    }

    #[cfg(test)]
    pub(super) fn fixture(provider: &str, origin_url: Url, cookie: &str) -> Self {
        Self {
            config: AccountBackup {
                id: 1,
                name: "fixture".into(),
                provider: provider.into(),
                default_dir: root(provider).into(),
                base_endpoint: origin(provider).unwrap().into(),
                members_allowed: false,
                cookie: cookie.into(),
                token: String::new(),
            },
            revision: String::new(),
            fixture: Some(origin_url),
        }
    }

    // Only process-injected fixtures can replace the transport origin. Account data and
    // request parameters must always pass the production origin checks first.
    fn transport_url(&self, url: Url) -> Result<Url, AppError> {
        let Some(fixture) = &self.fixture else {
            return Ok(url);
        };
        let loopback = match fixture.host() {
            Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
            Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
            _ => false,
        };
        if !loopback
            || fixture.scheme() != "http"
            || !fixture.username().is_empty()
            || fixture.password().is_some()
            || fixture.path() != "/"
            || fixture.query().is_some()
            || fixture.fragment().is_some()
        {
            return Err(AppError::internal("Invalid cloud fixture transport"));
        }
        let mut target = fixture.clone();
        target.set_path(url.path());
        target.set_query(url.query());
        Ok(target)
    }

    pub(super) fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, AppError> {
        let allowed = Url::parse(origin(&self.config.provider)?)
            .map_err(|_| AppError::internal("Invalid provider origin"))?;
        let url = allowed
            .join(path)
            .map_err(|_| AppError::bad_request("Invalid provider request"))?;
        if url.origin() != allowed.origin()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(AppError::bad_request("Invalid provider request origin"));
        }
        let mut req = playback::no_redirect_client()?
            .request(method, self.transport_url(url)?)
            .timeout(Duration::from_secs(30));
        if !self.config.cookie.is_empty() {
            let mut header = HeaderValue::from_str(&self.config.cookie)
                .map_err(|_| AppError::bad_request("Invalid provider credential"))?;
            header.set_sensitive(true);
            req = req.header(header::COOKIE, header);
        }
        if !self.config.token.is_empty() {
            req = req.bearer_auth(&self.config.token);
        }
        Ok(req)
    }
}

#[derive(Serialize)]
pub(super) struct Folder {
    pub id: String,
    pub name: String,
}

#[derive(Serialize)]
pub(crate) struct CheckedShare {
    pub valid: bool,
    pub title: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SavedShare {
    pub path: String,
    pub task_id: Option<String>,
    #[serde(skip)]
    pub(super) media_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FolderQuery {
    account_id: i64,
    dir: Option<String>,
}

async fn folders(
    State(state): State<Shared>,
    user: AuthUser,
    Query(input): Query<FolderQuery>,
) -> Result<Json<Vec<Folder>>, AppError> {
    let account = DriveAccount::load(&state, user.id, input.account_id)?;
    let provider = account.config.provider.as_str();
    let dir = input.dir.as_deref().unwrap_or_else(|| root(provider));
    validate_dir(dir)?;
    let result = match provider {
        "quark" => quark::folders(&account, dir).await?,
        "115" => pan115::folders(&account, dir).await?,
        "tianyi" => tianyi::folders(&account, dir).await?,
        _ => return Err(missing()),
    };
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckInput {
    account_id: i64,
    url: String,
    code: Option<String>,
}

fn validate_share(url: &str, code: Option<&str>) -> Result<(), AppError> {
    if url.len() > 4096
        || url.chars().any(char::is_control)
        || code.is_some_and(|s| s.len() > 64 || s.chars().any(char::is_control))
    {
        return Err(AppError::bad_request(
            "Invalid cloud share link or extraction code",
        ));
    }
    Ok(())
}

async fn check(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<CheckInput>,
) -> Result<Json<CheckedShare>, AppError> {
    Ok(Json(
        check_for(
            &state,
            &user,
            input.account_id,
            &input.url,
            input.code.as_deref(),
        )
        .await?,
    ))
}

pub(crate) async fn check_for(
    state: &Shared,
    user: &AuthUser,
    account_id: i64,
    url: &str,
    code: Option<&str>,
) -> Result<CheckedShare, AppError> {
    let account = DriveAccount::load(state, user.id, account_id)?;
    validate_share(url, code)?;
    match account.config.provider.as_str() {
        "quark" => quark::check(&account, url, code).await,
        "115" => pan115::check(&account, url, code).await,
        "tianyi" => tianyi::check(&account, url, code).await,
        _ => Err(missing()),
    }
}

async fn transfer(
    account: &DriveAccount,
    url: &str,
    code: Option<&str>,
    dir: Option<&str>,
) -> Result<SavedShare, AppError> {
    validate_share(url, code)?;
    let dir = dir.unwrap_or(&account.config.default_dir);
    validate_dir(dir)?;
    match account.config.provider.as_str() {
        "quark" => quark::save(account, url, code, dir).await,
        "115" => pan115::save(account, url, code, dir).await,
        "tianyi" => tianyi::save(account, url, code, dir).await,
        _ => Err(missing()),
    }
}

pub(crate) async fn save_for(
    state: &Shared,
    user: &AuthUser,
    account_id: i64,
    url: &str,
    code: Option<&str>,
    dir: Option<&str>,
) -> Result<SavedShare, AppError> {
    let account = DriveAccount::load(state, user.id, account_id)?;
    transfer(&account, url, code, dir).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveInput {
    account_id: i64,
    url: String,
    code: Option<String>,
    dir: Option<String>,
    #[serde(default)]
    export_strm: bool,
}

#[derive(Serialize)]
struct SaveResult {
    #[serde(flatten)]
    saved: SavedShare,
    #[serde(skip_serializing_if = "Option::is_none")]
    strm_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    strm_notice: Option<&'static str>,
}

async fn save(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<SaveInput>,
) -> Result<Json<SaveResult>, AppError> {
    let account = DriveAccount::load(&state, user.id, input.account_id)?;
    let public_origin = if input.export_strm {
        require_admin(&user)?;
        let configured = match &state.cloud_fixture {
            Some(fixture) => fixture.to_string(),
            None => std::env::var("POLYVIEW_PUBLIC_URL").map_err(|_| {
                AppError::bad_request("Configure POLYVIEW_PUBLIC_URL before exporting STRM files")
            })?,
        };
        Some(crate::tvbox::validate_origin(&configured)?)
    } else {
        None
    };
    let saved = transfer(
        &account,
        &input.url,
        input.code.as_deref(),
        input.dir.as_deref(),
    )
    .await?;
    let mut result = SaveResult {
        saved,
        strm_file: None,
        strm_notice: None,
    };
    if let Some(origin) = public_origin {
        if let Some(media_id) = &result.saved.media_id {
            if quark::media_url(&account, media_id).await.is_err() {
                result.strm_notice = Some("已转存，但网盘尚未提供此文件的可播放视频地址。");
                return Ok(Json(result));
            }
            let media_id = media_id.clone();
            let export = tokio::task::spawn_blocking(move || {
                export_strm(&state, user.id, &account, &media_id, &origin)
            })
            .await;
            match export {
                Ok(Ok(name)) => result.strm_file = Some(name),
                _ => result.strm_notice = Some("已转存，未能写入 STRM 文件。"),
            }
        } else {
            result.strm_notice = Some("已提交转存。仅在网盘确认单个视频文件已完成后可导出 STRM。");
        }
    }
    Ok(Json(result))
}

fn export_strm(
    state: &Shared,
    owner: i64,
    account: &DriveAccount,
    media_id: &str,
    origin: &str,
) -> Result<String, AppError> {
    let grant = Uuid::new_v4().simple().to_string();
    let mut file = tempfile::Builder::new()
        .prefix("cloud-")
        .suffix(".strm")
        .tempfile_in(&state.media_dir)?;
    writeln!(file, "{origin}/cloud/stream/{grant}")?;
    file.as_file().sync_all()?;
    let name = file
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| AppError::internal("Invalid STRM filename"))?
        .to_owned();
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    // Refuse an export if credentials, account access, or the operator changed during the transfer.
    if tx.execute("INSERT INTO cloud_playback_grants(grant,account_id,owner_id,media_id)
        SELECT ?,a.id,u.id,? FROM cloud_accounts a,users u WHERE a.id=? AND a.revision=? AND u.id=? AND u.role='admin' AND u.disabled=0",
        params![grant,media_id,account.config.id,account.revision,owner])? != 1 { return Err(missing()); }
    let (_, path) = file.keep().map_err(|e| AppError::from(e.error))?;
    if let Err(error) = tx.commit() {
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(name)
}

async fn stream(
    State(state): State<Shared>,
    Path(grant): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (account, media_id) = {
        let conn = state.conn()?;
        let (config, revision, media_id) = conn.query_row(
            "SELECT a.id,a.name,a.provider,a.default_dir,a.base_endpoint,a.members_allowed,a.cookie,a.token,a.revision,g.media_id
             FROM cloud_playback_grants g JOIN cloud_accounts a ON a.id=g.account_id JOIN users u ON u.id=g.owner_id
             WHERE g.grant=? AND u.role='admin' AND u.disabled=0 AND a.provider='quark'",
            [grant],|row|Ok((AccountBackup::row(row)?,row.get(8)?,row.get::<_,String>(9)?)),
        ).optional()?.ok_or_else(||AppError::not_found("Playback grant unavailable"))?;
        config.validate()?;
        (
            DriveAccount {
                config,
                revision,
                fixture: state.cloud_fixture.clone(),
            },
            media_id,
        )
    };
    let media = account.transport_url(quark::media_url(&account, &media_id).await?)?;
    let mut request = playback::no_redirect_client()?
        .get(media)
        .header(header::ACCEPT_ENCODING, "identity");
    for name in [header::RANGE, header::IF_RANGE] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value);
        }
    }
    let upstream = tokio::time::timeout(Duration::from_secs(30), request.send())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| AppError(StatusCode::BAD_GATEWAY, "Cloud video is unavailable".into()))?;
    playback::relay_response(upstream, false, |_, _| {
        Err(AppError::bad_request("Expected a single video file"))
    })
    .await
}
