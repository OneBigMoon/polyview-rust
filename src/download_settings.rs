use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    settings::{get_setting_json, set_setting_json},
};
use axum::{Json, Router, extract::State, routing::get};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadSettings {
    pub enabled: bool,
    pub concurrency: u8,
    pub max_attempts: u8,
    pub n_m3u8dl_path: String,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub tdl_path: String,
    pub telegram_directory: String,
    pub telegram_namespace: String,
    pub telegram_proxy: String,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            concurrency: 1,
            max_attempts: 3,
            n_m3u8dl_path: "N_m3u8DL-RE".into(),
            ffmpeg_path: "ffmpeg".into(),
            ffprobe_path: "ffprobe".into(),
            tdl_path: "tdl".into(),
            telegram_directory: "./telegram".into(),
            telegram_namespace: "polyview".into(),
            telegram_proxy: String::new(),
        }
    }
}

impl DownloadSettings {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=4).contains(&self.concurrency) || !(1..=10).contains(&self.max_attempts) {
            return Err(AppError::bad_request(
                "Invalid download concurrency or attempt limit",
            ));
        }
        for path in [
            &self.n_m3u8dl_path,
            &self.ffmpeg_path,
            &self.ffprobe_path,
            &self.tdl_path,
            &self.telegram_directory,
        ] {
            if path.trim().is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
                return Err(AppError::bad_request(
                    "Invalid download tool or storage path",
                ));
            }
        }
        if self.telegram_namespace.is_empty()
            || self.telegram_namespace.len() > 100
            || !self
                .telegram_namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(AppError::bad_request("Invalid Telegram namespace"));
        }
        if !self.telegram_proxy.is_empty()
            && (self.telegram_proxy.len() > 8192
                || self.telegram_proxy.chars().any(char::is_control)
                || !Url::parse(&self.telegram_proxy).is_ok_and(|url| {
                    url.has_host()
                        && matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                }))
        {
            return Err(AppError::bad_request("Invalid Telegram proxy URL"));
        }
        Ok(())
    }
}

pub fn download_settings(conn: &Connection) -> Result<DownloadSettings, AppError> {
    let settings: DownloadSettings = get_setting_json(conn, "downloads")?;
    settings.validate()?;
    Ok(settings)
}

pub fn router() -> Router<Shared> {
    Router::new().route("/api/settings/downloads", get(load).put(save))
}

fn public_settings(mut settings: DownloadSettings) -> Result<Json<Value>, AppError> {
    let has_proxy = !std::mem::take(&mut settings.telegram_proxy).is_empty();
    let mut value = serde_json::to_value(settings)
        .map_err(|_| AppError::internal("Could not serialize download settings"))?;
    let fields = value
        .as_object_mut()
        .ok_or_else(|| AppError::internal("Invalid download settings"))?;
    fields.remove("telegram_proxy");
    fields.insert("telegram_proxy_configured".into(), Value::Bool(has_proxy));
    Ok(Json(value))
}

async fn load(State(state): State<Shared>, user: AuthUser) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    public_settings(download_settings(&conn)?)
}

async fn save(
    State(state): State<Shared>,
    user: AuthUser,
    Json(mut input): Json<Map<String, Value>>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    input.remove("telegram_proxy_configured");
    if !input.contains_key("telegram_proxy") {
        input.insert(
            "telegram_proxy".into(),
            Value::String(download_settings(&conn)?.telegram_proxy),
        );
    }
    let settings: DownloadSettings = serde_json::from_value(Value::Object(input))
        .map_err(|_| AppError::bad_request("Invalid download settings"))?;
    settings.validate()?;
    set_setting_json(&conn, "downloads", &settings)?;
    drop(conn);
    state.download_runtime.wake();
    public_settings(settings)
}
