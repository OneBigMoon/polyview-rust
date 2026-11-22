use crate::{
    AppError, Shared,
    auth::AuthUser,
    catalog::{self, CatalogDetail},
    network, now,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, time::Duration};
use tokio::task::JoinHandle;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tracking_subscriptions (
            id INTEGER PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE RESTRICT,
            media_id TEXT NOT NULL,
            title TEXT NOT NULL,
            cover TEXT NOT NULL DEFAULT '',
            total_episodes INTEGER NOT NULL DEFAULT 0,
            watched_episodes INTEGER NOT NULL DEFAULT 0,
            auto_download INTEGER NOT NULL DEFAULT 0 CHECK(auto_download IN (0,1)),
            last_checked INTEGER,
            error TEXT,
            UNIQUE(user_id,source_id,media_id)
        );
        CREATE TABLE IF NOT EXISTS tracking_downloads (
            subscription_id INTEGER NOT NULL REFERENCES tracking_subscriptions(id) ON DELETE CASCADE,
            episode_key TEXT NOT NULL,
            download_id INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
            PRIMARY KEY(subscription_id,episode_key)
        );
        CREATE INDEX IF NOT EXISTS tracking_user ON tracking_subscriptions(user_id,id);",
    )?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/tracking", get(list).post(create))
        .route("/api/tracking/check", post(check))
        .route("/api/tracking/{id}", delete(remove).put(update))
}

#[derive(Debug, Serialize)]
pub struct Subscription {
    pub id: i64,
    pub source_id: i64,
    pub media_id: String,
    pub title: String,
    pub cover: String,
    pub total_episodes: i64,
    pub watched_episodes: i64,
    pub auto_download: bool,
    pub last_checked: Option<i64>,
    pub error: Option<String>,
    pub queued_episodes: i64,
    pub completed_downloads: i64,
}

const SUBSCRIPTION_SELECT: &str =
    "SELECT s.id,s.source_id,s.media_id,s.title,s.cover,s.total_episodes,
            s.watched_episodes,s.auto_download,s.last_checked,s.error,
            (SELECT COUNT(*) FROM tracking_downloads td WHERE td.subscription_id=s.id),
            (SELECT COUNT(*) FROM tracking_downloads td JOIN downloads d ON d.id=td.download_id
             WHERE td.subscription_id=s.id AND d.status='completed')
     FROM tracking_subscriptions s";

fn subscription_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Subscription> {
    Ok(Subscription {
        id: row.get(0)?,
        source_id: row.get(1)?,
        media_id: row.get(2)?,
        title: row.get(3)?,
        cover: row.get(4)?,
        total_episodes: row.get(5)?,
        watched_episodes: row.get(6)?,
        auto_download: row.get(7)?,
        last_checked: row.get(8)?,
        error: row.get(9)?,
        queued_episodes: row.get(10)?,
        completed_downloads: row.get(11)?,
    })
}

fn owned_subscription(state: &Shared, id: i64, user_id: i64) -> Result<Subscription, AppError> {
    state
        .conn()?
        .query_row(
            &format!("{SUBSCRIPTION_SELECT} WHERE s.id=? AND s.user_id=?"),
            params![id, user_id],
            subscription_row,
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Subscription not found"))
}

async fn list(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Subscription>>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!(
        "{SUBSCRIPTION_SELECT} WHERE s.user_id=? ORDER BY s.id DESC"
    ))?;
    Ok(Json(
        stmt.query_map([user.id], subscription_row)?
            .collect::<Result<_, _>>()?,
    ))
}

#[derive(Deserialize)]
struct CreateInput {
    source_id: i64,
    media_id: String,
    #[serde(default)]
    auto_download: bool,
}

