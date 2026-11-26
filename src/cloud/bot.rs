use super::{accounts, search};
use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    network, now,
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{FromRequest, Request, State},
    routing::get,
};
use reqwest::Client;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};
use tokio::task::JoinHandle;

const TELEGRAM: &str = "https://api.telegram.org";
const UNCERTAIN: &str = "服务中断，操作状态未确认。请先在网盘或下载队列中核对结果。";

pub(crate) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cloud_bot_config (
           id INTEGER PRIMARY KEY CHECK (id=1), enabled INTEGER NOT NULL DEFAULT 0,
           token TEXT NOT NULL DEFAULT '', generation INTEGER NOT NULL DEFAULT 0,
           next_offset INTEGER NOT NULL DEFAULT 0, active_since INTEGER NOT NULL DEFAULT 0,
           last_update_at INTEGER NOT NULL DEFAULT 0
         );
         INSERT OR IGNORE INTO cloud_bot_config(id) VALUES(1);
         CREATE TABLE IF NOT EXISTS cloud_bot_bindings (
           sender_id INTEGER NOT NULL, chat_id INTEGER NOT NULL,
           user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
           PRIMARY KEY(sender_id,chat_id)
         );
         CREATE TABLE IF NOT EXISTS cloud_bot_replies (
           update_id INTEGER PRIMARY KEY, sender_id INTEGER NOT NULL, chat_id INTEGER NOT NULL,
           user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
           message_id INTEGER NOT NULL, outcome TEXT NOT NULL,
           job_id INTEGER, last_sent TEXT NOT NULL DEFAULT ''
         );",
    )?;
    Ok(())
}

