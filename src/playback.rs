mod grants;
mod live;

pub use grants::PlaybackGrants;
pub(crate) use grants::forward_grant;
pub(crate) use grants::{DownloadRelay, fetch_media, grant_url, no_redirect_client};
pub(crate) use live::{CatchupQuery, channel_logo, line_stream};

use crate::{AppError, Shared, auth::AuthUser, settings::play_settings};
use axum::{
    Router,
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use std::{io, time::Duration};
use url::Url;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PLAYLIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/playback", get(proxy))
        .route("/api/playback/grants/{token}", get(grants::proxy))
        .route("/api/channels/lines/{id}/stream", get(live::stream))
        .route("/api/channels/{id}/logo", get(live::logo))
        .route("/api/image", get(image))
}

#[derive(Deserialize)]
struct PlaybackQuery {
    url: String,
}

fn http_url(raw: &str) -> Result<Url, AppError> {
    if raw.len() > 16_384 || raw.chars().any(char::is_control) {
        return Err(AppError::bad_request(
            "Playback URL is too long or contains control characters",
        ));
    }
    let mut url = Url::parse(raw).map_err(|_| AppError::bad_request("Invalid playback URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(AppError::bad_request(
            "Playback requires an HTTP(S) URL without embedded credentials",
        ));
    }
    url.set_fragment(None);
    Ok(url)
}

fn source_error(message: impl Into<String>) -> AppError {
    AppError(StatusCode::BAD_GATEWAY, message.into())
}

fn playlist_url(base: &Url, raw: &str) -> Result<Url, AppError> {
    let resolved = base
        .join(raw)
        .map_err(|_| source_error("Playlist contains an invalid media URL"))?;
    http_url(resolved.as_str())
        .map_err(|_| source_error("Playlist contains an unsupported media URL"))
}

fn proxy_url(base: &Url, raw: &str) -> Result<String, AppError> {
    let url = playlist_url(base, raw)?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("url", url.as_str())
        .finish();
    Ok(format!("/api/playback?{query}"))
}

fn cue_out(line: &str) -> bool {
    line == "#EXT-X-CUE-OUT" || line.starts_with("#EXT-X-CUE-OUT:")
}

fn playlist_path(url: &Url) -> bool {
    url.path()
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("m3u8"))
}

fn rewrite_playlist(bytes: &[u8], base: &Url, ad_block: bool) -> Result<String, AppError> {
    rewrite_links(bytes, ad_block, false, |raw| proxy_url(base, raw))
}

fn playback_tag(line: &str) -> bool {
    let tag = line.split(':').next().unwrap_or(line);
    matches!(
        tag,
        "#EXTM3U"
            | "#EXT-X-VERSION"
            | "#EXT-X-INDEPENDENT-SEGMENTS"
            | "#EXT-X-START"
            | "#EXT-X-MEDIA"
            | "#EXT-X-STREAM-INF"
            | "#EXT-X-I-FRAME-STREAM-INF"
            | "#EXT-X-SESSION-KEY"
            | "#EXT-X-TARGETDURATION"
            | "#EXT-X-MEDIA-SEQUENCE"
            | "#EXT-X-DISCONTINUITY-SEQUENCE"
            | "#EXT-X-ENDLIST"
            | "#EXT-X-PLAYLIST-TYPE"
            | "#EXT-X-I-FRAMES-ONLY"
            | "#EXT-X-BYTERANGE"
            | "#EXT-X-DISCONTINUITY"
            | "#EXT-X-KEY"
            | "#EXT-X-MAP"
            | "#EXT-X-PROGRAM-DATE-TIME"
            | "#EXT-X-GAP"
            | "#EXT-X-BITRATE"
            | "#EXT-X-PART-INF"
            | "#EXT-X-SERVER-CONTROL"
            | "#EXT-X-PART"
            | "#EXT-X-PRELOAD-HINT"
            | "#EXT-X-RENDITION-REPORT"
            | "#EXT-X-SKIP"
    )
}

