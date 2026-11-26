use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const EXPORT: &str = "/api/settings/export";
const IMPORT: &str = "/api/settings/import";

async fn export(app: &TestApp, admin: &str) -> Value {
    let reply = app.request("GET", EXPORT, Some(admin), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    reply.json()
}

fn without_timestamp(mut value: Value) -> Value {
    assert!(
        value
            .as_object_mut()
            .unwrap()
            .remove("exported_at")
            .unwrap()
            .is_i64()
    );
    value
}

async fn restore(app: &TestApp, admin: &str, value: Value) -> Reply {
    app.request("POST", IMPORT, Some(admin), Some(value)).await
}

fn sample(backup: &mut Value) {
    backup["catalog_sources"] = json!([{
        "id":42,"name":"Private CMS","url":"https://cms.invalid/api?key=example",
        "enabled":true,"adult":true,"priority":17,"searchable":false,
        "custom_ua":"BackupFixture","custom_referer":"https://client.invalid/",
        "custom_cookie":"key=fixture-cookie","proxy_url":"http://example:proxy-pass@localhost:8888",
        "timeout_secs":45
    }]);
    backup["live_sources"] = json!([{
        "id":19,"name":"Television","url":"https://tv.invalid/guide.m3u","enabled":false
    }]);
    backup["live_channels"] = json!([{
        "name":"News HD", "group_name":"News", "tvg_id":"news.hd", "logo":"",
        "url":"https://tv.invalid/hd", "sources":[
            {"url":"https://tv.invalid/hd", "label":"HD", "position":0,
             "headers_json":"{}", "catchup_mode":"", "catchup_source":""},
            {"url":"https://tv.invalid/sd", "label":"SD", "position":1,
             "headers_json":r#"{"Cookie":"key=fixture-secret"}"#,
             "catchup_mode":"default", "catchup_source":"/replay?begin=${(b)yyyyMMddHHmmss}"}
        ]
    }]);
    backup["epg_config"] =
        json!({"url":"https://epg.invalid/one.xml\nhttps://epg.invalid/two.xml"});
    let mut downloads = backup["app_settings"]["downloads"].clone();
    downloads["concurrency"] = json!(3);
    backup["app_settings"] = json!({
        "adult_sources_enabled":true,"epg_aliases":[{"from":"News HD","to":"News"}],
        "live_merge_similar":true,
        "downloads":downloads,
        "telegram_native":{"api_id":1234,"api_hash":"0123456789abcdef0123456789abcdef"},
        "calendar":{"enabled":false,"api_url":"https://calendar.invalid/feed"},
        "notify":{"bark_url":"https://alerts.invalid/private/token","webhook_post_url":"","webhook_get_url":""},
        "play":{"ad_block":true,"play_ua":"Fixture","play_referer":"","image_referer":""},
        "douban_movie_url":"https://movie.invalid/one","douban_series_url":"https://series.invalid/two"
    });
}

fn cache_keys(app: &TestApp) -> Vec<String> {
    let conn = app.state.conn().unwrap();
    let mut stmt = conn
        .prepare("SELECT cache_key FROM remote_cache ORDER BY cache_key")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn seed_cache(app: &TestApp) {
    app.state.conn().unwrap().execute_batch(
        "INSERT INTO remote_cache VALUES('calendar','{}',1),('catalog_recommend_douban','{}',1),('unrelated','{}',1)",
    ).unwrap();
}

#[tokio::test]
async fn backup_roundtrips_configuration_and_preserves_members_sessions_and_source_references() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let feed = app
        .request("GET", "/api/tvbox", Some(&viewer), None)
        .await
        .json();
    let mut backup = export(&app, &admin).await;
    assert_eq!(backup["version"], 1);
    assert!(backup.get("users").is_none());
    assert!(!backup.to_string().contains(feed["token"].as_str().unwrap()));
    for (method, route, body) in [
        ("GET", EXPORT, None),
        ("POST", IMPORT, Some(backup.clone())),
    ] {
        assert_eq!(
            app.request(method, route, Some(&viewer), body.clone())
                .await
                .status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.request(method, route, None, body).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    sample(&mut backup);
    seed_cache(&app);
    assert_eq!(
        restore(&app, &admin, backup.clone()).await.status,
        StatusCode::OK
    );
    assert_eq!(cache_keys(&app), ["unrelated"]);
    let downloads = app
        .request("GET", "/api/settings/downloads", Some(&admin), None)
        .await
        .json();
    assert_eq!(downloads["concurrency"], 3);
    assert_eq!(downloads["telegram_proxy_configured"], false);
    assert!(downloads.get("telegram_proxy").is_none());
    for key in ["calendar", "notify", "play"] {
        let path = format!("/api/settings/{key}");
        let live = app.request("GET", &path, Some(&admin), None).await;
        assert_eq!(live.json(), backup["app_settings"][key]);
    }
    let aliases = app
        .request("GET", "/api/epg/aliases", Some(&admin), None)
        .await;
    assert_eq!(aliases.json(), backup["app_settings"]["epg_aliases"]);
    let epg = app
        .request("GET", "/api/epg/config", Some(&admin), None)
        .await;
    assert_eq!(epg.json()["url"], backup["epg_config"]["url"]);
    let catalog = app
        .request("GET", "/api/catalog/settings", Some(&admin), None)
        .await;
    assert_eq!(catalog.json()["adult_sources_enabled"], true);
    let exported = export(&app, &admin).await;
    assert_eq!(
        without_timestamp(exported.clone()),
        without_timestamp(backup.clone())
    );
    let fresh = TestApp::new();
    let fresh_cookie = fresh.setup().await;
    assert_eq!(
        restore(&fresh, &fresh_cookie, exported.clone())
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        without_timestamp(export(&fresh, &fresh_cookie).await),
        without_timestamp(exported)
    );

    // A restored URL can carry a different ID on this installation. Keep the
    // target ID, including its child rows and runtime status, and retain unrelated sources.
    app.state
        .conn()
        .unwrap()
        .execute_batch(
            "UPDATE catalog_sources SET id=99,last_latency_ms=45 WHERE id=42;
         INSERT INTO catalog_types(source_id,type_id,type_name) VALUES(99,7,'local category');
         INSERT INTO catalog_sources(id,name,url) VALUES(42,'Other','https://other.invalid/api');
         UPDATE live_sources SET id=25,channel_count=9 WHERE id=19;
         INSERT INTO remote_cache VALUES('calendar','{}',1),('catalog_recommend_douban','{}',1);
         INSERT INTO app_settings VALUES('private_binding','invalid-json');",
        )
        .unwrap();
    backup["catalog_sources"][0]["name"] = json!("Restored CMS");
    backup["live_sources"][0]["name"] = json!("Restored television");
    assert_eq!(
        restore(&app, &admin, backup.clone()).await.status,
        StatusCode::OK
    );
    assert_eq!(
        cache_keys(&app),
        ["calendar", "catalog_recommend_douban", "unrelated"]
    );
    let exported = export(&app, &admin).await;
    assert_eq!(exported["catalog_sources"][0]["name"], "Other");
    assert_eq!(exported["catalog_sources"][1]["id"], 99);
    assert_eq!(exported["catalog_sources"][1]["name"], "Restored CMS");
    assert_eq!(exported["live_sources"][0]["id"], 25);
    assert_eq!(exported["live_sources"][0]["name"], "Restored television");
    assert!(exported["app_settings"].get("private_binding").is_none());
    {
        let conn = app.state.conn().unwrap();
        let preserved: (i64, String, i64, String) = conn.query_row(
            "SELECT c.last_latency_ms,t.type_name,l.channel_count,s.value FROM catalog_sources c
             JOIN catalog_types t ON c.id=t.source_id JOIN live_sources l ON l.id=25
             JOIN app_settings s ON s.key='private_binding' WHERE c.id=99",
            [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
        ).unwrap();
        assert_eq!(
            preserved,
            (45, "local category".into(), 9, "invalid-json".into())
        );
    }
    assert_eq!(
        app.request("GET", "/api/tvbox", Some(&viewer), None)
            .await
            .json(),
        feed
    );
    assert_eq!(
        app.request("GET", "/api/users", Some(&admin), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // An absent mirror restores built-in defaults and invalidates only its recommendations.
    backup["app_settings"]
        .as_object_mut()
        .unwrap()
        .remove("douban_movie_url");
    assert_eq!(restore(&app, &admin, backup).await.status, StatusCode::OK);
    assert_eq!(cache_keys(&app), ["calendar", "unrelated"]);
    assert!(
        export(&app, &admin).await["app_settings"]
            .get("douban_movie_url")
            .is_none()
    );
}

#[tokio::test]
async fn backup_rejects_unknown_keys_invalid_values_and_rolls_back_partial_imports() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let original = export(&app, &admin).await;
    let mut backup = original.clone();
    sample(&mut backup);
    seed_cache(&app);
    let mut invalid = Vec::new();
    for (pointer, value) in [
        ("/version", json!(2)),
        ("/catalog_sources/0/url", json!("file:///tmp/private")),
        (
            "/catalog_sources/0/custom_cookie",
            json!("bad\r\nfixture-secret"),
        ),
        ("/catalog_sources/0/timeout_secs", json!(2)),
        ("/catalog_sources/0/proxy_url", json!("file:///bad")),
        ("/live_sources/0/url", json!("file:///tmp/invalid")),
        ("/live_sources/0/id", json!(0)),
        ("/live_channels", Value::Null),
        ("/live_channels/0/tvg_id", json!("bad\nmetadata")),
        ("/live_channels/0/url", json!("file:///bad")),
        ("/live_channels/0/sources/1/url", json!("file:///bad")),
        ("/live_channels/0/sources/1/position", json!(-1)),
        (
            "/live_channels/0/sources/1/headers_json",
            json!(r#"{"Cookie":"bad\r\nfixture-secret"}"#),
        ),
        (
            "/live_channels/0/sources/1/headers_json",
            json!(r#"{"Host":"fixture-secret"}"#),
        ),
        (
            "/live_channels/0/sources/1/catchup_source",
            json!("file:///fixture-secret"),
        ),
        ("/epg_config/url", json!("data:text/plain,no")),
        ("/app_settings/epg_aliases/0/from", json!("")),
        ("/app_settings/adult_sources_enabled", json!("0")),
        ("/app_settings/play/play_ua", json!("fixture-secret\r\n")),
        (
            "/app_settings/calendar/api_url",
            json!("file:///tmp/fixture-secret"),
        ),
        (
            "/app_settings/notify/bark_url",
            json!("file:///tmp/fixture-secret"),
        ),
        (
            "/app_settings/douban_movie_url",
            json!("javascript:fixture-secret"),
        ),
        (
            "/app_settings/douban_series_url",
            json!({"url":"https://example.invalid/"}),
        ),
        ("/app_settings/douban_series_url", Value::Null),
    ] {
        let mut bad = backup.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        invalid.push(bad);
    }
    for key in ["sessions", "user_id", "private_binding"] {
        let mut bad = backup.clone();
        bad["app_settings"][key] = json!({"role":"admin"});
        invalid.push(bad);
    }
    for invalid_section in [Value::Null, json!({}), json!([{"private_binding": true}])] {
        let mut bad = backup.clone();
        bad["source_subscriptions"] = invalid_section;
        invalid.push(bad);
    }
    let mut bad = backup.clone();
    bad["users"] = json!([]);
    invalid.push(bad);
    let mut duplicate = backup.clone();
    let entry = duplicate["live_sources"][0].clone();
    duplicate["live_sources"]
        .as_array_mut()
        .unwrap()
        .push(entry);
    invalid.push(duplicate);
    let mut duplicate_channel = backup.clone();
    let channel = duplicate_channel["live_channels"][0].clone();
    duplicate_channel["live_channels"]
        .as_array_mut()
        .unwrap()
        .push(channel);
    invalid.push(duplicate_channel);
    let mut duplicate = backup.clone();
    duplicate["live_channels"][0]["sources"][1]["url"] =
        duplicate["live_channels"][0]["url"].clone();
    invalid.push(duplicate);
    let mut runtime_field = backup.clone();
    runtime_field["live_channels"][0]["sources"][0]["fail_count"] = json!(9);
    invalid.push(runtime_field);
    let mut bad_mode = backup.clone();
    bad_mode["live_channels"][0]["sources"][1]["catchup_mode"] = json!("bad\nmode");
    invalid.push(bad_mode);
    for value in invalid {
        let reply = restore(&app, &admin, value).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!String::from_utf8_lossy(&reply.body).contains("fixture-secret"));
    }
    assert_eq!(
        without_timestamp(export(&app, &admin).await),
        without_timestamp(original)
    );
    assert_eq!(
        cache_keys(&app),
        ["calendar", "catalog_recommend_douban", "unrelated"]
    );

    // A conflict at the final source rolls back all prior sources, settings and cache invalidations.
    app.state.conn().unwrap().execute("INSERT INTO live_sources(id,name,url) VALUES(19,'existing','https://existing.invalid/live')", []).unwrap();
    let before = export(&app, &admin).await;
    assert_eq!(
        restore(&app, &admin, backup).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        without_timestamp(export(&app, &admin).await),
        without_timestamp(before)
    );
    assert_eq!(
        cache_keys(&app),
        ["calendar", "catalog_recommend_douban", "unrelated"]
    );
}

#[tokio::test]
async fn feed_backup_restores_selection_ownership_and_parser_configuration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let cms = format!("{base}/api.php/provide/vod/");
    let live = format!("{base}/channels.m3u");
    let parser = format!("{base}/parse?url=");
    let ignored = format!("{base}/ignored/api.php/provide/vod/");
    let feed = Arc::new(Mutex::new(json!({
        "sites": [
            {"key":"cms","name":"Films","type":1,"api":cms},
            {"key":"ignored","name":"Unselected","type":1,"api":ignored}
        ],
        "lives":[{"name":"TV","type":0,"url":live}],
        "parses":[{"name":"Fixture parser","type":1,"url":parser}]
    })));
    let stream_requests = Arc::new(AtomicUsize::new(0));
    let routes = Router::new()
        .route(
            "/config",
            axum::routing::get({
                let feed = feed.clone();
                move || std::future::ready(axum::Json(feed.lock().unwrap().clone()))
            }),
        )
        .fallback({
            let hits = stream_requests.clone();
            move || {
                hits.fetch_add(1, Ordering::SeqCst);
                std::future::ready(StatusCode::NOT_FOUND)
            }
        });
    let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    let origin = TestApp::new();
    let admin = origin.setup().await;
    let selection = json!([
        format!("cms:{cms}"),
        format!("live:{live}"),
        format!("parser:{parser}")
    ]);
    let created = origin
        .request(
            "POST",
            "/api/source-subscriptions",
            Some(&admin),
            Some(json!({
                "name":"Cinema", "url":format!("{base}/config?token=feed-secret"),
                "enabled":true, "selected":selection
            })),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let sync = "/api/source-subscriptions/sync";
    let reply = origin
        .request("POST", sync, Some(&admin), Some(json!({})))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let playlist = format!(
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"news.hd\" catchup=\"default\" catchup-source=\"{base}/archive?start=${{(b)yyyyMMddHHmmss}}\",News HD\n{base}/stream.hls|Cookie=session%3Dline-secret\n"
    );
    let channels = origin
        .request(
            "POST",
            "/api/channels/import",
            Some(&admin),
            Some(json!({"playlist":playlist})),
        )
        .await;
    assert_eq!(channels.status, StatusCode::OK);
    let mut backup = export(&origin, &admin).await;
    assert_eq!(backup["catalog_sources"].as_array().unwrap().len(), 1);
    assert_eq!(backup["catalog_sources"][0]["url"], cms);
    assert_eq!(backup["live_sources"][0]["url"], live);
    let channel = &backup["live_channels"][0];
    assert_eq!(channel["tvg_id"], "news.hd");
    let line = &channel["sources"][0];
    assert!(
        line["headers_json"]
            .as_str()
            .unwrap()
            .contains("session=line-secret")
    );
    assert_eq!(line["catchup_mode"], "default");
    assert_eq!(
        line["catchup_source"],
        format!("{base}/archive?start=${{(b)yyyyMMddHHmmss}}")
    );
    let sources = backup["source_subscriptions"]["sources"]
        .as_array_mut()
        .unwrap();
    let parser_index = sources
        .iter()
        .position(|source| source["kind"] == "parser" && source["url"] == parser)
        .unwrap();
    let declaration = &mut sources[parser_index]["parser"];
    assert_eq!(declaration["name"], "Fixture parser");
    declaration["headers_json"] = json!(r#"{"X-Fixture":"header-secret"}"#);
    declaration["flags"] = json!("qq,youku");
    declaration["priority"] = json!(73);
    declaration["timeout_ms"] = json!(3500);
    let config = &backup["source_subscriptions"];
    let subscriptions = config["subscriptions"].as_array().unwrap();
    assert_eq!(subscriptions.len(), 1);
    let saved = subscriptions[0]["selected"].as_array().unwrap();
    assert_eq!(saved.len(), 3);
    assert!(
        selection
            .as_array()
            .unwrap()
            .iter()
            .all(|key| saved.contains(key))
    );
    assert_eq!(subscriptions[0]["items"].as_array().unwrap().len(), 3);

    let target = TestApp::new();
    let target_admin = target.setup().await;
    let reply = restore(&target, &target_admin, backup.clone()).await;
    assert_eq!(reply.status, StatusCode::OK);
    let expected = without_timestamp(backup.clone());
    assert_eq!(
        without_timestamp(export(&target, &target_admin).await),
        expected
    );

    // Legacy backups omit subscriptions entirely. Their ordinary settings still apply.
    for includes_downloads in [false, true] {
        let mut legacy = backup.clone();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("source_subscriptions");
        if !includes_downloads {
            legacy["app_settings"]
                .as_object_mut()
                .unwrap()
                .remove("downloads");
        }
        legacy["app_settings"]["adult_sources_enabled"] = json!(true);
        assert_eq!(
            restore(&target, &target_admin, legacy).await.status,
            StatusCode::OK
        );
        let actual = export(&target, &target_admin).await;
        assert_eq!(actual["source_subscriptions"], *config);
        assert_eq!(actual["app_settings"]["adult_sources_enabled"], true);
    }

    // Invalid feed metadata must roll back earlier source and global-setting writes.
    let before = without_timestamp(export(&target, &target_admin).await);
    for (field, invalid) in [
        (
            "/subscriptions/0/items/0/url".to_string(),
            json!(format!("{base}/missing?token=bad-secret")),
        ),
        (
            "/subscriptions/0/url".to_string(),
            json!("file:///tmp/bad-secret"),
        ),
        (
            format!("/sources/{parser_index}/parser/timeout_ms"),
            json!(0),
        ),
        (
            format!("/sources/{parser_index}/parser/headers_json"),
            json!("bad-secret"),
        ),
    ] {
        let mut bad = backup.clone();
        bad["catalog_sources"][0]["name"] = json!("Should roll back");
        *bad["source_subscriptions"].pointer_mut(&field).unwrap() = invalid;
        let reply = restore(&target, &target_admin, bad).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{field}");
        assert!(!String::from_utf8_lossy(&reply.body).contains("bad-secret"));
    }
    assert_eq!(
        without_timestamp(export(&target, &target_admin).await),
        before
    );

    target.create_viewer(&target_admin).await;
    let viewer = target.login("viewer", PASSWORD).await.cookie();
    for (method, route, body) in [
        ("GET", EXPORT, None),
        ("POST", IMPORT, Some(backup.clone())),
    ] {
        let reply = target.request(method, route, Some(&viewer), body).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN);
        let body = String::from_utf8_lossy(&reply.body);
        assert!(!body.contains("feed-secret"));
        assert!(!body.contains("header-secret"));
        assert!(!body.contains("line-secret"));
    }

    let manual = TestApp::new();
    let manual_admin = manual.setup().await;
    {
        let conn = manual.state.conn().unwrap();
        conn.execute(
            "INSERT INTO catalog_sources(id,name,url) VALUES(91,'manual',?)",
            [&cms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO live_sources(id,name,url) VALUES(92,'manual',?)",
            [&live],
        )
        .unwrap();
    }
    assert_eq!(
        restore(&manual, &manual_admin, backup).await.status,
        StatusCode::OK
    );
    // The same incoming URLs now identify different rows. Only sources actually
    // managed by the subscription may be disabled when the feed drops them.
    {
        let mut updated = feed.lock().unwrap();
        updated["sites"] = json!([{"key":"ignored","name":"Unselected","type":1,"api":ignored}]);
        updated["lives"] = json!([]);
    }
    for (app, cookie, managed) in [
        (&target, &target_admin, true),
        (&manual, &manual_admin, false),
    ] {
        let reply = app
            .request("POST", sync, Some(cookie), Some(json!({})))
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        let actual = export(app, cookie).await;
        assert_eq!(actual["catalog_sources"][0]["enabled"], !managed);
        assert_eq!(actual["live_sources"][0]["enabled"], !managed);
        if !managed {
            assert_eq!(actual["catalog_sources"][0]["id"], 91);
            assert_eq!(actual["live_sources"][0]["id"], 92);
        }
    }
    assert_eq!(stream_requests.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn large_configuration_backups_roundtrip_without_raising_other_request_limits() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let mut backup = export(&app, &admin).await;
    let header = "a".repeat(8192);
    let rows: Vec<_> = (1..=257).map(|id| json!({
        "id":id,"name":"CMS","url":format!("https://example.invalid/{id}"),"custom_cookie":header
    })).collect();
    backup["catalog_sources"] = Value::Array(rows);
    let rejected = app
        .request(
            "POST",
            "/api/sources/batch",
            Some(&admin),
            Some(backup["catalog_sources"].clone()),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(restore(&app, &admin, backup).await.status, StatusCode::OK);
    let exported = export(&app, &admin).await;
    let rows = exported["catalog_sources"].as_array().unwrap();
    assert_eq!(rows.len(), 257);
    assert_eq!(rows[256]["custom_cookie"], header);
    assert_eq!(restore(&app, &admin, exported).await.status, StatusCode::OK);
}

#[tokio::test]
async fn live_backup_keeps_line_owners_favorites_and_destination_health() {
    let target = TestApp::new();
    let admin = target.setup().await;
    let member_id = target.create_viewer(&admin).await;
    let member = target.login("viewer", PASSWORD).await.cookie();
    let mut backup = export(&target, &admin).await;
    sample(&mut backup);
    let template = "/replay?from=${(b)yyyyMMddHHmmss}&to=${(e)yyyyMMddHHmmss}";
    backup["live_channels"][0]["logo"] = json!("https://tv.invalid/logo.png");
    backup["live_channels"][0]["sources"][1]["catchup_source"] = json!(template);
    {
        let conn = target.state.conn().unwrap();
        conn.execute_batch(
            "INSERT INTO channels(id,name,group_name,logo,url) VALUES
             (75,'Old','Old','','https://tv.invalid/hd'),
             (76,'Other','Other','','https://tv.invalid/other');
             INSERT INTO channel_sources(id,channel_id,url,label,position,latency_ms,last_ok_at,fail_count) VALUES
             (81,75,'https://tv.invalid/sd','Old line',2,23,98765,4),
             (82,76,'https://tv.invalid/sd','Different owner',0,17,12345,1);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO channel_favorites(user_id,channel_id,created_at) VALUES(?,75,12345)",
            [member_id],
        )
        .unwrap();
    }
    for _ in 0..2 {
        assert_eq!(
            restore(&target, &admin, backup.clone()).await.status,
            StatusCode::OK
        );
    }
    let saved = export(&target, &admin).await;
    assert_eq!(saved["live_channels"].as_array().unwrap().len(), 2);
    assert_eq!(saved["live_channels"][0], backup["live_channels"][0]);
    let local_line = &saved["live_channels"][1]["sources"][0];
    assert_eq!(local_line["label"], "Different owner");
    assert_eq!(local_line["headers_json"], "{}");
    for key in ["latency_ms", "last_ok_at", "fail_count"] {
        assert!(local_line.get(key).is_none());
    }
    let denied = target.request("GET", EXPORT, Some(&member), None).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(!String::from_utf8_lossy(&denied.body).contains("fixture-secret"));

    // Before M9 the backup had no live channel section. Restoring it can still change aliases.
    backup.as_object_mut().unwrap().remove("live_channels");
    backup["app_settings"]
        .as_object_mut()
        .unwrap()
        .remove("live_merge_similar");
    backup["app_settings"]["epg_aliases"] = json!([{"from":"Old TV","to":"News HD"}]);
    assert_eq!(
        restore(&target, &admin, backup.clone()).await.status,
        StatusCode::OK
    );
    let legacy = export(&target, &admin).await;
    assert_eq!(legacy["app_settings"]["live_merge_similar"], true);
    assert_eq!(
        legacy["app_settings"]["epg_aliases"],
        backup["app_settings"]["epg_aliases"]
    );
    assert_eq!(legacy["live_channels"], saved["live_channels"]);
    let conn = target.state.conn().unwrap();
    let retained: (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT s.id,s.latency_ms,s.last_ok_at,s.fail_count FROM channel_sources s
             JOIN channel_favorites f ON f.channel_id=s.channel_id
             WHERE f.user_id=? AND f.channel_id=75 AND s.url='https://tv.invalid/sd'",
            [member_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(retained, (81, 23, 98765, 4));
}
