use super::{browse, media, signed_in};
use crate::{AppError, Shared, auth::AuthUser};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, put},
};
use futures_util::{StreamExt, stream};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::task::JoinHandle;

pub(super) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS telegram_keyword_subscriptions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            chat_id INTEGER NOT NULL, keyword TEXT NOT NULL COLLATE NOCASE,
            enabled INTEGER NOT NULL DEFAULT 1, last_message_id INTEGER NOT NULL,
            scan_offset INTEGER NOT NULL DEFAULT 0, scan_newest INTEGER NOT NULL DEFAULT 0,
            UNIQUE(user_id,chat_id,keyword)
        );
        CREATE TABLE IF NOT EXISTS telegram_keyword_seen (
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            chat_id INTEGER NOT NULL, message_id INTEGER NOT NULL,
            PRIMARY KEY(user_id,chat_id,message_id)
        ) WITHOUT ROWID;",
    )?;
    Ok(())
}

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/telegram/subscriptions", get(list).post(create))
        .route(
            "/api/telegram/subscriptions/{id}",
            put(toggle).delete(remove),
        )
}

#[derive(Serialize)]
struct Rule {
    id: i64,
    chat_id: i64,
    keyword: String,
    enabled: bool,
    last_message_id: i64,
    #[serde(skip)]
    user_id: i64,
    #[serde(skip)]
    scan_offset: i64,
    #[serde(skip)]
    scan_newest: i64,
}

const COLUMNS: &str = "id,chat_id,keyword,enabled,last_message_id,user_id,scan_offset,scan_newest";

fn rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: row.get(0)?,
        chat_id: row.get(1)?,
        keyword: row.get(2)?,
        enabled: row.get(3)?,
        last_message_id: row.get(4)?,
        user_id: row.get(5)?,
        scan_offset: row.get(6)?,
        scan_newest: row.get(7)?,
    })
}

async fn list(State(state): State<Shared>, user: AuthUser) -> Result<Json<Vec<Rule>>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM telegram_keyword_subscriptions WHERE user_id=? ORDER BY id"
    ))?;
    let rules = stmt.query_map([user.id], rule)?.collect::<Result<_, _>>()?;
    Ok(Json(rules))
}

#[derive(Deserialize)]
struct NewRule {
    chat_id: i64,
    keyword: String,
}

async fn create(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(input): Json<NewRule>,
) -> Result<(StatusCode, Json<Rule>), AppError> {
    let user = signed_in(&*state.conn()?, &headers)?;
    let keyword = input.keyword.trim();
    if keyword.is_empty() || keyword.chars().count() > 120 || keyword.chars().any(char::is_control)
    {
        return Err(AppError::bad_request(
            "Provide a keyword of 1 to 120 characters",
        ));
    }
    // Start after the current chat head. A genuinely empty channel has head zero.
    let page = browse::messages(&state, user.id, input.chat_id, "", 0).await?;
    let conn = state.conn()?;
    signed_in(&conn, &headers)?;
    let inserted = conn.execute(
        "INSERT INTO telegram_keyword_subscriptions (user_id,chat_id,keyword,last_message_id)
         VALUES(?,?,?,?) ON CONFLICT(user_id,chat_id,keyword) DO NOTHING",
        params![user.id, input.chat_id, keyword, page.newest_id.unwrap_or(0)],
    )?;
    if inserted == 0 {
        return Err(AppError(
            StatusCode::CONFLICT,
            "Keyword subscription already exists".into(),
        ));
    }
    let created = conn.query_row(
        &format!("SELECT {COLUMNS} FROM telegram_keyword_subscriptions WHERE id=?"),
        [conn.last_insert_rowid()],
        rule,
    )?;
    Ok((StatusCode::CREATED, Json(created)))
}

#[derive(Deserialize)]
struct Toggle {
    enabled: bool,
}

async fn toggle(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<Toggle>,
) -> Result<StatusCode, AppError> {
    let changed = state.conn()?.execute(
        "UPDATE telegram_keyword_subscriptions SET enabled=? WHERE id=? AND user_id=?",
        params![input.enabled, id, user.id],
    )?;
    changed_rule(changed)
}

async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let changed = state.conn()?.execute(
        "DELETE FROM telegram_keyword_subscriptions WHERE id=? AND user_id=?",
        params![id, user.id],
    )?;
    changed_rule(changed)
}

fn changed_rule(count: usize) -> Result<StatusCode, AppError> {
    if count == 0 {
        Err(AppError::not_found("Subscription not found"))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

fn enabled_rules(state: &Shared) -> Result<Vec<Rule>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM telegram_keyword_subscriptions WHERE enabled=1 AND user_id IN (SELECT id FROM users WHERE disabled=0) ORDER BY id"))?;
    Ok(stmt.query_map([], rule)?.collect::<Result<_, _>>()?)
}

async fn scan_page(state: &Shared, rule: &mut Rule) -> Result<bool, AppError> {
    let page = browse::messages(state, rule.user_id, rule.chat_id, "", rule.scan_offset).await?;
    let newest = rule
        .scan_newest
        .max(page.newest_id.unwrap_or(0))
        .max(rule.last_message_id);
    let next = page
        .next_offset
        .filter(|n| *n > rule.last_message_id && newest > rule.last_message_id);
    let keyword = rule.keyword.to_lowercase();
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    // Another worker, a pause, or a deleted account can invalidate this fetch.
    // Publish the cursor and its jobs together. A restarted worker resumes even
    // across large gaps containing only text or nonmatching media.
    let changed = tx.execute(
        "UPDATE telegram_keyword_subscriptions SET last_message_id=?,scan_offset=?,scan_newest=?
         WHERE id=? AND user_id=? AND enabled=1 AND last_message_id=? AND scan_offset=? AND scan_newest=?
         AND user_id IN (SELECT id FROM users WHERE disabled=0)",
        params![if next.is_none() { newest } else { rule.last_message_id }, next.unwrap_or(0), if next.is_some() {newest} else {0}, rule.id, rule.user_id, rule.last_message_id,rule.scan_offset,rule.scan_newest],
    )?;
    if changed == 0 {
        return Ok(false);
    }
    for message in page.messages.iter().rev() {
        if message.is_video && message.id > rule.last_message_id
            && (message.text.to_lowercase().contains(&keyword) || message.file_name.to_lowercase().contains(&keyword))
            && tx.execute("INSERT INTO telegram_keyword_seen(user_id,chat_id,message_id) VALUES (?,?,?) ON CONFLICT DO NOTHING", params![rule.user_id,rule.chat_id,message.id])? != 0
        {
            media::enqueue_message(&tx, state, rule.user_id, message)?;
        }
    }
    tx.commit()?;
    rule.scan_offset = next.unwrap_or(0);
    rule.scan_newest = newest;
    Ok(next.is_some())
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match enabled_rules(&state) {
                Ok(rules) => stream::iter(rules).for_each_concurrent(4, |mut rule| {
                    let state = &state;
                    async move {
                        // Bound the number of calls per rule per round; the next round
                        // continues at the durable offset instead of replaying this page.
                        for _ in 0..8 {
                            match scan_page(state, &mut rule).await {
                                Ok(true) => (),
                                Ok(false) => break,
                                Err(error) => {
                                    tracing::warn!(subscription_id=rule.id, status=%error.0, "Telegram subscription scan failed");
                                    break;
                                }
                            }
                        }
                    }
                }).await,
                Err(error) => tracing::error!(error=%error.1, "Could not schedule Telegram subscriptions"),
            }
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    })
}
