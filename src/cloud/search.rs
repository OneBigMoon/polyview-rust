use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    network::http_url,
    telegram::browse,
};
use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use futures_util::{StreamExt, stream};
use rusqlite::{Connection, params};
use scraper::{Html, Node, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
    time::Duration,
};
use url::Url;

const MAX_CHANNELS: usize = 100;
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_RESULTS: usize = 256;
const PARALLEL: usize = 6;

pub(crate) fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cloud_search_channels (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('public','private','api')),
            target TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1,
            UNIQUE(kind,target)
        )",
    )?;
    Ok(())
}

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/cloud/search", get(search))
        .route("/api/cloud/channels", get(channels).post(create))
        .route("/api/cloud/channels/{id}", put(update).delete(remove))
        .route("/api/cloud/channels/import", post(import))
        .route("/api/cloud/channels/export", get(export))
}

fn public() -> String {
    "public".into()
}
fn enabled() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
struct Config {
    #[serde(alias = "channelName")]
    name: String,
    #[serde(default = "public")]
    kind: String,
    #[serde(alias = "channelId", alias = "username", alias = "channelUsername")]
    target: String,
    #[serde(default = "enabled")]
    enabled: bool,
}

#[derive(Serialize, Deserialize)]
struct Channel {
    id: i64,
    #[serde(flatten)]
    config: Config,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Backup {
    channels: Vec<Channel>,
}

fn invalid() -> AppError {
    AppError::bad_request("无效的网盘搜索频道配置")
}
fn upstream() -> AppError {
    AppError(StatusCode::BAD_GATEWAY, "无法读取网盘搜索源".into())
}
fn db_error(e: rusqlite::Error) -> AppError {
    if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        AppError(StatusCode::CONFLICT, "搜索频道已存在或编号冲突".into())
    } else {
        e.into()
    }
}

impl Config {
    fn validate(mut self) -> Result<Self, AppError> {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() || self.name.len() > 256 || self.name.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let raw = self.target.trim();
        self.target = match self.kind.as_str() {
            "public" => {
                let name = raw
                    .strip_prefix('@')
                    .or_else(|| raw.strip_prefix("https://t.me/s/"))
                    .or_else(|| raw.strip_prefix("https://t.me/"))
                    .unwrap_or(raw);
                if !(1..=32).contains(&name.len())
                    || !name.as_bytes()[0].is_ascii_alphabetic()
                    || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                {
                    return Err(invalid());
                }
                name.to_ascii_lowercase()
            }
            "private" => raw
                .parse::<i64>()
                .ok()
                .filter(|id| (-1_999_999_999_999..0).contains(id) && *id != -1_000_000_000_000)
                .ok_or_else(invalid)?
                .to_string(),
            "api" => {
                if raw.len() > 4096
                    || raw.contains('\\')
                    || raw.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(invalid());
                }
                let url = http_url(raw)?;
                if url.fragment().is_some() || url.query_pairs().any(|(key, _)| key == "q") {
                    return Err(invalid());
                }
                url.to_string()
            }
            _ => return Err(invalid()),
        };
        Ok(self)
    }
}