async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<CreateInput>,
) -> Result<(StatusCode, Json<Subscription>), AppError> {
    let media_id = input.media_id.trim();
    if media_id.is_empty()
        || media_id.chars().count() > 256
        || media_id.chars().any(char::is_control)
    {
        return Err(AppError::bad_request(
            "Media ID must contain 1–256 characters",
        ));
    }
    let id = {
        let conn = state.conn()?;
        let available: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM catalog_sources WHERE id=? AND enabled=1)",
            [input.source_id],
            |row| row.get(0),
        )?;
        if !available {
            return Err(AppError::not_found("Source not found or disabled"));
        }
        match conn.execute(
            "INSERT INTO tracking_subscriptions(user_id,source_id,media_id,title,auto_download)
             VALUES(?,?,?,?,?)",
            params![
                user.id,
                input.source_id,
                media_id,
                media_id,
                input.auto_download
            ],
        ) {
            Err(rusqlite::Error::SqliteFailure(code, _))
                if code.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(AppError(
                    StatusCode::CONFLICT,
                    "This title is already in your subscriptions".into(),
                ));
            }
            result => {
                result?;
            }
        }
        conn.last_insert_rowid()
    };
    // Keep a failed source visible in the list so it can be retried without losing the subscription.
    let _ = check_one(&state, id, user.id).await;
    Ok((
        StatusCode::CREATED,
        Json(owned_subscription(&state, id, user.id)?),
    ))
}

#[derive(Default, Deserialize)]
struct UpdateInput {
    watched_episodes: Option<i64>,
    auto_download: Option<bool>,
}

