use super::{
    config::{COLUMNS, Parser, checked_url, endpoint, matches_flag, request_headers},
    *,
};
use crate::{catalog::is_direct_media_url, now};
use axum::http::{HeaderName, HeaderValue};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};
use url::Url;

const MAX_RESPONSE: usize = 256 * 1024;
const CACHE_TTL: i64 = 600;
const CACHE_LIMIT: usize = 512;

fn failed() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "Parser could not resolve playable media".into(),
    )
}

pub(crate) fn has_matching_parser(conn: &Connection, flag: &str) -> Result<bool, AppError> {
    let mut stmt = conn.prepare_cached("SELECT flags FROM parsers WHERE enabled=1")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if matches_flag(
            row.get_ref(0)?
                .as_str()
                .map_err(|_| AppError::internal("Invalid parser flag"))?,
            flag,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn media_headers(value: Option<&Value>) -> Result<HeaderMap, AppError> {
    let mut result = HeaderMap::new();
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(result);
    };
    for (name, value) in value.as_object().ok_or_else(failed)? {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| failed())?;
        if matches!(
            name.as_str(),
            "referer"
                | "user-agent"
                | "origin"
                | "cookie"
                | "authorization"
                | "x-playback-token"
                | "x-media-token"
                | "x-auth-token"
                | "x-api-key"
        ) {
            let text = value
                .as_str()
                .filter(|v| v.len() <= 8192 && v.is_ascii())
                .ok_or_else(failed)?;
            let value = HeaderValue::from_str(text).map_err(|_| failed())?;
            result.insert(name, value);
        }
    }
    Ok(result)
}

fn parsed(json: &[u8]) -> Result<ResolvedMedia, AppError> {
    let document: Value = serde_json::from_slice(json).map_err(|_| failed())?;
    let result = if document.get("url").is_some() {
        &document
    } else {
        document.get("data").ok_or_else(failed)?
    };
    let url = checked_url(result["url"].as_str().ok_or_else(failed)?).map_err(|_| failed())?;
    if !is_direct_media_url(url.as_str()) {
        return Err(failed());
    }
    fn returned_headers(obj: &Value) -> Option<&Value> {
        obj.get("headers").or_else(|| obj.get("header"))
    }
    let headers = media_headers(returned_headers(result).or_else(|| returned_headers(&document)))?;
    Ok(ResolvedMedia {
        url: url.into(),
        headers,
    })
}

pub(super) async fn request(parser: &Parser, input: &Url) -> Result<ResolvedMedia, AppError> {
    let timeout = u64::try_from(parser.timeout_ms)
        .ok()
        .filter(|t| (1..=120000).contains(t))
        .ok_or_else(failed)?;
    let target = endpoint(&parser.url_template, input).map_err(|_| failed())?;
    let headers = request_headers(&parser.headers_json).map_err(|_| failed())?;
    let mut response = playback::no_redirect_client()?
        .get(target)
        .headers(headers)
        .timeout(Duration::from_millis(timeout))
        .send()
        .await
        .map_err(|_| failed())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_RESPONSE as u64)
    {
        return Err(failed());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failed())? {
        if chunk.len() > MAX_RESPONSE - body.len() {
            return Err(failed());
        }
        body.extend_from_slice(&chunk);
    }
    parsed(&body)
}

fn revision(conn: &Connection) -> Result<i64, AppError> {
    Ok(
        conn.query_row("SELECT revision FROM parser_revision WHERE id=1", [], |r| {
            r.get(0)
        })?,
    )
}

fn cached(conn: &Connection, url: &str, flag: &str) -> Result<Option<ResolvedMedia>, AppError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT media,headers FROM parser_cache WHERE input=? AND flag=? AND expires>?",
            params![url, flag, now()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(url, raw)| {
        let value = serde_json::from_str(&raw)
            .map_err(|_| AppError::internal("Invalid cached media headers"))?;
        Ok(ResolvedMedia {
            url,
            headers: media_headers(Some(&value))?,
        })
    })
    .transpose()
}

fn save(
    conn: &mut Connection,
    expected: i64,
    url: &str,
    flag: &str,
    media: &ResolvedMedia,
) -> Result<(), AppError> {
    let headers = media
        .headers
        .iter()
        .map(|(k, v)| Ok((k.as_str(), v.to_str().map_err(|_| failed())?)))
        .collect::<Result<BTreeMap<_, _>, AppError>>()?;
    let headers = serde_json::to_string(&headers).map_err(|_| failed())?;
    let tx = conn.transaction()?;
    if revision(&tx)? != expected {
        return Err(AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "Parser settings changed during resolution".into(),
        ));
    }
    let timestamp = now();
    tx.execute("DELETE FROM parser_cache WHERE expires<=?", [timestamp])?;
    tx.execute(
        "INSERT OR REPLACE INTO parser_cache(input,flag,media,headers,expires) VALUES(?,?,?,?,?)",
        params![url, flag, media.url, headers, timestamp + CACHE_TTL],
    )?;
    tx.execute("DELETE FROM parser_cache WHERE rowid IN (SELECT rowid FROM parser_cache ORDER BY expires DESC,rowid DESC LIMIT -1 OFFSET ?)", [CACHE_LIMIT])?;
    tx.commit()?;
    Ok(())
}

pub(crate) async fn resolve_media(
    state: &Shared,
    url: &str,
    flag: &str,
) -> Result<ResolvedMedia, AppError> {
    let input = checked_url(url)?;
    if flag.len() > 4096 || flag.chars().any(char::is_control) {
        return Err(AppError::bad_request("Invalid parser flag"));
    }
    if is_direct_media_url(input.as_str()) {
        return Ok(ResolvedMedia {
            url: input.into(),
            headers: HeaderMap::new(),
        });
    }
    let flag = flag.trim().to_ascii_lowercase();
    let (expected, parsers) = {
        let conn = state.conn()?;
        if let Some(media) = cached(&conn, input.as_str(), &flag)? {
            return Ok(media);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM parsers WHERE enabled=1 ORDER BY priority DESC,id"
        ))?;
        let mut rows = stmt.query([])?;
        let mut parsers = Vec::new();
        while let Some(row) = rows.next()? {
            let flags: String = row.get(4)?;
            if matches_flag(&flags, &flag) {
                parsers.push(Parser::row(row)?)
            }
        }
        (revision(&conn)?, parsers)
    };
    if parsers.is_empty() {
        return Err(AppError::not_found("No enabled parser matches this source"));
    }
    let media = tokio::time::timeout(Duration::from_secs(120), async {
        for parser in parsers {
            if let Ok(media) = request(&parser, &input).await {
                return Ok(media);
            }
        }
        Err(failed())
    })
    .await
    .map_err(|_| failed())??;
    save(&mut *state.conn()?, expected, input.as_str(), &flag, &media)?;
    Ok(media)
}
