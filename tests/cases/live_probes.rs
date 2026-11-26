use super::*;
use axum::routing::get;
use futures_util::{StreamExt, stream};
use rusqlite::params;
use std::{convert::Infallible, time::Duration};
use tokio::sync::Notify;

const PROBE: &str = "/api/channels/probe";
const CLEANUP: &str = "/api/channels/cleanup";

async fn serve(routes: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    (base, task)
}

fn channel(app: &TestApp, name: &str, group: &str, base: &str, paths: &[&str]) -> i64 {
    let conn = app.state.conn().unwrap();
    conn.execute(
        "INSERT INTO channels(name,group_name,logo,url) VALUES(?1,?2,'',?3)",
        params![name, group, format!("{base}/{}", paths[0])],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    for (index, path) in paths.iter().enumerate() {
        conn.execute(
            "INSERT INTO channel_sources(channel_id,url,label,position) VALUES(?1,?2,?3,?4)",
            params![id, format!("{base}/{path}"), path, index],
        )
        .unwrap();
    }
    id
}

#[tokio::test]
async fn probes_check_first_bytes_with_private_headers_and_sort_only_the_requested_group() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let routes = Router::new()
        .route(
            "/slow",
            get(|| async {
                Body::from_stream(stream::once(async {
                    tokio::time::sleep(Duration::from_millis(220)).await;
                    Ok::<_, Infallible>("slow bytes")
                }))
            }),
        )
        .route(
            "/fast",
            get(|headers: HeaderMap| async move {
                if headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) != Some("key=private")
                {
                    return (StatusCode::FORBIDDEN, Body::empty());
                }
                let first = stream::iter([Ok::<_, Infallible>("immediate bytes")]);
                (
                    StatusCode::OK,
                    Body::from_stream(first.chain(stream::pending())),
                )
            }),
        )
        .route("/failed", get(|| async { StatusCode::BAD_GATEWAY }))
        .route(
            "/stall",
            get(|| async {
                Body::from_stream(stream::pending::<Result<&'static str, Infallible>>())
            }),
        );
    let (base, server) = serve(routes).await;
    let news = channel(
        &app,
        "News",
        "A",
        &base,
        &["slow", "failed", "stall", "fast?token=secret"],
    );
    channel(&app, "Other", "B", &base, &["untouched"]);
    {
        let conn = app.state.conn().unwrap();
        conn.execute(
            "UPDATE channel_sources SET headers_json=?1,fail_count=4 WHERE label='fast?token=secret'",
            [r#"{"Cookie":"key=private"}"#],
        )
        .unwrap();
        conn.execute(
            "UPDATE channel_sources SET fail_count=1,last_ok_at=123 WHERE label='failed'",
            [],
        )
        .unwrap();
    }
    let reply = tokio::time::timeout(
        Duration::from_secs(25),
        app.request("POST", PROBE, Some(&admin), Some(json!({"group":"A"}))),
    )
    .await
    .expect("probe bounded even when an upstream never produces a byte");
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"checked":4,"healthy":2,"failed":2}));
    let list = app
        .request("GET", "/api/channels", Some(&admin), None)
        .await
        .json();
    let rows = list[0]["sources"].as_array().unwrap();
    assert_eq!(rows[0]["label"], "fast?token=secret");
    assert_eq!(rows[0]["fail_count"], 0);
    assert!(rows[0]["last_ok_at"].as_i64().unwrap() > 123);
    assert_eq!(rows[1]["label"], "slow");
    assert!(rows[1]["latency_ms"].as_i64().unwrap() >= 200);
    assert!(rows[0]["latency_ms"].as_i64().unwrap() < rows[1]["latency_ms"].as_i64().unwrap());
    assert_eq!(rows[2]["label"], "failed");
    assert_eq!(rows[2]["fail_count"], 2);
    assert_eq!(rows[2]["last_ok_at"], 123);
    assert!(rows[2]["latency_ms"].is_null());
    assert_eq!(rows[3]["label"], "stall");
    assert_eq!(rows[3]["fail_count"], 1);
    assert!(list[1]["sources"][0]["last_ok_at"].is_null());
    assert_eq!(list[1]["sources"][0]["fail_count"], 0);
    let primary: String = app
        .state
        .conn()
        .unwrap()
        .query_row("SELECT url FROM channels WHERE id=?1", [news], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(primary, format!("{base}/fast?token=secret"));
    server.abort();
}