async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<UpdateInput>,
) -> Result<Json<Subscription>, AppError> {
    {
        let conn = state.conn()?;
        let (watched, total, auto): (i64, i64, bool) = conn
            .query_row(
                "SELECT watched_episodes,total_episodes,auto_download FROM tracking_subscriptions WHERE id=? AND user_id=?",
                params![id, user.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| AppError::not_found("Subscription not found"))?;
        let watched = input.watched_episodes.unwrap_or(watched);
        if watched < 0 || (input.watched_episodes.is_some() && watched > total) {
            return Err(AppError::bad_request(
                "Watched episodes must be between zero and the episode count",
            ));
        }
        conn.execute(
            "UPDATE tracking_subscriptions SET watched_episodes=?,auto_download=? WHERE id=? AND user_id=?",
            params![watched, input.auto_download.unwrap_or(auto), id, user.id],
        )?;
    }
    if input.auto_download == Some(true) {
        let _ = check_one(&state, id, user.id).await;
    }
    Ok(Json(owned_subscription(&state, id, user.id)?))
}

async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    if state.conn()?.execute(
        "DELETE FROM tracking_subscriptions WHERE id=? AND user_id=?",
        params![id, user.id],
    )? == 0
    {
        return Err(AppError::not_found("Subscription not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, Serialize)]
pub struct CheckResult {
    pub checked: usize,
    pub queued: usize,
    pub errors: Vec<CheckError>,
}

#[derive(Debug, Serialize)]
pub struct CheckError {
    pub id: i64,
    pub message: String,
}

async fn check(State(state): State<Shared>, user: AuthUser) -> Result<Json<CheckResult>, AppError> {
    check_user(&state, Some(user.id)).await.map(Json)
}

pub async fn check_user(state: &Shared, user_id: Option<i64>) -> Result<CheckResult, AppError> {
    let subscriptions: Vec<(i64, i64)> = {
        let conn = state.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id,user_id FROM tracking_subscriptions WHERE (?1 IS NULL OR user_id=?1) ORDER BY id",
        )?;
        stmt.query_map([user_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    let mut result = CheckResult::default();
    for (id, owner) in subscriptions {
        result.checked += 1;
        match check_one(state, id, owner).await {
            Ok(queued) => result.queued += queued,
            Err(error) => result.errors.push(CheckError {
                id,
                message: error.1,
            }),
        }
    }
    Ok(result)
}

async fn check_one(state: &Shared, id: i64, user_id: i64) -> Result<usize, AppError> {
    let subscription = owned_subscription(state, id, user_id)?;
    let detail = catalog::detail(state, subscription.source_id, &subscription.media_id).await;
    let result = detail.and_then(|detail| apply_detail(state, id, user_id, &detail));
    if let Err(ref error) = result {
        state.conn()?.execute(
            "UPDATE tracking_subscriptions SET last_checked=?,error=? WHERE id=? AND user_id=?",
            params![now(), error.1, id, user_id],
        )?;
    }
    result
}

fn apply_detail(
    state: &Shared,
    id: i64,
    user_id: i64,
    detail: &CatalogDetail,
) -> Result<usize, AppError> {
    let episodes = &detail
        .groups
        .iter()
        .find(|group| group.direct && !group.episodes.is_empty())
        .ok_or_else(|| {
            AppError(
                StatusCode::BAD_GATEWAY,
                "Source has no directly playable HTTP(S) episodes; parsing-only sources cannot be tracked for download".into(),
            )
        })?
        .episodes;
    if episodes.len() > 10_000 {
        return Err(AppError::bad_request(
            "A subscription supports at most 10,000 episodes",
        ));
    }
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let current: Option<(i64, bool)> = tx
        .query_row(
            "SELECT watched_episodes,auto_download FROM tracking_subscriptions WHERE id=? AND user_id=?",
            params![id, user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((watched, automatic)) = current else {
        return Err(AppError::not_found("Subscription not found"));
    };
    tx.execute(
        "UPDATE tracking_subscriptions SET title=?,cover=?,total_episodes=?,last_checked=?,error=NULL WHERE id=? AND user_id=?",
        params![detail.title, detail.cover, episodes.len() as i64, now(), id, user_id],
    )?;
    let mut queued = 0;
    if automatic {
        let mut seen = HashSet::new();
        for (index, episode) in episodes.iter().enumerate() {
            let ordinal = index as i64 + 1;
            // Episode names survive refreshed signed URLs; duplicate labels use their stable playlist position.
            let name = episode.name.trim().to_lowercase();
            let key = if name.is_empty() || !seen.insert(name.clone()) {
                format!("position:{ordinal}")
            } else {
                format!("name:{name}")
            };
            if ordinal <= watched {
                continue;
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tracking_downloads WHERE subscription_id=? AND episode_key=?)",
                params![id, key],
                |row| row.get(0),
            )?;
            if exists {
                continue;
            }
            let title = format!(
                "{} S01E{ordinal:03}",
                detail.title.chars().take(180).collect::<String>()
            );
            let download_id = network::enqueue_in(&tx, state, user_id, &title, &episode.url)?;
            tx.execute(
                "INSERT INTO tracking_downloads(subscription_id,episode_key,download_id) VALUES(?,?,?)",
                params![id, key, download_id],
            )?;
            queued += 1;
        }
    }
    tx.commit()?;
    Ok(queued)
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10 * 60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match check_user(&state, None).await {
                Ok(result) => {
                    for error in result.errors {
                        tracing::warn!(subscription_id = error.id, message = %error.message, "Subscription check failed");
                    }
                }
                Err(error) => tracing::error!(message = %error.1, "Could not check subscriptions"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn state() -> (TempDir, Shared) {
        let temp = tempfile::tempdir().unwrap();
        let media_dir = temp.path().canonicalize().unwrap();
        let download_dir = media_dir.join("downloads");
        std::fs::create_dir(&download_dir).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE users(id INTEGER PRIMARY KEY); INSERT INTO users VALUES(1),(2);").unwrap();
        catalog::init_schema(&conn).unwrap();
        network::init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            media_dir,
            download_dir,
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        (temp, state)
    }

    fn user(id: i64) -> AuthUser {
        AuthUser {
            id,
            username: format!("user{id}"),
            role: "user".into(),
        }
    }

    fn cms(episodes: &str) -> Value {
        json!({"code":1,"list":[{"vod_id":42,"vod_name":"中文故事","vod_pic":"",
            "vod_play_from":"文件","vod_play_url":episodes}]})
    }

    fn cms_groups(from: &str, playlists: &str) -> Value {
        json!({"code":1,"list":[{"vod_id":42,"vod_name":"中文故事","vod_pic":"",
            "vod_play_from":from,"vod_play_url":playlists}]})
    }

    async fn fixture(state: &Shared, value: Value) -> (Arc<Mutex<Value>>, JoinHandle<()>, i64) {
        let response = Arc::new(Mutex::new(value));
        let served = response.clone();
        let app = Router::new().route(
            "/cms",
            get(move || {
                let served = served.clone();
                async move { Json(served.lock().unwrap().clone()) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let id = {
            let conn = state.conn().unwrap();
            conn.execute(
                "INSERT INTO catalog_sources(name,url) VALUES('fixture',?)",
                [format!("http://{address}/cms")],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        (response, task, id)
    }

    async fn subscribe(state: &Shared, source_id: i64, automatic: bool) -> Subscription {
        let (status, Json(subscription)) = create(
            State(state.clone()),
            user(1),
            Json(CreateInput {
                source_id,
                media_id: "42".into(),
                auto_download: automatic,
            }),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::CREATED);
        subscription
    }

    fn download_count(state: &Shared) -> i64 {
        state
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM downloads", [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn checks_append_new_episodes_without_duplicate_jobs_or_signed_url_duplicates() {
        let (_temp, state) = state();
        let (response, server, source_id) = fixture(
            &state,
            cms("第1集$https://example.org/1.mp4#第2集$https://example.org/2.m3u8"),
        )
        .await;
        let subscription = subscribe(&state, source_id, true).await;
        assert_eq!(subscription.total_episodes, 2);
        assert_eq!(subscription.queued_episodes, 2);
        assert_eq!(download_count(&state), 2);
        let (first, second) =
            tokio::join!(check_user(&state, Some(1)), check_user(&state, Some(1)));
        assert_eq!(first.unwrap().queued, 0);
        assert_eq!(second.unwrap().queued, 0);
        *response.lock().unwrap() = cms(
            "第1集$https://example.org/1.mp4?token=refreshed#第2集$https://example.org/2.m3u8#第3集$https://example.org/3.mp4",
        );
        assert_eq!(check_user(&state, Some(1)).await.unwrap().queued, 1);
        assert_eq!(download_count(&state), 3);
        let refreshed = owned_subscription(&state, subscription.id, 1).unwrap();
        assert_eq!(refreshed.total_episodes, 3);
        assert!(refreshed.error.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn tracking_is_user_owned_and_auto_download_skips_watched_episodes() {
        let (_temp, state) = state();
        let (_response, server, source_id) = fixture(
            &state,
            cms("第1集$https://example.org/1.mp4#第2集$https://example.org/2.mp4"),
        )
        .await;
        let subscription = subscribe(&state, source_id, false).await;
        assert_eq!(download_count(&state), 0);
        assert!(
            list(State(state.clone()), user(2))
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(
            update(
                State(state.clone()),
                user(2),
                Path(subscription.id),
                Json(UpdateInput {
                    watched_episodes: Some(1),
                    auto_download: None
                }),
            )
            .await
            .unwrap_err()
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            remove(State(state.clone()), user(2), Path(subscription.id))
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let Json(updated) = update(
            State(state.clone()),
            user(1),
            Path(subscription.id),
            Json(UpdateInput {
                watched_episodes: Some(1),
                auto_download: Some(true),
            }),
        )
        .await
        .unwrap();
        assert_eq!(updated.watched_episodes, 1);
        assert_eq!(updated.queued_episodes, 1);
        assert_eq!(download_count(&state), 1);
        assert!(check_user(&state, Some(2)).await.unwrap().errors.is_empty());
        assert_eq!(check_user(&state, Some(2)).await.unwrap().checked, 0);
        assert!(
            update(
                State(state.clone()),
                user(1),
                Path(subscription.id),
                Json(UpdateInput {
                    watched_episodes: Some(3),
                    auto_download: None
                })
            )
            .await
            .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn source_failures_are_visible_and_retry_does_not_lose_subscription() {
        let (_temp, state) = state();
        let (response, server, source_id) = fixture(&state, cms("第1集$javascript:alert(1)")).await;
        let subscription = subscribe(&state, source_id, true).await;
        assert!(
            subscription
                .error
                .as_ref()
                .unwrap()
                .contains("no directly playable")
        );
        assert_eq!(subscription.queued_episodes, 0);
        *response.lock().unwrap() =
            cms("第1集$https://example.org/1.mp4#第2集$https://example.org/2.mp4");
        let result = check_user(&state, Some(1)).await.unwrap();
        assert_eq!(result.queued, 2);
        assert!(result.errors.is_empty());
        let subscription = owned_subscription(&state, subscription.id, 1).unwrap();
        assert!(subscription.error.is_none());
        state
            .conn()
            .unwrap()
            .execute(
                "UPDATE catalog_sources SET enabled=0 WHERE id=?",
                [source_id],
            )
            .unwrap();
        assert_eq!(check_user(&state, Some(1)).await.unwrap().errors.len(), 1);
        assert_eq!(download_count(&state), 2);
        assert!(
            owned_subscription(&state, subscription.id, 1)
                .unwrap()
                .error
                .is_some()
        );
        server.abort();
    }

    #[tokio::test]
    async fn invalid_download_rolls_back_episode_claims_and_retry_can_queue_all() {
        let (_temp, state) = state();
        let (response, server, source_id) = fixture(
            &state,
            cms("第1集$https://example.org/1.mp4#第2集$https://example.org/player.html"),
        )
        .await;
        let subscription = subscribe(&state, source_id, true).await;
        assert!(subscription.error.is_some());
        assert_eq!(download_count(&state), 0);
        assert_eq!(subscription.queued_episodes, 0);
        *response.lock().unwrap() =
            cms("第1集$https://example.org/1.mp4#第2集$https://example.org/2.mp4");
        assert_eq!(check_user(&state, Some(1)).await.unwrap().queued, 2);
        assert_eq!(download_count(&state), 2);
        server.abort();
    }

    #[tokio::test]
    async fn duplicate_episode_labels_keep_identity_when_watched_count_changes() {
        let (_temp, state) = state();
        let (_response, server, source_id) = fixture(
            &state,
            cms("播放$https://example.org/1.mp4#播放$https://example.org/2.mp4"),
        )
        .await;
        let subscription = subscribe(&state, source_id, false).await;
        for watched in [1, 0] {
            let _ = update(
                State(state.clone()),
                user(1),
                Path(subscription.id),
                Json(UpdateInput {
                    watched_episodes: Some(watched),
                    auto_download: Some(true),
                }),
            )
            .await
            .unwrap();
        }
        let conn = state.conn().unwrap();
        let urls: Vec<String> = conn
            .prepare("SELECT url FROM downloads ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            urls,
            ["https://example.org/2.mp4", "https://example.org/1.mp4"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn automatic_download_skips_share_pages_and_uses_the_direct_group() {
        let (_temp, state) = state();
        let (_response, server, source_id) = fixture(
            &state,
            cms_groups(
                "share,ffm3u8",
                "第1集$https://example.org/share/one#第2集$https://example.org/share/two$$$第1集$https://example.org/1.m3u8#第2集$https://example.org/2.m3u8",
            ),
        )
        .await;
        let subscription = subscribe(&state, source_id, true).await;
        assert_eq!(subscription.total_episodes, 2);
        assert!(subscription.error.is_none());
        let conn = state.conn().unwrap();
        let urls: Vec<String> = conn
            .prepare("SELECT url FROM downloads ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            urls,
            ["https://example.org/1.m3u8", "https://example.org/2.m3u8"]
        );
        server.abort();
    }
}
