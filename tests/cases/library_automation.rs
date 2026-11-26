use super::*;
use axum::routing::get;
use base64::Engine;
use std::{
    fs,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::sync::Notify;

async fn api(app: &TestApp, cookie: &str, method: &str, path: &str, data: Option<Value>) -> Value {
    let response = app.request(method, path, Some(cookie), data).await;
    assert!(
        response.status.is_success(),
        "{method} {path}: {}",
        String::from_utf8_lossy(&response.body)
    );
    response.json()
}

async fn processed(app: &TestApp, cookie: &str, path: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let item = api(app, cookie, "GET", path, None).await;
            if item["automation_status"] == "done" {
                return item;
            }
            if item["automation_status"] == "retry" {
                let diagnostic: String = app
                    .state
                    .conn()
                    .unwrap()
                    .query_row(
                        "SELECT error FROM library_automation WHERE media_id=?",
                        [item["id"].as_i64().unwrap()],
                        |r| r.get(0),
                    )
                    .unwrap();
                panic!("library processing failed: {diagnostic}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("library automation did not finish")
}

#[tokio::test]
async fn discovered_release_keeps_watch_identity_and_publishes_local_art_to_web_and_tvbox() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let searches = Arc::new(AtomicUsize::new(0));
    let arrived = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jCJkAAAAASUVORK5CYII=").unwrap();
    let art_requested = Arc::new(Notify::new());
    let art_ready = Arc::new(tokio::sync::Semaphore::new(0));
    let mock = Router::new()
        .route("/search/movie",get({
            let searches = searches.clone();
            let arrived = arrived.clone();
            let release = release.clone();
            move |axum::extract::Query(query):axum::extract::Query<std::collections::HashMap<String,String>>| async move {
                assert_eq!(query["query"],"Space Story");
                assert_eq!(query["year"],"2024");
                searches.fetch_add(1,Ordering::SeqCst);
                arrived.notify_one();
                release.notified().await;
                axum::Json(json!({"results":[{"id":37}]}))
            }
        }))
        .route("/movie/37",get(|| async {axum::Json(json!({"title":"星河","original_title":"Space Story","release_date":"2024-04-20","vote_average":8.4,"overview":"太空旅行","genres":[{"name":"科幻"}],"poster_path":"/image.png","backdrop_path":"/image.png"}))}))
        .route("/movie/37/credits",get(|| async {axum::Json(json!({"cast":[{"name":"陈舟","character":"船长"}],"crew":[{"name":"林星","job":"Director"}]}))}))
        .route("/image.png",get({
            let image=png.clone();
            let requested=art_requested.clone();
            let ready=art_ready.clone();
            move || async move {
                requested.notify_one();
                let _permit=ready.acquire().await.unwrap();
                ([(header::CONTENT_TYPE,"image/png")],image)
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    api(&app,&admin,"PUT","/api/settings/scraper",Some(json!({"enabled":true,"tmdb_key":"fixture","tmdb_url":origin,"image_url":origin,"douban_url":"","bangumi_url":""}))).await;
    api(
        &app,
        &admin,
        "PUT",
        "/api/settings/organizer",
        Some(json!({"auto_organize":true})),
    )
    .await;
    let source = app
        .state
        .media_dir
        .join("[Released] Space.Story.2024.2160p.x265.mkv");
    fs::write(&source, b"test film bytes").unwrap();
    let worker = polyview::library::start_automation_worker(app.state.clone());
    tokio::time::timeout(Duration::from_secs(5), arrived.notified())
        .await
        .expect("periodic discovery must reach the TMDB fixture");
    // These requests must finish while the remote lookup is blocked.
    let id = api(&app, &viewer, "GET", "/api/library", None).await[0]["id"]
        .as_i64()
        .unwrap();
    let detail_path = format!("/api/library/{id}");
    api(
        &app,
        &viewer,
        "PUT",
        &format!("{detail_path}/favorite"),
        Some(json!({"favorite":true})),
    )
    .await;
    api(
        &app,
        &viewer,
        "PUT",
        &format!("{detail_path}/progress"),
        Some(json!({"position":24,"duration":200,"watched":false})),
    )
    .await;
    assert_eq!(
        app.request(
            "POST",
            &format!("{detail_path}/scrape"),
            Some(&viewer),
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request(
            "POST",
            &format!("{detail_path}/reidentify"),
            Some(&viewer),
            Some(json!({"title":"other"}))
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), art_requested.notified())
        .await
        .expect("artwork phase after organization");
    worker.abort();
    let _ = worker.await;
    art_ready.add_permits(1);
    let worker = polyview::library::start_automation_worker(app.state.clone());
    let detail = processed(&app, &viewer, &detail_path).await;
    worker.abort();
    let _ = worker.await;
    assert_eq!(detail["id"], id);
    assert_eq!(detail["title"], "星河");
    assert_eq!(detail["favorite"], true);
    assert_eq!(detail["position"], 24.0);
    assert_eq!(detail["rating"], 8.4);
    assert_eq!(detail["cast"][0]["name"], "陈舟");
    assert_eq!(detail["genres"], json!(["科幻"]));
    assert_eq!(detail["original_title"], "Space Story");
    assert_eq!(detail["description"], "太空旅行");
    let organized = app
        .state
        .media_dir
        .join("Movies/星河 (2024)/星河 (2024).mkv");
    assert_eq!(fs::read(&organized).unwrap(), fs::read(&source).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            fs::metadata(&organized).unwrap().ino(),
            fs::metadata(&source).unwrap().ino()
        );
    }
    assert!(
        fs::read_to_string(organized.with_extension("nfo"))
            .unwrap()
            .contains("星河")
    );
    assert!(
        fs::read_dir(organized.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(
                |entry| entry.file_name().to_string_lossy().contains("poster")
                    && fs::read(entry.path()).unwrap() == png
            )
    );
    for field in ["cover", "backdrop"] {
        let path = detail[field].as_str().unwrap();
        assert_eq!(
            app.request("GET", path, None, None).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.request("GET", path, Some(&viewer), None).await.body,
            png
        );
    }
    let feed = api(&app, &viewer, "GET", "/api/tvbox", None).await;
    let catalog = app
        .request_with_headers(
            "GET",
            feed["catalog_path"].as_str().unwrap(),
            None,
            None,
            &[(header::HOST.as_str(), "tv.local")],
        )
        .await;
    assert_eq!(catalog.status, StatusCode::OK);
    let catalog = catalog.json();
    let vod = &catalog["list"][0];
    assert_eq!(vod["vod_name"], "星河");
    assert_eq!(vod["vod_actor"], "陈舟");
    assert_eq!(vod["vod_director"], "林星");
    assert_eq!(vod["vod_year"], "2024");
    assert_eq!(vod["vod_class"], "科幻");
    assert_eq!(vod["vod_content"], "太空旅行");
    for field in ["vod_pic", "vod_backdrop"] {
        let url = url::Url::parse(vod[field].as_str().unwrap()).unwrap();
        assert_eq!(app.request("GET", url.path(), None, None).await.body, png);
        let invalid = url
            .path()
            .replace(feed["token"].as_str().unwrap(), "invalid");
        assert!(
            !app.request("GET", &invalid, None, None)
                .await
                .status
                .is_success()
        );
    }
    let scanned = api(&app, &admin, "POST", "/api/library/scan", None).await;
    assert_eq!(scanned["total"], 1);
    assert_eq!(scanned["added"], 0);
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", "星河")
        .finish();
    let list = api(&app, &viewer, "GET", &format!("/api/library?{query}"), None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);
    assert_eq!(searches.load(Ordering::SeqCst), 1);
    let (jobs, done): (i64, bool) = app
        .state
        .conn()
        .unwrap()
        .query_row(
            "SELECT count(*),min(status='done') FROM library_automation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(jobs, 1);
    assert!(done);
    for (action, body) in [
        (
            "reidentify",
            Some(json!({"title":"Space Story","year":"2024"})),
        ),
        ("scrape", None),
    ] {
        let queued = app
            .request(
                "POST",
                &format!("{detail_path}/{action}"),
                Some(&admin),
                body,
            )
            .await;
        assert_eq!(queued.status, StatusCode::ACCEPTED);
    }
    server.abort();
}

#[tokio::test]
async fn season_metadata_and_local_episodes_reach_web_and_grouped_tvbox_detail() {
    let app = TestApp::new();
    let admin = app.setup().await;
    for number in [1, 2] {
        fs::write(
            app.state
                .media_dir
                .join(format!("Voyager.S02E{number:02}.mkv")),
            b"episode",
        )
        .unwrap();
    }
    api(&app, &admin, "POST", "/api/library/scan", None).await;
    let list = api(&app, &admin, "GET", "/api/library", None).await;
    let later = list[1]["id"].as_i64().unwrap();
    let mock = Router::new()
        .route("/tv/9", get(|| async { axum::Json(json!({"name":"航行者", "seasons":[{"season_number":2}]})) }))
        .route("/tv/9/credits", get(|| async { axum::Json(json!({"cast":[],"crew":[]})) }))
        .route("/tv/9/season/2", get(|| async { axum::Json(json!({"name":"第二季", "episodes":[{"episode_number":1,"name":"出发"},{"episode_number":2,"name":"归航"}]})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    api(
        &app,
        &admin,
        "PUT",
        "/api/settings/scraper",
        Some(json!({"tmdb_key":"fixture", "tmdb_url":origin, "bangumi_url":"", "douban_url":""})),
    )
    .await;
    let path = format!("/api/library/{later}");
    api(
        &app,
        &admin,
        "POST",
        &format!("{path}/reidentify"),
        Some(json!({"tmdb_id":9})),
    )
    .await;
    let worker = polyview::library::start_automation_worker(app.state.clone());
    let detail = processed(&app, &admin, &path).await;
    assert_eq!(detail["series"], "航行者");
    assert_eq!(detail["title"], "归航");
    assert_eq!(detail["seasons"][0]["episodes"][0]["title"], "出发");
    assert_eq!(detail["episodes"].as_array().unwrap().len(), 2);
    let feed = api(&app, &admin, "GET", "/api/tvbox", None).await;
    let catalog = app
        .request_with_headers(
            "GET",
            feed["catalog_path"].as_str().unwrap(),
            None,
            None,
            &[(header::HOST.as_str(), "tv.local")],
        )
        .await
        .json();
    let show = &catalog["list"][0];
    assert_eq!(catalog["list"].as_array().unwrap().len(), 1);
    assert_eq!(show["vod_name"], "航行者");
    assert_eq!(show["seasons"], detail["seasons"]);
    let lines = show["vod_play_url"].as_str().unwrap();
    assert_eq!(lines.split('#').count(), 2);
    assert!(lines.contains("S02E02 归航$http://tv.local/tvbox/"));
    worker.abort();
    server.abort();
}

#[tokio::test]
async fn native_queue_cleanup_preserves_pending_organization_and_media_identity() {
    let app = TestApp::new();
    let admin = app.setup().await;
    api(
        &app,
        &admin,
        "PUT",
        "/api/settings/organizer",
        Some(json!({"auto_organize":true,"mode":"move"})),
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/film.mp4", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/film.mp4", get(|| async { "queue payload" })),
        )
        .await
        .unwrap();
    });
    let download = api(
        &app,
        &admin,
        "POST",
        "/api/downloads",
        Some(json!({"url":url,"title":"Voyager.2020.1080p"})),
    )
    .await;
    let native = polyview::network::start_worker(app.state.clone());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let jobs = api(&app, &admin, "GET", "/api/downloads", None).await;
            if jobs[0]["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("native queue did not publish");
    native.abort();
    api(&app, &admin, "DELETE", "/api/downloads/completed", None).await;
    let items = api(&app, &admin, "GET", "/api/library", None).await;
    let id = items[0]["id"].as_i64().unwrap();
    let path = format!("/api/library/{id}");
    let pending = api(&app, &admin, "GET", &path, None).await;
    assert_eq!(pending["automation_status"], "queued");
    assert_eq!(pending["relative_path"], download["relative_path"]);
    let worker = polyview::library::start_automation_worker(app.state.clone());
    let moved = processed(&app, &admin, &path).await;
    assert_eq!(moved["id"], id);
    assert_eq!(
        moved["relative_path"],
        "Movies/Voyager (2020)/Voyager (2020).mp4"
    );
    assert!(
        !app.state
            .media_dir
            .join(download["relative_path"].as_str().unwrap())
            .exists()
    );
    assert_eq!(
        fs::read(
            app.state
                .media_dir
                .join(moved["relative_path"].as_str().unwrap())
        )
        .unwrap(),
        b"queue payload"
    );
    worker.abort();
    server.abort();
}
