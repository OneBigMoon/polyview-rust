use super::*;
use axum::{Json, extract::Query, routing::get};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::timeout,
};

async fn api(app: &TestApp, cookie: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let reply = app.request(method, path, Some(cookie), body).await;
    assert!(
        reply.status.is_success(),
        "{method} {path}: {}",
        String::from_utf8_lossy(&reply.body)
    );
    reply.json()
}

fn catalog(flags: &str, playlists: &str) -> Json<Value> {
    Json(json!({"code":1,"list":[{
        "vod_id":"42","vod_name":"星河","vod_year":"2026","vod_content":"太空故事",
        "vod_play_from":flags,"vod_play_url":playlists
    }]}))
}

async fn configure(app: &TestApp, admin: &str, origin: &str) -> i64 {
    api(
        app,
        admin,
        "POST",
        "/api/parsers",
        Some(json!({
            "name":"fixture parser", "url_template":format!("{origin}/parse?url={{url}}"),
            "flags":"qq", "timeout_ms":10000, "enabled":true
        })),
    )
    .await;
    let source = api(
        app,
        admin,
        "POST",
        "/api/sources",
        Some(json!({"name":"fixture CMS","url":format!("{origin}/cms")})),
    )
    .await;
    source["id"].as_i64().unwrap()
}

