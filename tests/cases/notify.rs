use super::*;
use axum::{
    Json,
    extract::{Path, Query},
    routing::{get, post},
};
use std::{collections::HashMap, time::Duration};
use tokio::{
    sync::{Barrier, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

const SETTINGS: &str = "/api/settings/notify";
const TEST: &str = "/api/settings/notify/test";

async fn serve(app: Router) -> (String, JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (base, task)
}

async fn receive<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn only_administrators_can_read_change_or_test_notification_settings() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    for (method, uri, body) in [
        ("GET", SETTINGS, None),
        ("PUT", SETTINGS, Some(json!({}))),
        ("POST", TEST, None),
    ] {
        for (cookie, status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(viewer.as_str()), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                app.request(method, uri, cookie, body.clone()).await.status,
                status
            );
        }
    }
    let empty = json!({"bark_url":"", "webhook_post_url":"", "webhook_get_url":""});
    assert_eq!(
        app.request("GET", SETTINGS, Some(&admin), None)
            .await
            .json(),
        empty
    );
    assert_eq!(
        app.request("POST", TEST, Some(&admin), None).await.json(),
        json!({"results":[]})
    );
    for (field, url) in [
        ("bark_url", "file:///tmp"),
        ("webhook_post_url", "missing host"),
        ("webhook_get_url", "https://site.test/\nsecret"),
    ] {
        assert_eq!(
            app.request("PUT", SETTINGS, Some(&admin), Some(json!({(field):url})))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        app.request("GET", SETTINGS, Some(&admin), None)
            .await
            .json(),
        empty
    );
}

#[tokio::test]
async fn notification_requests_reach_all_three_sinks_concurrently_and_report_individual_results() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let barrier = Arc::new(Barrier::new(3));
    let bark_barrier = barrier.clone();
    let post_barrier = barrier.clone();
    let (bark, mut bark_rx) = mpsc::unbounded_channel();
    let (post_sink, mut post_rx) = mpsc::unbounded_channel();
    let (get_sink, mut get_rx) = mpsc::unbounded_channel();
    let (bark_url, bark_task) = serve(Router::new().route(
        "/device/{title}/{body}",
        get(move |Path(pair): Path<(String, String)>| {
            let barrier = bark_barrier.clone();
            bark.send(pair).unwrap();
            async move {
                barrier.wait().await;
                StatusCode::OK
            }
        }),
    ))
    .await;
    let (post_url, post_task) = serve(Router::new().route(
        "/post",
        post(move |Json(payload): Json<Value>| {
            let barrier = post_barrier.clone();
            post_sink.send(payload).unwrap();
            async move {
                barrier.wait().await;
                StatusCode::NO_CONTENT
            }
        }),
    ))
    .await;
    let (get_url, get_task) = serve(Router::new().route(
        "/get",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let barrier = barrier.clone();
            let fail = params.contains_key("fail");
            get_sink.send(params).unwrap();
            async move {
                barrier.wait().await;
                if fail {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::OK
                }
            }
        }),
    ))
    .await;
    let mut settings = json!({"bark_url":format!("{bark_url}/device/"), "webhook_post_url":format!("{post_url}/post"), "webhook_get_url":format!("{get_url}/get?key=existing")});
    assert_eq!(
        app.request("PUT", SETTINGS, Some(&admin), Some(settings.clone()))
            .await
            .json(),
        settings
    );
    assert_eq!(
        app.request("GET", SETTINGS, Some(&admin), None)
            .await
            .json(),
        settings
    );
    let title = "标题 / 100%";
    let body = "第 2 集 / ?&# +";
    let reports = polyview::notify::send_notification(&app.state, title, body)
        .await
        .unwrap();
    assert_eq!(reports.len(), 3);
    assert!(reports.iter().all(|report| report.ok));
    assert_eq!(receive(&mut bark_rx).await, (title.into(), body.into()));
    assert_eq!(
        receive(&mut post_rx).await,
        json!({"title":title,"body":body,"text":format!("{title}\n{body}")})
    );
    let query = receive(&mut get_rx).await;
    assert_eq!(query["key"], "existing");
    assert_eq!(query["title"], title);
    assert_eq!(query["body"], body);

    settings["webhook_get_url"] = json!(format!("{get_url}/get?fail=1"));
    assert_eq!(
        app.request("PUT", SETTINGS, Some(&admin), Some(settings))
            .await
            .status,
        StatusCode::OK
    );
    let reply = app.request("POST", TEST, Some(&admin), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let report = reply.json();
    let results = report["results"].as_array().unwrap();
    assert_eq!(results.len(), 3);
    for (result, (channel, ok)) in results.iter().zip([
        ("bark", true),
        ("webhook_post", true),
        ("webhook_get", false),
    ]) {
        assert_eq!(result["channel"], channel);
        assert_eq!(result["ok"], ok);
        assert!(!result["message"].as_str().unwrap().is_empty());
    }
    assert!(results[2]["message"].as_str().unwrap().contains("502"));
    bark_task.abort();
    post_task.abort();
    get_task.abort();
}

