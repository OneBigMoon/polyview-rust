use super::{
    Episode, PlayGroup, Source, active_sources, detail_item, ensure_source_visible, fetch_items,
    media_kind, play_groups, source_by_id, text, validate_text,
};
use crate::{AppError, Shared, auth::AuthUser};
use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use futures_util::{StreamExt, stream};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

const MAX_SOURCES: usize = 12;
const MAX_DETAILS_PER_SOURCE: usize = 3;
const MAX_CANDIDATES: usize = 24;

// The media server and its CMS may fail together. Keep the identity and episode labels
// from recently opened titles on this connection so recovery can still search other sources.
pub(super) fn init_cache(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS opened_catalog (
            slot INTEGER PRIMARY KEY AUTOINCREMENT, source_id INTEGER NOT NULL, vod_id TEXT NOT NULL,
            title TEXT, year TEXT, category TEXT, flags TEXT, playlist TEXT, opened_at INTEGER NOT NULL
        );
        CREATE TEMP TRIGGER IF NOT EXISTS opened_catalog_limit AFTER INSERT ON opened_catalog BEGIN
            DELETE FROM opened_catalog WHERE slot <= NEW.slot - 64;
        END;
        CREATE TEMP TRIGGER IF NOT EXISTS opened_catalog_removed AFTER DELETE ON main.catalog_sources BEGIN
            DELETE FROM opened_catalog WHERE source_id=OLD.id;
        END;
        CREATE TEMP TRIGGER IF NOT EXISTS opened_catalog_changed AFTER UPDATE OF url ON main.catalog_sources
        WHEN NEW.url <> OLD.url BEGIN
            DELETE FROM opened_catalog WHERE source_id=OLD.id;
        END;",
    )?;
    Ok(())
}

pub(super) fn open_detail(
    state: &Shared,
    source_id: i64,
    id: &str,
) -> Result<(Source, i64), AppError> {
    let conn = state.conn()?;
    let source = source_by_id(&conn, source_id)?;
    ensure_source_visible(&conn, &source)?;
    // Reserve before fetching. Deleting or repointing the source invalidates in-flight writes too.
    conn.execute(
        "INSERT INTO opened_catalog(source_id,vod_id,opened_at) VALUES(?,?,unixepoch())",
        params![source_id, id],
    )?;
    Ok((source, conn.last_insert_rowid()))
}

pub(super) fn remember_detail(state: &Shared, slot: i64, item: &Value) -> Result<(), AppError> {
    let title = item["vod_name"].as_str().unwrap_or_default();
    let year = text(item, "vod_year");
    let category = item["type_name"].as_str().unwrap_or_default();
    let flags = item["vod_play_from"].as_str().unwrap_or_default();
    let playlist = item["vod_play_url"].as_str().unwrap_or_default();
    if title.len() + year.len() + category.len() + flags.len() + playlist.len() <= 256 * 1024 {
        state.conn()?.execute(
            "UPDATE opened_catalog SET title=?,year=?,category=?,flags=?,playlist=? WHERE slot=?",
            params![title, year, category, flags, playlist, slot],
        )?;
    }
    Ok(())
}

fn origin_detail(
    state: &Shared,
    source_id: i64,
    id: &str,
) -> Result<(Source, Option<Value>), AppError> {
    let conn = state.conn()?;
    let source = source_by_id(&conn, source_id)?;
    ensure_source_visible(&conn, &source)?;
    let item = conn.query_row(
        "SELECT title,year,category,flags,playlist FROM opened_catalog WHERE source_id=? AND vod_id=?
         AND title IS NOT NULL AND opened_at >= unixepoch()-21600 ORDER BY slot DESC LIMIT 1",
        params![source_id, id],
        |row| Ok(serde_json::json!({
            "vod_name": row.get::<_, String>(0)?, "vod_year": row.get::<_, String>(1)?,
            "type_name": row.get::<_, String>(2)?, "vod_play_from": row.get::<_, String>(3)?,
            "vod_play_url": row.get::<_, String>(4)?
        })),
    ).optional()?;
    Ok((source, item))
}

#[derive(Deserialize)]
pub(super) struct FallbackQuery {
    source_id: i64,
    vod_id: String,
    episode: usize,
    flag: Option<String>,
}

#[derive(Serialize)]
pub(super) struct Candidates {
    items: Vec<Candidate>,
}

#[derive(Serialize)]
struct Candidate {
    source_id: i64,
    vod_id: String,
    title: String,
    flag: String,
    url: String,
    direct: bool,
}

fn narrow(c: char) -> char {
    match c {
        '！'..='～' => char::from_u32(c as u32 - 0xfee0).unwrap_or(c),
        _ => c,
    }
}

