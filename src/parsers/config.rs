use super::*;
use axum::http::{HeaderName, HeaderValue};
use rusqlite::Row;
use std::collections::BTreeMap;
use url::Url;

pub(super) const COLUMNS: &str =
    "id,name,url_template,headers_json,flags,priority,timeout_ms,enabled";

#[derive(Serialize)]
pub(super) struct Parser {
    pub id: i64,
    pub name: String,
    #[serde(skip)]
    pub url_template: String,
    #[serde(skip)]
    pub headers_json: String,
    pub flags: String,
    pub priority: i64,
    pub timeout_ms: i64,
    pub enabled: bool,
}

impl Parser {
    pub(super) fn row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            url_template: row.get(2)?,
            headers_json: row.get(3)?,
            flags: row.get(4)?,
            priority: row.get(5)?,
            timeout_ms: row.get(6)?,
            enabled: row.get(7)?,
        })
    }
}

pub(super) fn checked_url(raw: &str) -> Result<Url, AppError> {
    if raw.len() > 16384
        || raw.contains('\\')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(AppError::bad_request("Invalid HTTP(S) URL"));
    }
    let mut url = crate::network::http_url(raw)?;
    url.set_fragment(None);
    Ok(url)
}

pub(super) fn endpoint(template: &str, input: &Url) -> Result<Url, AppError> {
    let invalid = || AppError::bad_request("Invalid JSON parser URL template");
    if template.len() > 4096 || template.contains('#') || template.matches("{url}").count() > 1 {
        return Err(invalid());
    }
    let base = checked_url(template)?;
    if !template.contains("{url}") && !template.ends_with('=') {
        let mut url = base;
        url.query_pairs_mut().append_pair("url", input.as_str());
        return Ok(url);
    }
    let encoded =
        url::form_urlencoded::byte_serialize(input.as_str().as_bytes()).collect::<String>();
    let raw = if template.contains("{url}") {
        template.replace("{url}", &encoded)
    } else {
        format!("{template}{encoded}")
    };
    let url = crate::network::http_url(&raw)?;
    if url.origin() != base.origin() {
        return Err(invalid());
    }
    Ok(url)
}

pub(super) fn request_headers(raw: &str) -> Result<HeaderMap, AppError> {
    let invalid = || AppError::bad_request("Invalid parser request headers");
    if raw.len() > 32768 {
        return Err(invalid());
    }
    let entries: BTreeMap<String, String> = serde_json::from_str(raw).map_err(|_| invalid())?;
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "keep-alive"
                | "content-length"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "proxy-connection"
                | "proxy-authorization"
        ) {
            return Err(invalid());
        }
        let value = HeaderValue::from_str(&value).map_err(|_| invalid())?;
        headers.insert(name, value);
    }
    Ok(headers)
}

pub(crate) fn validate_config(
    name: &str,
    url_template: &str,
    headers_json: &str,
    flags: &str,
    timeout_ms: i64,
) -> Result<(), AppError> {
    if name.trim().is_empty()
        || name.chars().count() > 100
        || name.chars().any(char::is_control)
        || flags.len() > 4096
        || flags.chars().any(char::is_control)
        || !(1..=120000).contains(&timeout_ms)
    {
        return Err(AppError::bad_request("Invalid parser configuration"));
    }
    endpoint(
        url_template,
        &Url::parse("https://example.invalid/video").expect("static URL"),
    )?;
    request_headers(headers_json)?;
    Ok(())
}

pub(super) fn matches_flag(flags: &str, flag: &str) -> bool {
    flags
        .split(',')
        .map(str::trim)
        .any(|f| f == "*" || (!f.is_empty() && f.eq_ignore_ascii_case(flag.trim())))
}
