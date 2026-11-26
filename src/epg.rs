use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    now,
};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
};
use flate2::read::GzDecoder;
use futures_util::{FutureExt, future};
use roxmltree::{Document, ParsingOptions};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use tokio::task::JoinHandle;

const DEFAULT_EPG_URLS: &[&str] = &[
    "http://epg.51zmt.top:8000/e.xml",
    "https://epg.112114.xyz/pp.xml",
    "https://live.fanmingming.com/e.xml",
    "https://raw.githubusercontent.com/litiande03/epg/master/pl.xml.gz",
];
const CHANNEL_ALIASES: &[(&str, &str)] = &[
    ("cctv世界地理", "世界地理"),
    ("cctv兵器科技", "兵器科技"),
    ("cctv卫生健康", "卫生健康"),
    ("cctv女性时尚", "女性时尚"),
    ("cctv风云音乐", "风云音乐"),
    ("cctv9纪录", "CCTV9"),
    ("东莞新闻综合", "东莞新闻"),
    ("广州综合", "广州新闻"),
    ("河北4k", "河北卫视"),
    ("苏州4k", "苏州新闻综合"),
    ("青岛qtv2", "青岛TV2"),
    ("青岛qtv5", "青岛TV5"),
    ("鳳凰香港台", "凤凰香港"),
    ("cgtn西语", "CGTN西班牙语"),
    ("人間衛視", "人间卫视"),
    ("无线新闻", "无线新闻台"),
    ("無綫新聞台", "无线新闻台"),
    ("無線新聞台", "无线新闻台"),
    ("好消息综合台", "好消息"),
    ("好消息綜合台", "好消息"),
    ("cgtn阿语", "CGTN阿拉伯语"),
    ("法国24", "France 24"),
    ("CCTV1综合", "CCTV1"),
    ("中央一套", "CCTV1"),
    ("央视综合", "CCTV1"),
    ("CCTV2财经", "CCTV2"),
    ("中央二套", "CCTV2"),
    ("CCTV3综艺", "CCTV3"),
    ("中央三套", "CCTV3"),
    ("CCTV4中文国际", "CCTV4"),
    ("中央四套", "CCTV4"),
    ("CCTV4欧洲", "CCTV4欧洲版"),
    ("CCTV4美洲", "CCTV4美洲版"),
    ("CCTV4亚洲", "CCTV4亚洲版"),
    ("CCTV5体育", "CCTV5"),
    ("中央五套", "CCTV5"),
    ("CCTV5+体育赛事", "CCTV5+"),
    ("CCTV5plus", "CCTV5+"),
    ("CCTV体育赛事", "CCTV5+"),
    ("CCTV6电影", "CCTV6"),
    ("中央六套", "CCTV6"),
    ("CCTV7国防军事", "CCTV7"),
    ("中央七套", "CCTV7"),
    ("CCTV8电视剧", "CCTV8"),
    ("中央八套", "CCTV8"),
    ("中央九套", "CCTV9"),
    ("CCTV10科教", "CCTV10"),
    ("中央十套", "CCTV10"),
    ("CCTV11戏曲", "CCTV11"),
    ("CCTV12社会与法", "CCTV12"),
    ("CCTV13新闻", "CCTV13"),
    ("央视新闻", "CCTV13"),
    ("CCTV14少儿", "CCTV14"),
    ("央视少儿", "CCTV14"),
    ("CCTV15音乐", "CCTV15"),
    ("CCTV16奥林匹克", "CCTV16"),
    ("CCTV17农业农村", "CCTV17"),
    ("CCTV4K超高清", "CCTV4K"),
    ("CCTV8K超高清", "CCTV8K"),
    ("CCTV第一剧场", "第一剧场"),
    ("CCTV怀旧剧场", "怀旧剧场"),
    ("CCTV风云剧场", "风云剧场"),
    ("CCTV风云足球", "风云足球"),
    ("CCTV高尔夫网球", "高尔夫网球"),
    ("CCTV央视台球", "央视台球"),
    ("CCTV电视指南", "电视指南"),
    ("CCTV文化精品", "央视文化精品"),
    ("CGTN英语", "CGTN"),
    ("CGTN法语", "CGTN法语频道"),
    ("CGTN俄语", "CGTN俄语频道"),
    ("CGTN Documentary", "CGTN纪录"),
    ("北京电视台卫视频道", "北京卫视"),
    ("BTV北京卫视", "北京卫视"),
    ("上海东方卫视", "东方卫视"),
    ("上海卫视", "东方卫视"),
    ("湖南电视台卫视频道", "湖南卫视"),
    ("浙江电视台卫视频道", "浙江卫视"),
    ("江苏电视台卫视频道", "江苏卫视"),
    ("山东电视台卫视频道", "山东卫视"),
    ("天津电视台卫视频道", "天津卫视"),
    ("重庆电视台卫视频道", "重庆卫视"),
    ("安徽电视台卫视频道", "安徽卫视"),
    ("河南电视台卫视频道", "河南卫视"),
    ("湖北电视台卫视频道", "湖北卫视"),
    ("江西电视台卫视频道", "江西卫视"),
    ("四川电视台卫视频道", "四川卫视"),
    ("广东电视台卫视频道", "广东卫视"),
    ("广东南方卫视", "大湾区卫视"),
    ("南方卫视", "大湾区卫视"),
    ("深圳电视台卫视频道", "深圳卫视"),
    ("福建东南卫视", "东南卫视"),
    ("厦门电视台卫视频道", "厦门卫视"),
    ("黑龙江电视台卫视频道", "黑龙江卫视"),
    ("辽宁电视台卫视频道", "辽宁卫视"),
    ("吉林电视台卫视频道", "吉林卫视"),
    ("贵州电视台卫视频道", "贵州卫视"),
    ("云南电视台卫视频道", "云南卫视"),
    ("广西电视台卫视频道", "广西卫视"),
    ("旅游卫视", "海南卫视"),
    ("湖南金鹰卡通", "金鹰卡通"),
    ("上海炫动卡通", "哈哈炫动"),
    ("北京卡酷少儿", "卡酷少儿"),
    ("江苏优漫卡通", "优漫卡通"),
    ("广东嘉佳卡通", "嘉佳卡通"),
    ("广东体育频道", "广东体育"),
    ("上海五星体育", "五星体育"),
    ("北京体育休闲", "北京广播电视台体育休闲频道"),
    ("鳳凰衛視中文台", "凤凰中文"),
    ("凤凰中文台", "凤凰中文"),
    ("凤凰卫视资讯台", "凤凰资讯"),
    ("凤凰资讯台", "凤凰资讯"),
    ("TVB翡翠台", "翡翠台"),
    ("無綫翡翠台", "翡翠台"),
    ("TVB明珠台", "明珠台"),
    ("TVB Pearl", "明珠台"),
    ("TVB Jade", "翡翠台"),
    ("TVB News Channel", "无线新闻台"),
    ("ViuTV 99", "ViuTV"),
    ("ViuTVsix 96", "ViuTVsix"),
    ("澳視澳門", "澳视澳门台"),
    ("澳門綜藝", "澳视综艺台"),
    ("台灣電視台", "台视主频"),
    ("台視", "台视主频"),
    ("中視主頻", "中视"),
    ("華視主頻", "华视"),
    ("民視無線台", "民视"),
    ("公共電視台", "公视"),
    ("東森新聞台", "东森新闻"),
    ("東森幼幼台", "东森幼幼"),
    ("緯來體育台", "纬来体育"),
    ("緯來日本台", "纬来日本"),
];

