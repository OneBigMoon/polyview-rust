use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    settings::{get_setting_json, read_cached, set_setting_json, write_cached},
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

const KEY: &str = "calendar";
const MAX_AGE: i64 = 12 * 60 * 60;
const MAX_BODY: usize = 4 * 1024 * 1024;
const LABELS: [&str; 7] = [
    "星期一",
    "星期二",
    "星期三",
    "星期四",
    "星期五",
    "星期六",
    "星期日",
];

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct CalendarSettings {
    pub enabled: bool,
    pub api_url: String,
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            api_url: "https://api.bgm.tv/calendar".into(),
        }
    }
}

impl CalendarSettings {
    pub fn validate(&self) -> Result<(), AppError> {
        let invalid = || AppError::bad_request("Calendar mirror must be an HTTP or HTTPS URL");
        let url = Url::parse(&self.api_url).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.has_host()
            || self
                .api_url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid());
        }
        Ok(())
    }
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/calendar", get(calendar))
        .route("/api/settings/calendar", get(settings).put(save_settings))
}

async fn settings(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<CalendarSettings>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    get_setting_json(&conn, KEY).map(Json)
}

async fn save_settings(
    State(state): State<Shared>,
    user: AuthUser,
    Json(settings): Json<CalendarSettings>,
) -> Result<Json<CalendarSettings>, AppError> {
    require_admin(&user)?;
    settings.validate()?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    if settings != get_setting_json::<CalendarSettings>(&tx, KEY)? {
        tx.execute("DELETE FROM remote_cache WHERE cache_key=?", [KEY])?;
    }
    set_setting_json(&tx, KEY, &settings)?;
    tx.commit()?;
    Ok(Json(settings))
}

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn cleaned_days(entries: &[Value]) -> Vec<Value> {
    let mut items: [Vec<Value>; 7] = Default::default();
    for entry in entries {
        let Some(day @ 1..=7) = entry["weekday"]["id"].as_u64() else {
            continue;
        };
        let Some(subjects) = entry["items"].as_array() else {
            continue;
        };
        for subject in subjects {
            let Some(id) = subject["id"].as_u64().filter(|id| *id > 0) else {
                continue;
            };
            let raw = text(&subject["name"]);
            let Some(title) = text(&subject["name_cn"]).or(raw) else {
                continue;
            };
            let cover = ["large", "common", "medium", "small", "grid"]
                .into_iter()
                .find_map(|key| text(&subject["images"][key]))
                .unwrap_or_default();
            items[day as usize - 1].push(json!({
                "id": id, "title_cn": title, "title_raw": raw.unwrap_or_default(), "cover": cover,
                "score": subject["rating"]["score"].as_f64(), "air_date": text(&subject["air_date"]).unwrap_or_default()
            }));
        }
    }
    items
        .into_iter()
        .enumerate()
        .map(|(index, items)| {
            json!({
                "weekday": index + 1, "label_cn": LABELS[index], "items": items
            })
        })
        .collect()
}

fn upstream_error() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "Could not load the broadcast calendar".into(),
    )
}

async fn calendar(State(state): State<Shared>, _user: AuthUser) -> Result<Json<Value>, AppError> {
    let settings: CalendarSettings = {
        let conn = state.conn()?;
        let settings: CalendarSettings = get_setting_json(&conn, KEY)?;
        if !settings.enabled {
            return Ok(Json(json!({"enabled": false, "days": cleaned_days(&[])})));
        }
        if let Some(mut cached) = read_cached(&conn, KEY, MAX_AGE)?
            && cached["api_url"].as_str() == Some(settings.api_url.as_str())
            && cached["response"]["days"].is_array()
        {
            return Ok(Json(cached["response"].take()));
        }
        settings
    };
    let mut response = state
        .client
        .get(&settings.api_url)
        .header(
            reqwest::header::USER_AGENT,
            concat!("PolyView/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| upstream_error())?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| upstream_error())? {
        if chunk.len() > MAX_BODY - body.len() {
            return Err(upstream_error());
        }
        body.extend_from_slice(&chunk);
    }
    let entries: Vec<Value> = serde_json::from_slice(&body).map_err(|_| upstream_error())?;
    let mut cached = json!({"api_url": settings.api_url, "response": {"enabled": true, "days": cleaned_days(&entries)}});
    let conn = state.conn()?;
    if settings == get_setting_json::<CalendarSettings>(&conn, KEY)? {
        write_cached(&conn, KEY, &cached)?;
    }
    Ok(Json(cached["response"].take()))
}
