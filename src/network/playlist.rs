use super::{PlaylistChannel, http_url};
use crate::AppError;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::{BTreeMap, HashSet};
use url::Url;

const MAX_URL: usize = 16_384;
const MAX_HEADERS: usize = 32_768;

type Headers<'a> = BTreeMap<&'static str, std::borrow::Cow<'a, str>>;

fn valid_url_text(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_URL
        && !raw.contains('\\')
        && !raw.chars().any(|c| c.is_control() || c.is_whitespace())
}

pub(crate) fn live_url(raw: &str) -> Result<Url, AppError> {
    if !valid_url_text(raw) {
        return Err(AppError::bad_request("Invalid live URL"));
    }
    http_url(raw)
}

pub(crate) fn validate_channel_metadata(
    name: &str,
    group: &str,
    tvg_id: &str,
    logo: &str,
) -> Result<(), AppError> {
    if name.trim().is_empty()
        || [name, group, tvg_id]
            .iter()
            .any(|v| v.len() > 300 || v.chars().any(char::is_control))
    {
        return Err(AppError::bad_request("Invalid live channel metadata"));
    }
    if !logo.is_empty() {
        live_url(logo)?;
    }
    Ok(())
}

pub(crate) fn validate_catchup(mode: &str, source: &str) -> Result<(), AppError> {
    let invalid = || AppError::bad_request("Invalid live catchup template");
    if mode.len() > 64 || mode.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(invalid());
    }
    if source.is_empty() {
        return Ok(());
    }
    if !valid_url_text(source) {
        return Err(invalid());
    }
    // Catchup sources may be absolute URLs, relative paths, or a query to append.
    // Parse only for validation: URL normalization escapes timestamp braces in paths.
    let base = Url::parse("https://playlist.invalid/live").expect("constant URL");
    let url = base.join(source).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

fn header_key(name: &str) -> Result<&'static str, AppError> {
    if name.eq_ignore_ascii_case("User-Agent") {
        Ok("User-Agent")
    } else if name.eq_ignore_ascii_case("Referer") || name.eq_ignore_ascii_case("Referrer") {
        Ok("Referer")
    } else if name.eq_ignore_ascii_case("Cookie") {
        Ok("Cookie")
    } else {
        Err(AppError::bad_request("Unsupported live request header"))
    }
}

fn header_value(value: &str) -> Result<HeaderValue, AppError> {
    if value.len() > MAX_HEADERS || value.chars().any(char::is_control) {
        return Err(AppError::bad_request("Invalid live request header value"));
    }
    HeaderValue::from_str(value)
        .map_err(|_| AppError::bad_request("Invalid live request header value"))
}

pub(crate) fn live_headers(raw: &str) -> Result<HeaderMap, AppError> {
    let invalid = || AppError::bad_request("Invalid live request headers");
    if raw.len() > MAX_HEADERS {
        return Err(invalid());
    }
    let values: BTreeMap<String, String> = serde_json::from_str(raw).map_err(|_| invalid())?;
    let mut headers = HeaderMap::with_capacity(values.len());
    for (name, value) in values {
        let key = HeaderName::from_bytes(header_key(&name)?.as_bytes()).expect("known header name");
        if headers.insert(key, header_value(&value)?).is_some() {
            return Err(invalid());
        }
    }
    Ok(headers)
}

pub(super) fn is_hls(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    path.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("m3u8"))
}

pub(super) fn catchup_hls(template: &str) -> Option<bool> {
    let path = template.split(['?', '#']).next()?;
    let extension = path.rsplit('/').next()?.rsplit_once('.')?.1;
    if extension.eq_ignore_ascii_case("m3u8") {
        Some(true)
    } else if ["ts", "flv", "mp4"]
        .iter()
        .any(|e| extension.eq_ignore_ascii_case(e))
    {
        Some(false)
    } else {
        None
    }
}

#[derive(Default)]
struct Metadata<'a> {
    name: &'a str,
    group: &'a str,
    logo: &'a str,
    tvg_id: &'a str,
    catchup_mode: &'a str,
    catchup_source: &'a str,
    headers: Headers<'a>,
}

impl<'a> Metadata<'a> {
    fn extinf(info: &'a str) -> Result<Self, AppError> {
        let invalid = || AppError::bad_request("Invalid M3U channel metadata");
        let mut quoted = false;
        let comma = info
            .char_indices()
            .find_map(|(i, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                (c == ',' && !quoted).then_some(i)
            })
            .ok_or_else(invalid)?;
        let mut attributes = info[..comma].trim();
        let mut meta = Self::default();
        // The first token is EXTINF's duration, followed by space-separated attributes.
        attributes = attributes.trim_start_matches(|c: char| !c.is_whitespace());
        while !attributes.trim_start().is_empty() {
            attributes = attributes.trim_start();
            let end = attributes
                .find(|c: char| c == '=' || c.is_whitespace())
                .unwrap_or(attributes.len());
            let key = &attributes[..end];
            attributes = attributes[end..].trim_start();
            let Some(rest) = attributes.strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim_start();
            let (value, tail) = if let Some(rest) = rest.strip_prefix('"') {
                rest.split_once('"').ok_or_else(invalid)?
            } else {
                rest.split_once(char::is_whitespace).unwrap_or((rest, ""))
            };
            match key {
                "tvg-id" => meta.tvg_id = value,
                "tvg-name" => meta.name = value,
                "tvg-logo" => meta.logo = value,
                "group-title" => meta.group = value,
                "catchup" => meta.catchup_mode = value,
                "catchup-source" => meta.catchup_source = value,
                _ => {}
            }
            attributes = tail;
        }
        let title = info[comma + 1..].trim();
        if !title.is_empty() {
            meta.name = title;
        }
        Ok(meta)
    }