// Index both spellings so an XMLTV feed may use either side of an alias.
static CHANNEL_NAMES: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    let mut names = HashMap::new();
    for &(alias, target) in CHANNEL_ALIASES {
        let key = normalize(target);
        names.insert(normalize(alias), key.clone());
        names.entry(key.clone()).or_insert(key);
    }
    names
});
const EPG_STALE_SECONDS: i64 = 6 * 60 * 60;
const CATCHUP_SECONDS: i64 = 7 * 24 * 3600;
pub(crate) const ALIASES_KEY: &str = "epg_aliases";

type FeedRefresh = future::BoxFuture<'static, Result<Value, AppError>>;

#[derive(Default)]
pub struct RefreshRuntime {
    active: Mutex<Option<future::WeakShared<FeedRefresh>>>,
}

impl RefreshRuntime {
    fn start_or_join(&self, state: &Shared) -> Result<future::Shared<FeedRefresh>, AppError> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| AppError::internal("EPG refresh lock failed"))?;
        if let Some(task) = active.as_ref().and_then(future::WeakShared::upgrade)
            && task.peek().is_none()
        {
            return Ok(task);
        }
        let state = state.clone();
        let task = async move { refresh_epg(&state).await }.boxed().shared();
        // The callers own the refresh. Cancelling the last one releases the slot.
        *active = task.downgrade();
        Ok(task)
    }
}

fn last_update(conn: &Connection) -> Result<i64, AppError> {
    Ok(setting_optional(conn, "epg_updated_at")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0))
}

fn stale(updated_at: i64, timestamp: i64) -> bool {
    updated_at <= 0 || timestamp.saturating_sub(updated_at) >= EPG_STALE_SECONDS
}

fn refresh_due(conn: &Connection, timestamp: i64) -> Result<bool, AppError> {
    let has_channels: bool =
        conn.query_row("SELECT EXISTS(SELECT 1 FROM channels)", [], |row| {
            row.get(0)
        })?;
    Ok(has_channels && stale(last_update(conn)?, timestamp))
}

