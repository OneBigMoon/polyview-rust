use super::{AppError, AuthUser, HeaderMap, Query, Response, Shared, State, Url, grants, http_url};
use axum::extract::Path;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;

const WEEK: i64 = 7 * 24 * 3600;
const MAX_URL: usize = 16_384;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatchupQuery {
    start: Option<i64>,
    end: Option<i64>,
}

pub(super) async fn stream(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Query(query): Query<CatchupQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    line_stream(&state, user.id, None, id, query, headers).await
}

pub(crate) async fn line_stream(
    state: &Shared,
    user_id: i64,
    feed_token: Option<&str>,
    id: i64,
    query: CatchupQuery,
    range: HeaderMap,
) -> Result<Response, AppError> {
    let (url, private) = {
        let conn = state.conn()?;
        let (raw, headers, mode, template): (String, String, String, String) = conn
            .query_row(
                "SELECT url,headers_json,catchup_mode,catchup_source FROM channel_sources WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| AppError::not_found("Channel line not found"))?;
        let base = http_url(&raw)?;
        let url = query.target(&conn, &base, &mode, &template, crate::now())?;
        let private = if grants::same_origin(&base, &url) {
            crate::network::live_headers(&headers)?
        } else {
            HeaderMap::new()
        };
        (url, private)
    };
    grants::live_stream(state, user_id, feed_token, url, private, range).await
}

impl CatchupQuery {
    fn target(
        &self,
        conn: &Connection,
        base: &Url,
        mode: &str,
        template: &str,
        now: i64,
    ) -> Result<Url, AppError> {
        match (self.start, self.end) {
            (None, None) => Ok(base.clone()),
            (Some(offset @ -604_800..=-1), None) => {
                let mut url = base.clone();
                // Preserve the exact escaping and order of signed upstream query parameters.
                let mut pairs: Vec<_> = base
                    .query()
                    .unwrap_or_default()
                    .split('&')
                    .filter(|part| {
                        !part.is_empty()
                            && !url::form_urlencoded::parse(part.as_bytes())
                                .any(|(key, _)| key == "start")
                    })
                    .collect();
                let start = format!("start={offset}");
                pairs.push(&start);
                url.set_query(Some(&pairs.join("&")));
                http_url(url.as_str())
            }
            (Some(start), Some(end)) if start >= now - WEEK && start < end && end <= now => {
                if template.is_empty() {
                    return Err(AppError::bad_request(
                        "Channel line has no catch-up template",
                    ));
                }
                render_template(conn, base, mode, template, start, end, now)
            }
            _ => Err(AppError::bad_request(
                "Catch-up requires a past interval within seven days",
            )),
        }
    }
}

fn invalid_template() -> AppError {
    AppError::bad_request("Invalid channel catch-up template")
}

fn render_template(
    conn: &Connection,
    base: &Url,
    mode: &str,
    template: &str,
    start: i64,
    end: i64,
    now: i64,
) -> Result<Url, AppError> {
    if template.len() > MAX_URL || template.chars().any(char::is_control) {
        return Err(invalid_template());
    }
    // XMLTV program boundaries have already been converted to Unix seconds. Render dates in UTC.
    let (begin_date, end_date): (String, String) = conn.query_row(
        "SELECT strftime('%Y%m%d%H%M%S',?1,'unixepoch'),strftime('%Y%m%d%H%M%S',?2,'unixepoch')",
        [start, end],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let begin = start.to_string();
    let finish = end.to_string();
    let duration = (end - start).to_string();
    let timestamp = now.to_string();
    let mut rest = template;
    let mut rendered = String::with_capacity(template.len());
    while let Some((prefix, tail)) = rest.split_once('{') {
        let (key, suffix) = tail.split_once('}').ok_or_else(invalid_template)?;
        rendered.push_str(prefix.strip_suffix('$').unwrap_or(prefix));
        rendered.push_str(match key {
            "(b)yyyyMMddHHmmss" => &begin_date,
            "(e)yyyyMMddHHmmss" => &end_date,
            "utc" | "start" | "unix" | "(b)unix" | "(b)timestamp" => &begin,
            "utcend" | "end" | "(e)unix" | "(e)timestamp" => &finish,
            "duration" => &duration,
            "timestamp" | "lutc" | "now" => &timestamp,
            _ => return Err(invalid_template()),
        });
        if rendered.len() > MAX_URL {
            return Err(invalid_template());
        }
        rest = suffix;
    }
    rendered.push_str(rest);
    if rendered.len() > MAX_URL || rendered.contains('}') {
        return Err(invalid_template());
    }
    let url = if rendered.starts_with('&')
        || mode.eq_ignore_ascii_case("append") && rendered.starts_with('?')
    {
        let suffix = &rendered[1..];
        let mut url = base.clone();
        match base.query().filter(|existing| !existing.is_empty()) {
            Some(existing) => url.set_query(Some(&format!("{existing}&{suffix}"))),
            None => url.set_query(Some(suffix)),
        }
        url
    } else {
        base.join(&rendered).map_err(|_| invalid_template())?
    };
    http_url(url.as_str()).map_err(|_| invalid_template())
}

pub(super) async fn logo(
    State(state): State<Shared>,
    _user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Response, AppError> {
    channel_logo(&state, id).await
}

pub(crate) async fn channel_logo(state: &Shared, id: i64) -> Result<Response, AppError> {
    let raw: String = state
        .conn()?
        .query_row(
            "SELECT logo FROM channels WHERE id=?1 AND logo<>''",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Channel logo not found"))?;
    let (upstream, _) = grants::fetch_media(http_url(&raw)?, None, &HeaderMap::new()).await?;
    tokio::time::timeout(super::REQUEST_TIMEOUT, super::raster_response(upstream))
        .await
        .map_err(|_| super::source_error("Channel logo transfer timed out"))?
}