fn rewrite_links(
    bytes: &[u8],
    ad_block: bool,
    private: bool,
    mut link: impl FnMut(&str) -> Result<String, AppError>,
) -> Result<String, AppError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| source_error("HLS playlist is not valid UTF-8"))?;
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.lines().next().map(str::trim) != Some("#EXTM3U") {
        return Err(source_error("Source did not return an HLS playlist"));
    }
    let mut output = String::with_capacity(text.len());
    let mut lines = text.lines();
    let mut preserve = 0;
    while let Some(raw_line) = lines.next() {
        if output.len() > MAX_PLAYLIST_BYTES * 4 {
            return Err(source_error("Rewritten HLS playlist is too large"));
        }
        let line = raw_line.trim();
        if preserve > 0 {
            preserve -= 1;
        } else if ad_block && cue_out(line) {
            // Commit the skip only after finding a single unambiguous cue pair.
            let mut ahead = lines.clone();
            let mut depth = 1;
            let mut nested = false;
            for candidate in ahead.by_ref() {
                preserve += 1;
                let candidate = candidate.trim();
                if cue_out(candidate) {
                    depth += 1;
                    nested = true;
                } else if candidate == "#EXT-X-CUE-IN" {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            if depth == 0 && !nested {
                lines = ahead;
                preserve = 0;
                continue;
            }
        }
        if line.is_empty() {
            output.push('\n');
            continue;
        }
        if !line.starts_with('#') {
            output.push_str(&link(line)?);
        } else {
            if private {
                // Optional metadata and IPTV player options can contain signed URLs or headers.
                if let Some(duration) = line.strip_prefix("#EXTINF:") {
                    let duration = duration.split(',').next().unwrap_or_default().trim();
                    if !duration
                        .bytes()
                        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-'))
                    {
                        return Err(source_error("Invalid protected playlist duration"));
                    }
                    output.push_str("#EXTINF:");
                    output.push_str(duration);
                    output.push_str(",\n");
                    continue;
                }
                if !playback_tag(line) {
                    continue;
                }
            }
            let tag_start = output.len();
            let mut rest = line;
            while let Some(start) = rest.find("URI=\"") {
                let value_start = start + 5;
                let end = rest[value_start..]
                    .find('"')
                    .map(|offset| value_start + offset)
                    .ok_or_else(|| {
                        source_error("Playlist contains an unterminated URI attribute")
                    })?;
                output.push_str(&rest[..value_start]);
                output.push_str(&link(&rest[value_start..end])?);
                output.push('"');
                if output.len() > MAX_PLAYLIST_BYTES * 4 {
                    return Err(source_error("Rewritten HLS playlist is too large"));
                }
                rest = &rest[end + 1..];
            }
            output.push_str(rest);
            if private && output[tag_start..].contains("://") {
                return Err(source_error(
                    "Unsupported external reference in protected playlist",
                ));
            }
        }
        output.push('\n');
    }
    Ok(output)
}

fn response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in [
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::CONTENT_LENGTH,
    ] {
        if let Some(value) = upstream.get(&name) {
            headers.insert(name, value.clone());
        }
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers
}

fn binary_content_type(upstream: &HeaderMap) -> Result<HeaderValue, AppError> {
    let Some(raw) = upstream.get(header::CONTENT_TYPE) else {
        return Ok(HeaderValue::from_static("application/octet-stream"));
    };
    let content_type = raw
        .to_str()
        .map_err(|_| source_error("Source returned an invalid media type"))?
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if content_type.starts_with("video/")
        || content_type.starts_with("audio/")
        || matches!(
            content_type.as_str(),
            "application/octet-stream"
                | "binary/octet-stream"
                | "application/mp4"
                | "application/x-mpegts"
                | "application/x-flv"
        )
    {
        return HeaderValue::from_str(&content_type)
            .map_err(|_| source_error("Source returned an invalid media type"));
    }
    if content_type == "text/plain" {
        return Ok(HeaderValue::from_static("application/octet-stream"));
    }
    // Some HLS CDNs label MPEG-TS segments as JPEG; browsers ignore the
    // segment type, so relay the bytes as opaque binary.
    if content_type == "image/jpeg" {
        return Ok(HeaderValue::from_static("application/octet-stream"));
    }
    Err(source_error(
        "Source returned a document instead of playable media",
    ))
}

fn configured_header(value: &str) -> Option<HeaderValue> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    HeaderValue::from_str(value).ok()
}