async fn scheduled_refresh(state: &Shared) -> Result<(), AppError> {
    let task = {
        let conn = state.conn()?;
        if !refresh_due(&conn, now())? {
            return Ok(());
        }
        // Reserve the refresh before a concurrent request can commit a newer timestamp.
        state.epg_refresh.start_or_join(state)?
    };
    task.await?;
    Ok(())
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            if let Err(error) = scheduled_refresh(&state).await {
                tracing::warn!(status = error.0.as_u16(), "Scheduled EPG refresh failed");
            }
            tokio::time::sleep(Duration::from_secs(EPG_STALE_SECONDS as u64)).await;
        }
    })
}

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY, value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS epg_programs (
            channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
            start INTEGER NOT NULL,
            stop INTEGER NOT NULL,
            title TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            PRIMARY KEY(channel_id,start,title)
        );
        CREATE INDEX IF NOT EXISTS epg_programs_channel_time
            ON epg_programs(channel_id,start);",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO settings(key,value) VALUES ('epg_url',?1)",
        [DEFAULT_EPG_URLS.join("\n")],
    )?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/epg", get(guide))
        .route("/api/epg/refresh", post(refresh))
        .route("/api/epg/config", get(config).put(save_config))
        .route("/api/epg/aliases", get(read_aliases).put(save_aliases))
}

#[derive(Debug, Deserialize)]
struct GuideQuery {
    start: Option<i64>,
    end: Option<i64>,
}

#[derive(Debug, Serialize)]
struct Program {
    channel_id: i64,
    start: i64,
    stop: i64,
    title: String,
    description: String,
}

