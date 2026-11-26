use super::*;
use axum::routing::get;
use std::sync::atomic::{AtomicUsize, Ordering};

const SOURCES: &str = "/api/live-sources";
const REFRESH: &str = "/api/live-sources/refresh";
const ALIASES: &str = "/api/epg/aliases";
const PLAYLIST: &str = "#EXTM3U\n#EXTINF:-1 group-title=\"National\",News\nhttps://stream.invalid/news-1\n#EXTINF:-1 group-title=\"National\",News\nhttps://stream.invalid/news-2\n";

async fn serve(routes: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    (base, task)
}

#[tokio::test]
async fn subscription_management_validates_urls_and_requires_an_admin() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let config = json!({"name":" News ", "url":"https://source.invalid/list#unused"});
    for (method, path, body) in [
        ("GET", SOURCES, None),
        ("POST", SOURCES, Some(config.clone())),
        ("PUT", "/api/live-sources/1", Some(config.clone())),
        ("DELETE", "/api/live-sources/1", None),
        ("POST", REFRESH, None),
        ("GET", ALIASES, None),
        ("PUT", ALIASES, Some(json!([]))),
        ("GET", "/api/settings/live", None),
        (
            "PUT",
            "/api/settings/live",
            Some(json!({"live_merge_similar":true})),
        ),
    ] {
        let reply = app.request(method, path, Some(&viewer), body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    for invalid in [
        json!({"name":"", "url":"https://source.invalid/list"}),
        json!({"name":"Feed", "url":"file:///feed"}),
        json!({"name":"Feed", "url":"http://user:secret@source.invalid/feed"}),
        json!({"name":"Feed", "url":"https://source.invalid/one\ntwo"}),
    ] {
        assert_eq!(
            app.request("POST", SOURCES, Some(&admin), Some(invalid))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let created = app
        .request("POST", SOURCES, Some(&admin), Some(config.clone()))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let row = created.json();
    assert_eq!(row["name"], "News");
    assert_eq!(row["url"], "https://source.invalid/list");
    assert_eq!(row["enabled"], true);
    assert_eq!(row["channel_count"], 0);
    assert!(row["updated_at"].is_null());
    assert_eq!(
        app.request("POST", SOURCES, Some(&admin), Some(config))
            .await
            .status,
        StatusCode::CONFLICT
    );
    let path = format!("{SOURCES}/{}", row["id"]);
    let changed = app
        .request(
            "PUT",
            &path,
            Some(&admin),
            Some(json!({"name":"Replaced", "url":"https://source.invalid/new", "enabled":false})),
        )
        .await
        .json();
    assert_eq!(changed["enabled"], false);
    assert_eq!(changed["name"], "Replaced");
    assert_eq!(
        app.request("GET", SOURCES, Some(&admin), None).await.json(),
        json!([changed])
    );
    assert_eq!(
        app.request("DELETE", &path, Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("DELETE", &path, Some(&admin), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.request("GET", SOURCES, Some(&admin), None).await.json(),
        json!([])
    );
}

#[tokio::test]
async fn refresh_merges_channels_and_preserves_last_good_import_on_failure() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let mode = Arc::new(AtomicUsize::new(0));
    let hits = Arc::new(AtomicUsize::new(0));
    let behavior = mode.clone();
    let disabled_hits = hits.clone();
    let (base, server) = serve(Router::new()
        .route("/news", get(move || { let behavior = behavior.clone(); async move {
            match behavior.load(Ordering::SeqCst) {
                1 => (StatusCode::SERVICE_UNAVAILABLE, "not available"),
                2 => (StatusCode::OK, "#EXTM3U\n#EXTINF:-1,Partial\nhttps://stream.invalid/partial\n#EXTINF:-1,Missing\n"),
                _ => (StatusCode::OK, PLAYLIST),
            }
        }}))
        .route("/other", get(|| async { "#EXTM3U\n#EXTINF:-1,Music\nhttps://stream.invalid/music\n" }))
        .route("/disabled", get(move || { disabled_hits.fetch_add(1, Ordering::SeqCst); async { PLAYLIST } }))).await;
    for (name, enabled) in [("news", true), ("other", true), ("disabled", false)] {
        let reply = app
            .request(
                "POST",
                SOURCES,
                Some(&admin),
                Some(json!({"name":name, "url":format!("{base}/{name}"), "enabled":enabled})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED);
    }
    let manual =
        "#EXTM3U\n#EXTINF:-1 group-title=\"National\",News\nhttps://stream.invalid/manual\n";
    assert_eq!(
        app.request(
            "POST",
            "/api/channels/import",
            Some(&admin),
            Some(json!({"playlist":manual}))
        )
        .await
        .status,
        StatusCode::OK
    );
    for (value, refreshed, failed) in [(0, 2, 0), (1, 1, 1), (2, 1, 1), (0, 2, 0)] {
        mode.store(value, Ordering::SeqCst);
        let result = app.request("POST", REFRESH, Some(&admin), None).await;
        assert_eq!(result.status, StatusCode::OK);
        assert_eq!(
            result.json(),
            json!({"refreshed":refreshed, "failed":failed})
        );
        let rows = app.request("GET", SOURCES, Some(&admin), None).await.json();
        assert_eq!(rows[0]["channel_count"], 1);
        assert!(rows[0]["updated_at"].as_i64().unwrap() > 0);
        match value {
            1 => assert!(rows[0]["error"].as_str().unwrap().contains("503")),
            2 => assert!(rows[0]["error"].as_str().unwrap().contains("M3U")),
            _ => assert!(rows[0]["error"].is_null()),
        }
        assert_eq!(rows[1]["channel_count"], 1);
        assert!(rows[2]["updated_at"].is_null());
        let channels = app
            .request("GET", "/api/channels", Some(&admin), None)
            .await
            .json();
        let channels = channels.as_array().unwrap();
        assert_eq!(channels.len(), 2);
        let news = channels.iter().find(|row| row["name"] == "News").unwrap();
        assert_eq!(news["sources"].as_array().unwrap().len(), 3);
        assert_eq!(news["url"], "https://stream.invalid/manual");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn disabled_subscription_discards_an_in_flight_playlist() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered = started.clone();
    let resume = release.clone();
    let (base, server) = serve(Router::new().route(
        "/live",
        get(move || {
            let entered = entered.clone();
            let resume = resume.clone();
            async move {
                entered.notify_one();
                resume.notified().await;
                PLAYLIST
            }
        }),
    ))
    .await;
    let mut config = json!({"name":"slow", "url":format!("{base}/live")});
    let row = app
        .request("POST", SOURCES, Some(&admin), Some(config.clone()))
        .await
        .json();
    let path = format!("{SOURCES}/{}", row["id"]);
    let update = async {
        started.notified().await;
        config["enabled"] = json!(false);
        let changed = app.request("PUT", &path, Some(&admin), Some(config)).await;
        release.notify_one();
        assert_eq!(changed.status, StatusCode::OK);
    };
    let (reply, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(app.request("POST", REFRESH, Some(&admin), None), update)
    })
    .await
    .unwrap();
    assert_eq!(reply.json(), json!({"refreshed":0,"failed":0}));
    assert_eq!(
        app.request("GET", "/api/channels", Some(&admin), None)
            .await
            .json(),
        json!([])
    );
    server.abort();
}

#[tokio::test]
async fn custom_alias_precedes_builtin_mapping_during_xmltv_refresh() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let (start, from, until): (i64, String, String) = app.state.conn().unwrap().query_row(
        "SELECT unixepoch(), strftime('%Y%m%d%H%M%S','now'), strftime('%Y%m%d%H%M%S','now','+1 hour')", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    let feed = format!(
        r#"<tv><channel id="a"><display-name>世界地理</display-name></channel>
        <channel id="b"><display-name>My feed</display-name></channel>
        <programme channel="a" start="{from} +0000" stop="{until} +0000"><title>Default</title></programme>
        <programme channel="b" start="{from} +0000" stop="{until} +0000"><title>Override</title></programme></tv>"#
    );
    let (base, server) =
        serve(Router::new().route("/guide", get(move || async move { feed }))).await;
    let playlist = "#EXTM3U\n#EXTINF:-1,CCTV 世界地理\nhttps://stream.invalid/world\n";
    assert_eq!(
        app.request(
            "POST",
            "/api/channels/import",
            Some(&admin),
            Some(json!({"playlist":playlist}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request(
            "PUT",
            "/api/epg/config",
            Some(&admin),
            Some(json!({"url":format!("{base}/guide")}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let aliases = json!([{"from":"cctv-世界地理", "to":"My feed"}]);
    assert_eq!(
        app.request("GET", ALIASES, Some(&admin), None).await.json(),
        json!([])
    );
    for (mapping, expected) in [(aliases.clone(), "Override"), (json!([]), "Default")] {
        assert_eq!(
            app.request("PUT", ALIASES, Some(&admin), Some(mapping.clone()))
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        let invalid = json!([{"from":"News", "to":"A"},{"from":"N-E-W-S", "to":"B"}]);
        assert_eq!(
            app.request("PUT", ALIASES, Some(&admin), Some(invalid))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            app.request("GET", ALIASES, Some(&admin), None).await.json(),
            mapping
        );
        let refreshed = app
            .request("POST", "/api/epg/refresh", Some(&admin), None)
            .await
            .json();
        assert_eq!(refreshed["matched_channels"], 1);
        let guide = app
            .request(
                "GET",
                &format!("/api/epg?start={start}&end={}", start + 3600),
                Some(&admin),
                None,
            )
            .await
            .json();
        assert_eq!(guide["programs"].as_array().unwrap().len(), 1);
        assert_eq!(guide["programs"][0]["title"], expected);
    }
    server.abort();
}
