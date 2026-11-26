use super::{
    native::{self, NativeClient},
    tdl_command,
};
use crate::{
    AppError, Shared, auth::AuthUser, download_settings::download_settings, library::is_video,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, put},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{StreamExt, TryStreamExt, stream};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    path::Path as FsPath,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    time::timeout,
};
use url::Url;

const CHANNEL_BASE: i64 = 1_000_000_000_000;
const MESSAGE_UNIT: i64 = 1 << 20;
const MAX_POST: i64 = i32::MAX as i64;
const PAGE_SIZE: i64 = 50;
const MAX_EXPORT: u64 = 8 * 1024 * 1024;

pub(super) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS telegram_favorites (
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        chat_id INTEGER NOT NULL, title TEXT NOT NULL, PRIMARY KEY(user_id,chat_id));",
    )?;
    Ok(())
}

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/telegram/chats", get(list_chats))
        .route("/api/telegram/chats/{chat_id}/messages", get(list_messages))
        .route("/api/telegram/favorites", put(add_favorite))
        .route("/api/telegram/favorites/{chat_id}", delete(remove_favorite))
}

#[derive(Debug, Serialize)]
pub(crate) struct Chat {
    pub id: i64,
    pub title: String,
    pub favorite: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Message {
    pub id: i64,
    pub chat_id: i64,
    pub text: String,
    pub file_name: String,
    pub file_size: i64,
    pub duration: i64,
    pub thumbnail: Option<String>,
    pub media_group_id: Option<String>,
    pub cloud_urls: Vec<String>,
    pub is_video: bool,
    pub url: String,
    pub date: i64,
    #[serde(skip)]
    pub(crate) file_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Page {
    pub messages: Vec<Message>,
    pub next_offset: Option<i64>,
    pub newest_id: Option<i64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Search {
    q: String,
    offset: i64,
}

async fn list_chats(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Chat>>, AppError> {
    Ok(Json(chats(&state, user.id).await?))
}

async fn list_messages(
    State(state): State<Shared>,
    user: AuthUser,
    Path(chat_id): Path<i64>,
    Query(query): Query<Search>,
) -> Result<Json<Page>, AppError> {
    Ok(Json(
        messages(&state, user.id, chat_id, &query.q, query.offset).await?,
    ))
}

fn upstream() -> AppError {
    AppError(StatusCode::BAD_GATEWAY, "无法读取 Telegram 数据".into())
}
fn missing() -> AppError {
    AppError::not_found("没有找到 Telegram 频道或资源")
}
fn num(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}
fn array(value: &Value) -> Result<&[Value], AppError> {
    value.as_array().map(Vec::as_slice).ok_or_else(upstream)
}
fn string(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}
fn valid_post(id: i64) -> bool {
    (1..=MAX_POST).contains(&id)
}
fn chat_peer(id: i64) -> Result<i64, AppError> {
    let id = id
        .checked_neg()
        .filter(|n| *n > 0 && *n < 2 * CHANNEL_BASE && *n != CHANNEL_BASE)
        .ok_or_else(|| AppError::bad_request("无效的 Telegram 群组或频道 ID"))?;
    Ok(if id > CHANNEL_BASE {
        id - CHANNEL_BASE
    } else {
        id
    })
}
fn post_url(chat: i64, post: i64) -> String {
    if chat < -CHANNEL_BASE {
        format!("https://t.me/c/{}/{post}", -CHANNEL_BASE - chat)
    } else {
        String::new()
    }
}
fn native_post(value: &Value) -> Option<i64> {
    let id = num(value)?;
    (id % MESSAGE_UNIT == 0 && valid_post(id / MESSAGE_UNIT)).then_some(id / MESSAGE_UNIT)
}
fn username(input: &str) -> bool {
    (1..=32).contains(&input.len())
        && input.as_bytes()[0].is_ascii_alphabetic()
        && input
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

fn scoped_command(state: &Shared, user_id: i64) -> Result<Command, AppError> {
    let conn = state.conn()?;
    let enabled: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=? AND disabled=0)",
        [user_id],
        |r| r.get(0),
    )?;
    if !enabled {
        return Err(AppError(StatusCode::FORBIDDEN, "账号不可用".into()));
    }
    tdl_command(&conn, &download_settings(&conn)?, user_id)
}
async fn read_json(reader: impl AsyncRead + Unpin) -> Result<Value, AppError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_EXPORT + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| upstream())?;
    if bytes.len() as u64 > MAX_EXPORT {
        return Err(upstream());
    }
    serde_json::from_slice(&bytes).map_err(|_| upstream())
}
fn quiet(command: &mut Command) -> &mut Command {
    command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
}

#[derive(Deserialize)]
struct Dialog {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    visible_name: String,
    #[serde(default)]
    username: String,
}
impl Dialog {
    fn chat(&self) -> Option<Chat> {
        if !(1..CHANNEL_BASE).contains(&self.id) {
            return None;
        }
        // tdl uses a literal dash for basic groups and the actual username for megagroups.
        let id = match self.kind.as_str() {
            "group" if self.username == "-" => -self.id,
            "channel" | "group" => -CHANNEL_BASE - self.id,
            _ => return None,
        };
        Some(Chat {
            id,
            title: self.visible_name.clone(),
            favorite: false,
        })
    }
}
async fn legacy_chats(state: &Shared, user_id: i64) -> Result<Vec<Dialog>, AppError> {
    let mut command = scoped_command(state, user_id)?;
    timeout(Duration::from_secs(45), async {
        let mut child = quiet(&mut command)
            .args(["chat", "ls", "-o", "json"])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|_| upstream())?;
        let result = read_json(child.stdout.take().ok_or_else(upstream)?).await?;
        if !child.wait().await.map_err(|_| upstream())?.success() {
            return Err(upstream());
        }
        serde_json::from_value(result).map_err(|_| upstream())
    })
    .await
    .map_err(|_| upstream())?
}