async fn guide(
    State(state): State<Shared>,
    user: AuthUser,
    Query(query): Query<GuideQuery>,
) -> Result<Json<Value>, AppError> {
    let start = query.start.unwrap_or_else(|| now() - 3600);
    let end = query.end.unwrap_or_else(|| now() + 7 * 3600);
    if end <= start || end.saturating_sub(start) > 48 * 3600 {
        return Err(AppError::bad_request(
            "EPG range must be positive and no longer than 48 hours",
        ));
    }
    let conn = state.conn()?;
    let mut statement = conn.prepare(
        "SELECT channel_id,start,stop,title,description FROM epg_programs
         WHERE stop>?1 AND start<?2 ORDER BY channel_id,start",
    )?;
    let programs = statement
        .query_map(params![start, end], |row| {
            Ok(Program {
                channel_id: row.get(0)?,
                start: row.get(1)?,
                stop: row.get(2)?,
                title: row.get(3)?,
                description: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let matched: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT channel_id) FROM epg_programs WHERE stop>?1 AND start<?2",
        params![start, end],
        |row| row.get(0),
    )?;
    let updated_at = last_update(&conn)?;
    let mut result = json!({
        "updated_at": updated_at,
        "stale": stale(updated_at, now()),
        "matched_channels": matched,
        "programs": programs,
    });
    if user.role == "admin" {
        result["url"] = Value::String(setting(&conn, "epg_url")?);
    }
    Ok(Json(result))
}

async fn config(State(state): State<Shared>, user: AuthUser) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let programs: i64 =
        conn.query_row("SELECT COUNT(*) FROM epg_programs", [], |row| row.get(0))?;
    Ok(Json(json!({
        "url": setting(&conn, "epg_url")?,
        "updated_at": last_update(&conn)?,
        "programs": programs,
    })))
}

#[derive(Debug, Deserialize)]
struct ConfigInput {
    url: String,
}

async fn save_config(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<ConfigInput>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let value = store_config(&conn, &input.url)?;
    Ok(Json(json!({"url": value})))
}

pub(crate) fn store_config(conn: &Connection, url: &str) -> Result<String, AppError> {
    let urls: Vec<&str> = url
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if urls.is_empty() || urls.len() > 4 {
        return Err(AppError::bad_request("Add between one and four XMLTV URLs"));
    }
    for url in &urls {
        let parsed = url::Url::parse(url)
            .map_err(|_| AppError::bad_request("Enter a valid XMLTV HTTP(S) URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || url.chars().any(char::is_control)
        {
            return Err(AppError::bad_request("Enter a valid XMLTV HTTP(S) URL"));
        }
    }
    let value = urls.join("\n");
    conn.execute(
        "INSERT INTO settings(key,value) VALUES ('epg_url',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [&value],
    )?;
    Ok(value)
}

#[derive(Deserialize, Serialize)]
pub(crate) struct ChannelAlias {
    pub from: String,
    pub to: String,
}

async fn read_aliases(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<ChannelAlias>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(crate::settings::get_setting_json(&conn, ALIASES_KEY)?))
}

async fn save_aliases(
    State(state): State<Shared>,
    user: AuthUser,
    Json(aliases): Json<Vec<ChannelAlias>>,
) -> Result<axum::http::StatusCode, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    store_aliases(&conn, aliases)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub(crate) fn store_aliases(
    conn: &Connection,
    mut aliases: Vec<ChannelAlias>,
) -> Result<(), AppError> {
    let mut seen = HashSet::new();
    for alias in &mut aliases {
        for name in [&mut alias.from, &mut alias.to] {
            *name = name.trim().to_owned();
            if name.len() > 300 || name.chars().any(char::is_control) || normalize(name).is_empty()
            {
                return Err(AppError::bad_request(
                    "Enter a valid channel name for each alias",
                ));
            }
        }
        if !seen.insert(normalize(&alias.from)) {
            return Err(AppError::bad_request("Duplicate channel alias"));
        }
    }
    crate::settings::set_setting_json(conn, ALIASES_KEY, &aliases)
}

async fn refresh(State(state): State<Shared>, user: AuthUser) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    Ok(Json(state.epg_refresh.start_or_join(&state)?.await?))
}

async fn refresh_epg(state: &Shared) -> Result<Value, AppError> {
    let urls: Vec<String> = {
        let conn = state.conn()?;
        setting(&conn, "epg_url")?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    };
    let mut feeds = Vec::new();
    let mut errors = Vec::new();
    for (index, url) in urls.iter().enumerate() {
        match fetch_epg(&state.client, url).await {
            Ok(parsed) => feeds.push(parsed),
            Err(error) => errors.push(format!("EPG source {}: {error}", index + 1)),
        }
    }
    if feeds.is_empty() {
        return Err(AppError(
            axum::http::StatusCode::BAD_GATEWAY,
            if errors.is_empty() {
                "No EPG sources configured".to_string()
            } else {
                errors.join("; ")
            },
        ));
    }
    let conn = state.conn()?;
    let updated_at = now();
    let (matched, programs) = store_feeds(&conn, &feeds, updated_at, errors.is_empty())?;
    if !errors.is_empty() {
        tracing::warn!(failed = errors.len(), "Some EPG sources failed to refresh");
    }
    Ok(json!({
        "updated_at": updated_at,
        "feeds": feeds.len(),
        "matched_channels": matched,
        "programs": programs,
        "errors": errors,
    }))
}

fn store_feeds(
    conn: &Connection,
    feeds: &[ParsedEpg],
    updated_at: i64,
    all_sources_succeeded: bool,
) -> Result<(usize, usize), AppError> {
    let custom = crate::settings::get_setting_json::<Vec<ChannelAlias>>(conn, ALIASES_KEY)?
        .into_iter()
        .map(|alias| (normalize(&alias.from), normalize(&alias.to)))
        .collect::<HashMap<_, _>>();
    let channels = {
        let mut statement = conn.prepare("SELECT id,name,tvg_id FROM channels ORDER BY id")?;
        statement
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let tvg_id: String = row.get(2)?;
                let prefer_id = !tvg_id.is_empty() && !custom.contains_key(&normalize(&name));
                Ok((
                    row.get::<_, i64>(0)?,
                    channel_key(&name, &custom),
                    tvg_id,
                    prefer_id,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut matched = HashSet::new();
    let mut programs = 0usize;
    let mut covered: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    let tx = conn.unchecked_transaction()?;
    let cutoff = updated_at - CATCHUP_SECONDS;
    tx.execute(
        "DELETE FROM epg_programs WHERE start < ?1 OR (?3 AND stop > ?2)",
        params![cutoff, updated_at, all_sources_succeeded],
    )?;
    for parsed in feeds {
        for (channel_id, key, tvg_id, prefer_id) in &channels {
            let items = if *prefer_id {
                parsed.programs.get(tvg_id)
            } else {
                None
            };
            let Some(items) = items.or_else(|| {
                find_channel(key, &parsed.aliases, &parsed.canonical)
                    .and_then(|xml_id| parsed.programs.get(xml_id))
            }) else {
                continue;
            };
            matched.insert(*channel_id);
            let ranges = covered.entry(*channel_id).or_default();
            for item in items {
                if item.start < cutoff {
                    continue;
                }
                if ranges
                    .iter()
                    .any(|(start, stop)| item.start < *stop && item.stop > *start)
                {
                    continue;
                }
                // On partial refreshes, preserve other feeds' cached slots until replaced.
                if !all_sources_succeeded || item.start < updated_at {
                    tx.execute(
                        "DELETE FROM epg_programs WHERE channel_id=?1 AND start<?2 AND stop>?3",
                        params![channel_id, item.stop, item.start],
                    )?;
                }
                programs += tx.execute(
                    "INSERT OR IGNORE INTO epg_programs(channel_id,start,stop,title,description)
                     VALUES (?1,?2,?3,?4,?5)",
                    params![
                        channel_id,
                        item.start,
                        item.stop,
                        item.title,
                        item.description
                    ],
                )?;
                ranges.push((item.start, item.stop));
            }
        }
    }
    if programs == 0 {
        return Err(AppError(
            axum::http::StatusCode::BAD_GATEWAY,
            "EPG sources contained no usable listings for the configured channels".into(),
        ));
    }
    tx.execute(
        "INSERT INTO settings(key,value) VALUES ('epg_updated_at',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [updated_at.to_string()],
    )?;
    tx.commit()?;
    Ok((matched.len(), programs))
}

async fn fetch_epg(client: &reqwest::Client, url: &str) -> Result<ParsedEpg, String> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|_| "request failed")?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    let bytes = response.bytes().await.map_err(|_| "body read failed")?;
    let body = decode_epg_body(&bytes).map_err(|_| "invalid encoding")?;
    let parsed = parse_xmltv(&body).map_err(|_| "invalid XMLTV")?;
    if parsed.programs.is_empty() {
        return Err("no programme listings".into());
    }
    Ok(parsed)
}

fn decode_epg_body(bytes: &[u8]) -> Result<String, AppError> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoder = GzDecoder::new(bytes);
        let mut decoded = String::new();
        decoder.read_to_string(&mut decoded).map_err(|error| {
            AppError(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("Invalid gzipped EPG: {error}"),
            )
        })?;
        Ok(decoded)
    } else {
        String::from_utf8(bytes.to_vec()).map_err(|error| {
            AppError(
                axum::http::StatusCode::BAD_GATEWAY,
                format!("Invalid EPG encoding: {error}"),
            )
        })
    }
}

