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
use roxmltree::{Document, ParsingOptions};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const DEFAULT_EPG_URLS: &[&str] = &[
    "http://epg.51zmt.top:8000/e.xml",
    "https://epg.112114.xyz/pp.xml",
];
const EPG_STALE_SECONDS: i64 = 6 * 60 * 60;

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
    _user: AuthUser,
    Query(query): Query<GuideQuery>,
) -> Result<Json<Value>, AppError> {
    let start = query.start.unwrap_or_else(|| now() - 3600);
    let end = query.end.unwrap_or_else(|| now() + 7 * 3600);
    if end <= start || end - start > 48 * 3600 {
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
    let url = setting(&conn, "epg_url")?;
    Ok(Json(json!({
        "updated_at": updated_at,
        "stale": now() - updated_at > EPG_STALE_SECONDS,
        "url": url,
        "matched_channels": matched,
        "programs": programs,
    })))
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
    let urls: Vec<&str> = input
        .url
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
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(AppError::bad_request("Enter a valid XMLTV HTTP(S) URL"));
        }
    }
    let value = urls.join("\n");
    let conn = state.conn()?;
    conn.execute(
        "INSERT INTO settings(key,value) VALUES ('epg_url',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [&value],
    )?;
    Ok(Json(json!({"url": value})))
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
    let channels = {
        let mut statement = conn.prepare("SELECT id,name FROM channels ORDER BY id")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut matched = HashSet::new();
    let mut programs = 0usize;
    let mut covered: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM epg_programs", [])?;
    for parsed in &feeds {
        for (channel_id, name) in &channels {
            let Some(xml_id) = find_channel(name, &parsed.aliases, &parsed.canonical) else {
                continue;
            };
            let Some(items) = parsed.programs.get(&xml_id) else {
                continue;
            };
            matched.insert(*channel_id);
            let ranges = covered.entry(*channel_id).or_default();
            for item in items {
                if ranges
                    .iter()
                    .any(|(start, stop)| item.start < *stop && item.stop > *start)
                {
                    continue;
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
    let updated_at = now();
    tx.execute(
        "INSERT INTO settings(key,value) VALUES ('epg_updated_at',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [updated_at.to_string()],
    )?;
    tx.commit()?;
    Ok(json!({
        "updated_at": updated_at,
        "feeds": feeds.len(),
        "matched_channels": matched.len(),
        "programs": programs,
        "errors": errors,
    }))
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
    let body = response
        .text()
        .await
        .map_err(|error| AppError(axum::http::StatusCode::BAD_GATEWAY, error.to_string()))?;
    parse_xmltv(&body)
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

fn find_channel(
    name: &str,
    aliases: &HashMap<String, String>,
    canonical_aliases: &HashMap<String, String>,
) -> Option<String> {
    aliases
        .get(&normalize(name))
        .or_else(|| canonical_aliases.get(&canonical(name)))
        .cloned()
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

    #[test]
    fn xmltv_times_convert_to_utc() {
        assert_eq!(parse_xmltv_time("19700101000000 +0000"), Some(0));
        assert_eq!(parse_xmltv_time("19700101080000 +0800"), Some(0));
        assert_eq!(parse_xmltv_time("19700101000000 Z"), Some(0));
        assert_eq!(parse_xmltv_time("bad"), None);
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
            find_channel("CCTV-1 综合", &aliases, &canonical),
            Some("1".into())
        );
        assert_eq!(
            find_channel("东方卫视 4K", &aliases, &canonical),
            Some("2".into())
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
