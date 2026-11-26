use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Query, State},
    http::{HeaderMap, Request, StatusCode, header},
    routing::get,
};
use polyview::{AppState, Shared, init_schema, network, router};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

const VIDEO_BYTES: &[u8] = b"fixture-video-file-0123456789";

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    data: Option<Value>,
    range: Option<&str>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "polyview.test")
        .header("x-polyview-request", "1");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(range) = range {
        builder = builder.header(header::RANGE, range);
    }
    let body = if let Some(data) = data {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(&data).unwrap())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: to_bytes(body, 1024 * 1024).await.unwrap().to_vec(),
    }
}

fn app_state(directory: &Path) -> Shared {
    let media_dir = directory.join("media");
    std::fs::create_dir_all(media_dir.join("downloads")).unwrap();
    let media_dir = std::fs::canonicalize(media_dir).unwrap();
    let conn = Connection::open(directory.join("polyview.sqlite3")).unwrap();
    init_schema(&conn).unwrap();
    Arc::new(AppState {
        db: Mutex::new(conn),
        download_runtime: Default::default(),
        download_dir: media_dir.join("downloads"),
        media_dir,
        client: reqwest::Client::new(),
        secure_cookie: false,
    })
}

async fn wait_for_downloads(app: &Router, cookie: &str, expected: usize) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let downloads = request(app, "GET", "/api/downloads", Some(cookie), None, None)
                .await
                .json();
            let jobs = downloads.as_array().unwrap();
            assert!(
                jobs.iter().all(|job| job["status"] != "failed"),
                "Download failed: {downloads}"
            );
            let catalog = request(app, "GET", "/api/library", Some(cookie), None, None)
                .await
                .json();
            if jobs.len() == expected
                && jobs.iter().all(|job| job["status"] == "completed")
                && catalog.as_array().unwrap().len() == expected
            {
                return catalog;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("download worker and automatic library scan timed out")
}

#[derive(Clone)]
struct Upstream {
    episode_count: Arc<Mutex<usize>>,
    origin: String,
}

async fn cms(
    State(upstream): State<Upstream>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    assert_eq!(query.get("ac").map(String::as_str), Some("detail"));
    let count = *upstream.episode_count.lock().unwrap();
    let episodes = (1..=count)
        .map(|episode| format!("第{episode}集${}/episode-{episode}.mp4", upstream.origin))
        .collect::<Vec<_>>()
        .join("#");
    Json(json!({
        "code":1,"page":1,"pagecount":1,"total":1,
        "class":[{"type_id":2,"type_name":"电视剧"}],
        "list":[{
            "vod_id":42,"vod_name":"在线追剧验证","vod_pic":"","vod_year":"2026",
            "vod_play_from":"文件线路","vod_play_url":episodes
        }]
    }))
}

#[tokio::test]
async fn online_search_subscription_download_web_and_tv_feed_survive_restart() {
    let temp = tempfile::tempdir().unwrap();
    let state = app_state(temp.path());
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = Upstream {
        episode_count: Arc::new(Mutex::new(2)),
        origin: format!("http://{}", listener.local_addr().unwrap()),
    };
    let fixture = Router::new()
        .route("/cms", get(cms))
        .route(
            "/{file}",
            get(|| async { ([(header::CONTENT_TYPE, "video/mp4")], VIDEO_BYTES) }),
        )
        .with_state(upstream.clone());
    let source_server = tokio::spawn(async move { axum::serve(listener, fixture).await.unwrap() });
    let setup = request(
        &app,
        "POST",
        "/api/auth/setup",
        None,
        Some(json!({"username":"online_admin","password":"online-flow-password"})),
        None,
    )
    .await;
    assert_eq!(setup.status, StatusCode::OK);
    let cookie = setup.headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let source = request(
        &app,
        "POST",
        "/api/sources",
        Some(&cookie),
        Some(json!({"name":"本地 CMS 验证","url":format!("{}/cms",upstream.origin)})),
        None,
    )
    .await;
    assert_eq!(source.status, StatusCode::CREATED);
    let source_id = source.json()["id"].as_i64().unwrap();
    let results = request(
        &app,
        "GET",
        "/api/catalog/search?q=verification",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(results.status, StatusCode::OK);
    assert_eq!(results.json()["results"][0]["id"], "42");
    let detail = request(
        &app,
        "GET",
        &format!("/api/catalog/{source_id}/detail?id=42"),
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(
        detail.json()["groups"][0]["episodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let subscription = request(
        &app,
        "POST",
        "/api/tracking",
        Some(&cookie),
        Some(json!({"source_id":source_id,"media_id":"42","auto_download":true})),
        None,
    )
    .await;
    assert_eq!(subscription.status, StatusCode::CREATED);
    assert_eq!(subscription.json()["queued_episodes"], 2);
    assert!(subscription.json()["error"].is_null());
    let worker = network::start_worker(state.clone());
    let catalog = wait_for_downloads(&app, &cookie, 2).await;
    assert_eq!(catalog[0]["kind"], "episode");
    let media_id = catalog[0]["id"].as_i64().unwrap();
    let web_stream = request(
        &app,
        "GET",
        &format!("/api/library/{media_id}/stream"),
        Some(&cookie),
        None,
        Some("bytes=2-9"),
    )
    .await;
    assert_eq!(web_stream.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(web_stream.body, &VIDEO_BYTES[2..10]);
    *upstream.episode_count.lock().unwrap() = 3;
    let checked = request(
        &app,
        "POST",
        "/api/tracking/check",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(checked.status, StatusCode::OK);
    assert_eq!(checked.json()["queued"], 1);
    wait_for_downloads(&app, &cookie, 3).await;
    let second_check = request(
        &app,
        "POST",
        "/api/tracking/check",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(second_check.json()["queued"], 0);
    let feed = request(&app, "GET", "/api/tvbox", Some(&cookie), None, None)
        .await
        .json();
    let config_path = feed["config_path"].as_str().unwrap();
    let tv_config = request(&app, "GET", config_path, None, None, None).await;
    assert_eq!(tv_config.status, StatusCode::OK);
    assert_eq!(tv_config.json()["sites"].as_array().unwrap().len(), 2);
    let tv_catalog = request(
        &app,
        "GET",
        feed["catalog_path"].as_str().unwrap(),
        None,
        None,
        None,
    )
    .await
    .json();
    let tv_item = &tv_catalog["list"][0];
    assert_eq!(tv_item["vod_name"], "在线追剧验证");
    let urls = tv_item["vod_play_url"]
        .as_str()
        .unwrap()
        .split('#')
        .collect::<Vec<_>>();
    assert_eq!(urls.len(), 3);
    let first_url = urls[0].split_once('$').unwrap().1;
    let first_url = url::Url::parse(first_url).unwrap();
    let tv_stream = request(&app, "GET", first_url.path(), None, None, Some("bytes=0-6")).await;
    assert_eq!(tv_stream.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(tv_stream.body, &VIDEO_BYTES[..7]);
    worker.abort();
    let _ = worker.await;
    let restarted = router(app_state(temp.path()));
    let tracking = request(
        &restarted,
        "GET",
        "/api/tracking",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(tracking.status, StatusCode::OK);
    assert_eq!(tracking.json()[0]["total_episodes"], 3);
    assert_eq!(tracking.json()[0]["completed_downloads"], 3);
    assert_eq!(
        request(&restarted, "GET", config_path, None, None, None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &restarted,
            "POST",
            "/api/tracking/check",
            Some(&cookie),
            None,
            None
        )
        .await
        .json()["queued"],
        0
    );
    source_server.abort();
}