fn title_without_year<'a>(title: &'a str, year: &str) -> &'a str {
    let title = title.trim();
    if year.len() == 4 && year.bytes().all(|b| b.is_ascii_digit()) {
        let end = title.trim_end_matches([')', '）']);
        if let Some(prefix) = end.strip_suffix(year) {
            let prefix = prefix.trim_end_matches(|c: char| {
                c.is_whitespace() || matches!(c, '(' | '（' | '.' | '·' | '-')
            });
            if !prefix.is_empty() {
                return prefix;
            }
        }
    }
    title
}

fn title_key(title: &str) -> String {
    title
        .chars()
        .map(narrow)
        .filter(|c| !c.is_whitespace() && !":,.!?、·・-—_()[]【】《》\"“”'’。".contains(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

#[derive(PartialEq)]
enum EpisodeKey {
    Number(usize, usize),
    Name(String),
}

fn episode_number(label: &str) -> Option<usize> {
    if let Ok(number) = label.parse() {
        return Some(number);
    }
    // Chinese labels commonly spell the ordinal, e.g. 第十二集.
    let digit = |c| "零一二三四五六七八九".chars().position(|n| n == c);
    if label.is_empty() {
        return None;
    }
    if !label.contains(['十', '百', '千']) {
        return label
            .chars()
            .try_fold(0usize, |n, c| n.checked_mul(10)?.checked_add(digit(c)?));
    }
    let mut total = 0usize;
    let mut pending = None;
    let mut last_unit = usize::MAX;
    for c in label.chars() {
        if let Some(value) = digit(c) {
            if pending.is_some_and(|previous| previous != 0) {
                return None;
            }
            pending = Some(value);
        } else {
            let unit = match c {
                '十' => 10,
                '百' => 100,
                '千' => 1000,
                _ => return None,
            };
            if unit >= last_unit {
                return None;
            }
            total += pending.take().unwrap_or(1) * unit;
            last_unit = unit;
        }
    }
    Some(total + pending.unwrap_or(0))
}

fn episode_key(name: &str, ordinal: usize, movie_singleton: bool) -> EpisodeKey {
    let label: String = name
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| narrow(c).to_ascii_lowercase())
        .collect();
    if label == "播放"
        || (movie_singleton
            && matches!(
                label.as_str(),
                "正片" | "电影" | "高清" | "hd" | "hd中字" | "1080p" | "4k"
            ))
    {
        return EpisodeKey::Number(1, ordinal);
    }
    if let Some((season, ep)) = label.strip_prefix('s').and_then(|s| s.split_once('e'))
        && let (Ok(season), Ok(ep)) = (season.parse(), ep.parse())
    {
        return EpisodeKey::Number(season, ep);
    }
    let stripped = label
        .strip_prefix('第')
        .and_then(|s| s.strip_suffix(['集', '话', '期']))
        .or_else(|| label.strip_prefix("episode"))
        .or_else(|| label.strip_prefix("ep"))
        .or_else(|| label.strip_prefix('e'))
        .unwrap_or(&label);
    match episode_number(stripped) {
        Some(number) => EpisodeKey::Number(1, number),
        None => EpisodeKey::Name(label),
    }
}

fn groups(item: &Value) -> Vec<PlayGroup> {
    play_groups(
        item["vod_play_from"].as_str().unwrap_or_default(),
        item["vod_play_url"].as_str().unwrap_or_default(),
    )
}

struct Target<'a> {
    search: &'a str,
    title_key: String,
    year: &'a str,
    category: &'a str,
    episode: EpisodeKey,
}

impl Target<'_> {
    fn kind_matches(&self, category: &str) -> bool {
        let kind = media_kind(self.category);
        kind == media_kind(category) && (kind != "other" || self.category == category)
    }

    fn matching_episode(&self, group: PlayGroup) -> Option<(String, Episode)> {
        let movie_singleton = media_kind(self.category) == "movie" && group.episodes.len() == 1;
        group
            .episodes
            .into_iter()
            .enumerate()
            .find(|(i, ep)| episode_key(&ep.name, i + 1, movie_singleton) == self.episode)
            .map(|(_, ep)| (group.name, ep))
    }
}

