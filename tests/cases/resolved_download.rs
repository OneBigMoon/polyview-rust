use super::*;
use axum::{Json, extract::Query, response::Redirect, routing::get};
use std::{collections::HashMap, time::Duration};

const LINK: &str = "https://catalog.invalid/watch/2";
const AUTH: &str = "Bearer media-secret";
const MEDIA_COOKIE: &str = "origin=media-cookie";
const TOKEN: &str = "media-token";
const REFERER: &str = "https://video.invalid/private-path";

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, task)
}

async fn parser(app: &TestApp, admin: &str, media: String) -> tokio::task::JoinHandle<()> {
    let (base, server) = serve(Router::new().route("/resolve", get(move |Query(q): Query<HashMap<String,String>>, h: HeaderMap| {
        let media = media.clone();
        async move {
            assert_eq!(q.get("url").unwrap(), LINK);
            assert_eq!(h["x-parser-key"], "parser-secret");
            Json(json!({"data":{"url":media,"headers": {
                "Authorization": AUTH, "Cookie": MEDIA_COOKIE, "X-Playback-Token":TOKEN, "Referer":REFERER
            }}}))
        }
    }))).await;
    let created = app
        .request(
            "POST",
            "/api/parsers",
            Some(admin),
            Some(json!({
                "name":"Offline", "url_template": format!("{base}/resolve"), "flags":"line",
                "headers_json": r#"{"x-parser-key":"parser-secret"}"#
            })),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    server
}

fn assert_authenticated(h: &HeaderMap) {
    assert_eq!(h[header::AUTHORIZATION], AUTH);
    assert_eq!(h[header::COOKIE], MEDIA_COOKIE);
    assert_eq!(h["x-playback-token"], TOKEN);
    assert_eq!(h[header::REFERER], REFERER);
    assert!(!h.contains_key("x-parser-key"));
}

fn public_response(reply: &Reply) -> &str {
    let text = std::str::from_utf8(&reply.body).unwrap();
    for secret in [
        AUTH,
        TOKEN,
        MEDIA_COOKIE,
        REFERER,
        "parser-secret",
        "signed-secret",
    ] {
        assert!(!text.contains(secret));
    }
    text
}

#[tokio::test]
async fn resolved_download_requires_parser_credentials_and_publishes_library_file() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let content: &'static [u8] = b"offline media payload";
    let (base, media_server) = serve(
        Router::new()
            .route(
                "/signed.mp4",
                get(
                    move |h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                        assert_authenticated(&h);
                        assert_eq!(q.get("token").unwrap(), "signed-secret");
                        ([(header::CONTENT_TYPE, "video/mp4")], content)
                    },
                ),
            )
            .route(
                "/no-extension",
                get(move || async move { ([(header::CONTENT_TYPE, "video/mp4")], content) }),
            ),
    )
    .await;
    let parser_server = parser(
        &app,
        &admin,
        format!("{base}/signed.mp4?token=signed-secret"),
    )
    .await;
    let missing = app
        .request(
            "POST",
            "/api/downloads",
            Some(&viewer),
            Some(json!({"url": LINK,"title":"No parser", "direct":false, "flag":"unknown"})),
        )
        .await;
    assert!(!missing.status.is_success());
    assert_eq!(
        app.request("GET", "/api/downloads", Some(&viewer), None)
            .await
            .json(),
        json!([])
    );
    for body in [
        json!({"url": LINK,"title":"Film", "direct":false,"flag":"line","year":"1997","description":"Episode"}),
        json!({"url": format!("{base}/no-extension"),"title":"File"}),
    ] {
        let created = app
            .request("POST", "/api/downloads", Some(&viewer), Some(body))
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        public_response(&created);
    }
    let worker = polyview::network::start_worker(app.state.clone());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let reply = app
                .request("GET", "/api/downloads", Some(&viewer), None)
                .await;
            public_response(&reply);
            let jobs = reply.json();
            let jobs = jobs.as_array().unwrap();
            assert_eq!(jobs.len(), 2);
            assert!(!jobs.iter().any(|j| j["status"] == "failed"), "{jobs:?}");
            if jobs.iter().all(|j| j["status"] == "completed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let library = app
        .request("GET", "/api/library", Some(&viewer), None)
        .await
        .json();
    assert_eq!(library.as_array().unwrap().len(), 2);
    let item = library
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title"] == "Film")
        .unwrap();
    assert_eq!(item["description"], "Episode");
    assert_eq!(item["year"], "1997");
    let stream = app
        .request(
            "GET",
            &format!("/api/library/{}/stream", item["id"]),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(stream.status, StatusCode::OK);
    assert_eq!(stream.body, content);
    worker.abort();
    parser_server.abort();
    media_server.abort();
}

#[tokio::test]
async fn nested_grants_protect_headers_and_signed_uris_across_origins_and_relay_ranges() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let (cdn, cdn_server) = serve(Router::new().route(
        "/segment.ts",
        get(|h: HeaderMap| async move {
            for name in [
                "cookie",
                "authorization",
                "referer",
                "x-playback-token",
                "x-parser-key",
            ] {
                assert!(!h.contains_key(name), "unexpected {name}");
            }
            assert_eq!(h[header::RANGE], "bytes=2-4");
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "video/mp2t"),
                    (header::CONTENT_RANGE, "bytes 2-4/10"),
                ],
                "234",
            )
        }),
    ))
    .await;
    let target = format!("{cdn}/segment.ts");
    let direct = target.clone();
    let (media, media_server) = serve(Router::new()
        .route("/master.m3u8", get(|h: HeaderMap| async move {
            assert_authenticated(&h);
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000\nvariant.m3u8?token=signed-secret\n"
        }))
        .route("/variant.m3u8", get(move |h: HeaderMap| {let direct=direct.clone(); async move {
            assert_authenticated(&h);
            format!("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?token=signed-secret\"\n#EXTINF:4,\nredirect.ts?token=signed-secret\n#EXTINF:4,\n{direct}?token=signed-secret\n#EXT-X-ENDLIST\n")
        }}))
        .route("/key.bin", get(|h: HeaderMap| async move {assert_authenticated(&h); ([(header::CONTENT_TYPE,"application/octet-stream")],"0123456789abcdef")}))
        .route("/redirect.ts", get(move |h: HeaderMap| {let target=target.clone(); async move {assert_authenticated(&h); Redirect::temporary(&target)}}))).await;
    let parser_server = parser(
        &app,
        &admin,
        format!("{media}/master.m3u8?token=signed-secret"),
    )
    .await;
    let result = app
        .request(
            "POST",
            "/api/playback/resolve",
            Some(&viewer),
            Some(json!({"url": LINK,"flag":"line"})),
        )
        .await;
    assert_eq!(result.status, StatusCode::OK);
    public_response(&result);
    let resolved = result.json();
    let root = resolved["playback_url"].as_str().unwrap();
    let master = app.request("GET", root, Some(&viewer), None).await;
    assert_eq!(master.status, StatusCode::OK);
    let variant = public_response(&master)
        .lines()
        .find(|line| line.starts_with('/'))
        .unwrap();
    let denied = app.request("GET", variant, Some(&admin), None).await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND);
    let manifest = app.request("GET", variant, Some(&viewer), None).await;
    assert_eq!(manifest.status, StatusCode::OK);
    let manifest = public_response(&manifest);
    let key = manifest
        .split("URI=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let key_response = app.request("GET", key, Some(&viewer), None).await;
    assert_eq!(key_response.body, b"0123456789abcdef");
    let segments: Vec<_> = manifest
        .lines()
        .filter(|line| line.starts_with('/'))
        .collect();
    assert_eq!(segments.len(), 2);
    for segment in segments {
        assert_eq!(
            app.request("GET", segment, None, None).await.status,
            StatusCode::UNAUTHORIZED
        );
        let reply = app
            .request_with_headers(
                "GET",
                segment,
                Some(&viewer),
                None,
                &[("range", "bytes=2-4"), ("authorization", "client-secret")],
            )
            .await;
        assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(reply.headers[header::CONTENT_RANGE], "bytes 2-4/10");
        assert_eq!(reply.body, b"234");
    }
    parser_server.abort();
    media_server.abort();
    cdn_server.abort();
}