async fn configure_post_sink(
    app: &TestApp,
    admin: &str,
) -> (mpsc::UnboundedReceiver<Value>, JoinHandle<()>) {
    let (events, receiver) = mpsc::unbounded_channel();
    let (base, task) = serve(Router::new().route(
        "/notify",
        post(move |Json(payload): Json<Value>| {
            events.send(payload).unwrap();
            async { StatusCode::OK }
        }),
    ))
    .await;
    let settings = json!({"webhook_post_url": format!("{base}/notify")});
    assert_eq!(
        app.request("PUT", SETTINGS, Some(admin), Some(settings))
            .await
            .status,
        StatusCode::OK
    );
    (receiver, task)
}

fn episodes(count: usize) -> Value {
    let urls: Vec<_> = (1..=count)
        .map(|i| format!("第{i}集$http://127.0.0.1/{i}.mp4"))
        .collect();
    json!({"code":1,"list":[{"vod_id":"42","vod_name":"故事","vod_play_from":"文件","vod_play_url":urls.join("#")}]})
}

#[test]
fn tracking_migration_preserves_existing_progress_on_repeated_startup() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE tracking_subscriptions(id INTEGER PRIMARY KEY, user_id INTEGER, watched_episodes INTEGER);
        INSERT INTO tracking_subscriptions VALUES(8,1,4);").unwrap();
    polyview::tracking::init_schema(&conn).unwrap();
    polyview::tracking::init_schema(&conn).unwrap();
    let values: (i64, bool, i64) = conn.query_row("SELECT watched_episodes,paused,notified_episodes FROM tracking_subscriptions WHERE id=8", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
    assert_eq!(values, (4, false, 0));
}

