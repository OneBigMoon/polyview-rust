use super::*;
use axum::{
    extract::Query,
    http::Uri,
    response::{IntoResponse, Redirect},
    routing::get,
};
use std::collections::HashMap;

const LINE: &str = "/api/channels/lines/1/stream";
const COOKIE: &str = "station=private-cookie";
const AGENT: &str = "private-station-player";
const REFERER: &str = "https://station.invalid/private-referer";

async fn serve(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    (
        base,
        tokio::spawn(async move { axum::serve(socket, router).await.unwrap() }),
    )
}

fn add_line(app: &TestApp, url: &str, template: &str, logo: &str) {
    let conn = app.state.conn().unwrap();
    conn.execute(
        "INSERT INTO channels(name,group_name,logo,url) VALUES('News','Tests',?1,?2)",
        [logo, url],
    )
    .unwrap();
    conn.execute("INSERT INTO channel_sources(channel_id,url,headers_json,catchup_mode,catchup_source) VALUES(last_insert_rowid(),?1,?2,'default',?3)",
        rusqlite::params![url, json!({"User-Agent":AGENT,"Cookie":COOKIE,"Referer":REFERER}).to_string(), template]).unwrap();
}

fn private_headers(h: &HeaderMap) {
    assert_eq!(h[header::COOKIE], COOKIE);
    assert_eq!(h[header::USER_AGENT], AGENT);
    assert_eq!(h[header::REFERER], REFERER);
}

fn public_text(reply: &Reply) -> &str {
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    let text = std::str::from_utf8(&reply.body).unwrap();
    for secret in [
        COOKIE,
        AGENT,
        REFERER,
        "private-signature",
        "127.0.0.1",
        "private-directory",
    ] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
    text
}