    fn vlc_option(&mut self, option: &'a str) -> Result<(), AppError> {
        let Some((key, value)) = option.split_once('=') else {
            return Ok(());
        };
        let key = match key {
            "http-user-agent" => "User-Agent",
            "http-referrer" | "http-referer" => "Referer",
            "http-cookie" => "Cookie",
            _ => return Ok(()),
        };
        header_value(value)?;
        self.headers.insert(key, value.into());
        Ok(())
    }
}

fn stream_lines(input: &str) -> impl Iterator<Item = &str> {
    // A URL fragment or an encoded '#' in header data is not a line separator.
    let mut remaining = Some(input);
    std::iter::from_fn(move || {
        let current = remaining.take()?;
        for (index, _) in current.match_indices('#') {
            let next = current[index + 1..].trim_start();
            let scheme_len = next.bytes().take_while(u8::is_ascii_alphabetic).count();
            if scheme_len != 0 && next[scheme_len..].starts_with("://") {
                remaining = Some(next);
                return Some(&current[..index]);
            }
        }
        Some(current)
    })
}

pub(crate) fn parse_playlist(text: &str) -> Result<Vec<PlaylistChannel>, AppError> {
    let mut channels = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: Option<Metadata<'_>> = None;
    let mut group = "";
    let mut saw_header = false;
    let mut saw_txt = false;
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}').trim_start();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("#EXTM3U") {
            saw_header = true;
        } else if let Some(info) = line.strip_prefix("#EXTINF:") {
            if pending.is_some() {
                return Err(AppError::bad_request("Missing M3U channel URL"));
            }
            pending = Some(Metadata::extinf(info)?);
        } else if let Some(option) = line.strip_prefix("#EXTVLCOPT:") {
            if let Some(meta) = &mut pending {
                meta.vlc_option(option)?;
            }
        } else if !line.starts_with('#') {
            let (meta, streams) = if let Some(meta) = pending.take() {
                (meta, line)
            } else {
                if let Some((title, marker)) = line.rsplit_once(',')
                    && marker.trim().eq_ignore_ascii_case("#genre#")
                {
                    group = title.trim();
                    continue;
                }
                let (name, streams) = line.split_once(',').ok_or_else(|| {
                    AppError::bad_request("Expected M3U metadata or a TXT channel name and URL")
                })?;
                let (name, streams) = (name.trim(), streams.trim());
                saw_txt = true;
                (
                    Metadata {
                        name,
                        group,
                        ..Default::default()
                    },
                    streams,
                )
            };
            validate_channel_metadata(meta.name, meta.group, meta.tvg_id, meta.logo)?;
            validate_catchup(meta.catchup_mode, meta.catchup_source)?;
            for stream in stream_lines(streams) {
                let stream = stream.trim();
                let (raw_url, params) = stream.split_once('|').unwrap_or((stream, ""));
                let raw_url = raw_url.trim();
                let url = String::from(live_url(raw_url)?);
                if seen.contains(url.as_str()) {
                    continue;
                }
                if channels.len() == 10_000 {
                    return Err(AppError::bad_request(
                        "Import at most 10,000 channel lines at a time",
                    ));
                }
                let mut headers = meta.headers.clone();
                for (key, value) in url::form_urlencoded::parse(params.as_bytes()) {
                    header_value(&value)?;
                    headers.insert(header_key(&key)?, value);
                }
                let headers_json =
                    serde_json::to_string(&headers).expect("string map is serializable");
                if headers_json.len() > MAX_HEADERS {
                    return Err(AppError::bad_request("Live request headers are too large"));
                }
                let key = if raw_url == url {
                    std::borrow::Cow::Borrowed(raw_url)
                } else {
                    std::borrow::Cow::Owned(url.clone())
                };
                seen.insert(key);
                channels.push(PlaylistChannel {
                    name: meta.name.into(),
                    group_name: meta.group.into(),
                    logo: meta.logo.into(),
                    tvg_id: meta.tvg_id.into(),
                    url,
                    headers_json,
                    catchup_mode: meta.catchup_mode.into(),
                    catchup_source: meta.catchup_source.into(),
                });
            }
        }
    }
    if (!saw_header && !saw_txt) || pending.is_some() || channels.is_empty() {
        return Err(AppError::bad_request(
            "Provide a complete M3U or TXT playlist with HTTP(S) channel URLs",
        ));
    }
    Ok(channels)
}
