use super::*;
use axum::routing::get;
use std::time::Duration;

async fn json_request(
    app: &TestApp,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Value {
    let response = app.request(method, path, Some(cookie), body).await;
    assert!(
        response.status.is_success(),
        "{method} {path}: {}",
        String::from_utf8_lossy(&response.body)
    );
    response.json()
}

async fn wait_for_completion(app: &TestApp, cookie: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let jobs = json_request(app, cookie, "GET", "/api/downloads", None).await;
            match jobs[0]["status"].as_str() {
                Some("completed") => return jobs[0].clone(),
                Some("failed") => panic!("download failed: {jobs}"),
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("download did not complete")
}

async fn clear_history(app: &TestApp, cookie: &str) {
    let reply = app
        .request("DELETE", "/api/downloads/completed", Some(cookie), None)
        .await;
    assert!(
        reply.status.is_success(),
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        json_request(app, cookie, "GET", "/api/downloads", None).await,
        json!([])
    );
}

fn assert_metadata(item: &Value, cover: &str, year: &str, description: &str) {
    assert_eq!(item["cover"], cover);
    assert_eq!(item["year"], year);
    assert_eq!(item["description"], description);
}

#[tokio::test]
async fn published_metadata_survives_history_cleanup_and_populates_user_views() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/film.mp4", get(|| async { "video bytes" })),
        )
        .await
        .unwrap();
    });
    let cover = format!("{base}/cover.jpg");
    let download = json_request(&app, &admin, "POST", "/api/downloads", Some(json!({
        "url": format!("{base}/film.mp4"), "title": "星辰", "cover": cover, "year":"2026", "description":"星际旅行"
    }))).await;
    let relative = download["relative_path"].as_str().unwrap();
    // A catalog imported from another root can have the same relative filename.
    app.state.conn().unwrap().execute(
        "INSERT INTO library_media(title,relative_path,root,kind,size,cover) VALUES('Other',?,'download','movie',0,'other-cover')", [relative],
    ).unwrap();
    let worker = polyview::network::start_worker(app.state.clone());
    wait_for_completion(&app, &admin).await;
    clear_history(&app, &admin).await;
    json_request(&app, &admin, "POST", "/api/library/scan", None).await;
    let items = json_request(&app, &admin, "GET", "/api/library", None).await;
    assert_eq!(items.as_array().unwrap().len(), 1);
    let item = &items[0];
    assert_metadata(item, &cover, "2026", "星际旅行");
    assert_eq!(item["relative_path"], relative);
    assert_eq!(
        fs::read(app.state.media_dir.join(relative)).unwrap(),
        b"video bytes"
    );
    let other_cover: String = app
        .state
        .conn()
        .unwrap()
        .query_row(
            "SELECT cover FROM library_media WHERE root='download' AND relative_path=?",
            [relative],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_cover, "other-cover");

    let id = item["id"].as_i64().unwrap();
    json_request(
        &app,
        &admin,
        "PUT",
        &format!("/api/library/{id}/favorite"),
        Some(json!({"favorite":true})),
    )
    .await;
    json_request(
        &app,
        &admin,
        "PUT",
        &format!("/api/library/{id}/progress"),
        Some(json!({"position":15,"duration":90,"watched":false})),
    )
    .await;
    let favorites = json_request(&app, &admin, "GET", "/api/favorites", None).await;
    let progress = json_request(&app, &admin, "GET", "/api/progress", None).await;
    assert_metadata(&favorites["local"][0], &cover, "2026", "星际旅行");
    assert_metadata(&progress[0], &cover, "2026", "星际旅行");
    assert_eq!(progress[0]["local_id"], id);
    assert_eq!(
        json_request(&app, &viewer, "GET", "/api/favorites", None).await["local"],
        json!([])
    );
    assert_eq!(
        json_request(&app, &viewer, "GET", "/api/progress", None).await,
        json!([])
    );
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn tracked_catalog_metadata_reaches_the_library_and_cleanup_keeps_episode_history() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let cover = format!("{base}/p.jpg");
    let cms = json!({"code":1,"list":[{"vod_id":42,"vod_name":"海洋","vod_pic":"/p.jpg","vod_year":2024,
        "vod_content":"海底故事","vod_play_from":"files","vod_play_url":format!("第一集${base}/one.mp4")} ]});
    let server = tokio::spawn(async move {
        let fixture = Router::new()
            .route("/cms", get(move || async move { axum::Json(cms) }))
            .route("/one.mp4", get(|| async { "episode bytes" }));
        axum::serve(listener, fixture).await.unwrap();
    });
    let source = json_request(
        &app,
        &admin,
        "POST",
        "/api/sources",
        Some(json!({"name":"Fixture","url":format!("{base}/cms")})),
    )
    .await;
    let subscription = json_request(
        &app,
        &admin,
        "POST",
        "/api/tracking",
        Some(json!({"source_id":source["id"],"media_id":"42","auto_download":true})),
    )
    .await;
    assert_eq!(subscription["queued_episodes"], 1, "{subscription}");
    let worker = polyview::network::start_worker(app.state.clone());
    let job = wait_for_completion(&app, &admin).await;
    assert_metadata(&job, &cover, "2024", "海底故事");
    clear_history(&app, &admin).await;
    let check = json_request(&app, &admin, "POST", "/api/tracking/check", None).await;
    assert_eq!(check["queued"], 0);
    let subscriptions = json_request(&app, &admin, "GET", "/api/tracking", None).await;
    assert_eq!(subscriptions[0]["queued_episodes"], 1);
    assert_eq!(subscriptions[0]["completed_downloads"], 1);
    let library = json_request(&app, &admin, "GET", "/api/library", None).await;
    assert_eq!(library[0]["series"], "海洋");
    assert_eq!(library[0]["episode"], 1);
    assert_metadata(&library[0], &cover, "2024", "海底故事");
    server.abort();
    worker.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn crash_after_publication_recovers_the_existing_media_id_with_metadata() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let job = json_request(&app, &admin, "POST", "/api/downloads", Some(json!({"url":"http://127.0.0.1:1/video.mp4", "title":"Recovered",
        "cover":"https://images.invalid/poster.jpg","year":"1997","description":"Recovered synopsis"}))).await;
    let final_path = app
        .state
        .media_dir
        .join(job["relative_path"].as_str().unwrap());
    let part = final_path.with_extension("part");
    fs::write(&part, b"previously downloaded bytes").unwrap();
    fs::hard_link(&part, &final_path).unwrap();
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE downloads SET status='running',attempts=1 WHERE id=?",
            [job["id"].as_i64().unwrap()],
        )
        .unwrap();
    json_request(&app, &admin, "POST", "/api/library/scan", None).await;
    let before = json_request(&app, &admin, "GET", "/api/library", None).await;
    assert_metadata(&before[0], "", "", "");
    let worker = polyview::network::start_worker(app.state.clone());
    wait_for_completion(&app, &admin).await;
    clear_history(&app, &admin).await;
    json_request(&app, &admin, "POST", "/api/library/scan", None).await;
    let after = json_request(&app, &admin, "GET", "/api/library", None).await;
    assert_eq!(after[0]["id"], before[0]["id"]);
    assert_metadata(
        &after[0],
        "https://images.invalid/poster.jpg",
        "1997",
        "Recovered synopsis",
    );
    assert_eq!(
        fs::read(final_path).unwrap(),
        b"previously downloaded bytes"
    );
    worker.abort();
}

