use super::*;

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
    backup["epg_config"] =
        json!({"url":"https://epg.invalid/one.xml\nhttps://epg.invalid/two.xml"});
    backup["app_settings"] = json!({
        "adult_sources_enabled":true,"epg_aliases":[{"from":"News HD","to":"News"}],
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
