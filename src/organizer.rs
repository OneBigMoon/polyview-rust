mod filename;
mod files;
mod journal;

pub use filename::{FilenameInfo, detect_filename};
pub use journal::{Record, init_schema, scan_guard, skip_scan_path};

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    settings::{get_setting_json, set_setting_json},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

fn conflict(message: &str) -> AppError {
    AppError(StatusCode::CONFLICT, message.to_owned())
}

#[derive(Debug, Default, Copy, Clone, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Hardlink,
    Move,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrganizerSettings {
    pub auto_organize: bool,
    pub mode: Mode,
    pub movie_template: String,
    pub episode_template: String,
}

impl Default for OrganizerSettings {
    fn default() -> Self {
        Self {
            auto_organize: false,
            mode: Mode::Hardlink,
            movie_template: "Movies/{title} ({year})/{title} ({year}).{ext}".into(),
            episode_template:
                "TV/{title} ({year})/Season {season:02}/{title} S{season:02}E{episode:02}.{ext}"
                    .into(),
        }
    }
}

impl OrganizerSettings {
    fn validate(&self) -> Result<(), AppError> {
        for (template, name) in [
            (&self.movie_template, "Film.2001.mkv"),
            (&self.episode_template, "Film.2001.S01E01.mkv"),
        ] {
            let mut probe = detect_filename(std::path::Path::new(name));
            probe.resolution = Some("1080p".into());
            probe.codec = Some("HEVC".into());
            if template.len() > 1024
                || !template.ends_with(".{ext}")
                || !template.contains("{title}")
            {
                return Err(AppError::bad_request(
                    "Organizer templates require {title} and a final .{ext}",
                ));
            }
            render_template(template, &probe)?;
        }
        Ok(())
    }
}

fn render_template(template: &str, info: &FilenameInfo) -> Result<String, AppError> {
    let safe_title = info.title.replace(
        |c: char| c.is_control() || matches!(c, '/' | '\\' | ':'),
        "_",
    );
    let safe_title = safe_title.trim().trim_matches('.').trim();
    if safe_title.is_empty() {
        return Err(AppError::bad_request("Organizer title is empty"));
    }
    let year = info.year.map(|n| n.to_string()).unwrap_or_default();
    let template = if year.is_empty() {
        template.replace(" ({year})", "")
    } else {
        template.to_owned()
    };
    let mut result = String::with_capacity(template.len());
    let mut rest = template.as_str();
    while let Some(i) = rest.find(['{', '}']) {
        result.push_str(&rest[..i]);
        let (token, tail) = rest[i..]
            .strip_prefix('{')
            .and_then(|s| s.split_once('}'))
            .ok_or_else(|| AppError::bad_request("Invalid organizer template braces"))?;
        match token {
            "title" => result.push_str(safe_title),
            "year" => result.push_str(&year),
            "season" | "season:02" | "episode" | "episode:02" => {
                let number = if token.starts_with("season") {
                    info.season
                } else {
                    info.episode
                };
                let n = number.ok_or_else(|| {
                    AppError::bad_request("Missing season or episode for organizer template")
                })?;
                if token.ends_with(":02") {
                    result.push_str(&format!("{n:02}"));
                } else {
                    result.push_str(&n.to_string());
                }
            }
            "resolution" => result.push_str(info.resolution.as_deref().unwrap_or_default()),
            "codec" => result.push_str(info.codec.as_deref().unwrap_or_default()),
            "ext" => result.push_str(&info.ext),
            _ => return Err(AppError::bad_request("Unknown organizer template field")),
        }
        rest = tail;
    }
    result.push_str(rest);
    if !files::relative_path(&result) {
        return Err(AppError::bad_request("Unsafe organizer destination"));
    }
    Ok(result)
}

pub type Backup = OrganizerSettings;

pub fn settings(conn: &Connection) -> Result<OrganizerSettings, AppError> {
    get_setting_json(conn, "organizer")
}

pub fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    settings(conn)
}

pub fn restore_config(conn: &Connection, config: Backup) -> Result<(), AppError> {
    config.validate()?;
    set_setting_json(conn, "organizer", &config)
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/settings/organizer", get(load).put(save))
        .route("/api/organizer/history", get(history))
        .route("/api/library/{id}/organize", post(organize))
        .route("/api/organizer/{id}/rollback", post(rollback))
}

async fn load(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<OrganizerSettings>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(settings(&conn)?))
}

async fn save(
    State(state): State<Shared>,
    user: AuthUser,
    Json(config): Json<OrganizerSettings>,
) -> Result<Json<OrganizerSettings>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    config.validate()?;
    set_setting_json(&conn, "organizer", &config)?;
    Ok(Json(config))
}

async fn history(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Record>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(journal::history(&conn)?))
}

async fn organize(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Record>, AppError> {
    require_admin(&user)?;
    let record = tokio::task::spawn_blocking(move || journal::organize(&state, id, None, None))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    Ok(Json(record))
}

async fn rollback(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Record>, AppError> {
    require_admin(&user)?;
    let record = tokio::task::spawn_blocking(move || journal::rollback(&state, id))
        .await
        .map_err(|e| AppError::internal(e.to_string()))??;
    Ok(Json(record))
}

pub async fn on_completion(
    state: &Shared,
    media_id: i64,
    title: Option<&str>,
    year: Option<u16>,
) -> Result<(), AppError> {
    let state = state.clone();
    let title = title.map(str::to_owned);
    tokio::task::spawn_blocking(move || {
        journal::automatic(&state, media_id, title.as_deref(), year)
    })
    .await
    .map_err(|e| AppError::internal(e.to_string()))?
}
