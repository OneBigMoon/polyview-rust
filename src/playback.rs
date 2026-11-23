use crate::{AppError, Shared, auth::AuthUser};
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

pub fn router() -> Router<Shared> {
    Router::new().route("/api/playback", get(proxy))
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

fn proxy_url(base: &Url, raw: &str) -> Result<String, AppError> {
    let resolved = base
        .join(raw)
        .map_err(|_| source_error("Playlist contains an invalid media URL"))?;
    let url = http_url(resolved.as_str())
        .map_err(|_| source_error("Playlist contains an unsupported media URL"))?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("url", url.as_str())
        .finish();
    Ok(format!("/api/playback?{query}"))
}

fn rewrite_playlist(bytes: &[u8], base: &Url) -> Result<String, AppError> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| source_error("HLS playlist is not valid UTF-8"))?;
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.lines().next().map(str::trim) != Some("#EXTM3U") {
        return Err(source_error("Source did not return an HLS playlist"));
    }
    let mut output = String::with_capacity(text.len());
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            output.push('\n');
            continue;
        }
        if !line.starts_with('#') {
            output.push_str(&proxy_url(base, line)?);
        } else {
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
                output.push_str(&proxy_url(base, &rest[value_start..end])?);
                output.push('"');
                rest = &rest[end + 1..];
            }
            output.push_str(rest);
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
        )
    {
        return Ok(raw.clone());
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

async fn proxy(
    State(state): State<Shared>,
    _user: AuthUser,
    Query(query): Query<PlaybackQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let url = http_url(&query.url)?;
    let mut request = state
        .client
        .get(url)
        .header(header::ACCEPT_ENCODING, "identity");
    for name in [header::RANGE, header::IF_RANGE] {
        if let Some(value) = headers.get(&name) {
            request = request.header(name, value.clone());
        }
    }
    let upstream = tokio::time::timeout(REQUEST_TIMEOUT, request.send())
        .await
        .map_err(|_| source_error("Media source did not respond within 30 seconds"))?
        .map_err(|_| source_error("Could not connect to the media source"))?;
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
    let playlist = final_url.path().to_ascii_lowercase().ends_with(".m3u8")
        || upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| content_type.to_ascii_lowercase().contains("mpegurl"));
    if playlist {
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
        let rewritten = rewrite_playlist(&bytes, &final_url)?;
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
    use crate::{AppState, auth};
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
        auth::init_schema(&conn).unwrap();
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
                media_dir: media_dir.clone(),
                download_dir: media_dir,
                client: reqwest::Client::new(),
                secure_cookie: false,
            }),
        )
    }

    fn request(url: &str, authenticated: bool) -> axum::http::request::Builder {
        let uri = format!(
            "/api/playback?{}",
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
        let playlist=rewrite_playlist(b"#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"../keys/key.bin?token=a&b=2\",IV=0x01\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4,\nsegments/one.ts?token=one%20two\n#EXT-X-MEDIA:TYPE=AUDIO,URI=\"audio/list.m3u8\"\n",&base).unwrap();
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
        assert!(rewrite_playlist(b"<html>not HLS</html>", &base).is_err());
        assert!(
            rewrite_playlist(
                b"#EXTM3U\n#EXT-X-KEY:URI=\"data:text/plain,secret\"\n",
                &base
            )
            .is_err()
        );
        assert!(rewrite_playlist(b"#EXTM3U\n#EXT-X-KEY:URI=\"unterminated\n", &base).is_err());
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