#[tokio::test]
async fn paused_and_overlapping_tracking_checks_cannot_duplicate_deliveries_or_queue_work_while_paused()
 {
    let app = TestApp::new();
    let admin = app.setup().await;
    let (mut notices, sink) = configure_post_sink(&app, &admin).await;
    let (queries, mut pending) = mpsc::unbounded_channel::<oneshot::Sender<Value>>();
    let (base, cms) = serve(Router::new().route(
        "/cms",
        get(move || {
            let (respond, wait) = oneshot::channel();
            queries.send(respond).unwrap();
            async move { Json(wait.await.unwrap()) }
        }),
    ))
    .await;
    let source = app
        .request(
            "POST",
            "/api/sources",
            Some(&admin),
            Some(json!({"name":"fixture","url":format!("{base}/cms")})),
        )
        .await
        .json();
    let create = app.request(
        "POST",
        "/api/tracking",
        Some(&admin),
        Some(json!({"source_id":source["id"],"media_id":"42","auto_download":true})),
    );
    let (created, ()) = tokio::join!(create, async {
        receive(&mut pending).await.send(episodes(1)).unwrap();
    });
    assert_eq!(created.status, StatusCode::CREATED);
    let initial = created.json();
    let uri = format!("/api/tracking/{}", initial["id"]);
    assert_eq!(initial["paused"], false);
    assert!(
        receive(&mut notices).await["body"]
            .as_str()
            .unwrap()
            .contains("1 集")
    );
    let check = || app.request("POST", "/api/tracking/check", Some(&admin), None);
    let (checked, ()) = tokio::join!(check(), async {
        let respond = receive(&mut pending).await;
        let response = app
            .request("PUT", &uri, Some(&admin), Some(json!({"paused":true})))
            .await;
        assert_eq!(response.json()["paused"], true);
        respond.send(episodes(2)).unwrap();
    });
    assert_eq!(checked.json()["queued"], 0);
    assert_eq!(check().await.json()["checked"], 0);
    assert!(pending.try_recv().is_err());
    let row = &app
        .request("GET", "/api/tracking", Some(&admin), None)
        .await
        .json()[0];
    assert_eq!(row["total_episodes"], 1);
    assert_eq!(row["queued_episodes"], 1);
    assert_eq!(row["last_checked"], initial["last_checked"]);
    assert_eq!(
        app.request("PUT", &uri, Some(&admin), Some(json!({"paused":false})))
            .await
            .json()["paused"],
        false
    );
    let (failed_check, ()) = tokio::join!(check(), async {
        let respond = receive(&mut pending).await;
        assert_eq!(
            app.request("PUT", &uri, Some(&admin), Some(json!({"paused":true})))
                .await
                .json()["paused"],
            true
        );
        respond.send(json!({"code":1,"list":[]})).unwrap();
    });
    assert_eq!(failed_check.json()["errors"], json!([]));
    let row = &app
        .request("GET", "/api/tracking", Some(&admin), None)
        .await
        .json()[0];
    assert_eq!(row["error"], Value::Null);
    assert_eq!(
        app.request("PUT", &uri, Some(&admin), Some(json!({"paused":false})))
            .await
            .json()["paused"],
        false
    );
    let (a, b, ()) = tokio::join!(check(), check(), async {
        let first = receive(&mut pending).await;
        let second = receive(&mut pending).await;
        first.send(episodes(2)).unwrap();
        second.send(episodes(2)).unwrap();
    });
    assert_eq!(
        a.json()["queued"].as_u64().unwrap() + b.json()["queued"].as_u64().unwrap(),
        1
    );
    assert!(
        receive(&mut notices).await["body"]
            .as_str()
            .unwrap()
            .contains("2 集")
    );
    for count in [1, 2] {
        let (reply, ()) = tokio::join!(check(), async {
            receive(&mut pending).await.send(episodes(count)).unwrap();
        });
        assert_eq!(reply.json()["queued"], 0);
    }
    let notified: i64 = app
        .state
        .conn()
        .unwrap()
        .query_row(
            "SELECT notified_episodes FROM tracking_subscriptions",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(notified, 2);
    assert!(
        timeout(Duration::from_millis(150), notices.recv())
            .await
            .is_err()
    );
    cms.abort();
    sink.abort();
}

#[tokio::test]
async fn completed_download_sends_a_notification_after_library_scan() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let (mut notices, sink) = configure_post_sink(&app, &admin).await;
    let (base, media) =
        serve(Router::new().route("/movie.mp4", get(|| async { "movie bytes" }))).await;
    // Fail the first scan deterministically, then let the second download be indexed.
    app.state
        .conn()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER fail_first_scan BEFORE INSERT ON library_media
        WHEN (SELECT COUNT(*) FROM downloads WHERE status='completed')=1
        BEGIN SELECT RAISE(ABORT, 'offline fixture scan failure'); END;",
        )
        .unwrap();
    for title in ["首次扫描失败", "测试电影"] {
        let created = app
            .request(
                "POST",
                "/api/downloads",
                Some(&admin),
                Some(json!({"title":title,"url":format!("{base}/movie.mp4")})),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
    }
    let worker = polyview::network::start_worker(app.state.clone());
    let notice = receive(&mut notices).await;
    assert_eq!(notice["title"], "下载已入库");
    assert_eq!(notice["body"], "测试电影");
    let library = app
        .request("GET", "/api/library", Some(&admin), None)
        .await
        .json();
    assert!(
        library
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["title"] == "测试电影")
    );
    assert!(
        timeout(Duration::from_millis(150), notices.recv())
            .await
            .is_err()
    );
    worker.abort();
    media.abort();
    sink.abort();
}