async fn image(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<PlaybackQuery>,
) -> Result<Response, AppError> {
    let url = http_url(&query.url)?;
    let settings = play_settings(&*state.conn()?)?;
    let referer = configured_header(&settings.image_referer)
        .or_else(|| configured_header(&url[..url::Position::BeforePath]))
        .ok_or_else(|| source_error("Invalid image origin"))?;
    let upstream = state
        .client
        .get(url)
        .header(header::REFERER, referer)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| source_error("Could not connect to the image source"))?;
    raster_response(upstream).await
}

async fn raster_response(upstream: reqwest::Response) -> Result<Response, AppError> {
    if upstream.status() != StatusCode::OK {
        return Err(source_error("Image source returned an unsuccessful status"));
    }
    let media_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim();
    // Use canonical raster MIME types so document types and upstream parameters never reach a browser.
    let raster_type = [
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "image/avif",
        "image/bmp",
        "image/x-icon",
        "image/vnd.microsoft.icon",
        "image/apng",
        "image/tiff",
        "image/jxl",
        "image/heic",
        "image/heif",
    ]
    .into_iter()
    .find(|candidate| media_type.eq_ignore_ascii_case(candidate))
    .ok_or_else(|| source_error("Image source did not return a supported raster image"))?;
    if upstream
        .content_length()
        .is_some_and(|len| len > MAX_IMAGE_BYTES as u64)
    {
        return Err(source_error("Image exceeds the 8 MiB limit"));
    }
    let bytes = axum::body::to_bytes(Body::from_stream(upstream.bytes_stream()), MAX_IMAGE_BYTES)
        .await
        .map_err(|_| source_error("Image transfer failed or exceeded the 8 MiB limit"))?;
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(raster_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn proxy(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<PlaybackQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let url = http_url(&query.url)?;
    let requested_playlist = playlist_path(&url);
    let settings = play_settings(&*state.conn()?)?;
    let mut request = state
        .client
        .get(url)
        .header(header::ACCEPT_ENCODING, "identity");
    for (name, value) in [
        (header::REFERER, &settings.play_referer),
        (header::USER_AGENT, &settings.play_ua),
    ] {
        if let Some(value) = configured_header(value) {
            request = request.header(name, value);
        }
    }
    for name in [header::RANGE, header::IF_RANGE] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value.clone());
        }
    }
    let upstream = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
        .await
        .map_err(|_| source_error("Media source did not respond within 30 seconds"))?
        .map_err(|_| source_error("Could not connect to the media source"))?;
    relay_response(upstream, requested_playlist, |bytes, base| {
        rewrite_playlist(bytes, base, settings.ad_block)
    })
    .await
}

