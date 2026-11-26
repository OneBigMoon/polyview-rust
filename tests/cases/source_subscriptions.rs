use super::*;
use axum::{Json, routing::get};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::{sync::Notify, task::JoinHandle};

const FEEDS: &str = "/api/source-subscriptions";
const PREVIEW: &str = "/api/source-subscriptions/preview";
const SYNC: &str = "/api/source-subscriptions/sync";
const CMS: &str = "https://cms.invalid/api";
const MANUAL: &str = "https://manual.invalid/api";
const PARSER: &str = "https://parser.invalid/resolve?url=";
const LIVE: &str = "https://live.invalid/guide.m3u";

async fn serve(routes: Router) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    (base, task)
}

async fn add(app: &TestApp, admin: &str, config: Value) -> i64 {
    let reply = app.request("POST", FEEDS, Some(admin), Some(config)).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    reply.json()["id"].as_i64().unwrap()
}

async fn sync(app: &TestApp, admin: &str, input: Value) -> Value {
    let reply = app.request("POST", SYNC, Some(admin), Some(input)).await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()
}

fn document(name: &str) -> Value {
    json!({
        "sites":[{"type":1,"name":name,"api":CMS},{"type":1,"name":"Advertised", "api":MANUAL}],
        "parses":[{"type":1,"name":"Resolve","url":PARSER}],
        "lives":[{"name":"TV","url":LIVE}]
    })
}