fn rows(conn: &Connection, only_enabled: bool) -> Result<Vec<Channel>, AppError> {
    let mut stmt = conn.prepare("SELECT id,name,kind,target,enabled FROM cloud_search_channels WHERE NOT ? OR enabled ORDER BY id")?;
    Ok(stmt
        .query_map([only_enabled], |r| {
            Ok(Channel {
                id: r.get(0)?,
                config: Config {
                    name: r.get(1)?,
                    kind: r.get(2)?,
                    target: r.get(3)?,
                    enabled: r.get(4)?,
                },
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    Ok(Backup {
        channels: rows(conn, false)?,
    })
}

fn check_count(conn: &Connection) -> Result<(), AppError> {
    let count: i64 = conn.query_row("SELECT count(*) FROM cloud_search_channels", [], |r| {
        r.get(0)
    })?;
    if count > MAX_CHANNELS as i64 {
        return Err(AppError::bad_request("搜索频道过多"));
    }
    Ok(())
}

fn upsert(conn: &Connection, id: Option<i64>, c: &Config) -> Result<(), AppError> {
    conn.execute("INSERT INTO cloud_search_channels(id,name,kind,target,enabled)
        VALUES(COALESCE((SELECT id FROM cloud_search_channels WHERE kind=?3 AND target=?4),?1),?2,?3,?4,?5)
        ON CONFLICT(kind,target) DO UPDATE SET name=excluded.name,enabled=excluded.enabled",
        params![id,c.name,c.kind,c.target,c.enabled]).map_err(db_error)?;
    Ok(())
}

pub(crate) fn restore_config(conn: &Connection, backup: Backup) -> Result<(), AppError> {
    if backup.channels.len() > MAX_CHANNELS {
        return Err(invalid());
    }
    let mut ids = HashSet::new();
    let mut targets = HashSet::new();
    for Channel { id, config } in backup.channels {
        let c = config.validate()?;
        if id <= 0 || !ids.insert(id) || !targets.insert((c.kind.clone(), c.target.clone())) {
            return Err(invalid());
        }
        upsert(conn, Some(id), &c)?;
    }
    check_count(conn)
}

async fn channels(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Channel>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(rows(&conn, false)?))
}
async fn export(State(state): State<Shared>, user: AuthUser) -> Result<Json<Backup>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(export_config(&conn)?))
}
async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    Json(c): Json<Config>,
) -> Result<(StatusCode, Json<Channel>), AppError> {
    require_admin(&user)?;
    let c = c.validate()?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO cloud_search_channels(name,kind,target,enabled) VALUES(?,?,?,?)",
        params![c.name, c.kind, c.target, c.enabled],
    )
    .map_err(db_error)?;
    let id = tx.last_insert_rowid();
    check_count(&tx)?;
    tx.commit()?;
    Ok((StatusCode::CREATED, Json(Channel { id, config: c })))
}
async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(c): Json<Config>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    let c = c.validate()?;
    let n = state
        .conn()?
        .execute(
            "UPDATE cloud_search_channels SET name=?,kind=?,target=?,enabled=? WHERE id=?",
            params![c.name, c.kind, c.target, c.enabled, id],
        )
        .map_err(db_error)?;
    changed(n)
}
async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    changed(
        state
            .conn()?
            .execute("DELETE FROM cloud_search_channels WHERE id=?", [id])?,
    )
}
fn changed(n: usize) -> Result<StatusCode, AppError> {
    if n == 0 {
        Err(AppError::not_found("没有找到搜索频道"))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImportRow {
    Config(Config),
    Telegram { id: String, name: String },
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Batch {
    Object { channels: Vec<ImportRow> },
    List(Vec<ImportRow>),
}
async fn import(
    State(state): State<Shared>,
    user: AuthUser,
    Json(batch): Json<Batch>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let (Batch::Object { channels } | Batch::List(channels)) = batch;
    let imported = channels.len();
    if imported > MAX_CHANNELS {
        return Err(invalid());
    }
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    for row in channels {
        let c = match row {
            ImportRow::Config(c) => c,
            ImportRow::Telegram { id, name } => Config {
                name,
                kind: public(),
                target: id,
                enabled: true,
            },
        }
        .validate()?;
        upsert(&tx, None, &c)?;
    }
    check_count(&tx)?;
    tx.commit()?;
    Ok(Json(json!({"imported": imported})))
}

#[derive(Serialize)]
pub(crate) struct SearchResult {
    pub provider: String,
    pub title: String,
    pub url: String,
    pub code: Option<String>,
    pub channel: String,
}

fn share_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn valid_code(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

// Keep only the share identity and its extraction code. Tracking parameters, userinfo,
// arbitrary fragments and other credentials never enter responses, Bot messages or logs.
fn share(raw: &str) -> Option<SearchResult> {
    if raw.len() > 4096
        || raw.contains('\\')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let u = http_url(raw).ok()?;
    if u.port().is_some() {
        return None;
    }
    let (provider, host) = match u.host_str()? {
        "pan.quark.cn" => ("quark", "pan.quark.cn"),
        "115.com" | "115cdn.com" | "anxia.com" => ("115", "115.com"),
        "cloud.189.cn" | "h5.cloud.189.cn" => ("tianyi", "cloud.189.cn"),
        "alipan.com" | "www.alipan.com" | "aliyundrive.com" | "www.aliyundrive.com" => {
            ("ali", "www.alipan.com")
        }
        "pan.baidu.com" | "yun.baidu.com" => ("baidu", "pan.baidu.com"),
        "drive.uc.cn" | "fast.uc.cn" => ("uc", u.host_str()?),
        "pan.xunlei.com" => ("xunlei", "pan.xunlei.com"),
        _ => return None,
    };
    if provider == "quark"
        && (u
            .fragment()
            .is_some_and(|f| !matches!(f.trim_end_matches('/'), "" | "/list/share"))
            || u.query_pairs()
                .any(|(k, v)| matches!(k.as_ref(), "fid" | "pdir_fid") && v != "0"))
    {
        return None;
    }
    let path = u.path().trim_end_matches('/');
    let query_id = |key| {
        u.query_pairs()
            .find(|(k, v)| k == key && share_id(v))
            .map(|(_, v)| v.into_owned())
    };
    let url = match (provider, path) {
        ("tianyi", "/share.html") if u.host_str() == Some("h5.cloud.189.cn") => {
            let id = u.fragment()?.strip_prefix("/t/")?;
            if !share_id(id) {
                return None;
            }
            format!("https://{host}/t/{id}")
        }
        ("tianyi", _) if u.host_str() == Some("h5.cloud.189.cn") => return None,
        ("baidu", "/share/init") => format!("https://{host}/s/1{}", query_id("surl")?),
        ("tianyi", "/web/share") => format!("https://{host}/t/{}", query_id("code")?),
        _ => {
            let prefix = if provider == "tianyi" { "/t/" } else { "/s/" };
            let id = path.strip_prefix(prefix)?;
            let valid_folder = provider == "ali"
                && id
                    .split_once("/folder/")
                    .is_some_and(|(key, folder)| share_id(key) && share_id(folder));
            if !share_id(id) && !valid_folder {
                return None;
            }
            format!("https://{host}{prefix}{id}")
        }
    };
    let code = u
        .query_pairs()
        .find_map(|(k, v)| {
            (matches!(
                k.as_ref(),
                "pwd"
                    | "password"
                    | "passcode"
                    | "accessCode"
                    | "receive_code"
                    | "access_code"
                    | "passCode"
            ) && valid_code(&v))
            .then(|| v.into_owned())
        })
        .or_else(|| {
            u.fragment()
                .filter(|s| provider == "xunlei" && s.len() == 4 && valid_code(s))
                .map(str::to_owned)
        });
    Some(SearchResult {
        provider: provider.into(),
        title: String::new(),
        url,
        code,
        channel: String::new(),
    })
}

fn text_code(text: &str) -> Option<String> {
    let end = text.char_indices().nth(128).map_or(text.len(), |(i, _)| i);
    let text = &text[..end];
    let (pos, label) = [
        "提取码",
        "访问码",
        "密码",
        "提取碼",
        "密碼",
        "passcode",
        "password",
    ]
    .into_iter()
    .filter_map(|s| text.find(s).map(|i| (i, s)))
    .min_by_key(|(i, _)| *i)?;
    let value = text[pos + label.len()..]
        .trim_start_matches(|c: char| c.is_whitespace() || ":：=【[(（".contains(c));
    let code = value
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
        .next()?;
    valid_code(code).then(|| code.to_owned())
}

fn delimiter(c: char) -> bool {
    c.is_whitespace() || "<>\"'[](){}，。：；、！？【】《》（）「」".contains(c)
}

pub(crate) fn extract_links(text: &str) -> Vec<SearchResult> {
    let mut links: Vec<SearchResult> = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("http") {
        rest = &rest[i..];
        let end = rest.find(delimiter).unwrap_or(rest.len());
        let token = rest[..end].trim_end_matches(['.', ',', ';', '!', '?']);
        rest = &rest[end..];
        if let Some(mut hit) = share(token) {
            if hit.code.is_none() {
                hit.code = text_code(rest.split("http").next().unwrap_or(rest));
            }
            if let Some(known) = links.iter_mut().find(|item| item.url == hit.url) {
                if known.code.is_none() {
                    known.code = hit.code;
                }
            } else {
                links.push(hit);
                if links.len() == MAX_RESULTS {
                    break;
                }
            }
        }
    }
    if let [hit] = links.as_mut_slice()
        && hit.code.is_none()
    {
        hit.code = text_code(text.split("http").next().unwrap_or_default());
    }
    links
}

fn post_results(
    text: &str,
    title: &str,
    hidden: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<SearchResult> {
    let mut hits = extract_links(text);
    hits.extend(
        hidden
            .into_iter()
            .filter_map(|s| share(s.as_ref()))
            .take(MAX_RESULTS),
    );
    let common_code = if hits
        .first()
        .is_some_and(|first| hits.iter().all(|h| h.url == first.url))
    {
        text_code(text)
    } else {
        None
    };
    let title = if title.trim().is_empty() {
        text.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .split("http")
            .next()
            .unwrap_or("")
            .trim()
    } else {
        title.trim()
    };
    for hit in &mut hits {
        hit.title = title
            .chars()
            .filter(|c| !c.is_control())
            .take(160)
            .collect();
        if hit.code.is_none() {
            hit.code.clone_from(&common_code);
        }
    }
    hits
}

fn public_results(html: &str) -> Vec<SearchResult> {
    static POSTS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse(".tgme_widget_message_text").expect("static selector"));
    let document = Html::parse_document(html);
    let mut results = Vec::new();
    for post in document.select(&POSTS) {
        let mut text = String::new();
        for node in post.descendants() {
            match node.value() {
                Node::Text(t) => text.push_str(t),
                Node::Element(e) if e.name() == "br" => text.push('\n'),
                Node::Element(e) if e.name() == "a" => {
                    if let Some(href) = e.attr("href") {
                        text.push(' ');
                        text.push_str(href);
                        text.push(' ');
                    }
                }
                _ => (),
            }
        }
        results.extend(
            post_results(&text, "", std::iter::empty::<&str>())
                .into_iter()
                .take(MAX_RESULTS - results.len()),
        );
        if results.len() == MAX_RESULTS {
            break;
        }
    }
    results
}

fn api_results(body: &[u8]) -> Result<Vec<SearchResult>, AppError> {
    let data: Value = serde_json::from_slice(body).map_err(|_| upstream())?;
    let rows = [
        Some(&data),
        data.get("results"),
        data.get("data"),
        data.pointer("/data/results"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_array)
    .ok_or_else(upstream)?;
    let mut results = Vec::new();
    for row in rows {
        let text = row
            .as_str()
            .or_else(|| row["text"].as_str())
            .or_else(|| row["content"].as_str())
            .unwrap_or("");
        let title = row["title"].as_str().unwrap_or("");
        let links = row["url"].as_str().into_iter().chain(
            row["links"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        );
        for mut hit in post_results(text, title, links) {
            if hit.code.is_none() {
                hit.code = row["code"]
                    .as_str()
                    .filter(|s| valid_code(s))
                    .map(str::to_owned);
            }
            results.push(hit);
            if results.len() == MAX_RESULTS {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

async fn fetch(url: Url, q: &str) -> Result<Bytes, AppError> {
    // API URLs can contain an administrator's key. Never attach them as a Referer
    // or let a different origin answer on behalf of the configured source.
    static HTTP: LazyLock<Result<reqwest::Client, reqwest::Error>> = LazyLock::new(|| {
        reqwest::Client::builder()
            .referer(false)
            .redirect(reqwest::redirect::Policy::none())
            .build()
    });
    let client = HTTP.as_ref().map_err(|_| upstream())?;
    let response = client
        .get(url)
        .query(&[("q", q)])
        .send()
        .await
        .map_err(|_| upstream())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err(upstream());
    }
    to_bytes(Body::from_stream(response.bytes_stream()), MAX_BODY)
        .await
        .map_err(|_| upstream())
}

async fn search_source(
    state: &Shared,
    user: i64,
    c: Config,
    q: &str,
) -> Result<Vec<SearchResult>, AppError> {
    let mut results = match c.kind.as_str() {
        "public" => {
            let url = match &state.cloud_fixture {
                Some(base) => base.join(&format!("/s/{}", c.target)),
                None => Url::parse(&format!("https://t.me/s/{}", c.target)),
            }
            .map_err(|_| invalid())?;
            let body = fetch(url, q).await?;
            public_results(std::str::from_utf8(&body).map_err(|_| upstream())?)
        }
        "api" => api_results(&fetch(http_url(&c.target)?, q).await?)?,
        "private" => {
            let chat = c.target.parse().map_err(|_| invalid())?;
            let page = browse::messages(state, user, chat, q, 0).await?;
            page.messages
                .into_iter()
                .flat_map(|m| post_results(&m.text, &m.file_name, m.cloud_urls))
                .take(MAX_RESULTS)
                .collect()
        }
        _ => return Err(invalid()),
    };
    for result in &mut results {
        result.channel.clone_from(&c.name);
        if result.title.is_empty() {
            result.title = q.to_owned();
        }
    }
    Ok(results)
}

pub(crate) async fn query(
    state: &Shared,
    user_id: i64,
    q: &str,
) -> Result<Vec<SearchResult>, AppError> {
    let q = q.trim();
    if q.is_empty() || q.len() > 512 || q.chars().any(char::is_control) {
        return Err(AppError::bad_request("请输入有效的搜索词"));
    }
    let channels = {
        let conn = state.conn()?;
        rows(&conn, true)?
    };
    let count = channels.len();
    let mut searches = stream::iter(channels.into_iter().map(|c| {
        tokio::time::timeout(
            Duration::from_secs(12),
            search_source(state, user_id, c.config, q),
        )
    }))
    .buffered(PARALLEL);
    let mut ok = 0;
    let mut results: Vec<SearchResult> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    while let Some(source) = searches.next().await {
        if let Ok(Ok(hits)) = source {
            ok += 1;
            for hit in hits {
                if let Some(&index) = seen.get(&hit.url) {
                    if results[index].code.is_none() {
                        results[index].code = hit.code;
                    }
                } else if results.len() < MAX_RESULTS {
                    seen.insert(hit.url.clone(), results.len());
                    results.push(hit);
                }
            }
        }
    }
    if count != 0 && ok == 0 {
        return Err(upstream());
    }
    Ok(results)
}

#[derive(Deserialize)]
struct Search {
    q: String,
}
async fn search(
    State(state): State<Shared>,
    user: AuthUser,
    Query(input): Query<Search>,
) -> Result<Json<Vec<SearchResult>>, AppError> {
    Ok(Json(query(&state, user.id, &input.q).await?))
}
