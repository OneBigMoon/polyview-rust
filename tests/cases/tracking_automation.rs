use super::*;
use axum::{
    Json,
    extract::Query,
    response::Html,
    routing::{get, post},
};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

async fn call(app: &TestApp, user: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let reply = app.request(method, path, Some(user), body).await;
    assert!(
        reply.status.is_success(),
        "{} {}: {}",
        method,
        path,
        String::from_utf8_lossy(&reply.body)
    );
    reply.json()
}

async fn source(app: &TestApp, admin: &str, origin: &str) -> i64 {
    call(
        app,
        admin,
        "POST",
        "/api/sources",
        Some(json!({"url":format!("{origin}/cms"),"name":"家庭片库"})),
    )
    .await["id"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn tracking_filters_skip_rejected_releases_before_parser_or_queue() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let result = json!({"list":[{"vod_id":"9","vod_name":"航程","vod_play_from":"sd$$$qq", "vod_play_url":format!("第一集${origin}/sd.mp4$$$第一集 2160p HEVC${origin}/good#第二集 4K HEVC CAM${origin}/cam#第三集 720p HEVC${origin}/720#第四集 4K AVC${origin}/avc")} ]});
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let target = format!("{origin}/one.mp4");
    let fixture = Router::new()
        .route(
            "/cms",
            get(move || {
                let data = result.clone();
                async move { Json(data) }
            }),
        )
        .route(
            "/parser",
            get(move |Query(params): Query<HashMap<String, String>>| {
                let target = target.clone();
                let sent = sent.clone();
                async move {
                    sent.send(params["url"].clone()).unwrap();
                    Json(json!({"url":target}))
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let id = source(&app, &admin, &origin).await;
    call(&app,&admin,"POST","/api/parsers",Some(json!({"name":"parse","url_template":format!("{origin}/parser?url={{url}}"),"enabled":true,"flags":"qq"}))).await;
    let input = json!({"source_id":id,"media_id":"9","auto_download":true,"rules":{"quality":"4K","include_keywords":["hevc"],"exclude_keywords":["cam"]}});
    let sub = call(&app, &viewer, "POST", "/api/tracking", Some(input)).await;
    assert_eq!(sub["queued_episodes"], 1, "{sub}");
    assert_eq!(sub["total_episodes"], 4);
    assert_eq!(received.try_recv().unwrap(), format!("{origin}/good"));
    assert!(received.try_recv().is_err());
    let subs = call(&app, &viewer, "GET", "/api/tracking", None).await;
    assert_eq!(subs[0]["rules"]["include_keywords"], json!(["hevc"]));
    let path = format!("/api/tracking/{}", sub["id"]);
    assert_eq!(
        app.request("PUT", &path, Some(&admin), Some(json!({"rules":{}})))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    call(
        &app,
        &viewer,
        "PUT",
        &path,
        Some(json!({"rules":{"quality":"1080p"}})),
    )
    .await;
    let check = call(&app, &viewer, "POST", "/api/tracking/check", None).await;
    assert_eq!(check["errors"], json!([]));
    assert_eq!(check["queued"], 0);
    assert!(received.try_recv().is_err());
    server.abort();
}

fn wish_item(id: u32, title: &str) -> String {
    format!(
        r#"<div class="item"><div class="info"><ul><li class="title"><a href="https://movie.douban.com/subject/{id}/"><em>{title}</em> / Original title</a></li><li class="intro"> 2025-01-01 / 中国大陆</li></ul></div></div>"#
    )
}

#[tokio::test]
async fn wishlist_worker_imports_public_pages_per_member_and_retries_unmatched_subjects() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let blocked = Arc::new(AtomicBool::new(false));
    let block = blocked.clone();
    let updated = Arc::new(AtomicBool::new(false));
    let available = updated.clone();
    let searches = Arc::new(AtomicUsize::new(0));
    let count = searches.clone();
    let fixture = Router::new().route("/people/member42/wish",get(move |Query(q):Query<HashMap<String,String>>| {
        let html = if block.load(Ordering::Relaxed) { "<h1>访问受限</h1>".to_owned() } else if q["start"] == "0" {
            format!(r#"<div class="grid-view">{}</div><div class="paginator"><span class="next"><a href="?start=15">后页</a></span></div>"#,wish_item(901,"迷途"))
        } else { format!("<div class='grid-view'>{}{}</div>", wish_item(901,"迷途"),wish_item(902,"新星")) };
        async move { Html(html) }
    })).route("/cms",get(move |Query(q):Query<HashMap<String,String>>| {
        let mut rows = Vec::new();
        if let Some(title) = q.get("wd") {
            count.fetch_add(1,Ordering::Relaxed);
            if title == "迷途" { rows.push(json!({"vod_id":"a","vod_name":"迷途","vod_year":"1999"})); }
            if title == "迷途" || available.load(Ordering::Relaxed) { rows.push(json!({"vod_id":title,"vod_name":title,"vod_year":"2025"})); }
        } else { let id = &q["ids"]; rows.push(json!({"vod_id":id,"vod_name":id,"vod_year":"2025","vod_play_url":"正片$https://example.invalid/file.mp4"})); }
        async move { Json(json!({"code":1,"list":rows})) }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let app = TestApp::with_cloud_fixture(origin.parse().unwrap());
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    source(&app, &admin, &origin).await;
    let path = "/api/tracking/douban";
    let sync = "/api/tracking/douban/sync";
    for profile in ["../admin", "http://localhost/private", "other/wish?start=0"] {
        assert_eq!(
            app.request(
                "PUT",
                path,
                Some(&viewer),
                Some(json!({"profile":profile,"enabled":true}))
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    call(
        &app,
        &viewer,
        "PUT",
        path,
        Some(json!({"profile":"member42","enabled":true,"interval_hours":1})),
    )
    .await;
    let worker = polyview::tracking::start_worker(app.state.clone());
    let status = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = call(&app, &viewer, "GET", path, None).await;
            if !value["last_checked"].is_null() {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    assert_eq!(status["imported"], 1, "{status}");
    assert!(status["last_error"].as_str().unwrap().contains("新星"));
    let subs = call(&app, &viewer, "GET", "/api/tracking", None).await;
    assert_eq!(subs[0]["media_id"], "迷途");
    assert_eq!(call(&app, &admin, "GET", path, None).await["profile"], "");
    assert_eq!(
        call(&app, &admin, "GET", "/api/tracking", None).await,
        json!([])
    );
    updated.store(true, Ordering::Relaxed);
    let status = call(&app, &viewer, "POST", sync, None).await;
    assert_eq!(status["imported"], 2);
    assert!(status["last_error"].is_null());
    let before = searches.load(Ordering::Relaxed);
    call(&app, &viewer, "POST", sync, None).await;
    assert_eq!(searches.load(Ordering::Relaxed), before);
    let subs = call(&app, &viewer, "GET", "/api/tracking", None).await;
    assert_eq!(subs.as_array().unwrap().len(), 2);
    blocked.store(true, Ordering::Relaxed);
    assert_eq!(
        app.request("POST", sync, Some(&viewer), None).await.status,
        StatusCode::BAD_GATEWAY
    );
    assert!(
        call(&app, &viewer, "GET", path, None).await["last_error"]
            .as_str()
            .unwrap()
            .contains("公开")
    );
    server.abort();
}

#[tokio::test]
async fn postprocessing_requires_refresh_acknowledgement_and_retries_only_failed_channels() {
    use polyview::tracking::automation::postprocess_complete;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let attempts = Arc::new(AtomicUsize::new(0));
    let refresh = attempts.clone();
    let notices = Arc::new(AtomicUsize::new(0));
    let calls = notices.clone();
    let fixture = Router::new()
        .route(
            "/base/Library/Refresh",
            post(move |headers: HeaderMap| {
                assert_eq!(headers["x-emby-token"], "private-key");
                let status = if refresh.fetch_add(1, Ordering::Relaxed) == 0 {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::NO_CONTENT
                };
                async move { status }
            }),
        )
        .route(
            "/base/emby/Library/Refresh",
            post(|headers: HeaderMap| async move {
                assert_eq!(headers["x-emby-token"], "private-key");
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/notify",
            post(move |Json(msg): Json<Value>| {
                assert_eq!(msg["title"], "媒体处理完成");
                assert_eq!(msg["body"], "片名");
                let status = if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::OK
                };
                async move { status }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let path = "/api/settings/media-server";
    let config = json!({"kind":"jellyfin","url":format!("{origin}/base"),"api_key":"private-key","enabled":true});
    for method in ["GET", "PUT"] {
        assert_eq!(
            app.request(
                method,
                path,
                Some(&viewer),
                (method == "PUT").then(|| config.clone())
            )
            .await
            .status,
            StatusCode::FORBIDDEN
        );
    }
    let saved = call(&app, &admin, "PUT", path, Some(config)).await;
    assert_eq!(saved["has_api_key"], true);
    assert!(!saved.to_string().contains("private-key"));
    call(
        &app,
        &admin,
        "PUT",
        "/api/settings/notify",
        Some(json!({"webhook_post_url":format!("{origin}/notify")})),
    )
    .await;
    app.state.conn().unwrap().execute_batch("INSERT INTO library_media(id,title,relative_path,root,kind,size) VALUES(1,'片名','one.mp4','media','movie',1),(2,'片名','two.mp4','media','movie',1)").unwrap();
    let rejected = postprocess_complete(&app.state, 1, "片名")
        .await
        .unwrap_err();
    assert!(rejected.1.contains("403"));
    assert_eq!(notices.load(Ordering::Relaxed), 0);
    let rejected = postprocess_complete(&app.state, 1, "片名")
        .await
        .unwrap_err();
    assert!(rejected.1.contains("503"));
    postprocess_complete(&app.state, 1, "片名").await.unwrap();
    postprocess_complete(&app.state, 1, "片名").await.unwrap();
    assert_eq!(attempts.load(Ordering::Relaxed), 2);
    assert_eq!(notices.load(Ordering::Relaxed), 2);
    call(
        &app,
        &admin,
        "PUT",
        path,
        Some(json!({"kind":"emby","url":format!("{origin}/base"),"enabled":true})),
    )
    .await;
    postprocess_complete(&app.state, 2, "片名").await.unwrap();
    assert_eq!(notices.load(Ordering::Relaxed), 3);
    let mut backup = call(&app, &admin, "GET", "/api/settings/export", None).await;
    assert_eq!(
        backup["automation"]["tracking"]["media_server"]["api_key"],
        "private-key"
    );
    backup["automation"]["tracking"]["media_server"]["url"] = json!("file:///etc");
    assert_eq!(
        app.request("POST", "/api/settings/import", Some(&admin), Some(backup))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, &admin, "GET", path, None).await["url"],
        format!("{origin}/base")
    );
    let moved = json!({"kind":"emby","url":"https://new-server.invalid","enabled":true});
    assert_eq!(
        app.request("PUT", path, Some(&admin), Some(moved))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let cleared = call(
        &app,
        &admin,
        "PUT",
        path,
        Some(json!({"kind":"emby","url":"https://new-server.invalid","enabled":false})),
    )
    .await;
    assert_eq!(cleared["has_api_key"], false);
    server.abort();
}
