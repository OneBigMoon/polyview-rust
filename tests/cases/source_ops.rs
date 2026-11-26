use super::*;
use axum::{Json, http::Uri, routing::get};
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn batch_reorder_and_management_permissions() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let member = app.login("viewer", PASSWORD).await.cookie();
    let input = json!({"name":"private", "url":"https://cms.invalid/?token=url-secret", "custom_cookie":"k=cookie-secret", "proxy_url":"http://user:proxy-secret@localhost:8111", "custom_ua":"ua-secret", "custom_referer":"https://referer-secret.invalid/", "timeout_secs":3});
    for (route, body) in [
        ("/api/sources/probe", json!({})),
        ("/api/sources/reorder", json!({"ids":[]})),
        ("/api/sources/batch", json!([input])),
    ] {
        for (cookie, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(member.as_str()), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                app.request("POST", route, cookie, Some(body.clone()))
                    .await
                    .status,
                status
            );
        }
    }
    let batch = json!([input, {"name":"duplicate","url":"https://cms.invalid/?token=url-secret#part"}, false, {"name":"invalid","url":"file:///tmp/cms"}, {"name":"hidden","url":"http://localhost:1/cms","enabled":false}]);
    let imported = app
        .request("POST", "/api/sources/batch", Some(&admin), Some(batch))
        .await;
    assert_eq!(imported.json(), json!({"imported":2,"skipped":3}));
    for (field, invalid) in [
        ("timeout_secs", json!(2)),
        ("timeout_secs", json!(61)),
        ("custom_cookie", json!("a\r\nb")),
        ("custom_ua", json!("a\tb")),
        ("proxy_url", json!("file:///etc")),
    ] {
        let mut invalid_source = input.clone();
        invalid_source[field] = invalid;
        assert_eq!(
            app.request("PUT", "/api/sources/1", Some(&admin), Some(invalid_source))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let updated = app
        .request(
            "PUT",
            "/api/sources/1",
            Some(&admin),
            Some(json!({"name":"renamed", "url":input["url"], "timeout_secs":60})),
        )
        .await
        .json();
    assert_eq!(updated["timeout_secs"], 60);
    assert_eq!(updated["custom_cookie"], input["custom_cookie"]);
    let order = |ids: Value| Some(json!({"ids":ids}));
    for ids in [json!([2, 2]), json!([2, 99])] {
        assert_eq!(
            app.request("POST", "/api/sources/reorder", Some(&admin), order(ids))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let listed = app
        .request("GET", "/api/sources", Some(&admin), None)
        .await
        .json();
    assert_eq!(listed[0]["priority"], 100);
    assert_eq!(listed[1]["priority"], 100);
    assert_eq!(
        app.request(
            "POST",
            "/api/sources/reorder",
            Some(&admin),
            order(json!([2, 1]))
        )
        .await
        .status,
        StatusCode::OK
    );
    let listed = app
        .request("GET", "/api/sources", Some(&admin), None)
        .await
        .json();
    assert_eq!(listed[0]["id"], 2);
    assert_eq!(listed[1]["id"], 1);
    let sources = app
        .request("GET", "/api/sources", Some(&member), None)
        .await
        .json();
    assert_eq!(sources.as_array().unwrap().len(), 1);
    assert_eq!(sources[0]["name"], "renamed");
    let feed = app
        .request("GET", "/api/tvbox", Some(&member), None)
        .await
        .json();
    let config = app
        .request_with_headers(
            "GET",
            feed["config_path"].as_str().unwrap(),
            None,
            None,
            &[("host", "polyview.test")],
        )
        .await;
    assert_eq!(config.status, StatusCode::OK);
    for exposed in [sources, config.json()] {
        let text = exposed.to_string();
        assert!(!text.contains("secret"));
        assert!(!text.contains("custom_cookie"));
        assert!(!text.contains("proxy_url"));
    }
}

#[tokio::test]
async fn probe_search_and_tv_use_source_overrides_and_recover_from_cooldown() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let failing = Arc::new(AtomicBool::new(true));
    let proxy = Router::new().fallback(get({
        let calls = calls.clone();
        let failing = failing.clone();
        move |headers: HeaderMap, uri: Uri| {
            calls.lock().unwrap().push((uri, headers));
            let failing = failing.load(Ordering::Relaxed);
            async move {
                if failing {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"message":"sensitive upstream details"})),
                    )
                } else {
                    (
                        StatusCode::OK,
                        Json(json!({"list":[{"vod_id":5,"vod_name":"Film"}]})),
                    )
                }
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, proxy).await.unwrap() });
    let mut input = json!({"name":"test", "url":"http://cms.invalid/source?key=secret", "proxy_url":proxy_url, "custom_cookie":"sid=secret", "custom_ua":"SourceAgent/2", "custom_referer":"https://referer.invalid/", "timeout_secs":3});
    assert_eq!(
        app.request("POST", "/api/sources", Some(&admin), Some(input.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    let created = app
        .request(
            "POST",
            "/api/sources",
            Some(&admin),
            Some(json!({"name":"disabled", "url":"http://127.0.0.1:1/disabled", "enabled":false})),
        )
        .await
        .json();
    assert_eq!(created["timeout_secs"], 12);
    let search = "/api/catalog/search?q=Film";
    let before = polyview::now();
    for _ in 0..3 {
        let result = app.request("GET", search, Some(&admin), None).await.json();
        assert_eq!(result["errors"][0]["message"], "Source returned HTTP 503");
    }
    let health = app
        .request("GET", "/api/sources", Some(&admin), None)
        .await
        .json();
    assert_eq!(health[0]["fail_streak"], 3);
    assert!(
        (before + 120..=polyview::now() + 120)
            .contains(&health[0]["cooldown_until"].as_i64().unwrap())
    );
    let result = app.request("GET", search, Some(&admin), None).await.json();
    assert!(
        result["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("cooling")
    );
    assert_eq!(calls.lock().unwrap().len(), 3);
    let probe = app
        .request(
            "POST",
            "/api/sources/probe",
            Some(&admin),
            Some(json!({"source_id":1})),
        )
        .await
        .json();
    assert_eq!(probe[0]["ok"], false);
    assert_eq!(probe[0]["error"], "Source returned HTTP 503");
    failing.store(false, Ordering::Relaxed);
    let probe = app
        .request("POST", "/api/sources/probe", Some(&admin), Some(json!({})))
        .await
        .json();
    assert_eq!(probe.as_array().unwrap().len(), 1);
    assert_eq!(probe[0]["source_id"], 1);
    assert_eq!(probe[0]["ok"], true);
    assert!(probe[0]["error"].is_null());
    let health = app
        .request("GET", "/api/sources", Some(&admin), None)
        .await
        .json();
    assert_eq!(health[0]["last_probe_ok"], true);
    assert!(health[0]["last_probe_at"].is_i64());
    assert_eq!(health[0]["last_latency_ms"], probe[0]["latency_ms"]);
    assert_eq!(health[0]["fail_streak"], 0);
    assert_eq!(health[0]["cooldown_until"], 0);
    let feed = app
        .request("GET", "/api/tvbox", Some(&admin), None)
        .await
        .json();
    let site = format!(
        "/tvbox/{}/source/1?wd=Film",
        feed["token"].as_str().unwrap()
    );
    assert_eq!(
        app.request("GET", &site, None, None).await.json()["list"][0]["vod_name"],
        "Film"
    );
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 6);
        for (url, headers) in calls.iter() {
            assert_eq!(url.host(), Some("cms.invalid"));
            assert_eq!(headers[header::USER_AGENT], "SourceAgent/2");
            assert_eq!(headers[header::REFERER], "https://referer.invalid/");
            assert_eq!(headers[header::COOKIE], "sid=secret");
        }
    }
    input["searchable"] = json!(false);
    assert_eq!(
        app.request("PUT", "/api/sources/1", Some(&admin), Some(input))
            .await
            .status,
        StatusCode::OK
    );
    for route in [search, "/api/catalog/discover"] {
        assert_eq!(
            app.request("GET", route, Some(&admin), None).await.json()["results"],
            json!([])
        );
    }
    assert_eq!(
        app.request("GET", &site, None, None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(calls.lock().unwrap().len(), 6);
    server.abort();
}

#[test]
fn source_migration_preserves_legacy_rows_and_applies_defaults_once() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE catalog_sources (id INTEGER PRIMARY KEY,name TEXT NOT NULL,url TEXT NOT NULL UNIQUE,enabled INTEGER NOT NULL DEFAULT 1);
        INSERT INTO catalog_sources(id,name,url,enabled) VALUES(9,'legacy','http://localhost/legacy',0)").unwrap();
    init_schema(&conn).unwrap();
    conn.execute("UPDATE catalog_sources SET priority=7 WHERE id=9", [])
        .unwrap();
    init_schema(&conn).unwrap();
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM catalog_sources WHERE id=9 AND name='legacy' AND enabled=0 AND priority=7 AND searchable=1 AND timeout_secs=12 AND custom_ua='' AND custom_referer='' AND custom_cookie='' AND proxy_url='' AND last_latency_ms IS NULL AND last_probe_ok IS NULL AND last_probe_at IS NULL AND fail_streak=0 AND cooldown_until=0",[],|row|row.get(0)).unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn catalog_lists_and_searches_follow_priority_then_id() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let cms = Router::new().fallback(get(|| async {
        Json(json!({"list":[{"vod_id":7,"vod_name":"Title"}]}))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, cms).await.unwrap() });
    for (index, priority) in [40, 20, 20].into_iter().enumerate() {
        let source = json!({"name":format!("Source {index}"),"url":format!("http://{address}/{index}"),"priority":priority});
        assert_eq!(
            app.request("POST", "/api/sources", Some(&admin), Some(source))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    for route in ["/api/catalog/search?q=Title", "/api/catalog/discover"] {
        let result = app.request("GET", route, Some(&admin), None).await.json();
        let ids: Vec<_> = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["source_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, [2, 3, 1]);
    }
    server.abort();
}

#[tokio::test]
async fn probes_overlap_and_ordinary_queries_retry_after_expiry() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let hang = Arc::new(AtomicBool::new(true));
    let cms = Router::new()
        .route(
            "/probe",
            get(move || {
                let barrier = barrier.clone();
                async move {
                    barrier.wait().await;
                    Json(json!({"list":[]}))
                }
            }),
        )
        .route(
            "/slow",
            get({
                let hang = hang.clone();
                move || {
                    let hang = hang.load(Ordering::Relaxed);
                    async move {
                        if hang {
                            std::future::pending::<()>().await;
                        }
                        Json(json!({"list":[{"vod_id":1,"vod_name":"recovered"}]}))
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, cms).await.unwrap() });
    for i in [1, 2] {
        assert_eq!(app.request("POST","/api/sources",Some(&admin),Some(json!({"name":"probe", "url":format!("http://{addr}/probe?i={i}"), "timeout_secs":3}))).await.status,StatusCode::CREATED);
    }
    let results = app
        .request("POST", "/api/sources/probe", Some(&admin), Some(json!({})))
        .await
        .json();
    assert_eq!(results.as_array().unwrap().len(), 2);
    assert!(results.as_array().unwrap().iter().all(|r| r["ok"] == true));
    let slow = app
        .request(
            "POST",
            "/api/sources",
            Some(&admin),
            Some(json!({"name":"slow","url":format!("http://{addr}/slow"),"timeout_secs":3})),
        )
        .await
        .json();
    let id = slow["id"].as_i64().unwrap();
    let route = format!("/api/catalog/discover?source_id={id}");
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE catalog_sources SET fail_streak=2 WHERE id=?", [id])
        .unwrap();
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(9),
        app.request("GET", &route, Some(&admin), None),
    )
    .await
    .expect("configured three-second timeout");
    assert_eq!(reply.json()["errors"][0]["source_id"], id);
    let health = app
        .request("GET", "/api/sources", Some(&admin), None)
        .await
        .json();
    assert_eq!(health[2]["fail_streak"], 3);
    assert!(health[2]["cooldown_until"].as_i64().unwrap() > polyview::now());
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE catalog_sources SET cooldown_until=?1 WHERE id=?2",
            rusqlite::params![polyview::now() - 1, id],
        )
        .unwrap();
    hang.store(false, Ordering::Relaxed);
    let recovered = app.request("GET", &route, Some(&admin), None).await.json();
    assert_eq!(recovered["results"][0]["title"], "recovered");
    let health = app
        .request("GET", "/api/sources", Some(&admin), None)
        .await
        .json();
    assert_eq!(health[2]["fail_streak"], 0);
    assert_eq!(health[2]["cooldown_until"], 0);
    server.abort();
}
