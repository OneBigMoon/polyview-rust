use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::HeaderValue,
    routing::get,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS skip_configs (
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            media_key TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
            intro_sec REAL NOT NULL DEFAULT 0, outro_sec REAL NOT NULL DEFAULT 0,
            PRIMARY KEY(user_id, media_key)
        );",
    )?;
    Ok(())
}

pub fn get_setting_json<T: DeserializeOwned + Default>(
    conn: &Connection,
    key: &str,
) -> Result<T, AppError> {
    let raw: Option<String> = conn
        .query_row("SELECT value FROM app_settings WHERE key=?", [key], |row| {
            row.get(0)
        })
        .optional()?;
    match raw {
        None => Ok(T::default()),
        Some(text) => serde_json::from_str(&text)
            .map_err(|_| AppError::internal(format!("Invalid stored setting: {key}"))),
    }
}

pub fn set_setting_json<T: Serialize>(
    conn: &Connection,
    key: &str,
    value: &T,
) -> Result<(), AppError> {
    let text = serde_json::to_string(value)
        .map_err(|_| AppError::internal("Could not serialize setting"))?;
    conn.execute("INSERT INTO app_settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, &text])?;
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaySettings {
    pub ad_block: bool,
    pub play_referer: String,
    pub play_ua: String,
    pub image_referer: String,
}

impl PlaySettings {
    pub(crate) fn validate(&self) -> Result<(), AppError> {
        for value in [&self.play_ua, &self.play_referer, &self.image_referer] {
            if value.len() > 8192
                || value.chars().any(char::is_control)
                || HeaderValue::from_str(value).is_err()
            {
                return Err(AppError::bad_request("Invalid playback header value"));
            }
        }
        Ok(())
    }
}

pub fn play_settings(conn: &Connection) -> Result<PlaySettings, AppError> {
    get_setting_json(conn, "play")
}

async fn read_play(
    State(state): State<Shared>,
    _user: AuthUser,
) -> Result<Json<PlaySettings>, AppError> {
    let conn = state.conn()?;
    play_settings(&conn).map(Json)
}

async fn save_play(
    State(state): State<Shared>,
    user: AuthUser,
    Json(settings): Json<PlaySettings>,
) -> Result<Json<PlaySettings>, AppError> {
    require_admin(&user)?;
    settings.validate()?;
    let conn = state.conn()?;
    set_setting_json(&conn, "play", &settings)?;
    Ok(Json(settings))
}

#[derive(Deserialize)]
struct Key {
    key: String,
}

#[derive(Deserialize, Serialize)]
struct SkipConfig {
    media_key: String,
    enabled: bool,
    intro_sec: f64,
    outro_sec: f64,
}

fn validate_key(key: &str) -> Result<(), AppError> {
    if key.trim().is_empty() || key.len() > 2048 || key.chars().any(char::is_control) {
        return Err(AppError::bad_request("Invalid media key"));
    }
    Ok(())
}

async fn read_skip(
    State(state): State<Shared>,
    user: AuthUser,
    Query(input): Query<Key>,
) -> Result<Json<SkipConfig>, AppError> {
    validate_key(&input.key)?;
    let (enabled, intro_sec, outro_sec) = state
        .conn()?
        .query_row(
            "SELECT enabled,intro_sec,outro_sec FROM skip_configs WHERE user_id=? AND media_key=?",
            params![user.id, input.key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .unwrap_or((false, 0.0, 0.0));
    Ok(Json(SkipConfig {
        media_key: input.key,
        enabled,
        intro_sec,
        outro_sec,
    }))
}

async fn save_skip(
    State(state): State<Shared>,
    user: AuthUser,
    Json(config): Json<SkipConfig>,
) -> Result<Json<SkipConfig>, AppError> {
    validate_key(&config.media_key)?;
    if ![config.intro_sec, config.outro_sec]
        .iter()
        .all(|s| (0.0..=600.0).contains(s))
    {
        return Err(AppError::bad_request(
            "Skip times must be between 0 and 600 seconds",
        ));
    }
    state.conn()?.execute("INSERT INTO skip_configs(user_id,media_key,enabled,intro_sec,outro_sec) VALUES(?,?,?,?,?) ON CONFLICT(user_id,media_key) DO UPDATE SET enabled=excluded.enabled,intro_sec=excluded.intro_sec,outro_sec=excluded.outro_sec",params![user.id,config.media_key,config.enabled,config.intro_sec,config.outro_sec])?;
    Ok(Json(config))
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/settings/play", get(read_play).put(save_play))
        .route("/api/skip", get(read_skip).put(save_skip))
}
