use super::{active_sources, fetch_items, summary, text};
use crate::{AppError, Shared, auth::AuthUser, settings};
use axum::{Json, extract::State};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{collections::BTreeSet, time::Duration};

const CACHE_KEY: &str = "catalog_recommend_douban";
const SIX_HOURS: i64 = 6 * 60 * 60;
const MOVIE_URL: &str = "https://movie.douban.com/j/search_subjects?type=movie&tag=%E7%83%AD%E9%97%A8&page_limit=20&page_start=0";
const SERIES_URL: &str = "https://movie.douban.com/j/search_subjects?type=tv&tag=%E7%83%AD%E9%97%A8&page_limit=20&page_start=0";

fn response(origin: &str, movies: Vec<Value>, series: Vec<Value>) -> Value {
    json!({"origin": origin, "groups": [
        {"title": "热门电影", "kind": "movie", "items": movies},
        {"title": "热门剧集", "kind": "series", "items": series}
    ]})
}

async fn subjects(client: &reqwest::Client, url: &str) -> Option<Vec<Value>> {
    let response = client
        .get(url)
        .header(reqwest::header::REFERER, "https://movie.douban.com/")
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let mut body = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.ok()?;
        if body.len().saturating_add(chunk.len()) > super::MAX_RESPONSE_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    let feed: Value = serde_json::from_slice(&body).ok()?;
    let items: Vec<_> = feed["subjects"]
        .as_array()?
        .iter()
        .filter_map(|item| {
            let title = item["title"].as_str()?.trim();
            (!title.is_empty()).then(|| json!({"title":title, "cover":text(item,"cover"), "year":text(item,"year"), "rating":text(item,"rate")}))
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

pub(super) async fn handler(
    State(state): State<Shared>,
    _user: AuthUser,
) -> Result<Json<Value>, AppError> {
    let (movie_url, series_url) = {
        let conn = state.conn()?;
        let movie = settings::get_setting_json::<Option<String>>(&conn, "douban_movie_url")?
            .unwrap_or_else(|| MOVIE_URL.into());
        let series = settings::get_setting_json::<Option<String>>(&conn, "douban_series_url")?
            .unwrap_or_else(|| SERIES_URL.into());
        if let Some(mut cached) = settings::read_cached(&conn, CACHE_KEY, SIX_HOURS)?
            && cached["movie_url"].as_str() == Some(movie.as_str())
            && cached["series_url"].as_str() == Some(series.as_str())
            && cached["response"]["origin"] == "douban"
        {
            return Ok(Json(cached["response"].take()));
        }
        (movie, series)
    };
    let (movies, series) = tokio::join!(
        subjects(&state.client, &movie_url),
        subjects(&state.client, &series_url)
    );
    if let (Some(movies), Some(series)) = (movies, series) {
        let mut entry = json!({"movie_url":movie_url, "series_url":series_url,
            "response":response("douban", movies, series)});
        let conn = state.conn()?;
        settings::write_cached(&conn, CACHE_KEY, &entry)?;
        return Ok(Json(entry["response"].take()));
    }

    let sources = active_sources(&state)?;
    let state = &state;
    let mut pages = stream::iter(sources.into_iter().map(|source| async move {
        let page = fetch_items(state, &source, &[("ac", "detail"), ("pg", "1")]).await;
        (source, page)
    }))
    .buffered(32);
    let mut movies = Vec::new();
    let mut series = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some((source, page)) = pages.next().await {
        let Ok(page) = page else { continue };
        for row in page.items {
            let Some(item) = summary(&source, &row) else {
                continue;
            };
            let group = match item.kind.as_str() {
                "movie" => &mut movies,
                "series" => &mut series,
                _ => continue,
            };
            if seen.insert((item.kind, item.title.clone(), item.year.clone())) {
                group.push(
                    json!({"title":item.title,"year":item.year,"cover":item.cover,
                    "rating":text(&row,"vod_score"),"source_id":source.id,"id":item.id}),
                );
            }
        }
    }
    Ok(Json(response("fallback", movies, series)))
}