#[test]
fn legacy_media_and_tracking_links_survive_repeated_schema_migrations() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;
        CREATE TABLE library_series(id INTEGER PRIMARY KEY,title TEXT UNIQUE);
        CREATE TABLE library_media(id INTEGER PRIMARY KEY,title TEXT,relative_path TEXT,root TEXT,kind TEXT,
            series_id INTEGER REFERENCES library_series(id),season INTEGER,episode INTEGER,size INTEGER,
            available INTEGER DEFAULT 1,UNIQUE(root,relative_path));
        INSERT INTO library_media(id,title,relative_path,root,kind,size) VALUES(7,'Old','old.mp4','media','movie',4);
        CREATE TABLE downloads(id INTEGER PRIMARY KEY);
        INSERT INTO downloads VALUES(5);
        CREATE TABLE tracking_subscriptions(id INTEGER PRIMARY KEY,user_id INTEGER);
        INSERT INTO tracking_subscriptions VALUES(2,1);
        CREATE TABLE tracking_downloads(subscription_id INTEGER NOT NULL REFERENCES tracking_subscriptions(id) ON DELETE CASCADE,
            episode_key TEXT NOT NULL,download_id INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
            PRIMARY KEY(subscription_id,episode_key));
        INSERT INTO tracking_downloads VALUES(2,'name:pilot',5);").unwrap();
    for _ in 0..2 {
        polyview::library::init_schema(&conn).unwrap();
        polyview::tracking::init_schema(&conn).unwrap();
    }
    let media: (String, String, String, String) = conn
        .query_row(
            "SELECT title,cover,year,description FROM library_media WHERE id=7",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        media,
        ("Old".into(), String::new(), String::new(), String::new())
    );
    let linked = || {
        conn.query_row("SELECT download_id FROM tracking_downloads WHERE subscription_id=2 AND episode_key='name:pilot'", [], |r| r.get::<_,Option<i64>>(0)).unwrap()
    };
    assert_eq!(linked(), Some(5));
    conn.execute("DELETE FROM downloads WHERE id=5", [])
        .unwrap();
    assert_eq!(linked(), None);
}