fn setting(conn: &Connection, key: &str) -> Result<String, AppError> {
    setting_optional(conn, key)?.ok_or_else(|| AppError::internal(format!("Missing setting {key}")))
}

fn setting_optional(conn: &Connection, key: &str) -> Result<Option<String>, AppError> {
    Ok(conn
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

struct RawProgram {
    start: i64,
    stop: i64,
    title: String,
    description: String,
}

struct ParsedEpg {
    aliases: HashMap<String, String>,
    canonical: HashMap<String, String>,
    programs: HashMap<String, Vec<RawProgram>>,
}

fn parse_xmltv(body: &str) -> Result<ParsedEpg, AppError> {
    let document = Document::parse_with_options(
        body,
        ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .map_err(|error| AppError::bad_request(format!("Invalid XMLTV: {error}")))?;
    let mut aliases = HashMap::new();
    let mut canonical_ids = HashMap::new();
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.has_tag_name("channel"))
    {
        let Some(id) = node.attribute("id") else {
            continue;
        };
        for value in std::iter::once(id)
            .chain(
                node.children()
                    .filter(|child| child.has_tag_name("display-name"))
                    .filter_map(|child| child.text()),
            )
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            aliases
                .entry(normalize(value))
                .or_insert_with(|| id.to_string());
            canonical_ids
                .entry(canonical(value))
                .or_insert_with(|| id.to_string());
        }
    }
    let mut programs: HashMap<String, Vec<RawProgram>> = HashMap::new();
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.has_tag_name("programme"))
    {
        let Some(channel) = node.attribute("channel") else {
            continue;
        };
        let (Some(start), Some(stop)) = (
            node.attribute("start").and_then(parse_xmltv_time),
            node.attribute("stop").and_then(parse_xmltv_time),
        ) else {
            continue;
        };
        if stop <= start {
            continue;
        }
        let title = node
            .children()
            .find(|child| child.has_tag_name("title"))
            .and_then(|child| child.text())
            .unwrap_or("未命名节目")
            .trim()
            .to_string();
        let description = node
            .children()
            .find(|child| child.has_tag_name("desc"))
            .and_then(|child| child.text())
            .unwrap_or("")
            .trim()
            .to_string();
        programs
            .entry(channel.to_string())
            .or_default()
            .push(RawProgram {
                start,
                stop,
                title,
                description,
            });
    }
    Ok(ParsedEpg {
        aliases,
        canonical: canonical_ids,
        programs,
    })
}

fn channel_key(name: &str, custom: &HashMap<String, String>) -> String {
    let key = normalize(name);
    custom.get(&key).cloned().unwrap_or(key)
}

fn find_channel<'a>(
    key: &str,
    aliases: &'a HashMap<String, String>,
    canonical_aliases: &'a HashMap<String, String>,
) -> Option<&'a String> {
    aliases
        .get(key)
        .or_else(|| canonical_aliases.get(&canonical(key)))
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .map(fold_channel_char)
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric() || *c == '+')
        .collect()
}

// Playlist and XMLTV names commonly mix simplified, traditional and full-width text.
fn fold_channel_char(c: char) -> char {
    match c {
        '！'..='～' => char::from_u32(c as u32 - 0xfee0).unwrap(),
        '臺' => '台',
        '衛' => '卫',
        '視' => '视',
        '綜' => '综',
        '藝' => '艺',
        '體' => '体',
        '賽' => '赛',
        '電' => '电',
        '劇' => '剧',
        '頻' => '频',
        '國' => '国',
        '際' => '际',
        '聞' => '闻',
        '軍' => '军',
        '紀' => '纪',
        '錄' => '录',
        '財' => '财',
        '經' => '经',
        '樂' => '乐',
        '兒' => '儿',
        '戲' => '戏',
        '會' => '会',
        '與' => '与',
        '奧' => '奥',
        '農' => '农',
        '業' => '业',
        '歐' => '欧',
        '亞' => '亚',
        '線' | '綫' => '线',
        '東' => '东',
        '廣' => '广',
        '滬' => '沪',
        '蘇' => '苏',
        '龍' => '龙',
        '遼' => '辽',
        '寧' => '宁',
        '雲' => '云',
        '貴' => '贵',
        '慶' => '庆',
        '門' => '门',
        '廈' => '厦',
        '島' => '岛',
        '灣' => '湾',
        '區' => '区',
        '藍' => '蓝',
        '標' => '标',
        '無' => '无',
        '鳳' => '凤',
        '資' => '资',
        '訊' => '讯',
        '來' => '来',
        '緯' => '纬',
        '華' => '华',
        '間' => '间',
        '優' => '优',
        '鷹' => '鹰',
        '動' => '动',
        '風' => '风',
        '語' => '语',
        '場' => '场',
        '懷' => '怀',
        '舊' => '旧',
        '爾' => '尔',
        '網' => '网',
        '時' => '时',
        _ => c,
    }
}

