use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    settings::{get_setting_json, set_setting_json},
};
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use url::Url;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifySettings {
    pub bark_url: String,
    pub webhook_post_url: String,
    pub webhook_get_url: String,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Bark,
    WebhookPost,
    WebhookGet,
}

impl NotifySettings {
    pub(crate) fn channels(&self) -> impl Iterator<Item = (Channel, &str)> {
        [
            (Channel::Bark, self.bark_url.as_str()),
            (Channel::WebhookPost, self.webhook_post_url.as_str()),
            (Channel::WebhookGet, self.webhook_get_url.as_str()),
        ]
        .into_iter()
        .filter(|(_, url)| !url.is_empty())
    }

    pub fn validate(&self) -> Result<(), AppError> {
        for (_, raw) in self.channels() {
            endpoint(raw)?;
        }
        Ok(())
    }
}

fn endpoint(raw: &str) -> Result<Url, AppError> {
    let invalid = || AppError::bad_request("Enter a valid HTTP or HTTPS notification URL");
    if raw.len() > 4096 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid());
    }
    let url = Url::parse(raw).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
        return Err(invalid());
    }
    Ok(url)
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route(
            "/api/settings/notify",
            get(read_settings).put(save_settings),
        )
        .route("/api/settings/notify/test", post(test_send))
}

async fn read_settings(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<NotifySettings>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    get_setting_json(&conn, "notify").map(Json)
}

async fn save_settings(
    State(state): State<Shared>,
    user: AuthUser,
    Json(settings): Json<NotifySettings>,
) -> Result<Json<NotifySettings>, AppError> {
    require_admin(&user)?;
    settings.validate()?;
    let conn = state.conn()?;
    set_setting_json(&conn, "notify", &settings)?;
    Ok(Json(settings))
}

#[derive(Serialize)]
pub struct Delivery {
    pub channel: Channel,
    pub ok: bool,
    pub message: String,
}

pub(crate) async fn deliver(
    state: &Shared,
    channel: Channel,
    raw: &str,
    title: &str,
    body: &str,
) -> Result<(), String> {
    let mut url = endpoint(raw).map_err(|error| error.1)?;
    let request = match channel {
        Channel::Bark => {
            url.path_segments_mut()
                .map_err(|_| "Invalid Bark URL")?
                .pop_if_empty()
                .push(title)
                .push(body);
            state.client.get(url)
        }
        Channel::WebhookPost => state
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(
                json!({"title":title, "body":body, "text":format!("{title}\n{body}")}).to_string(),
            ),
        Channel::WebhookGet => {
            url.query_pairs_mut()
                .append_pair("title", title)
                .append_pair("body", body);
            state.client.get(url)
        }
    };
    let response = request
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                "Notification request timed out"
            } else {
                "Notification request failed"
            }
        })?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    Ok(())
}

pub async fn send_notification(
    state: &Shared,
    title: &str,
    body: &str,
) -> Result<Vec<Delivery>, AppError> {
    let settings: NotifySettings = {
        let conn = state.conn()?;
        get_setting_json(&conn, "notify")?
    };
    let mut deliveries = Vec::new();
    for (channel, raw) in settings.channels() {
        deliveries.push(async move {
            let result = deliver(state, channel, raw, title, body).await;
            Delivery {
                channel,
                ok: result.is_ok(),
                message: result.err().unwrap_or_else(|| "已发送".into()),
            }
        });
    }
    Ok(join_all(deliveries).await)
}

pub(crate) fn send_in_background(state: &Shared, title: &'static str, body: String) {
    let state = state.clone();
    tokio::spawn(async move {
        match send_notification(&state, title, &body).await {
            Ok(results) => {
                for result in results.into_iter().filter(|delivery| !delivery.ok) {
                    tracing::warn!(channel = ?result.channel, message = %result.message, "Notification delivery failed");
                }
            }
            Err(error) => tracing::warn!(error = %error.1, "Could not read notification settings"),
        }
    });
}

#[derive(Serialize)]
struct TestReport {
    results: Vec<Delivery>,
}

async fn test_send(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<TestReport>, AppError> {
    require_admin(&user)?;
    let results = send_notification(&state, "PolyView 测试通知", "这是一条测试消息。").await?;
    Ok(Json(TestReport { results }))
}