async fn legacy_export(
    state: &Shared,
    user_id: i64,
    peer: &str,
    mode: &str,
    bounds: &str,
) -> Result<Value, AppError> {
    let file = tempfile::NamedTempFile::new()?;
    let mut command = scoped_command(state, user_id)?;
    let exit = timeout(
        Duration::from_secs(45),
        quiet(&mut command)
            .args([
                "chat",
                "export",
                "-c",
                peer,
                "-T",
                mode,
                "-i",
                bounds,
                "--all",
                "--with-content",
                "--raw",
                "-o",
            ])
            .arg(file.path())
            .stdout(Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| upstream())?
    .map_err(|_| upstream())?;
    if !exit.success() {
        return Err(upstream());
    }
    read_json(tokio::fs::File::from_std(file.reopen()?)).await
}

fn native_chat(v: &Value) -> Option<Chat> {
    if !matches!(
        string(&v["type"]["@type"]),
        "chatTypeSupergroup" | "chatTypeBasicGroup"
    ) {
        return None;
    }
    let id = num(&v["id"])?;
    chat_peer(id).ok()?;
    Some(Chat {
        id,
        title: v["title"].as_str()?.to_owned(),
        favorite: false,
    })
}

pub(crate) async fn chats(state: &Shared, user_id: i64) -> Result<Vec<Chat>, AppError> {
    let mut list: Vec<Chat> = if let Some(client) = native::authorized(state, user_id).await? {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for list in ["chatListMain", "chatListArchive"] {
            let reply = client
                .request(json!({"@type":"getChats","chat_list":{"@type":list},"limit":10000}))
                .await?;
            for id in array(&reply["chat_ids"])?.iter().filter_map(num) {
                if chat_peer(id).is_ok() && seen.insert(id) {
                    ids.push(id);
                }
            }
        }
        stream::iter(ids)
            .map(|id| {
                let client = &client;
                async move {
                    let row = client
                        .request(json!({"@type":"getChat","chat_id":id}))
                        .await?;
                    Ok::<_, AppError>(native_chat(&row).filter(|c| c.id == id))
                }
            })
            .buffered(8)
            .try_filter_map(|chat| async move { Ok(chat) })
            .try_collect()
            .await?
    } else {
        legacy_chats(state, user_id)
            .await?
            .iter()
            .filter_map(Dialog::chat)
            .collect()
    };
    let conn = state.conn()?;
    let mut stmt = conn
        .prepare("SELECT chat_id,title FROM telegram_favorites WHERE user_id=? ORDER BY chat_id")?;
    let mut favorites = stmt
        .query_map([user_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    for chat in &mut list {
        chat.favorite = favorites.remove(&chat.id).is_some();
    }
    list.extend(favorites.into_iter().map(|(id, title)| Chat {
        id,
        title,
        favorite: true,
    }));
    Ok(list)
}

#[derive(Deserialize)]
struct Favorite {
    channel: String,
}
async fn add_favorite(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<Favorite>,
) -> Result<StatusCode, AppError> {
    let input = input.channel.trim();
    let id = input.parse::<i64>().ok();
    let name = input.trim_start_matches('@');
    if let Some(id) = id {
        chat_peer(id)?;
    } else if !username(name) {
        return Err(AppError::bad_request("请输入频道用户名或频道 ID"));
    }
    let chat = if let Some(client) = native::authorized(&state, user.id).await? {
        let request = match id {
            Some(id) => json!({"@type":"getChat","chat_id":id}),
            None => json!({"@type":"searchPublicChat","username":name}),
        };
        native_chat(&client.request(request).await?)
            .filter(|chat| id.is_none_or(|id| id == chat.id))
            .ok_or_else(missing)?
    } else {
        let joined = legacy_chats(&state, user.id).await?;
        let joined = joined.iter().find_map(|d| {
            d.chat().filter(|c| {
                Some(c.id) == id || (id.is_none() && d.username.eq_ignore_ascii_case(name))
            })
        });
        if let Some(chat) = joined {
            chat
        } else if id.is_none() {
            let export = legacy_export(&state, user.id, name, "last", "1").await?;
            let peer = num(&export["id"])
                .filter(|n| (1..CHANNEL_BASE).contains(n))
                .ok_or_else(missing)?;
            if !array(&export["messages"])?
                .iter()
                .any(|row| num(&row["raw"]["PeerID"]["ChannelID"]) == Some(peer))
            {
                return Err(missing());
            }
            Chat {
                id: -CHANNEL_BASE - peer,
                title: format!("@{name}"),
                favorite: true,
            }
        } else {
            return Err(missing());
        }
    };
    state.conn()?.execute("INSERT INTO telegram_favorites(user_id,chat_id,title) VALUES(?,?,?) ON CONFLICT(user_id,chat_id) DO UPDATE SET title=excluded.title", params![user.id,chat.id,chat.title])?;
    Ok(StatusCode::NO_CONTENT)
}
async fn remove_favorite(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    state.conn()?.execute(
        "DELETE FROM telegram_favorites WHERE user_id=? AND chat_id=?",
        params![user.id, id],
    )?;
    Ok(StatusCode::NO_CONTENT)
}

fn links(text: &str, entities: impl Iterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut result = Vec::new();
    let mut add = |raw: &str| {
        if raw.len() > 4096 || result.len() >= 32 || raw.contains('\\') {
            return;
        }
        let Ok(url) = Url::parse(raw) else {
            return;
        };
        if !matches!(url.scheme(), "https" | "http")
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return;
        }
        let path = url.path();
        let shared = match url.host_str() {
            Some(
                "pan.quark.cn"
                | "drive.uc.cn"
                | "fast.uc.cn"
                | "pan.baidu.com"
                | "yun.baidu.com"
                | "www.alipan.com"
                | "alipan.com"
                | "www.aliyundrive.com"
                | "aliyundrive.com"
                | "pan.xunlei.com"
                | "115.com"
                | "115cdn.com"
                | "anxia.com",
            ) => {
                path.starts_with("/s/")
                    || matches!(url.host_str(), Some("pan.baidu.com" | "yun.baidu.com"))
                        && path == "/share/init"
            }
            Some("cloud.189.cn") => path.starts_with("/t/") || path == "/web/share",
            _ => false,
        };
        if shared && !result.iter().any(|s| s == url.as_str()) {
            result.push(url.to_string());
        }
    };
    for token in text
        .split(|c: char| c.is_whitespace() || "<>\"'[](){}，。：；、！？【】《》）「」".contains(c))
    {
        if let Some(start) = token.find("http") {
            add(token[start..].trim_end_matches(['.', ',', ';', '!', '?']));
        }
    }
    for entity in entities {
        add(entity.as_ref());
    }
    result
}
fn thumbnail(data: &Value) -> Option<String> {
    let data = data.as_str()?;
    (data.starts_with("/9j/")
        && data.len() <= 64 * 1024
        && data
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/+=".contains(&b)))
    .then(|| format!("data:image/jpeg;base64,{data}"))
}

// MTProto photoStrippedSize stores just the height, width and JPEG scan.
// Use the fixed JPEG header specified at https://core.telegram.org/api/files#stripped-thumbnails.
fn legacy_thumbnail(size: &Value) -> Option<String> {
    let data = &size["Bytes"];
    if let Some(jpeg) = thumbnail(data) {
        return Some(jpeg);
    }
    if data.as_str()?.len() > 64 * 1024 {
        return None;
    }
    let packed = STANDARD.decode(data.as_str()?).ok()?;
    let [1, height, width, scan @ ..] = packed.as_slice() else {
        return None;
    };
    if *height == 0 || *width == 0 || scan.is_empty() {
        return None;
    }
    static HEADER: std::sync::LazyLock<Vec<u8>> = std::sync::LazyLock::new(|| {
        STANDARD.decode(concat!(
        "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDACgcHiMeGSgjISMtKygwPGRBPDc3PHtYXUlkkYCZlo+AjIqgtObDoKrarYqMyP/L2u71////",
        "m8H///", "/6/+b9//j/2wBDASstLTw1PHZBQXb4pYyl+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj/",
        "wAARCAAAAAADASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/",
        "8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0R",
        "FRkd", "ISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2",
        "uHi4", "+Tl5ufo6erx8vP09fb3+Pn6/8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/",
        "8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkN",
        "ERUZ", "HSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2",
        "Nna4", "uPk5ebn6Onq8vP09fb3+Pn6/9oADAMBAAIRAxEAPwA="
    )).expect("invalid stripped JPEG header")
    });
    let mut jpeg = Vec::with_capacity(HEADER.len() + scan.len() + 2);
    jpeg.extend_from_slice(&HEADER);
    jpeg[164] = *height;
    jpeg[166] = *width;
    jpeg.extend_from_slice(scan);
    jpeg.extend_from_slice(&[0xff, 0xd9]);
    Some(format!("data:image/jpeg;base64,{}", STANDARD.encode(jpeg)))
}
fn album_id(value: &Value) -> Option<String> {
    num(value).filter(|&id| id != 0).map(|id| id.to_string())
}
fn file_name(value: &Value) -> String {
    string(value)
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(255)
        .collect()
}
fn video_document(name: &str, mime: &Value) -> bool {
    string(mime).starts_with("video/") || is_video(FsPath::new(name))
}
fn native_row(chat: i64, row: &Value) -> Option<Message> {
    if num(&row["chat_id"]) != Some(chat) {
        return None;
    }
    let id = native_post(&row["id"])?;
    let content = &row["content"];
    let (media, file) = match string(&content["@type"]) {
        "messageVideo" => (&content["video"], &content["video"]["video"]),
        "messageDocument" => (&content["document"], &content["document"]["document"]),
        _ => (&Value::Null, &Value::Null),
    };
    let name = file_name(&media["file_name"]);
    let is_video = content["@type"] == "messageVideo" || video_document(&name, &media["mime_type"]);
    let caption = content.get("caption").unwrap_or(&content["text"]);
    let text = string(&caption["text"]);
    let entities = caption["entities"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v["type"]["url"].as_str());
    let cloud_urls = links(text, entities);
    if !is_video && cloud_urls.is_empty() {
        return None;
    }
    Some(Message {
        id,
        chat_id: chat,
        text: text.into(),
        file_name: name,
        file_size: num(&file["size"]).unwrap_or(0).max(0),
        duration: num(&media["duration"]).unwrap_or(0).max(0),
        thumbnail: thumbnail(&media["minithumbnail"]["data"])
            .or_else(|| thumbnail(&content["photo"]["minithumbnail"]["data"])),
        media_group_id: album_id(&row["media_album_id"]),
        cloud_urls,
        is_video,
        url: post_url(chat, id),
        date: num(&row["date"]).unwrap_or(0),
        file_id: num(&file["id"]).filter(|id| *id > 0),
    })
}

fn legacy_post(chat: i64, peer: i64, row: &Value) -> Option<i64> {
    let kind = if chat < -CHANNEL_BASE {
        "ChannelID"
    } else {
        "ChatID"
    };
    if num(&row["raw"]["PeerID"][kind]) != Some(peer) {
        return None;
    }
    num(&row["id"]).filter(|id| valid_post(*id))
}

fn legacy_row(chat: i64, peer: i64, row: &Value) -> Option<Message> {
    let id = legacy_post(chat, peer, row)?;
    let raw = &row["raw"];
    let doc = &raw["Media"]["Document"];
    let attrs = doc["Attributes"].as_array();
    let name = file_name(&row["file"]);
    let duration = attrs
        .into_iter()
        .flatten()
        .find_map(|a| a["Duration"].as_f64())
        .unwrap_or(0.0)
        .max(0.0) as i64;
    let is_video = video_document(&name, &doc["MimeType"]);
    let text = string(&row["text"]);
    let cloud_urls = links(
        text,
        raw["Entities"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| e["URL"].as_str()),
    );
    if !is_video && cloud_urls.is_empty() {
        return None;
    }
    let thumbnail = doc["Thumbs"]
        .as_array()
        .or_else(|| raw["Media"]["Photo"]["Sizes"].as_array())
        .into_iter()
        .flatten()
        .find_map(legacy_thumbnail);
    Some(Message {
        id,
        chat_id: chat,
        text: text.into(),
        file_name: name,
        file_size: num(&doc["Size"]).unwrap_or(0).max(0),
        duration,
        thumbnail,
        media_group_id: album_id(&raw["GroupedID"]),
        cloud_urls,
        is_video,
        url: post_url(chat, id),
        date: num(&row["date"]).unwrap_or(0),
        file_id: None,
    })
}

pub(crate) async fn messages(
    state: &Shared,
    user_id: i64,
    chat: i64,
    q: &str,
    offset: i64,
) -> Result<Page, AppError> {
    let peer = chat_peer(chat)?;
    if !(0..=MAX_POST).contains(&offset) || q.len() > 512 || q.chars().any(char::is_control) {
        return Err(AppError::bad_request("无效的搜索词或消息偏移量"));
    }
    let query = q.trim();
    if let Some(client) = native::authorized(state, user_id).await? {
        let mut request = json!({"@type":"getChatHistory","chat_id":chat,"from_message_id":offset*MESSAGE_UNIT,"limit":PAGE_SIZE,"offset":0,"only_local":false});
        if !query.is_empty() {
            request = json!({"@type":"searchChatMessages","chat_id":chat,"query":query,"from_message_id":offset*MESSAGE_UNIT,"offset":0,"limit":PAGE_SIZE});
        }
        let reply = client.request(request).await?;
        let rows = array(&reply["messages"])?;
        let mut page = Page {
            messages: Vec::new(),
            next_offset: None,
            newest_id: None,
        };
        let mut oldest = None;
        for row in rows {
            if num(&row["chat_id"]) != Some(chat) {
                continue;
            }
            let Some(id) = native_post(&row["id"]).filter(|id| offset == 0 || *id <= offset) else {
                continue;
            };
            oldest = Some(oldest.map_or(id, |n: i64| n.min(id)));
            page.newest_id = Some(page.newest_id.map_or(id, |n| n.max(id)));
            if let Some(msg) = native_row(chat, row) {
                page.messages.push(msg);
            }
        }
        page.next_offset = match reply.get("next_from_message_id") {
            Some(next) => native_post(next),
            None => oldest.filter(|id| *id > 1).map(|id| id - 1),
        }
        .filter(|next| offset == 0 || *next < offset);
        return Ok(page);
    }
    let lower = (offset - PAGE_SIZE + 1).max(1);
    let (mode, bounds) = if offset == 0 {
        ("last", PAGE_SIZE.to_string())
    } else {
        ("id", format!("{lower},{offset}"))
    };
    let export = legacy_export(state, user_id, &peer.to_string(), mode, &bounds).await?;
    let rows = export_rows(&export, peer)?;
    let query = query.to_lowercase();
    let mut oldest = None;
    let mut newest_id = None;
    let mut messages = Vec::new();
    for row in rows {
        let Some(id) =
            legacy_post(chat, peer, row).filter(|id| offset == 0 || (lower..=offset).contains(id))
        else {
            continue;
        };
        oldest = Some(oldest.map_or(id, |n: i64| n.min(id)));
        newest_id = Some(newest_id.map_or(id, |n: i64| n.max(id)));
        if let Some(msg) = legacy_row(chat, peer, row)
            && (query.is_empty()
                || msg.text.to_lowercase().contains(&query)
                || msg.file_name.to_lowercase().contains(&query)
                || msg
                    .cloud_urls
                    .iter()
                    .any(|s| s.to_lowercase().contains(&query)))
        {
            messages.push(msg);
        }
    }
    let next_offset = if offset > 0 {
        (lower > 1).then_some(lower - 1)
    } else if rows.len() >= PAGE_SIZE as usize {
        oldest.filter(|id| *id > 1).map(|id| id - 1)
    } else {
        None
    };
    messages.sort_unstable_by_key(|msg| std::cmp::Reverse(msg.id));
    Ok(Page {
        messages,
        next_offset,
        newest_id,
    })
}
fn export_rows(export: &Value, peer: i64) -> Result<&[Value], AppError> {
    if num(&export["id"]) != Some(peer) {
        return Err(upstream());
    }
    array(&export["messages"])
}

pub(super) async fn native_message(
    client: &NativeClient,
    chat: i64,
    id: i64,
) -> Result<Message, AppError> {
    chat_peer(chat)?;
    if !valid_post(id) {
        return Err(AppError::bad_request("无效的 Telegram 消息 ID"));
    }
    let row = client
        .request(json!({"@type":"getMessage","chat_id":chat,"message_id":id*MESSAGE_UNIT}))
        .await?;
    native_row(chat, &row)
        .filter(|m| m.id == id)
        .ok_or_else(missing)
}
pub(crate) async fn message(
    state: &Shared,
    user_id: i64,
    chat: i64,
    id: i64,
) -> Result<Message, AppError> {
    let peer = chat_peer(chat)?;
    if !valid_post(id) {
        return Err(AppError::bad_request("无效的 Telegram 消息 ID"));
    }
    if let Some(client) = native::authorized(state, user_id).await? {
        return native_message(&client, chat, id).await;
    }
    let export = legacy_export(
        state,
        user_id,
        &peer.to_string(),
        "id",
        &format!("{id},{id}"),
    )
    .await?;
    export_rows(&export, peer)?
        .iter()
        .filter_map(|row| legacy_row(chat, peer, row))
        .find(|m| m.id == id)
        .ok_or_else(missing)
}

pub(crate) async fn album(
    state: &Shared,
    user_id: i64,
    chat: i64,
    selected: &Message,
) -> Result<Vec<Message>, AppError> {
    let peer = chat_peer(chat)?;
    if chat != selected.chat_id || !valid_post(selected.id) {
        return Err(missing());
    }
    let Some(group) = &selected.media_group_id else {
        let current = message(state, user_id, chat, selected.id).await?;
        return Ok(current.is_video.then_some(current).into_iter().collect());
    };
    let low = (selected.id - 10).max(1);
    let high = (selected.id + 10).min(MAX_POST);
    let mut result = Vec::new();
    if let Some(client) = native::authorized(state, user_id).await? {
        let mut cursor = high;
        while cursor >= low {
            let reply = client.request(json!({"@type":"getChatHistory","chat_id":chat,"from_message_id":cursor*MESSAGE_UNIT,"offset":0,"limit":high-low+1,"only_local":false})).await?;
            let rows = array(&reply["messages"])?;
            let mut next = None;
            for row in rows {
                if num(&row["chat_id"]) != Some(chat) {
                    continue;
                }
                let Some(id) = native_post(&row["id"]).filter(|id| *id <= cursor) else {
                    continue;
                };
                next = Some(next.map_or(id - 1, |n: i64| n.min(id - 1)));
                if id >= low
                    && let Some(msg) = native_row(chat, row)
                {
                    result.push(msg);
                }
            }
            let Some(n) = next else {
                break;
            };
            cursor = n;
        }
    } else {
        let export = legacy_export(
            state,
            user_id,
            &peer.to_string(),
            "id",
            &format!("{low},{high}"),
        )
        .await?;
        result.extend(
            export_rows(&export, peer)?
                .iter()
                .filter_map(|row| legacy_row(chat, peer, row)),
        );
    }
    result.retain(|msg| {
        (low..=high).contains(&msg.id) && msg.is_video && msg.media_group_id.as_ref() == Some(group)
    });
    result.sort_unstable_by_key(|msg| msg.id);
    result.dedup_by_key(|msg| msg.id);
    Ok(result)
}