pub(crate) fn public_channel_key(key: &str) -> bool {
    CHANNEL_NAMES.contains_key(key)
}

pub(crate) fn canonical(value: &str) -> String {
    let mut name = normalize(value);
    // A trailing number, '+', or region is part of a station's identity. Only
    // remove recognized presentation suffixes, after trying the full name.
    loop {
        if let Some(target) = CHANNEL_NAMES.get(&name) {
            return target.clone();
        }
        let Some(suffix) = [
            "超高清",
            "高清",
            "超清",
            "标清",
            "蓝光",
            "1080p",
            "1080i",
            "720p",
            "uhd",
            "fhd",
            "hd",
            "sd",
            "8k",
            "4k",
            "频道",
            "台",
        ]
        .into_iter()
        .find(|&suffix| name.len() > suffix.len() && name.ends_with(suffix)) else {
            return name;
        };
        name.truncate(name.len() - suffix.len());
    }
}

fn parse_xmltv_time(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let bytes = raw.as_bytes();
    if bytes.len() < 14 || !bytes[..14].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number =
        |start: usize, end: usize| -> Option<i64> { raw.get(start..end)?.parse::<i64>().ok() };
    let year = number(0, 4)?;
    let month = number(4, 6)?;
    let day = number(6, 8)?;
    let hour = number(8, 10)?;
    let minute = number(10, 12)?;
    let second = number(12, 14)?;
    let mut offset = 0;
    let rest = raw[14..].trim();
    if rest == "Z" {
        offset = 0;
    } else if !rest.is_empty() {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let zone = rest.get(1..)?;
        if zone.len() < 4 {
            return None;
        }
        let hours = zone.get(0..2)?.parse::<i64>().ok()?;
        let minutes = zone.get(2..4)?.parse::<i64>().ok()?;
        offset = sign * (hours * 3600 + minutes * 60);
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use std::{
        io::Write,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn default_key(name: &str) -> String {
        channel_key(name, &HashMap::new())
    }

    fn scheduler_state() -> Shared {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE channels (id INTEGER PRIMARY KEY, name TEXT, tvg_id TEXT)",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        crate::settings::init_schema(&conn).unwrap();
        Arc::new(crate::AppState {
            db: Mutex::new(conn),
            epg_refresh: Default::default(),
            playback_grants: Default::default(),
            telegram_native: Default::default(),
            cloud_fixture: None,
            download_runtime: Default::default(),
            media_dir: Default::default(),
            download_dir: Default::default(),
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            secure_cookie: false,
        })
    }

    fn admin() -> AuthUser {
        AuthUser {
            id: 1,
            username: "test".into(),
            role: "admin".into(),
        }
    }

    fn seed_stale_guide(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO channels VALUES (1,'News','news');
             INSERT INTO settings VALUES ('epg_updated_at','1');
             INSERT INTO epg_programs VALUES (1,unixepoch(),unixepoch()+3600,'Cached','');",
        )
        .unwrap();
    }

    async fn serve_feed(router: Router) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (base, server)
    }

    #[test]
    fn scheduler_requires_channels_and_includes_the_six_hour_boundary() {
        let state = scheduler_state();
        let conn = state.conn().unwrap();
        let timestamp = EPG_STALE_SECONDS * 3;
        assert!(!refresh_due(&conn, timestamp).unwrap());
        conn.execute("INSERT INTO channels VALUES (1,'News','news')", [])
            .unwrap();
        assert!(refresh_due(&conn, timestamp).unwrap());
        conn.execute(
            "INSERT INTO settings VALUES ('epg_updated_at',?1)",
            [timestamp.to_string()],
        )
        .unwrap();
        assert!(!refresh_due(&conn, timestamp + EPG_STALE_SECONDS - 1).unwrap());
        assert!(refresh_due(&conn, timestamp + EPG_STALE_SECONDS).unwrap());
    }

    #[tokio::test]
    async fn concurrent_refreshes_fetch_once_while_guide_reads_cached_rows() {
        let state = scheduler_state();
        let hits = Arc::new(AtomicUsize::new(0));
        let requests = hits.clone();
        let router = Router::new().route(
            "/feed",
            get(move || {
                requests.fetch_add(1, Ordering::SeqCst);
                async { r#"<tv><programme channel="news" start="20990101000000 +0000" stop="20990101010000 +0000"><title>New</title></programme></tv>"# }
            }),
        );
        let (base, server) = serve_feed(router).await;
        {
            let conn = state.conn().unwrap();
            seed_stale_guide(&conn);
            store_config(&conn, &format!("{base}/feed")).unwrap();
        }
        let query = Query(GuideQuery {
            start: None,
            end: None,
        });
        let (worker, manual, guide_button, cached) = tokio::join!(
            biased;
            scheduled_refresh(&state),
            refresh(State(state.clone()), admin()),
            refresh(State(state.clone()), admin()),
            guide(State(state.clone()), admin(), query),
        );
        worker.unwrap();
        let report = manual.unwrap().0;
        assert_eq!(report, guide_button.unwrap().0);
        assert_eq!(report["programs"], 1);
        assert_eq!(cached.unwrap().0["programs"][0]["title"], "Cached");
        scheduled_refresh(&state).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let forced = refresh(State(state.clone()), admin()).await.unwrap();
        assert_eq!(forced.0["matched_channels"], 1);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn failed_refresh_preserves_timestamp_and_data_without_exposing_source_secrets() {
        let state = scheduler_state();
        let (base, server) = serve_feed(Router::new().route(
            "/unmatched",
            get(|| async { r#"<tv><programme channel="missing" start="20990101000000 +0000" stop="20990101010000 +0000"/></tv>"# }),
        ))
        .await;
        seed_stale_guide(&state.conn().unwrap());
        for (path, expected) in [
            ("unavailable", "EPG source 1: HTTP 404"),
            (
                "unmatched",
                "EPG sources contained no usable listings for the configured channels",
            ),
        ] {
            store_config(
                &state.conn().unwrap(),
                &format!("{base}/{path}?token=secret"),
            )
            .unwrap();
            let error = scheduled_refresh(&state).await.unwrap_err();
            assert_eq!(error.1, expected);
            let conn = state.conn().unwrap();
            assert_eq!(last_update(&conn).unwrap(), 1);
            let title: String = conn
                .query_row("SELECT title FROM epg_programs", [], |row| row.get(0))
                .unwrap();
            assert_eq!(title, "Cached");
        }
        server.abort();
    }

    #[tokio::test]
    async fn partial_refresh_keeps_future_listings_from_unavailable_sources() {
        let state = scheduler_state();
        let (base, server) = serve_feed(Router::new().route(
            "/good",
            get(|| async { r#"<tv><programme channel="news" start="20990101000000 +0000" stop="20990101010000 +0000"><title>New</title></programme></tv>"# }),
        ))
        .await;
        {
            let conn = state.conn().unwrap();
            seed_stale_guide(&conn);
            conn.execute_batch(
                "INSERT INTO channels VALUES (2,'Sports','sports');
                 INSERT INTO epg_programs VALUES (2,unixepoch()+7200,unixepoch()+10800,'Sports','');",
            )
            .unwrap();
            store_config(&conn, &format!("{base}/good\n{base}/missing?token=secret")).unwrap();
        }
        let report = refresh(State(state.clone()), admin()).await.unwrap().0;
        assert_eq!(report["errors"], json!(["EPG source 2: HTTP 404"]));
        let conn = state.conn().unwrap();
        let titles = conn
            .prepare("SELECT title FROM epg_programs ORDER BY title")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(titles, ["Cached", "New", "Sports"]);
        server.abort();
    }

    #[test]
    fn refresh_keeps_one_week_of_history_and_replaces_corrected_slots_by_tvg_id() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE channels (id INTEGER PRIMARY KEY, name TEXT, tvg_id TEXT);
             INSERT INTO channels VALUES (1,'Different channel name','feed.17');",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        crate::settings::init_schema(&conn).unwrap();
        let now = parse_xmltv_time("19700110120000 +0000").unwrap();
        let feed = |programs: &str| parse_xmltv(&format!("<tv>{programs}</tv>")).unwrap();
        let original = feed(
            r#"<programme channel="feed.17" start="19700102100000 +0000" stop="19700102110000 +0000"><title>Expired</title></programme>
               <programme channel="feed.17" start="19700103120000 +0000" stop="19700103130000 +0000"><title>Oldest replay</title></programme>
               <programme channel="feed.17" start="19700109080000 +0000" stop="19700109090000 +0000"><title>Wrong title</title></programme>
               <programme channel="feed.17" start="19700111100000 +0000" stop="19700111110000 +0000"><title>Withdrawn future slot</title></programme>"#,
        );
        assert_eq!(
            store_feeds(&conn, &[original], now - 3600, true).unwrap(),
            (1, 3)
        );
        let revised = feed(
            r#"<programme channel="feed.17" start="19700109081500 +0000" stop="19700109090000 +0000"><title>Corrected</title></programme>"#,
        );
        for _ in 0..2 {
            assert_eq!(
                store_feeds(&conn, std::slice::from_ref(&revised), now, true).unwrap(),
                (1, 1)
            );
            let titles = conn
                .prepare("SELECT title FROM epg_programs ORDER BY start")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(titles, ["Oldest replay", "Corrected"]);
        }
    }

    #[test]
    fn xmltv_times_convert_to_utc() {
        assert_eq!(parse_xmltv_time("19700101000000 +0000"), Some(0));
        assert_eq!(parse_xmltv_time("19700101080000 +0800"), Some(0));
        assert_eq!(parse_xmltv_time("19700101000000 Z"), Some(0));
        assert_eq!(parse_xmltv_time("bad"), None);
    }
    #[test]
    fn gzipped_epg_bodies_are_decoded() {
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"<tv></tv>").unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(decode_epg_body(&compressed).unwrap(), "<tv></tv>");
        assert_eq!(decode_epg_body(b"<tv></tv>").unwrap(), "<tv></tv>");
    }

    #[test]
    fn guide_channel_names_match_cctv_and_high_definition_aliases() {
        let aliases = HashMap::from([
            (normalize("CCTV1"), "1".to_string()),
            (normalize("东方卫视"), "2".to_string()),
        ]);
        let canonical = HashMap::from([
            (canonical("CCTV1"), "1".to_string()),
            (canonical("东方卫视"), "2".to_string()),
        ]);
        assert_eq!(
            find_channel(&default_key("CCTV-1 综合"), &aliases, &canonical).map(String::as_str),
            Some("1")
        );
        assert_eq!(
            find_channel(&default_key("东方卫视 4K"), &aliases, &canonical).map(String::as_str),
            Some("2")
        );
    }
    #[test]
    fn public_aliases_match_both_xmltv_spellings_and_preserve_distinct_stations() {
        let unique: HashSet<_> = CHANNEL_ALIASES
            .iter()
            .map(|(alias, _)| normalize(alias))
            .collect();
        assert!(unique.len() >= 80);
        for &(alias, target) in CHANNEL_ALIASES {
            for (feed_name, local_name) in [(alias, target), (target, alias)] {
                let feed = parse_xmltv(&format!(
                    "<tv><channel id='station'><display-name>{feed_name}</display-name></channel></tv>"
                )).unwrap();
                assert_eq!(
                    find_channel(&default_key(local_name), &feed.aliases, &feed.canonical)
                        .map(String::as_str),
                    Some("station"),
                    "{local_name} against {feed_name}"
                );
            }
        }
        assert_eq!(
            canonical("ＣＣＴＶ－５＋體育賽事 高清"),
            canonical("CCTV5+")
        );
        assert_eq!(canonical("東方衛視 超高清"), canonical("上海东方卫视"));
        for (left, right) in [
            ("CCTV-1 4K", "CCTV10高清"),
            ("CCTV5体育", "CCTV5+体育赛事"),
            ("CCTV4K", "CCTV4"),
            ("CCTV8K", "CCTV8"),
            ("CCTV4欧洲版", "CCTV4美洲版"),
            ("鳳凰香港台", "鳳凰衛視中文台"),
            ("青岛QTV2", "青岛QTV5"),
            ("東森新聞台", "東森財經新聞台"),
        ] {
            assert_ne!(canonical(left), canonical(right), "{left} / {right}");
        }
    }

    #[test]
    fn curated_channel_aliases_match_xmltv_names() {
        let body = r#"<?xml version="1.0"?>
<tv>
<channel id="1649"><display-name>世界地理</display-name></channel>
<channel id="青岛tv2"><display-name>青岛TV2</display-name></channel>
<channel id="CGTN西班牙语"><display-name>CGTN西班牙语</display-name></channel>
</tv>"#;
        let parsed = parse_xmltv(body).unwrap();
        assert_eq!(
            find_channel(
                &default_key("CCTV 世界地理"),
                &parsed.aliases,
                &parsed.canonical
            )
            .map(String::as_str),
            Some("1649")
        );
        assert_eq!(
            find_channel(
                &default_key("青岛 QTV-2"),
                &parsed.aliases,
                &parsed.canonical
            )
            .map(String::as_str),
            Some("青岛tv2")
        );
        assert_eq!(
            find_channel(
                &default_key("CGTN 西语"),
                &parsed.aliases,
                &parsed.canonical
            )
            .map(String::as_str),
            Some("CGTN西班牙语")
        );
    }
    #[test]
    fn xmltv_with_a_doctype_is_accepted() {
        let body = r#"<?xml version="1.0"?>
<!DOCTYPE tv SYSTEM "xmltv.dtd">
<tv><channel id="1"><display-name>CCTV1</display-name></channel>
<programme start="20261123120000 +0800" stop="20261123130000 +0800" channel="1">
<title>新闻联播</title></programme></tv>"#;
        let parsed = parse_xmltv(body).unwrap();
        assert_eq!(parsed.programs["1"].len(), 1);
        assert_eq!(parsed.programs["1"][0].title, "新闻联播");
    }
}