async fn relay_response(
    upstream: reqwest::Response,
    requested_playlist: bool,
    playlist: impl FnOnce(&[u8], &Url) -> Result<String, AppError>,
) -> Result<Response, AppError> {
    let final_url = http_url(upstream.url().as_str())
        .map_err(|_| source_error("Media source redirected to an unsupported URL"))?;
    let status = upstream.status();
    let mut downstream_headers = response_headers(upstream.headers());
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        downstream_headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
        let mut response = Response::new(Body::empty());
        *response.status_mut() = status;
        *response.headers_mut() = downstream_headers;
        return Ok(response);
    }
    if !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        return Err(source_error(format!(
            "Media source returned HTTP {}",
            status.as_u16()
        )));
    }
    if upstream
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
    {
        return Err(source_error(
            "Media source ignored the uncompressed transfer request",
        ));
    }
    let is_playlist = requested_playlist
        || playlist_path(&final_url)
        || upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| content_type.to_ascii_lowercase().contains("mpegurl"));
    if is_playlist {
        if status == StatusCode::PARTIAL_CONTENT {
            return Err(source_error("A partial HLS playlist cannot be played"));
        }
        if upstream
            .content_length()
            .is_some_and(|length| length > MAX_PLAYLIST_BYTES as u64)
        {
            return Err(source_error("HLS playlist exceeds the 2 MiB limit"));
        }
        let collect = async {
            let mut stream = upstream.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| source_error("HLS playlist transfer failed"))?;
                if bytes.len().saturating_add(chunk.len()) > MAX_PLAYLIST_BYTES {
                    return Err(source_error("HLS playlist exceeds the 2 MiB limit"));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok::<_, AppError>(bytes)
        };
        let bytes = tokio::time::timeout(REQUEST_TIMEOUT, collect)
            .await
            .map_err(|_| source_error("HLS playlist transfer timed out"))??;
        let rewritten = playlist(&bytes, &final_url)?;
        downstream_headers.remove(header::CONTENT_RANGE);
        downstream_headers.remove(header::ACCEPT_RANGES);
        downstream_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/vnd.apple.mpegurl; charset=utf-8"),
        );
        downstream_headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&rewritten.len().to_string())
                .map_err(|_| source_error("Invalid rewritten playlist length"))?,
        );
        let mut response = Response::new(Body::from(rewritten));
        *response.headers_mut() = downstream_headers;
        return Ok(response);
    }
    downstream_headers.insert(
        header::CONTENT_TYPE,
        binary_content_type(upstream.headers())?,
    );
    let stream = stream::unfold(
        (upstream.bytes_stream(), false),
        |(mut stream, finished)| async move {
            if finished {
                return None;
            }
            match tokio::time::timeout(REQUEST_TIMEOUT, stream.next()).await {
                Ok(Some(Ok(bytes))) => Some((Ok(bytes), (stream, false))),
                Ok(None) => None,
                Ok(Some(Err(_))) => Some((
                    Err(io::Error::other("Media source connection interrupted")),
                    (stream, true),
                )),
                Err(_) => Some((
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Media source stalled for 30 seconds",
                    )),
                    (stream, true),
                )),
            }
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = downstream_headers;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use axum::{body::to_bytes, http::Request};
    use rusqlite::Connection;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn state() -> (TempDir, Shared) {
        let temp = tempfile::tempdir().unwrap();
        let media_dir = temp.path().canonicalize().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        crate::init_schema(&conn).unwrap();
        conn.execute("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(1,'user','unused','user',0)", []).unwrap();
        conn.execute(
            "INSERT INTO sessions(token,user_id,expires_at) VALUES('local-session',1,?1)",
            [crate::now() + 3600],
        )
        .unwrap();
        (
            temp,
            Arc::new(AppState {
                db: Mutex::new(conn),
                playback_grants: Default::default(),
                telegram_native: Default::default(),
                download_runtime: Default::default(),
                media_dir: media_dir.clone(),
                download_dir: media_dir,
                client: reqwest::Client::new(),
                secure_cookie: false,
            }),
        )
    }

    fn request(url: &str, authenticated: bool) -> axum::http::request::Builder {
        proxy_request("/api/playback", url, authenticated)
    }

    fn proxy_request(path: &str, url: &str, authenticated: bool) -> axum::http::request::Builder {
        let uri = format!(
            "{path}?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("url", url)
                .finish()
        );
        let mut request = Request::builder().uri(uri);
        if authenticated {
            request = request.header(header::COOKIE, "polyview_session=local-session");
        }
        request
    }

    #[test]
    fn ad_block_removes_only_unambiguous_closed_cue_blocks() {
        let base = Url::parse("https://example.org/list.m3u8").unwrap();
        let before = "#EXTM3U\n#EXT-X-CUE-OUT-CONT:6/30\n#EXTINF:4,\nbefore.ts\n";
        let ad = "#EXT-X-CUE-OUT:8\n#EXTINF:4,\nad1.ts\n#EXTINF:4,\nad2.ts\n#EXT-X-CUE-IN\n";
        let after = "#EXTINF:4,\nafter.ts\n#EXT-X-CUE-OUT\n#EXTINF:4,\nopen.ts\n";
        let playlist = format!("{before}{ad}{ad}{after}");
        let expected = format!("{before}{after}");
        assert_eq!(
            rewrite_playlist(playlist.as_bytes(), &base, true).unwrap(),
            rewrite_playlist(expected.as_bytes(), &base, false).unwrap(),
        );
        let intact = rewrite_playlist(playlist.as_bytes(), &base, false).unwrap();
        assert_eq!(intact.matches("ad1.ts").count(), 2);
        for body in [
            "#EXT-X-CUE-OUT\nmain.ts\n#EXT-X-CUE-IN-UNRELATED\n",
            "#EXT-X-CUE-OUT\na.ts\n#EXT-X-CUE-OUT\nb.ts\n#EXT-X-CUE-IN\n#EXT-X-CUE-IN\n",
        ] {
            let playlist = format!("#EXTM3U\n{body}");
            assert_eq!(
                rewrite_playlist(playlist.as_bytes(), &base, true).unwrap(),
                rewrite_playlist(playlist.as_bytes(), &base, false).unwrap(),
            );
        }
    }

    #[tokio::test]
    async fn image_proxy_and_playback_use_persisted_settings() {
        let (_temp, state) = state();
        state
            .conn()
            .unwrap()
            .execute("UPDATE users SET role='admin' WHERE id=1", [])
            .unwrap();
        let app = crate::router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let expected_origin = origin.clone();
        let upstream = Router::new()
            .route(
                "/cover",
                get(move |headers: HeaderMap| async move {
                    assert_eq!(headers[header::REFERER].to_str().unwrap(), expected_origin);
                    assert!(!headers.contains_key(header::COOKIE));
                    (
                        [(header::CONTENT_TYPE, "image/png")],
                        b"\x89PNG\r\n\x1a\n".as_slice(),
                    )
                }),
            )
            .route(
                "/custom",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers[header::REFERER], "https://posters.test/");
                    (
                        [(header::CONTENT_TYPE, "image/jpeg")],
                        b"\xff\xd8\xff\xd9".as_slice(),
                    )
                }),
            )
            .route(
                "/html",
                get(|| async { axum::response::Html("<p>document</p>") }),
            )
            .route(
                "/svg",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "Image/SVG+XML; charset=utf-8")],
                        "<svg/>",
                    )
                }),
            )
            .route(
                "/chunked/{extra}",
                get(
                    |axum::extract::Path(extra): axum::extract::Path<usize>| async move {
                        use axum::body::Bytes;
                        let chunk = Bytes::from_static(&[1; 64 * 1024]);
                        let chunks = (0..MAX_IMAGE_BYTES / chunk.len() + extra)
                            .map(move |_| Ok::<_, io::Error>(chunk.clone()));
                        (
                            [(header::CONTENT_TYPE, "image/png")],
                            Body::from_stream(stream::iter(chunks)),
                        )
                    },
                ),
            )
            .route(
                "/list.m3u8",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers[header::REFERER], "https://play.test/");
                    assert_eq!(headers[header::USER_AGENT], "PolyViewTest/1");
                    "#EXTM3U\n#EXT-X-CUE-OUT:4\nad.ts\n#EXT-X-CUE-IN\nmain.ts\n"
                }),
            );
        let task = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let fetch = |path: &str, upstream: &str, auth| {
            app.clone().oneshot(
                proxy_request(path, upstream, auth)
                    .body(Body::empty())
                    .unwrap(),
            )
        };
        let cover = format!("{origin}/cover");
        assert_eq!(
            fetch("/api/image", &cover, false).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            fetch("/api/image", "file:///cover.png", true)
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let response = fetch("/api/image", &cover, true).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, max-age=86400"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            to_bytes(response.into_body(), 8).await.unwrap().as_ref(),
            b"\x89PNG\r\n\x1a\n"
        );
        for resource in ["html", "svg", "chunked/1"] {
            let response = fetch("/api/image", &format!("{origin}/{resource}"), true)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{resource}");
        }
        let exact = fetch("/api/image", &format!("{origin}/chunked/0"), true)
            .await
            .unwrap();
        assert_eq!(exact.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(exact.into_body(), MAX_IMAGE_BYTES)
                .await
                .unwrap()
                .len(),
            MAX_IMAGE_BYTES
        );

        let settings = serde_json::json!({ "ad_block": true, "play_referer": "https://play.test/", "play_ua": "PolyViewTest/1", "image_referer": "https://posters.test/" });
        let updated = app
            .clone()
            .oneshot(
                Request::put("/api/settings/play")
                    .header(header::COOKIE, "polyview_session=local-session")
                    .header("x-polyview-request", "1")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(settings.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(updated.status().is_success());
        assert_eq!(
            fetch("/api/image", &format!("{origin}/custom"), true)
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let media = fetch("/api/playback", &format!("{origin}/list.m3u8"), true)
            .await
            .unwrap();
        assert_eq!(media.status(), StatusCode::OK);
        let body = to_bytes(media.into_body(), 512).await.unwrap();
        let playlist = std::str::from_utf8(&body).unwrap();
        assert!(playlist.contains("main.ts"));
        assert!(!playlist.contains("ad.ts"));
        task.abort();
    }

    #[test]
    fn mislabeled_image_segments_are_relayed_as_binary() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
        assert_eq!(
            binary_content_type(&headers).unwrap(),
            "application/octet-stream"
        );
    }

    #[test]
    fn playlists_rewrite_relative_paths_attributes_and_query_tokens() {
        let base = Url::parse("https://example.org/hls/master.m3u8?auth=one").unwrap();
        let playlist=rewrite_playlist(b"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../keys/key.bin?token=a&b=2\",IV=0x01\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nsegments/one.ts?token=one%20two\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio/list.m3u8\"\n",&base,false).unwrap();
        assert!(playlist.contains("URI=\"/api/playback?url=https%3A%2F%2Fexample.org%2Fkeys%2Fkey.bin%3Ftoken%3Da%26b%3D2\",IV=0x01"));
        let media_line = playlist
            .lines()
            .find(|line| !line.starts_with('#'))
            .unwrap();
        let query: HashMap<String, String> =
            url::form_urlencoded::parse(media_line.split_once('?').unwrap().1.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(
            query["url"],
            "https://example.org/hls/segments/one.ts?token=one%20two"
        );
        assert!(playlist.contains("audio%2Flist.m3u8"));
        assert!(rewrite_playlist(b"<html>not HLS</html>", &base, false).is_err());
        assert!(
            rewrite_playlist(
                b"#EXTM3U\n#EXT-X-KEY:URI=\"data:text/plain,secret\"\n",
                &base,
                false
            )
            .is_err()
        );
        assert!(
            rewrite_playlist(b"#EXTM3U\n#EXT-X-KEY:URI=\"unterminated\n", &base, false).is_err()
        );
        for raw in [
            "file:///etc/passwd",
            "data:text/plain,test",
            "https://user:pass@example.org/video.mp4",
            "http://example.org/a\r\nHeader:evil",
        ] {
            assert!(http_url(raw).is_err(), "{raw}");
        }
    }

    #[tokio::test]
    async fn authenticated_proxy_streams_ranges_without_forwarding_credentials() {
        let (_temp, state) = state();
        let upstream = Router::new()
            .route(
                "/video.mp4",
                get(|headers: HeaderMap| async move {
                    assert!(!headers.contains_key(header::COOKIE));
                    assert!(!headers.contains_key(header::AUTHORIZATION));
                    assert!(!headers.contains_key("x-private"));
                    assert_eq!(headers[header::RANGE], "bytes=2-5");
                    assert_eq!(headers[header::IF_RANGE], "etag-test");
                    (
                        StatusCode::PARTIAL_CONTENT,
                        [
                            (header::CONTENT_TYPE, "video/mp4"),
                            (header::CONTENT_RANGE, "bytes 2-5/10"),
                            (header::CONTENT_LENGTH, "4"),
                            (header::ACCEPT_RANGES, "bytes"),
                            (header::SET_COOKIE, "remote-cookie=must-not-leak"),
                        ],
                        "2345",
                    )
                }),
            )
            .route(
                "/unavailable.mp4",
                get(|| async {
                    (
                        StatusCode::RANGE_NOT_SATISFIABLE,
                        [(header::CONTENT_RANGE, "bytes */10")],
                        "remote error page",
                    )
                }),
            )
            .route(
                "/page.html",
                get(|| async { axum::response::Html("<script>evil</script>") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let app = router().with_state(state);
        let url = format!("http://{address}/video.mp4");
        let response = app
            .clone()
            .oneshot(request(&url, false).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(
                request(&url, true)
                    .header(header::AUTHORIZATION, "Bearer local-secret")
                    .header("x-private", "secret")
                    .header(header::RANGE, "bytes=2-5")
                    .header(header::IF_RANGE, "etag-test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "4");
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(
            to_bytes(response.into_body(), 100).await.unwrap().as_ref(),
            b"2345"
        );
        let response = app
            .clone()
            .oneshot(
                request(&format!("http://{address}/unavailable.mp4"), true)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
        assert!(
            to_bytes(response.into_body(), 100)
                .await
                .unwrap()
                .is_empty()
        );
        let response = app
            .clone()
            .oneshot(
                request(&format!("http://{address}/page.html"), true)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let response = app
            .oneshot(
                request("file:///etc/passwd", true)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        task.abort();
    }

    #[tokio::test]
    async fn hls_proxy_rewrites_nested_manifests_and_key_requests() {
        let (_temp, state) = state();
        let upstream=Router::new()
            .route("/master.m3u8",get(||async { ([(header::CONTENT_TYPE,"application/vnd.apple.mpegurl")],"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\nsub/variant.m3u8?token=a%26b\n") }))
            .route("/sub/variant.m3u8",get(|Query(query):Query<HashMap<String,String>>|async move {assert_eq!(query.get("token").map(String::as_str),Some("a&b")); ([(header::CONTENT_TYPE,"text/plain")],"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../key.bin?secret=one\"\n#EXTINF:4,\none.ts\n#EXT-X-ENDLIST\n") }))
            .route("/key.bin",get(||async { ([(header::CONTENT_TYPE,"application/octet-stream")],"0123456789abcdef") }))
            .route("/sub/one.ts",get(||async { ([(header::CONTENT_TYPE,"video/mp2t")],"segment-bytes") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let app = router().with_state(state);
        let response = app
            .clone()
            .oneshot(
                request(&format!("http://{address}/master.m3u8"), true)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = String::from_utf8(
            to_bytes(response.into_body(), 10000)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let variant = text
            .lines()
            .find(|line| line.starts_with("/api/playback?"))
            .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(variant)
                    .header(header::COOKIE, "polyview_session=local-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = String::from_utf8(
            to_bytes(response.into_body(), 10000)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let key = text
            .split("URI=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(key)
                    .header(header::COOKIE, "polyview_session=local-session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 100).await.unwrap().as_ref(),
            b"0123456789abcdef"
        );
        task.abort();
    }
}