#[tokio::test]
async fn cleanup_removes_only_qualifying_lines_and_repairs_primary_and_empty_channels() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let base = "http://127.0.0.1";
    let news = channel(&app, "News", "A", base, &["dead", "recovering", "healthy"]);
    let empty = channel(&app, "Retired", "A", base, &["retired"]);
    channel(&app, "Other", "B", base, &["unrelated"]);
    {
        let conn = app.state.conn().unwrap();
        conn.execute_batch(
            "UPDATE channel_sources SET fail_count=CASE label WHEN 'dead' THEN 3 WHEN 'retired' THEN 4
             WHEN 'unrelated' THEN 3 WHEN 'recovering' THEN 2 ELSE 0 END;
             UPDATE channel_sources SET last_ok_at=123,latency_ms=10 WHERE label='healthy'",
        )
        .unwrap();
        for id in [news, empty] {
            conn.execute(
                "INSERT INTO channel_favorites(user_id,channel_id,created_at) VALUES(1,?1,123)",
                [id],
            )
            .unwrap();
        }
    }
    for expected in [2, 0] {
        let reply = app
            .request("POST", CLEANUP, Some(&admin), Some(json!({"group":"A"})))
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        assert_eq!(reply.json(), json!({"removed":expected}));
    }
    {
        let conn = app.state.conn().unwrap();
        polyview::init_schema(&conn).unwrap();
        let primary: String = conn
            .query_row("SELECT url FROM channels WHERE id=?1", [news], |r| r.get(0))
            .unwrap();
        assert_eq!(primary, format!("{base}/healthy"));
        let favorite: i64 = conn
            .query_row("SELECT channel_id FROM channel_favorites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(favorite, news);
        let count: i64 = conn
            .query_row("SELECT count(*) FROM channel_favorites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
    let list = app
        .request("GET", "/api/channels", Some(&admin), None)
        .await
        .json();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["sources"].as_array().unwrap().len(), 2);
    assert_eq!(list[0]["sources"][0]["label"], "healthy");
    assert_eq!(list[0]["sources"][1]["label"], "recovering");
    assert_eq!(list[1]["sources"][0]["label"], "unrelated");
    assert_eq!(list[1]["sources"][0]["fail_count"], 3);
    assert_eq!(
        app.request("POST", CLEANUP, Some(&admin), None)
            .await
            .json(),
        json!({"removed":1})
    );
}

#[tokio::test]
async fn probes_discard_measurements_if_headers_changed_in_flight() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let wait = {
        let entered = entered.clone();
        let release = release.clone();
        move || async move {
            entered.notify_one();
            release.notified().await;
            "stale result"
        }
    };
    let (base, server) = serve(Router::new().route("/mutable", get(wait))).await;
    channel(&app, "Mutable", "A", &base, &["mutable"]);
    let (reply, ()) = tokio::join!(app.request("POST", PROBE, Some(&admin), None), async {
        tokio::time::timeout(Duration::from_secs(25), entered.notified())
            .await
            .unwrap();
        app.state
            .conn()
            .unwrap()
            .execute(
                "UPDATE channel_sources SET headers_json=?1",
                [r#"{"Cookie":"replacement"}"#],
            )
            .unwrap();
        release.notify_one();
    });
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"checked":0,"healthy":0,"failed":0}));
    let list = app
        .request("GET", "/api/channels", Some(&admin), None)
        .await
        .json();
    assert!(list[0]["sources"][0]["last_ok_at"].is_null());
    assert!(list[0]["sources"][0]["latency_ms"].is_null());
    server.abort();
}

#[tokio::test]
async fn probe_and_cleanup_require_admin_session_and_csrf_header() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    for path in [PROBE, CLEANUP] {
        assert_eq!(
            app.request("POST", path, None, None).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.request("POST", path, Some(&viewer), None).await.status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.request_with_headers("POST", path, Some(&admin), None, &[])
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
}