#[derive(Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct Binding {
    sender_id: i64,
    chat_id: i64,
    user_id: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    enabled: bool,
    token: Option<String>,
    bindings: Vec<Binding>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Backup {
    enabled: bool,
    token: String,
    bindings: Vec<NamedBinding>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedBinding {
    sender_id: i64,
    chat_id: i64,
    username: String,
}

fn bindings(conn: &Connection) -> Result<Vec<Binding>, AppError> {
    let mut stmt = conn.prepare(
        "SELECT sender_id,chat_id,user_id FROM cloud_bot_bindings ORDER BY sender_id,chat_id",
    )?;
    Ok(stmt
        .query_map([], |row| {
            Ok(Binding {
                sender_id: row.get(0)?,
                chat_id: row.get(1)?,
                user_id: row.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    let (enabled, token) =
        conn.query_row("SELECT enabled,token FROM cloud_bot_config", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let mut stmt = conn.prepare(
        "SELECT sender_id,chat_id,username FROM cloud_bot_bindings b JOIN users u
         ON u.id=b.user_id ORDER BY sender_id,chat_id",
    )?;
    let bindings = stmt
        .query_map([], |r| {
            Ok(NamedBinding {
                sender_id: r.get(0)?,
                chat_id: r.get(1)?,
                username: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(Backup {
        enabled,
        token,
        bindings,
    })
}

pub(crate) fn restore_config(conn: &Connection, backup: Backup) -> Result<(), AppError> {
    let mut bindings = Vec::with_capacity(backup.bindings.len());
    for row in backup.bindings {
        let user_id = conn
            .query_row(
                "SELECT id FROM users WHERE username=?",
                [row.username],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| AppError::bad_request("Bot 绑定的成员不存在"))?;
        bindings.push(Binding {
            sender_id: row.sender_id,
            chat_id: row.chat_id,
            user_id,
        });
    }
    configure(
        conn,
        Settings {
            enabled: backup.enabled,
            token: Some(backup.token),
            bindings,
        },
        true,
    )
}

fn configure(conn: &Connection, mut settings: Settings, restoring: bool) -> Result<(), AppError> {
    let (old_enabled, old_token): (bool, String) =
        conn.query_row("SELECT enabled,token FROM cloud_bot_config", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    let token = settings.token.as_deref().unwrap_or(&old_token);
    if token.is_empty() && settings.enabled || !token.is_empty() && !valid_token(token) {
        return Err(AppError::bad_request("Bot Token 格式错误"));
    }
    if settings.bindings.len() > 1024 {
        return Err(AppError::bad_request("Bot 绑定过多"));
    }
    let mut identities = HashSet::new();
    for row in &settings.bindings {
        if row.sender_id <= 0
            || row.chat_id == 0
            || (row.chat_id > 0 && row.chat_id != row.sender_id)
            || !identities.insert((row.sender_id, row.chat_id))
        {
            return Err(AppError::bad_request("Bot 发送者或聊天绑定无效"));
        }
        if !conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id=?)",
            [row.user_id],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(AppError::bad_request("Bot 绑定的成员不存在"));
        }
    }
    settings.bindings.sort_unstable();
    if old_enabled == settings.enabled && token == old_token && settings.bindings == bindings(conn)?
    {
        return Ok(());
    }
    let same_bot = token.split_once(':').map(|t| t.0) == old_token.split_once(':').map(|t| t.0);
    conn.execute(
        "UPDATE cloud_bot_config SET enabled=?1, token=?2, generation=generation+1,
         active_since=?3, next_offset=CASE WHEN ?4 THEN -1 WHEN ?5 THEN next_offset ELSE 0 END WHERE id=1",
        params![settings.enabled, token, now(), restoring && !same_bot, same_bot],
    )?;
    conn.execute("DELETE FROM cloud_bot_bindings", [])?;
    for b in &settings.bindings {
        conn.execute(
            "INSERT INTO cloud_bot_bindings(sender_id,chat_id,user_id) VALUES (?,?,?)",
            params![b.sender_id, b.chat_id, b.user_id],
        )?;
    }
    conn.execute(
        "DELETE FROM cloud_bot_replies WHERE NOT ?1 OR NOT EXISTS
         (SELECT 1 FROM cloud_bot_bindings b WHERE b.sender_id=cloud_bot_replies.sender_id
          AND b.chat_id=cloud_bot_replies.chat_id AND b.user_id=cloud_bot_replies.user_id)",
        [same_bot],
    )?;
    Ok(())
}

fn valid_token(token: &str) -> bool {
    token.len() <= 256
        && token.split_once(':').is_some_and(|(id, secret)| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && secret.len() >= 16
                && secret
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
}

pub(crate) fn router() -> Router<Shared> {
    Router::new().route("/api/cloud/bot", get(read_settings).put(update_settings))
}

fn public_config(conn: &Connection) -> Result<Json<Value>, AppError> {
    let (enabled, configured): (bool, bool) =
        conn.query_row("SELECT enabled,token<>'' FROM cloud_bot_config", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    Ok(Json(
        json!({"enabled":enabled,"token_configured":configured,"bindings":bindings(conn)?}),
    ))
}

async fn read_settings(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    public_config(&*state.conn()?)
}

async fn update_settings(
    State(state): State<Shared>,
    user: AuthUser,
    request: Request,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let body = Bytes::from_request(request, &state)
        .await
        .map_err(|_| AppError::bad_request("无效的 Bot 设置"))?;
    let settings =
        serde_json::from_slice(&body).map_err(|_| AppError::bad_request("无效的 Bot 设置"))?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    configure(&tx, settings, false)?;
    let result = public_config(&tx)?;
    tx.commit()?;
    Ok(result)
}

struct PollConfig {
    token: String,
    generation: i64,
    offset: i64,
    active_since: i64,
}

fn poll_config(state: &Shared) -> Result<Option<PollConfig>, AppError> {
    let conn = state.conn()?;
    let row = conn
        .query_row(
            "SELECT token,generation,next_offset,active_since,last_update_at FROM cloud_bot_config
        WHERE enabled AND token<>''",
            [],
            |r| {
                Ok((
                    PollConfig {
                        token: r.get(0)?,
                        generation: r.get(1)?,
                        offset: r.get(2)?,
                        active_since: r.get(3)?,
                    },
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((mut cfg, last_update)) = row else {
        return Ok(None);
    };
    // Telegram randomizes the next update ID after an idle week. Its queue retains only 24 hours.
    if cfg.offset > 0 && last_update > 0 && last_update <= now() - 6 * 86400 {
        conn.execute(
            "UPDATE cloud_bot_config SET next_offset=0,last_update_at=0 WHERE generation=?",
            [cfg.generation],
        )?;
        cfg.offset = 0;
    }
    Ok(Some(cfg))
}

struct BotApi {
    client: Client,
    base: String,
}

enum ApiError {
    Retry,
    Rejected,
}

impl BotApi {
    async fn call(
        &self,
        cfg: &PollConfig,
        method: &str,
        payload: Value,
    ) -> Result<Value, ApiError> {
        let url = format!("{}/bot{}/{method}", self.base, cfg.token);
        let mut reply = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
            .await
            .map_err(|_| ApiError::Retry)?;
        let status = reply.status();
        let mut body = Vec::new();
        while let Some(chunk) = reply.chunk().await.map_err(|_| ApiError::Retry)? {
            if chunk.len() > 2 * 1024 * 1024 - body.len() {
                return Err(ApiError::Retry);
            }
            body.extend_from_slice(&chunk);
        }
        let mut value: Value = serde_json::from_slice(&body).map_err(|_| ApiError::Retry)?;
        if !status.is_success() || value["ok"] != true {
            if method == "editMessageText"
                && status.as_u16() == 400
                && value["description"]
                    .as_str()
                    .is_some_and(|s| s.contains("message is not modified"))
            {
                return Ok(Value::Null);
            }
            return Err(if matches!(status.as_u16(), 400 | 403) {
                ApiError::Rejected
            } else {
                ApiError::Retry
            });
        }
        Ok(value["result"].take())
    }

    async fn send(&self, cfg: &PollConfig, chat_id: i64, text: &str) -> Result<i64, ApiError> {
        self.call(
            cfg,
            "sendMessage",
            json!({"chat_id":chat_id,"text":text,"link_preview_options":{"is_disabled":true}}),
        )
        .await?
        .get("message_id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or(ApiError::Retry)
    }
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let base = match &state.cloud_fixture {
            None => TELEGRAM.to_owned(),
            Some(url)
                if url.scheme() == "http"
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && match url.host() {
                        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                        _ => false,
                    } =>
            {
                url.origin().ascii_serialization()
            }
            Some(_) => return,
        };
        let Ok(client) = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(40))
            .build()
        else {
            return;
        };
        let api = BotApi { client, base };
        loop {
            let delay = match poll_config(&state) {
                Ok(Some(cfg)) => match poll(&state, &api, &cfg).await {
                    Ok(()) => Duration::from_millis(200),
                    Err(_) => Duration::from_secs(3),
                },
                _ => Duration::from_secs(2),
            };
            tokio::time::sleep(delay).await;
        }
    })
}

fn unavailable() -> AppError {
    AppError::internal("Bot API 暂不可用")
}

async fn poll(state: &Shared, api: &BotApi, cfg: &PollConfig) -> Result<(), AppError> {
    let pending = flush_replies(state, api, cfg).await?;
    let updates = api.call(cfg, "getUpdates", json!({
        "offset":cfg.offset, "timeout":if cfg.offset < 0 { 0 } else if pending { 3 } else { 25 },
        "limit":20,"allowed_updates":["message"]
    })).await.map_err(|_| unavailable())?;
    let updates = updates.as_array().ok_or_else(unavailable)?;
    if cfg.offset < 0 {
        let offset = updates
            .iter()
            .filter_map(|u| u["update_id"].as_i64())
            .max()
            .unwrap_or(-1)
            .saturating_add(1);
        state.conn()?.execute("UPDATE cloud_bot_config SET next_offset=?,last_update_at=? WHERE generation=? AND next_offset=-1", params![offset,now(),cfg.generation])?;
        return Ok(());
    }
    for update in updates {
        let Some(id) = update["update_id"]
            .as_i64()
            .filter(|v| (0..i64::MAX).contains(v))
        else {
            continue;
        };
        if !can_claim(&*state.conn()?, cfg, id)? {
            continue;
        }
        if let Some(msg) = incoming(state, cfg, &update["message"])? {
            dispatch(state, api, cfg, id, msg).await?;
        } else {
            advance(&*state.conn()?, cfg, id)?;
        }
    }
    flush_replies(state, api, cfg).await?;
    Ok(())
}

fn can_claim(conn: &Connection, cfg: &PollConfig, id: i64) -> Result<bool, AppError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM cloud_bot_config WHERE generation=? AND enabled AND next_offset<=?)",
        params![cfg.generation,id], |r| r.get(0))?)
}

fn advance(conn: &Connection, cfg: &PollConfig, id: i64) -> Result<bool, AppError> {
    Ok(conn.execute("UPDATE cloud_bot_config SET next_offset=?1+1,last_update_at=?3 WHERE generation=?2 AND enabled AND next_offset<=?1",
        params![id,cfg.generation,now()])? == 1)
}

enum Command<'a> {
    Search(&'a str),
    Share {
        download: bool,
        links: Vec<search::SearchResult>,
    },
}

struct Message<'a> {
    sender: i64,
    chat: i64,
    user: AuthUser,
    command: Command<'a>,
    target: Option<&'a str>,
}

fn member(
    conn: &Connection,
    generation: i64,
    sender: i64,
    chat: i64,
) -> Result<Option<AuthUser>, AppError> {
    Ok(conn.query_row("SELECT u.id,u.username,u.role FROM cloud_bot_bindings b JOIN users u ON u.id=b.user_id
        JOIN cloud_bot_config c ON c.id=1 WHERE c.enabled AND c.generation=? AND b.sender_id=? AND b.chat_id=? AND u.disabled=0",
        params![generation,sender,chat], |r| Ok(AuthUser {id:r.get(0)?,username:r.get(1)?,role:r.get(2)?})).optional()?)
}

fn incoming<'a>(
    state: &Shared,
    cfg: &PollConfig,
    msg: &'a Value,
) -> Result<Option<Message<'a>>, AppError> {
    let (Some(sender), Some(chat), Some(text), Some(date)) = (
        msg["from"]["id"].as_i64(),
        msg["chat"]["id"].as_i64(),
        msg["text"].as_str().or_else(|| msg["caption"].as_str()),
        msg["date"].as_i64(),
    ) else {
        return Ok(None);
    };
    let valid_chat = match msg["chat"]["type"].as_str() {
        Some("private") => chat == sender,
        Some("group" | "supergroup") => chat < 0,
        _ => false,
    };
    if !valid_chat
        || sender <= 0
        || msg["from"]["is_bot"] != false
        || !msg["sender_chat"].is_null()
        || date < cfg.active_since
    {
        return Ok(None);
    }
    let Some(user) = member(&*state.conn()?, cfg.generation, sender, chat)? else {
        return Ok(None);
    };
    let text = text.trim();
    let (command, target) = if text.starts_with('/') {
        let (name, arg) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let (name, target) = name
            .split_once('@')
            .map_or((name, None), |(n, t)| (n, Some(t)));
        let command = match name {
            "/search" => Command::Search(arg.trim()),
            "/save" | "/download" => Command::Share {
                download: name == "/download",
                links: search::extract_links(arg),
            },
            _ => return Ok(None),
        };
        (command, target)
    } else {
        let links = search::extract_links(text);
        if links.is_empty() {
            return Ok(None);
        }
        (
            Command::Share {
                download: false,
                links,
            },
            None,
        )
    };
    Ok(Some(Message {
        sender,
        chat,
        user,
        command,
        target,
    }))
}

async fn dispatch(
    state: &Shared,
    api: &BotApi,
    cfg: &PollConfig,
    id: i64,
    mut msg: Message<'_>,
) -> Result<(), AppError> {
    if let Some(target) = msg.target {
        let me = api
            .call(cfg, "getMe", json!({}))
            .await
            .map_err(|_| unavailable())?;
        if !me["username"]
            .as_str()
            .is_some_and(|u| u.eq_ignore_ascii_case(target))
        {
            advance(&*state.conn()?, cfg, id)?;
            return Ok(());
        }
    }
    let reply = match api.send(cfg, msg.chat, "已收到，正在处理…").await {
        Ok(reply) => reply,
        Err(ApiError::Rejected) => {
            advance(&*state.conn()?, cfg, id)?;
            return Ok(());
        }
        Err(ApiError::Retry) => return Err(unavailable()),
    };
    {
        let mut conn = state.conn()?;
        let tx = conn.transaction()?;
        if !advance(&tx, cfg, id)? {
            return Ok(());
        }
        // Claim before transfer or queue mutation. After a crash an ambiguous transfer is reported, never replayed.
        if let Some(user) = member(&tx, cfg.generation, msg.sender, msg.chat)? {
            msg.user = user;
            tx.execute("INSERT INTO cloud_bot_replies(update_id,sender_id,chat_id,user_id,message_id,outcome) VALUES(?,?,?,?,?,?)",
                params![id,msg.sender,msg.chat,msg.user.id,reply,UNCERTAIN])?;
        } else {
            tx.commit()?;
            return Ok(());
        }
        tx.commit()?;
    }
    let outcome = match execute(state, api, cfg, id, &msg).await {
        Ok(Some(text)) => text,
        Ok(None) => return Ok(()),
        Err(_) => "操作失败，请在 PolyView 核对资源、账号授权或解析配置。需要身份验证时，请在服务商界面完成。".into(),
    };
    state.conn()?.execute("UPDATE cloud_bot_replies SET outcome=? WHERE update_id=? AND user_id=? AND message_id=? AND chat_id=?",
        params![outcome,id,msg.user.id,reply,msg.chat])?;
    Ok(())
}

async fn execute(
    state: &Shared,
    api: &BotApi,
    cfg: &PollConfig,
    id: i64,
    msg: &Message<'_>,
) -> Result<Option<String>, AppError> {
    let (download, links) = match &msg.command {
        Command::Share { download, links } => (*download, links),
        Command::Search(query) => return search_reply(state, api, cfg, msg, query).await,
    };
    let [link] = links.as_slice() else {
        return Ok(Some("请每次发送一个支持的网盘分享链接".into()));
    };
    let account = accounts::choices_for(state, &msg.user)?
        .into_iter()
        .find(|a| a.provider == link.provider)
        .ok_or_else(|| AppError::not_found("没有可用的网盘账号"))?;
    if !download {
        let saved = accounts::save_for(
            state,
            &msg.user,
            account.id,
            &link.url,
            link.code.as_deref(),
            None,
        )
        .await?;
        let status = if saved.task_id.is_some() {
            "已提交转存任务"
        } else {
            "已转存"
        };
        return Ok(Some(if msg.chat < 0 {
            status.to_string()
        } else {
            format!("{status}：{}", saved.path)
        }));
    }
    let checked = accounts::check_for(
        state,
        &msg.user,
        account.id,
        &link.url,
        link.code.as_deref(),
    )
    .await?;
    if !checked.valid {
        return Ok(Some("分享链接无效或需要在网盘完成验证".into()));
    }
    // Only a provider-checked share resolved by a configured media parser can enter the queue.
    if crate::catalog::is_direct_media_url(&link.url) {
        return Ok(Some("无法解析该分享链接".into()));
    }
    let mut share_url =
        url::Url::parse(&link.url).map_err(|_| AppError::bad_request("分享链接无效"))?;
    if let Some(code) = &link.code {
        let field = match link.provider.as_str() {
            "115" => "password",
            "tianyi" => "accessCode",
            _ => "pwd",
        };
        share_url.query_pairs_mut().append_pair(field, code);
    }
    let media =
        network::prepare_catalog_download(state, share_url.as_str(), &link.provider, false).await?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    if member(&tx, cfg.generation, msg.sender, msg.chat)?
        .is_none_or(|u| u.id != msg.user.id || u.role != msg.user.role)
    {
        return Ok(None);
    }
    let title = checked
        .title
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("网盘视频");
    let title: String = title.chars().take(200).collect();
    let job =
        network::enqueue_prepared(&tx, state, msg.user.id, &title, &media, &Default::default())?;
    tx.execute(
        "UPDATE cloud_bot_replies SET job_id=? WHERE update_id=?",
        params![job, id],
    )?;
    tx.commit()?;
    Ok(None)
}

async fn search_reply(
    state: &Shared,
    api: &BotApi,
    cfg: &PollConfig,
    msg: &Message<'_>,
    query: &str,
) -> Result<Option<String>, AppError> {
    if query.is_empty() {
        return Ok(Some("用法：/search 片名".into()));
    }
    let hits = search::query(state, msg.user.id, query).await?;
    if hits.is_empty() {
        return Ok(Some("没有找到匹配的网盘资源".into()));
    }
    let mut lines = String::new();
    let mut length = 0;
    for hit in hits.into_iter().take(10) {
        let title: String = hit.title.chars().take(100).collect();
        let mut line = format!("{} · {}\n{}", hit.provider, title, hit.url);
        if let Some(code) = hit.code {
            line.push_str("\n提取码：");
            line.push_str(&code);
        }
        let units = line.encode_utf16().count() + 2;
        if length + units > 4000 {
            break;
        }
        length += units;
        lines.push_str(&line);
        lines.push_str("\n\n");
    }
    if lines.is_empty() {
        return Ok(Some("资源链接过长，请在 PolyView 中查看".into()));
    }
    lines.truncate(lines.len() - 2);
    if msg.chat < 0 {
        // Results may come from the member's private Telegram channels.
        if member(&*state.conn()?, cfg.generation, msg.sender, msg.chat)?
            .is_none_or(|u| u.id != msg.user.id)
        {
            return Ok(None);
        }
        let text = if api.send(cfg, msg.sender, &lines).await.is_ok() {
            "已将搜索结果私发给发送者"
        } else {
            "请先私聊启动本 Bot，再重试搜索"
        };
        return Ok(Some(text.into()));
    }
    Ok(Some(lines))
}

struct Reply {
    update: i64,
    chat: i64,
    message: i64,
    sender: i64,
    user: i64,
    text: String,
    last_sent: String,
    terminal: bool,
}

fn replies(state: &Shared, cfg: &PollConfig) -> Result<Vec<Reply>, AppError> {
    let conn = state.conn()?;
    let mut stmt = conn.prepare("SELECT r.update_id,r.chat_id,r.message_id,r.outcome,r.job_id,r.last_sent,
        d.status,d.bytes_written,d.total_bytes,r.sender_id,r.user_id FROM cloud_bot_replies r
        JOIN cloud_bot_config c ON c.enabled AND c.generation=?
        JOIN users u ON u.id=r.user_id AND u.disabled=0
        JOIN cloud_bot_bindings b ON b.user_id=r.user_id AND b.chat_id=r.chat_id AND b.sender_id=r.sender_id
        LEFT JOIN downloads d ON d.id=r.job_id AND d.user_id=r.user_id ORDER BY r.update_id")?;
    Ok(stmt
        .query_map([cfg.generation], |r| {
            let job: Option<i64> = r.get(4)?;
            let (text, terminal) = if let Some(job) = job {
                match r.get::<_, Option<String>>(6)?.as_deref() {
                    Some("completed") => (format!("下载 #{job} 已完成"), true),
                    Some("failed" | "cancelled" | "canceled") | None => {
                        (format!("下载 #{job} 已停止，请在下载队列查看详情"), true)
                    }
                    Some("running") => {
                        let written: i64 = r.get(7)?;
                        let progress = match r.get::<_, Option<i64>>(8)? {
                            Some(total) if total > 0 => {
                                format!("{}%", (written.saturating_mul(100) / total).clamp(0, 100))
                            }
                            _ => format!("{} MiB", written / 1_048_576),
                        };
                        (format!("下载 #{job} 进行中 · {progress}"), false)
                    }
                    Some("paused") => (format!("下载 #{job} 已暂停"), false),
                    _ => (format!("下载 #{job} 已加入队列"), false),
                }
            } else {
                (r.get(3)?, true)
            };
            Ok(Reply {
                update: r.get(0)?,
                chat: r.get(1)?,
                message: r.get(2)?,
                sender: r.get(9)?,
                user: r.get(10)?,
                last_sent: r.get(5)?,
                text,
                terminal,
            })
        })?
        .collect::<Result<_, _>>()?)
}

async fn flush_replies(state: &Shared, api: &BotApi, cfg: &PollConfig) -> Result<bool, AppError> {
    let mut pending = false;
    for reply in replies(state, cfg)? {
        if reply.text == reply.last_sent {
            pending = true;
            continue;
        }
        if member(&*state.conn()?, cfg.generation, reply.sender, reply.chat)?
            .is_none_or(|u| u.id != reply.user)
        {
            continue;
        }
        let result = api
            .call(
                cfg,
                "editMessageText",
                json!({"chat_id":reply.chat,"message_id":reply.message,"text":reply.text,
            "link_preview_options":{"is_disabled":true}}),
            )
            .await;
        if matches!(result, Err(ApiError::Retry)) {
            pending = true;
            continue;
        }
        let conn = state.conn()?;
        if reply.terminal || matches!(result, Err(ApiError::Rejected)) {
            conn.execute(
                "DELETE FROM cloud_bot_replies WHERE update_id=? AND chat_id=? AND message_id=?",
                params![reply.update, reply.chat, reply.message],
            )?;
        } else {
            pending = true;
            conn.execute("UPDATE cloud_bot_replies SET last_sent=? WHERE update_id=? AND chat_id=? AND message_id=?", params![reply.text,reply.update,reply.chat,reply.message])?;
        }
    }
    Ok(pending)
}