#[tokio::test]
async fn token_and_member_live_streams_rewrite_every_child_and_revalidate_its_audience() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let (cdn, cdn_server) = serve(Router::new().route(
        "/segment.ts",
        get(|h: HeaderMap| async move {
            assert!(!h.contains_key(header::COOKIE));
            assert!(!h.contains_key(header::REFERER));
            assert_ne!(h[header::USER_AGENT], AGENT);
            assert_eq!(h[header::RANGE], "bytes=2-3");
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "video/mp2t"),
                    (header::CONTENT_RANGE, "bytes 2-3/6"),
                ],
                "cd",
            )
        }),
    ))
    .await;
    let (station, station_server) = serve(Router::new()
        .route("/master.m3u8",get(|h: HeaderMap| async move {
            private_headers(&h);
            "#EXTM3U\n#EXTVLCOPT:http-user-agent=private-station-player\n#EXT-X-STREAM-INF:BANDWIDTH=100\nprivate-directory/list.m3u8?k=private-signature\n"
        }))
        .route("/private-directory/list.m3u8",get(|h: HeaderMap| async move {
            private_headers(&h);
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin?k=private-signature\"\n#EXTINF:4,private-signature\nsegment.ts?k=private-signature\n"
        }))
        .route("/private-directory/key.bin",get(|h: HeaderMap| async move {
            private_headers(&h);
            ([(header::CONTENT_TYPE,"application/octet-stream")],"0123456789abcdef")
        }))
        .route("/private-directory/segment.ts",get(move |h: HeaderMap| {
            let target = format!("{cdn}/segment.ts");
            async move { private_headers(&h); Redirect::temporary(&target) }
        }))
        .route("/logo",get(|Query(q):Query<HashMap<String,String>>| async move {
            assert_eq!(q["key"], "private-signature");
            ([(header::CONTENT_TYPE,"image/png")],b"\x89PNG\r\n\x1a\n".as_slice())
        }))).await;
    add_line(
        &app,
        &format!("{station}/master.m3u8?k=private-signature"),
        "?replay={utc}",
        &format!("{station}/logo?key=private-signature"),
    );
    let feed = app
        .request("GET", "/api/tvbox", Some(&viewer), None)
        .await
        .json();
    let token = feed["token"].as_str().unwrap();
    let feed_path = feed["live_path"].as_str().unwrap();
    let feed = app
        .request_with_headers("GET", feed_path, None, None, &[("host", "nas.local")])
        .await;
    let list = public_text(&feed);
    let tvroot = format!("/tvbox/{token}/lines/1/stream");
    assert!(
        list.lines()
            .any(|line| line == format!("http://nas.local{tvroot}"))
    );
    let logo = format!("/tvbox/{token}/channels/1/logo");
    assert!(list.contains(&format!("tvg-logo=\"http://nas.local{logo}\"")));
    assert!(list.contains("?start={utc}&end={utcend}"));
    let image = app.request("GET", &logo, None, None).await;
    assert_eq!(image.status, StatusCode::OK);
    assert_eq!(image.headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(
        app.request("GET", LINE, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    for (root, auth, prefix) in [
        (
            LINE,
            Some(viewer.as_str()),
            "/api/playback/grants/".to_owned(),
        ),
        (tvroot.as_str(), None, format!("/tvbox/{token}/grants/")),
    ] {
        let master = app.request("GET", root, auth, None).await;
        let child = public_text(&master)
            .lines()
            .find(|line| line.starts_with('/'))
            .unwrap();
        assert!(child.starts_with(&prefix));
        let reloaded = app.request("GET", root, auth, None).await;
        assert_eq!(public_text(&master), public_text(&reloaded));
        let other_scope = if auth.is_some() {
            format!("/tvbox/{token}/grants/")
        } else {
            "/api/playback/grants/".into()
        };
        let crossed = format!("{other_scope}{}", child.strip_prefix(&prefix).unwrap());
        assert_eq!(
            app.request("GET", &crossed, Some(&viewer), None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        let variant = app.request("GET", child, auth, None).await;
        let text = public_text(&variant);
        let key = text
            .split_once("URI=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap();
        assert!(key.starts_with(&prefix));
        assert_eq!(
            app.request("GET", key, auth, None).await.body,
            b"0123456789abcdef"
        );
        let segment = text.lines().find(|l| l.starts_with('/')).unwrap();
        let data = app
            .request_with_headers("GET", segment, auth, None, &[("range", "bytes=2-3")])
            .await;
        assert_eq!(data.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(data.headers[header::CONTENT_RANGE], "bytes 2-3/6");
        assert_eq!(data.body, b"cd");
        if auth.is_none() {
            assert_eq!(
                app.request("POST", "/api/tvbox/token", Some(&viewer), None)
                    .await
                    .status,
                StatusCode::OK
            );
            for path in [root, child, key, segment, &logo] {
                assert_eq!(
                    app.request("GET", path, None, None).await.status,
                    StatusCode::NOT_FOUND
                );
            }
        }
    }
    station_server.abort();
    cdn_server.abort();
}

#[tokio::test]
async fn live_binary_ranges_and_past_epg_templates_are_bounded_and_origin_specific() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let (base, server) = serve(
        Router::new().fallback(get(|h: HeaderMap, uri: Uri| async move {
            private_headers(&h);
            if uri.path() == "/archive" || uri.query().is_some_and(|q| q.contains("replay=")) {
                return (
                    [(header::CONTENT_TYPE, "video/mp2t")],
                    uri.query().unwrap().to_owned(),
                )
                    .into_response();
            }
            if uri.query().is_some_and(|q| q.contains("start=-300")) {
                assert_eq!(uri.query().unwrap(), "key=A%2bb&start=-300");
            }
            assert_eq!(h[header::RANGE], "bytes=1-3");
            assert_eq!(h[header::IF_RANGE], "\"v1\"");
            let mime = if uri.path().ends_with(".flv") {
                "video/x-flv"
            } else {
                "video/mp2t"
            };
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CONTENT_RANGE, "bytes 1-3/5"),
                ],
                "123",
            )
                .into_response()
        })),
    )
    .await;
    let template = "/archive?b=${(b)yyyyMMddHHmmss}&e=${(e)yyyyMMddHHmmss}&start={utc}&end={utcend}&dur=${duration}&unix=${(b)unix}";
    add_line(
        &app,
        &format!("{base}/stream.ts?key=A%2bb&start=0"),
        template,
        "",
    );
    add_line(&app, &format!("{base}/stream.flv"), "", "");
    for (path, mime) in [
        (format!("{LINE}?start=-300"), "video/mp2t"),
        ("/api/channels/lines/2/stream".to_owned(), "video/x-flv"),
    ] {
        let data = app
            .request_with_headers(
                "GET",
                &path,
                Some(&admin),
                None,
                &[("range", "bytes=1-3"), ("if-range", "\"v1\"")],
            )
            .await;
        assert_eq!(data.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(data.headers[header::CONTENT_TYPE], mime);
        assert_eq!(data.headers[header::CONTENT_RANGE], "bytes 1-3/5");
        assert_eq!(data.body, b"123");
    }
    let now = polyview::now();
    let start = now - 7200;
    let end = now - 3600;
    let replay = format!("{LINE}?start={start}&end={end}");
    let dates = |epoch| -> String {
        app.state
            .conn()
            .unwrap()
            .query_row(
                "SELECT strftime('%Y%m%d%H%M%S',?1,'unixepoch')",
                [epoch],
                |row| row.get(0),
            )
            .unwrap()
    };
    let reply = app.request("GET", &replay, Some(&admin), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        String::from_utf8(reply.body).unwrap(),
        format!(
            "b={}&e={}&start={start}&end={end}&dur=3600&unix={start}",
            dates(start),
            dates(end)
        )
    );
    for query in [
        format!("start={}&end={end}", now - 604_802),
        format!("start={start}&end={}", now + 7200),
        format!("start={end}&end={start}"),
        format!("start={start}"),
        "end=1".into(),
        "start=-604801".into(),
        "start=-9223372036854775808".into(),
    ] {
        assert_eq!(
            app.request("GET", &format!("{LINE}?{query}"), Some(&admin), None)
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    app.state.conn().unwrap().execute("UPDATE channel_sources SET catchup_mode='append',catchup_source='?replay={utc}' WHERE id=1",[]).unwrap();
    let appended = app.request("GET", &replay, Some(&admin), None).await;
    assert_eq!(
        appended.body,
        format!("key=A%2bb&start=0&replay={start}").as_bytes()
    );
    for invalid in [
        "file:///private-signature",
        "https://private-signature@example.invalid/media",
        "?start={unknown}",
        "?start={utc",
        &"x".repeat(16_385),
    ] {
        app.state
            .conn()
            .unwrap()
            .execute(
                "UPDATE channel_sources SET catchup_source=?1 WHERE id=1",
                [invalid],
            )
            .unwrap();
        let reply = app.request("GET", &replay, Some(&admin), None).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!String::from_utf8_lossy(&reply.body).contains("private-signature"));
    }
    let (foreign, foreign_server) = serve(Router::new().route(
        "/archive",
        get(|h: HeaderMap| async move {
            assert!(!h.contains_key(header::COOKIE));
            assert!(!h.contains_key(header::REFERER));
            assert_ne!(h[header::USER_AGENT], AGENT);
            ([(header::CONTENT_TYPE, "video/mp2t")], "foreign archive")
        }),
    ))
    .await;
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE channel_sources SET catchup_source=?1 WHERE id=1",
            [format!("{foreign}/archive?b={{utc}}")],
        )
        .unwrap();
    let reply = app.request("GET", &replay, Some(&admin), None).await;
    assert_eq!(reply.body, b"foreign archive");
    server.abort();
    foreign_server.abort();
}