async fn source_candidates(state: &Shared, source: Source, target: &Target<'_>) -> Vec<Candidate> {
    let Ok(page) = fetch_items(state, &source, &[("ac", "detail"), ("wd", target.search)]).await
    else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut detail_requests = 0;
    for mut item in page.items.into_iter().take(100) {
        let title_matches = |item: &Value| {
            item["vod_name"].as_str().is_some_and(|title| {
                title_key(title_without_year(title, target.year)) == target.title_key
            })
        };
        if !title_matches(&item) {
            continue;
        }
        let year = text(&item, "vod_year");
        let category = item["type_name"].as_str().unwrap_or_default().trim();
        if (!year.trim().is_empty() && year.trim() != target.year)
            || (!category.is_empty() && !target.kind_matches(category))
        {
            continue;
        }
        let id = text(&item, "vod_id");
        if validate_text(&id, "Media ID", 256).is_err() {
            continue;
        }
        // Some CMS searches return only summary rows, even with ac=detail.
        if item["vod_play_url"].as_str().unwrap_or_default().is_empty()
            || year.trim() != target.year
            || !target.kind_matches(category)
        {
            if detail_requests == MAX_DETAILS_PER_SOURCE {
                continue;
            }
            detail_requests += 1;
            let Ok(detail) = detail_item(state, &source, &id).await else {
                continue;
            };
            item = detail;
            if !title_matches(&item)
                || text(&item, "vod_year").trim() != target.year
                || !target.kind_matches(item["type_name"].as_str().unwrap_or_default().trim())
            {
                continue;
            }
        }
        for group in groups(&item) {
            if let Some((flag, ep)) = target.matching_episode(group) {
                found.push(Candidate {
                    source_id: source.id,
                    vod_id: id.clone(),
                    title: text(&item, "vod_name"),
                    flag,
                    url: ep.url,
                    direct: ep.direct,
                });
                if found.len() == MAX_CANDIDATES {
                    return found;
                }
            }
        }
    }
    found
}

pub(super) async fn handler(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<FallbackQuery>,
) -> Result<Json<Candidates>, AppError> {
    let id = validate_text(&query.vod_id, "Media ID", 256)?;
    let index = query
        .episode
        .checked_sub(1)
        .ok_or_else(|| AppError::bad_request("Episode must be at least 1"))?;
    let (origin, cached) = origin_detail(&state, query.source_id, id)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let item = match cached {
        Some(item) => item,
        None => timeout_at(deadline, detail_item(&state, &origin, id))
            .await
            .map_err(|_| AppError(StatusCode::BAD_GATEWAY, "Source timed out".into()))??,
    };
    let groups = groups(&item);
    let group = if let Some(flag) = query.flag.as_deref() {
        groups.iter().find(|g| g.name == flag)
    } else {
        groups.iter().find(|g| g.direct).or_else(|| groups.first())
    };
    let Some((group, origin_ep)) = group.and_then(|g| g.episodes.get(index).map(|ep| (g, ep)))
    else {
        return Ok(Json(Candidates { items: Vec::new() }));
    };
    let year = text(&item, "vod_year");
    let year = year.trim();
    let title = item["vod_name"].as_str().unwrap_or_default();
    let search = title_without_year(title, year);
    let category = item["type_name"].as_str().unwrap_or_default().trim();
    let target = Target {
        search: validate_text(search, "Title", 256)?,
        title_key: title_key(search),
        year,
        category,
        episode: episode_key(
            &origin_ep.name,
            query.episode,
            media_kind(category) == "movie" && group.episodes.len() == 1,
        ),
    };
    if target.title_key.is_empty() {
        return Ok(Json(Candidates { items: Vec::new() }));
    }
    let now = crate::now();
    let sources = active_sources(&state)?
        .into_iter()
        .filter(|s| s.id != origin.id && s.cooldown_until <= now)
        .take(MAX_SOURCES);
    let target = &target;
    let state = &state;
    let mut requests = stream::iter(sources.enumerate().map(|(rank, source)| async move {
        let rows = tokio::time::timeout(
            Duration::from_secs(3),
            source_candidates(state, source, target),
        )
        .await
        .unwrap_or_default();
        (rank, rows)
    }))
    .buffer_unordered(4);
    let mut completed = Vec::new();
    while let Ok(Some(result)) = timeout_at(deadline, requests.next()).await {
        completed.push(result);
    }
    completed.sort_unstable_by_key(|(rank, _)| *rank);
    let mut items: Vec<Candidate> = Vec::new();
    for row in completed.into_iter().flat_map(|(_, rows)| rows) {
        let same_stream =
            |url: &str, flag: &str| row.url == url && (row.direct || row.flag == flag);
        if same_stream(&origin_ep.url, &group.name)
            || items
                .iter()
                .any(|known| same_stream(&known.url, &known.flag))
        {
            continue;
        }
        items.push(row);
        if items.len() == MAX_CANDIDATES {
            break;
        }
    }
    Ok(Json(Candidates { items }))
}