#[tokio::test]
async fn tracked_mixed_playlist_resolves_only_share_pages_and_publishes_both_episodes() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let share = format!("{origin}/share?id=2&line=qq");
    let playlists = format!("另一线路${origin}/other$$$第一集${origin}/one.mp4#第二集${share}");
    let destination = format!("{origin}/two.mp4?ticket=private-ticket");
    const REFERER: &str = "https://fixture.invalid/private-referrer";
    let (parsed, mut requests) = mpsc::unbounded_channel();
    let available = Arc::new(AtomicBool::new(false));
    let parser_available = available.clone();
    let fixture = Router::new()
        .route(
            "/cms",
            get(move || async move { catalog("other$$$qq", &playlists) }),
        )
        .route(
            "/parse",
            get(move |Query(query): Query<HashMap<String, String>>| {
                let reply = json!({"url":destination,"headers":{"Referer":REFERER}});
                parsed.send(query["url"].clone()).unwrap();
                let status = if parser_available.load(Ordering::Relaxed) {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_GATEWAY
                };
                async move { (status, Json(reply)) }
            }),
        )
        .route("/one.mp4", get(|| async { "direct episode bytes" }))
        .route(
            "/two.mp4",
            get(
                |headers: HeaderMap, Query(query): Query<HashMap<String, String>>| async move {
                    let referer = headers.get(header::REFERER).and_then(|v| v.to_str().ok());
                    let ticket = query.get("ticket").map(String::as_str);
                    if referer == Some(REFERER) && ticket == Some("private-ticket") {
                        (StatusCode::OK, "resolved episode bytes")
                    } else {
                        (StatusCode::FORBIDDEN, "missing media credentials")
                    }
                },
            ),
        );
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let source = configure(&app, &admin, &origin).await;
    let tracking = api(
        &app,
        &viewer,
        "POST",
        "/api/tracking",
        Some(json!({"source_id":source,"media_id":"42","auto_download":true})),
    )
    .await;
    assert!(tracking["error"].is_string(), "{tracking}");
    assert_eq!(tracking["queued_episodes"], 0);
    let jobs = api(&app, &viewer, "GET", "/api/downloads", None).await;
    assert_eq!(jobs, json!([]));
    assert_eq!(requests.try_recv().unwrap(), share);
    available.store(true, Ordering::Relaxed);
    let retry = api(&app, &viewer, "POST", "/api/tracking/check", None).await;
    assert_eq!(retry["queued"], 2);
    assert_eq!(retry["errors"], json!([]));
    assert_eq!(requests.try_recv().unwrap(), share);
    assert!(requests.try_recv().is_err());
    let checked = api(&app, &viewer, "POST", "/api/tracking/check", None).await;
    assert_eq!(checked["queued"], 0);
    assert_eq!(checked["errors"], json!([]));
    let worker = polyview::network::start_worker(app.state.clone());
    let library = timeout(Duration::from_secs(10), async {
        loop {
            let jobs = api(&app, &viewer, "GET", "/api/downloads", None).await;
            let public = jobs.to_string();
            assert!(!public.contains("private-ticket") && !public.contains("private-referrer"));
            let jobs = jobs.as_array().unwrap();
            assert!(jobs.iter().all(|job| job["status"] != "failed"), "{jobs:?}");
            let library = api(&app, &viewer, "GET", "/api/library", None).await;
            if library.as_array().unwrap().len() == 2 {
                break library;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker did not publish both episodes");
    for (episode, bytes) in [(1, "direct episode bytes"), (2, "resolved episode bytes")] {
        let item = library
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["episode"] == episode)
            .unwrap();
        assert_eq!(item["series"], "星河");
        assert_eq!(item["description"], "太空故事");
        let path = format!("/api/library/{}/stream", item["id"]);
        let stream = app.request("GET", &path, Some(&viewer), None).await;
        assert_eq!(stream.status, StatusCode::OK);
        assert_eq!(stream.body, bytes.as_bytes());
    }
    worker.abort();
    server.abort();
}

async fn next_request(
    pending: &mut mpsc::UnboundedReceiver<oneshot::Sender<()>>,
) -> oneshot::Sender<()> {
    timeout(Duration::from_secs(5), pending.recv())
        .await
        .expect("resolver was never called")
        .expect("parser fixture stopped")
}

#[tokio::test]
async fn pending_resolution_respects_watch_edits_and_concurrent_checks_claim_once() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let cms_origin = origin.clone();
    let destination = format!("{origin}/episode.mp4");
    let revision = Arc::new(AtomicUsize::new(0));
    let (parsed, mut pending) = mpsc::unbounded_channel::<oneshot::Sender<()>>();
    let fixture = Router::new()
        .route(
            "/cms",
            get(move || {
                let revision = revision.fetch_add(1, Ordering::Relaxed);
                let playlist = format!("第一集${cms_origin}/share?revision={revision}");
                let reply = catalog("qq", &playlist);
                async move { reply }
            }),
        )
        .route(
            "/parse",
            get(move || {
                let (release, wait) = oneshot::channel();
                parsed.send(release).unwrap();
                let reply = json!({"url":destination});
                async move {
                    wait.await.unwrap();
                    Json(reply)
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let source = configure(&app, &admin, &origin).await;
    let tracking = api(
        &app,
        &admin,
        "POST",
        "/api/tracking",
        Some(json!({"source_id":source,"media_id":"42","auto_download":false})),
    )
    .await;
    assert_eq!(tracking["total_episodes"], 1);
    assert!(pending.try_recv().is_err());
    let path = format!("/api/tracking/{}", tracking["id"]);
    let (updated, ()) = tokio::join!(
        api(
            &app,
            &admin,
            "PUT",
            &path,
            Some(json!({"auto_download":true}))
        ),
        async {
            let release = next_request(&mut pending).await;
            assert!(app.state.db.try_lock().is_ok());
            let edited = api(
                &app,
                &admin,
                "PUT",
                &path,
                Some(json!({"watched_episodes":1})),
            )
            .await;
            assert_eq!(edited["watched_episodes"], 1);
            release.send(()).unwrap();
        }
    );
    assert_eq!(updated["queued_episodes"], 0);
    api(
        &app,
        &admin,
        "PUT",
        &path,
        Some(json!({"watched_episodes":0})),
    )
    .await;
    let check = || api(&app, &admin, "POST", "/api/tracking/check", None);
    let (a, b, ()) = tokio::join!(check(), check(), async {
        let first = next_request(&mut pending).await;
        let second = next_request(&mut pending).await;
        second.send(()).unwrap();
        first.send(()).unwrap();
    });
    assert_eq!(a["errors"], json!([]));
    assert_eq!(b["errors"], json!([]));
    assert_eq!(
        a["queued"].as_i64().unwrap() + b["queued"].as_i64().unwrap(),
        1
    );
    let jobs = api(&app, &admin, "GET", "/api/downloads", None).await;
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    let repeated = check().await;
    assert_eq!(repeated["queued"], 0);
    assert_eq!(repeated["errors"], json!([]));
    assert!(pending.try_recv().is_err());
    server.abort();
}
