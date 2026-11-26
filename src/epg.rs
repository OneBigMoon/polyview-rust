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
use roxmltree::{Document, ParsingOptions};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
};

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
    ("凤凰香港", "凤凰中文"),
    ("cgtn西语", "CGTN西班牙语"),
    ("人间卫视", "人間衛視"),
    ("无线新闻", "無綫新聞台"),
    ("好消息综合台", "好消息"),
    ("cgtn阿语", "CGTN阿拉伯语"),
    ("法国24", "France 24"),
];
const EPG_STALE_SECONDS: i64 = 6 * 60 * 60;
const CATCHUP_SECONDS: i64 = 7 * 24 * 3600;
pub(crate) const ALIASES_KEY: &str = "epg_aliases";

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
    let updated_at = setting_optional(&conn, "epg_updated_at")?
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut result = json!({
        "updated_at": updated_at,
        "stale": now() - updated_at > EPG_STALE_SECONDS,
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
        "updated_at": setting_optional(&conn, "epg_updated_at")?
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0),
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
    Ok(Json(refresh_epg(&state).await?))
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
    for url in &urls {
        match fetch_epg(&state.client, url).await {
            Ok(parsed) => feeds.push(parsed),
            Err(error) => errors.push(format!("{url}: {}", error.1)),
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
    let (matched, programs) = store_feeds(&conn, &feeds, updated_at)?;
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
        "DELETE FROM epg_programs WHERE start < ?1 OR stop > ?2",
        params![cutoff, updated_at],
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
                // A fresh listing supersedes overlapping archived schedules, including retitles.
                if item.start < updated_at && item.stop > cutoff {
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
    tx.execute(
        "INSERT INTO settings(key,value) VALUES ('epg_updated_at',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [updated_at.to_string()],
    )?;
    tx.commit()?;
    Ok((matched.len(), programs))
}

async fn fetch_epg(client: &reqwest::Client, url: &str) -> Result<ParsedEpg, AppError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| AppError(axum::http::StatusCode::BAD_GATEWAY, error.to_string()))?;
    if !response.status().is_success() {
        return Err(AppError(
            axum::http::StatusCode::BAD_GATEWAY,
            format!("EPG source returned HTTP {}", response.status()),
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| AppError(axum::http::StatusCode::BAD_GATEWAY, error.to_string()))?;
    let body = decode_epg_body(&bytes)?;
    parse_xmltv(&body)
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
    if let Some(target) = custom.get(&key) {
        return target.clone();
    }
    CHANNEL_ALIASES
        .iter()
        .find(|(local, _)| *local == key)
        .map(|(_, target)| normalize(target))
        .unwrap_or(key)
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
        .flat_map(char::to_lowercase)
        .filter(|c| c.is_alphanumeric() || *c == '+')
        .collect()
}

fn canonical(value: &str) -> String {
    let mut name = normalize(value);
    for suffix in ["高清", "超清", "蓝光", "4k", "hd", "频道", "台"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_string();
        }
    }
    let Some(rest) = name.strip_prefix("cctv") else {
        return name;
    };
    let mut digits = String::new();
    let mut plus = false;
    for c in rest.chars() {
        if c.is_ascii_digit() && !plus {
            digits.push(c);
        } else if c == '+' && !digits.is_empty() {
            plus = true;
            break;
        } else {
            break;
        }
    }
    if digits.is_empty() {
        name
    } else {
        format!("cctv{digits}{}", if plus { "+" } else { "" })
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
    use std::io::Write;

    fn default_key(name: &str) -> String {
        channel_key(name, &HashMap::new())
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
        assert_eq!(store_feeds(&conn, &[original], now - 3600).unwrap(), (1, 3));
        let revised = feed(
            r#"<programme channel="feed.17" start="19700109081500 +0000" stop="19700109090000 +0000"><title>Corrected</title></programme>"#,
        );
        for _ in 0..2 {
            assert_eq!(
                store_feeds(&conn, std::slice::from_ref(&revised), now).unwrap(),
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
