mod wishlist;

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    notify::{self, Channel, NotifySettings},
    settings::{get_setting_json, set_setting_json},
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use reqwest::header::HeaderValue;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use url::Url;

pub(super) use wishlist::sync_due;

pub(crate) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS media_completion_deliveries (
        media_id INTEGER NOT NULL REFERENCES library_media(id) ON DELETE CASCADE,
        channel TEXT NOT NULL, PRIMARY KEY(media_id,channel)
    );",
    )?;
    wishlist::init_schema(conn)
}

pub(crate) fn router() -> Router<Shared> {
    wishlist::router().route("/api/settings/media-server", get(read).put(save))
}

#[derive(Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ServerKind {
    Emby,
    #[default]
    Jellyfin,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaServer {
    enabled: bool,
    kind: ServerKind,
    url: String,
    api_key: String,
}

impl MediaServer {
    fn validate(&self) -> Result<(), AppError> {
        if self.api_key.len() > 4096
            || HeaderValue::from_str(&self.api_key).is_err()
            || self.api_key.chars().any(char::is_whitespace)
        {
            return Err(AppError::bad_request("无效的媒体服务器 API 密钥"));
        }
        if self.enabled && (self.url.is_empty() || self.api_key.is_empty()) {
            return Err(AppError::bad_request("请填写媒体服务器地址与 API 密钥"));
        }
        if !self.url.is_empty() {
            self.refresh_url()?;
        }
        Ok(())
    }

    fn refresh_url(&self) -> Result<Url, AppError> {
        let invalid = || AppError::bad_request("媒体服务器地址必须是 HTTP(S) 服务根地址");
        let mut url = Url::parse(&self.url).map_err(|_| invalid())?;
        if self.url.len() > 4096
            || self.url.chars().any(char::is_whitespace)
            || !matches!(url.scheme(), "http" | "https")
            || !url.has_host()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        let add_prefix =
            self.kind == ServerKind::Emby && !url.path().trim_end_matches('/').ends_with("/emby");
        {
            let mut segments = url.path_segments_mut().map_err(|_| invalid())?;
            segments.pop_if_empty();
            if add_prefix {
                segments.push("emby");
            }
            segments.extend(["Library", "Refresh"]);
        }
        Ok(url)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Backup {
    media_server: MediaServer,
}

pub(crate) fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    Ok(Backup {
        media_server: get_setting_json(conn, "media_server")?,
    })
}

pub(crate) fn restore_config(conn: &Connection, config: Backup) -> Result<(), AppError> {
    config.media_server.validate()?;
    set_setting_json(conn, "media_server", &config.media_server)
}

pub(crate) fn refresh_enabled(conn: &Connection) -> Result<bool, AppError> {
    Ok(get_setting_json::<MediaServer>(conn, "media_server")?.enabled)
}

#[derive(Deserialize)]
struct Update {
    kind: ServerKind,
    url: String,
    enabled: bool,
    api_key: Option<String>,
}

#[derive(Serialize)]
struct View {
    kind: ServerKind,
    url: String,
    enabled: bool,
    has_api_key: bool,
}

impl From<MediaServer> for View {
    fn from(server: MediaServer) -> Self {
        Self {
            kind: server.kind,
            url: server.url,
            enabled: server.enabled,
            has_api_key: !server.api_key.is_empty(),
        }
    }
}

async fn read(State(state): State<Shared>, user: AuthUser) -> Result<Json<View>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(
        get_setting_json::<MediaServer>(&conn, "media_server")?.into(),
    ))
}

async fn save(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<Update>,
) -> Result<Json<View>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let existing: MediaServer = get_setting_json(&conn, "media_server")?;
    let same_origin = Url::parse(&existing.url)
        .ok()
        .zip(Url::parse(&input.url).ok())
        .is_some_and(|(old, new)| old.origin() == new.origin());
    let api_key = input.api_key.unwrap_or_else(|| {
        if same_origin {
            existing.api_key
        } else {
            String::new()
        }
    });
    let config = MediaServer {
        kind: input.kind,
        url: input.url,
        enabled: input.enabled,
        api_key,
    };
    config.validate()?;
    set_setting_json(&conn, "media_server", &config)?;
    Ok(Json(config.into()))
}

async fn refresh(config: &MediaServer) -> Result<(), AppError> {
    let mut token = HeaderValue::from_str(&config.api_key)
        .map_err(|_| AppError::bad_request("无效的媒体服务器 API 密钥"))?;
    token.set_sensitive(true);
    // Never forward a custom token header to a redirect destination.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| AppError::internal("Could not initialize media server client"))?;
    let response = client
        .post(config.refresh_url()?)
        .header("X-Emby-Token", token)
        .send()
        .await
        .map_err(|_| {
            AppError(
                StatusCode::BAD_GATEWAY,
                "无法连接媒体服务器，请检查地址及网络".into(),
            )
        })?;
    if !response.status().is_success() {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            format!(
                "媒体库刷新失败（HTTP {}），请检查服务地址及密钥权限",
                response.status()
            ),
        ));
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/html"))
    {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            "媒体服务器返回了网页，请检查 API 根地址和反向代理".into(),
        ));
    }
    Ok(())
}

fn delivered(state: &Shared, media_id: i64, channel: &str) -> Result<bool, AppError> {
    Ok(state.conn()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM media_completion_deliveries WHERE media_id=? AND channel=?)",
        params![media_id, channel],
        |r| r.get(0),
    )?)
}

fn record(state: &Shared, media_id: i64, channel: &str) -> Result<(), AppError> {
    state.conn()?.execute(
        "INSERT OR IGNORE INTO media_completion_deliveries(media_id,channel) VALUES(?,?)",
        params![media_id, channel],
    )?;
    Ok(())
}

/// The processing queue serializes work per media row. Persist acknowledgements so a
/// failed channel can be retried without replaying the other completion actions.
pub async fn postprocess_complete(
    state: &Shared,
    media_id: i64,
    title: &str,
) -> Result<(), AppError> {
    let (server, notifications): (MediaServer, NotifySettings) = {
        let conn = state.conn()?;
        (
            get_setting_json(&conn, "media_server")?,
            get_setting_json(&conn, "notify")?,
        )
    };
    if server.enabled && !delivered(state, media_id, "refresh")? {
        refresh(&server).await?;
        record(state, media_id, "refresh")?;
    }
    let mut errors = Vec::new();
    for (channel, url) in notifications.channels() {
        let key = match channel {
            Channel::Bark => "bark",
            Channel::WebhookPost => "webhook_post",
            Channel::WebhookGet => "webhook_get",
        };
        if !delivered(state, media_id, key)? {
            match notify::deliver(state, channel, url, "媒体处理完成", title).await {
                Ok(()) => record(state, media_id, key)?,
                Err(message) => errors.push(format!("{key} 通知失败：{message}")),
            }
        }
    }
    if !errors.is_empty() {
        return Err(AppError(StatusCode::BAD_GATEWAY, errors.join("；")));
    }
    Ok(())
}