fn catalog(app: &TestApp, url: &str) -> (i64, String, bool) {
    app.state
        .conn()
        .unwrap()
        .query_row(
            "SELECT id,name,enabled FROM catalog_sources WHERE url=?",
            [url],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn subscription_admin_crud_selection_and_preview() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let (base, server) =
        serve(Router::new().route("/config", get(|| async { Json(document("Movie")) }))).await;
    let url = format!("{base}/config");
    let config = json!({"name":" Family ","url":url});
    for (method, path, body) in [
        ("GET", FEEDS, None),
        ("POST", FEEDS, Some(config.clone())),
        ("PUT", "/api/source-subscriptions/1", Some(config.clone())),
        ("DELETE", "/api/source-subscriptions/1", None),
        ("POST", PREVIEW, Some(json!({"url":url}))),
        ("POST", SYNC, Some(json!({}))),
    ] {
        for (cookie, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(viewer.as_str()), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                app.request(method, path, cookie, body.clone()).await.status,
                status,
                "{method} {path}"
            );
        }
        if method != "GET" {
            assert_eq!(
                app.request_with_headers(method, path, Some(&admin), body, &[])
                    .await
                    .status,
                StatusCode::FORBIDDEN
            );
        }
    }
    for invalid in [
        json!({"name":"", "url":url}),
        json!({"name":"Bad", "url":"file:///dev/null"}),
        json!({"name":"Bad", "url":url,"selected":["script:file:///tmp/x"]}),
    ] {
        assert_eq!(
            app.request("POST", FEEDS, Some(&admin), Some(invalid))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let preview = app
        .request("POST", PREVIEW, Some(&admin), Some(json!({"url":url})))
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(preview.json()["items"].as_array().unwrap().len(), 4);
    assert_eq!(
        app.state
            .conn()
            .unwrap()
            .query_row("SELECT count(*) FROM catalog_sources", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let id = add(&app, &admin, config.clone()).await;
    let path = format!("{FEEDS}/{id}");
    assert_eq!(
        app.request("POST", FEEDS, Some(&admin), Some(config))
            .await
            .status,
        StatusCode::CONFLICT
    );
    let rows = app.request("GET", FEEDS, Some(&admin), None).await.json();
    assert_eq!(rows[0]["name"], "Family");
    assert!(rows[0]["selected"].is_null());
    assert_eq!(rows[0]["enabled"], true);
    let selected = vec![format!("cms:{CMS}")];
    for (keys, count) in [(json!(selected), 1), (json!([]), 0)] {
        let put = app
            .request(
                "PUT",
                &path,
                Some(&admin),
                Some(json!({"name":"Family","url":url,"selected":keys})),
            )
            .await;
        assert_eq!(put.json()["selected"], keys);
        assert_eq!(
            sync(&app, &admin, json!({"id":id})).await,
            json!({"synced":1,"failed":0})
        );
        assert_eq!(
            app.request("GET", FEEDS, Some(&admin), None).await.json()[0]["item_count"],
            count
        );
        assert_eq!(catalog(&app, CMS).2, count == 1);
    }
    let disabled = json!({"name":"Paused","url":url,"enabled":false});
    let update = app
        .request("PUT", &path, Some(&admin), Some(disabled))
        .await;
    assert_eq!(update.json()["enabled"], false);
    assert_eq!(
        sync(&app, &admin, json!({})).await,
        json!({"synced":0,"failed":0})
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
        app.request("POST", SYNC, Some(&admin), Some(json!({"id":id})))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    server.abort();
}

#[tokio::test]
async fn overlapping_subscriptions_preserve_manual_rows_history_and_parser_settings() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let first_doc = Arc::new(Mutex::new(document("Original")));
    let shared = first_doc.clone();
    let (base, server) = serve(
        Router::new()
            .route(
                "/one",
                get(move || {
                    let value = shared.lock().unwrap().clone();
                    async move { Json(value) }
                }),
            )
            .route("/two", get(|| async { Json(document("Second")) })),
    )
    .await;
    let first = add(
        &app,
        &admin,
        json!({"name":"First","url":format!("{base}/one")}),
    )
    .await;
    let second = add(
        &app,
        &admin,
        json!({"name":"Second","url":format!("{base}/two")}),
    )
    .await;
    app.state
        .conn()
        .unwrap()
        .execute(
            "INSERT INTO catalog_sources(name,url,enabled,priority) VALUES('Manual',?,0,17)",
            [MANUAL],
        )
        .unwrap();
    assert_eq!(
        sync(&app, &admin, json!({})).await,
        json!({"synced":2,"failed":0})
    );
    let (id, name, active) = catalog(&app, CMS);
    assert_eq!(name, "Original");
    assert!(active);
    {
        let conn = app.state.conn().unwrap();
        conn.execute("INSERT INTO catalog_favorites(user_id,source_id,media_id,title,created_at) SELECT id,?,'42','Film',1 FROM users", [id]).unwrap();
        let defaults: (String,String,i64,i64,bool) = conn.query_row("SELECT headers_json,flags,priority,timeout_ms,enabled FROM parsers WHERE url_template=?",[PARSER],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(defaults, ("{}".into(), "*".into(), 100, 10_000, true));
        conn.execute(
            "UPDATE parsers SET priority=7 WHERE url_template=?",
            [PARSER],
        )
        .unwrap();
    }
    *first_doc.lock().unwrap() = document("Renamed");
    sync(&app, &admin, json!({"id":first})).await;
    assert_eq!(catalog(&app, CMS), (id, "Renamed".into(), true));
    *first_doc.lock().unwrap() = json!({"sites":[]});
    sync(&app, &admin, json!({"id":first})).await;
    assert_eq!(catalog(&app, CMS), (id, "Second".into(), true));
    assert_eq!(
        app.request("DELETE", &format!("{FEEDS}/{second}"), Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(catalog(&app, CMS), (id, "Second".into(), false));
    {
        let conn = app.state.conn().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM catalog_favorites WHERE source_id=?",
                [id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert!(
            !conn
                .query_row(
                    "SELECT enabled FROM live_sources WHERE url=?",
                    [LIVE],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        assert!(
            !conn
                .query_row(
                    "SELECT enabled FROM parsers WHERE url_template=?",
                    [PARSER],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        let manual: (String, bool, i64) = conn
            .query_row(
                "SELECT name,enabled,priority FROM catalog_sources WHERE url=?",
                [MANUAL],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(manual, ("Manual".into(), false, 17));
    }
    *first_doc.lock().unwrap() = document("Returned");
    sync(&app, &admin, json!({"id":first})).await;
    assert_eq!(catalog(&app, CMS), (id, "Returned".into(), true));
    let parser: (i64, bool) = app
        .state
        .conn()
        .unwrap()
        .query_row(
            "SELECT priority,enabled FROM parsers WHERE url_template=?",
            [PARSER],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(parser, (7, true));
    server.abort();
}

#[tokio::test]
async fn failed_repository_branch_preserves_last_good_snapshot_and_retries_per_feed() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let fail = Arc::new(AtomicBool::new(false));
    let branch_failure = fail.clone();
    let partial = fail.clone();
    let stable_hits = Arc::new(AtomicUsize::new(0));
    let hits = stable_hits.clone();
    let (base, server) = serve(
        Router::new()
            .route(
                "/root",
                get(move || {
                    let sites = if partial.load(Ordering::SeqCst) {
                        json!([{"type":1,"name":"Partial","api":"https://partial.invalid/api"}])
                    } else {
                        json!([])
                    };
                    async move { Json(json!({"sites":sites,"urls":["/one","/two"]})) }
                }),
            )
            .route("/one", get(|| async { Json(json!({"sites":[]})) }))
            .route(
                "/two",
                get(move || {
                    let broken = branch_failure.load(Ordering::SeqCst);
                    async move {
                        if broken {
                            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({})))
                        } else {
                            (StatusCode::OK, Json(document("Complete")))
                        }
                    }
                }),
            )
            .route(
                "/stable",
                get(move || {
                    hits.fetch_add(1, Ordering::SeqCst);
                    async { Json(json!({"sites":[]})) }
                }),
            ),
    )
    .await;
    let id = add(
        &app,
        &admin,
        json!({"name":"Nested","url":format!("{base}/root?private=token")}),
    )
    .await;
    add(
        &app,
        &admin,
        json!({"name":"Stable","url":format!("{base}/stable")}),
    )
    .await;
    assert_eq!(
        sync(&app, &admin, json!({})).await,
        json!({"synced":2,"failed":0})
    );
    let before = catalog(&app, CMS);
    fail.store(true, Ordering::SeqCst);
    let started = polyview::now();
    assert_eq!(
        sync(&app, &admin, json!({"id":id})).await,
        json!({"synced":0,"failed":1})
    );
    assert_eq!(catalog(&app, CMS), before);
    let rows = app.request("GET", FEEDS, Some(&admin), None).await.json();
    assert_eq!(rows[0]["item_count"], 4);
    assert!(!rows[0]["error"].as_str().unwrap().contains("token"));
    {
        let conn = app.state.conn().unwrap();
        let retry: i64 = conn
            .query_row(
                "SELECT next_sync_at FROM source_subscriptions WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert!((started + 900..=polyview::now() + 900).contains(&retry));
        let interval: i64 = conn
            .query_row(
                "SELECT next_sync_at-updated_at FROM source_subscriptions WHERE name='Stable'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(interval, 43_200);
        let applied: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM catalog_sources WHERE name='Partial')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!applied);
        conn.execute(
            "UPDATE source_subscriptions SET next_sync_at=0 WHERE id=?",
            [id],
        )
        .unwrap();
    }
    fail.store(false, Ordering::SeqCst);
    let worker = polyview::source_subscriptions::start_worker(app.state.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let recovered: bool = app
                .state
                .conn()
                .unwrap()
                .query_row(
                    "SELECT error IS NULL FROM source_subscriptions WHERE id=?",
                    [id],
                    |r| r.get(0),
                )
                .unwrap();
            if recovered {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    assert_eq!(stable_hits.load(Ordering::SeqCst), 1);
    assert_eq!(catalog(&app, CMS), before);
    server.abort();
}

#[tokio::test]
async fn manual_sync_and_background_share_claims_and_discard_results_after_edits() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let delayed = {
        let entered = entered.clone();
        let release = release.clone();
        move || {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                Json(document("Stale"))
            }
        }
    };
    let (base, server) = serve(
        Router::new()
            .route("/slow", get(delayed))
            .route("/new", get(|| async { Json(document("Current")) })),
    )
    .await;
    let old = json!({"name":"Old","url":format!("{base}/slow")});
    let id = add(&app, &admin, old.clone()).await;
    let worker = polyview::source_subscriptions::start_worker(app.state.clone());
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert_eq!(
        sync(&app, &admin, json!({"id":id})).await,
        json!({"synced":0,"failed":0})
    );
    let path = format!("{FEEDS}/{id}");
    assert_eq!(
        app.request(
            "PUT",
            &path,
            Some(&admin),
            Some(json!({"name":"New","url":format!("{base}/new")}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        sync(&app, &admin, json!({"id":id})).await,
        json!({"synced":1,"failed":0})
    );
    release.notify_one();
    // Run a second edit race with an awaitable manual request, checking the stale completion itself.
    app.request("PUT", &path, Some(&admin), Some(old)).await;
    let manual = sync(&app, &admin, json!({"id":id}));
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            app.request("DELETE", &path, Some(&admin), None)
                .await
                .status,
            StatusCode::NO_CONTENT
        );
        release.notify_one();
    };
    let (report, ()) = tokio::join!(manual, cancel);
    assert_eq!(report, json!({"synced":0,"failed":0}));
    let (_, name, enabled) = catalog(&app, CMS);
    assert_eq!(name, "Current");
    assert!(!enabled);
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn materialization_failure_rolls_back_earlier_rows_and_releases_the_claim() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let (base, server) =
        serve(Router::new().route("/config", get(|| async { Json(document("Movie")) }))).await;
    let id = add(
        &app,
        &admin,
        json!({"name":"Atomic","url":format!("{base}/config")}),
    )
    .await;
    app.state.conn().unwrap().execute_batch(
        "CREATE TRIGGER reject_parser BEFORE INSERT ON parsers BEGIN SELECT RAISE(ABORT, 'fixture'); END;",
    ).unwrap();
    assert_eq!(
        sync(&app, &admin, json!({"id":id})).await,
        json!({"synced":0,"failed":1})
    );
    {
        let conn = app.state.conn().unwrap();
        let count: i64 = conn.query_row("SELECT (SELECT count(*) FROM catalog_sources) + (SELECT count(*) FROM source_subscription_items) + (SELECT count(*) FROM subscription_owned_sources)", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0);
        conn.execute_batch("DROP TRIGGER reject_parser").unwrap();
    }
    assert_eq!(
        sync(&app, &admin, json!({"id":id})).await,
        json!({"synced":1,"failed":0})
    );
    assert!(catalog(&app, CMS).2);
    server.abort();
}
