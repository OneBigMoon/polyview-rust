use super::*;
use axum::{Json, routing::get};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn serve(routes: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    format!("http://{address}")
}

fn feeds(app: &TestApp, movie: &str, series: &str) {
    let conn = app.state.conn().unwrap();
    polyview::settings::set_setting_json(&conn, "douban_movie_url", &movie).unwrap();
    polyview::settings::set_setting_json(&conn, "douban_series_url", &series).unwrap();
}

async fn recommendations(app: &TestApp, cookie: &str) -> Value {
    let reply = app
        .request("GET", "/api/catalog/recommend", Some(cookie), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()
}

#[tokio::test]
async fn douban_groups_use_a_six_hour_cache() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let movie_calls = calls.clone();
    let base = serve(Router::new().route("/movie", get(move || {
        let count = movie_calls.fetch_add(1, Ordering::SeqCst) + 1;
        async move { Json(json!({"subjects":[{"title":format!("Film {count}"),"year":2025,"cover":"poster","rate":"8.2"}]})) }
    })).route("/tv", get(|| async { Json(json!({"subjects":[{"title":"Show"}]})) }))).await;
    feeds(&app, &format!("{base}/movie"), &format!("{base}/tv"));
    let first = recommendations(&app, &admin).await;
    assert_eq!(first["origin"], "douban");
    assert_eq!(first["groups"][0]["kind"], "movie");
    assert_eq!(
        first["groups"][0]["items"][0],
        json!({"title":"Film 1", "year":"2025", "cover":"poster", "rating":"8.2"})
    );
    assert_eq!(first["groups"][1]["kind"], "series");
    assert_eq!(first["groups"][1]["items"][0]["title"], "Show");
    for (age, expected) in [(6 * 3600 - 60, "Film 1"), (6 * 3600 + 60, "Film 2")] {
        app.state
            .conn()
            .unwrap()
            .execute("UPDATE remote_cache SET fetched_at=unixepoch()-?", [age])
            .unwrap();
        assert_eq!(
            recommendations(&app, &admin).await["groups"][0]["items"][0]["title"],
            expected
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_feeds_fall_back_to_currently_visible_cms_sources() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let base = serve(Router::new().route("/cms", get(|| async { Json(json!({"list":[
        {"vod_name":"Film","type_name":"电影","vod_id":4,"vod_year":"2023","vod_pic":"art","vod_score":7.5},
        {"vod_name":"Show","type_name":"国产剧","vod_id":5}]})) }))
        .route("/adult", get(|| async { Json(json!({"list":[{"vod_name":"Restricted","type_name":"电影","vod_id":6}]})) }))
        .route("/invalid", get(|| async { Json(json!({"error":"unavailable"})) }))).await;
    feeds(&app, &format!("{base}/invalid"), &format!("{base}/missing"));
    {
        let conn = app.state.conn().unwrap();
        conn.execute("INSERT INTO catalog_sources(id,name,url,adult) VALUES(7,'normal',?1,0),(8,'adult',?2,1)", [format!("{base}/cms"),format!("{base}/adult")]).unwrap();
        conn.execute(
            "UPDATE settings SET value='1' WHERE key='adult_sources_enabled'",
            [],
        )
        .unwrap();
    }
    assert_eq!(
        recommendations(&app, &admin).await["groups"][0]["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE settings SET value='0' WHERE key='adult_sources_enabled'",
            [],
        )
        .unwrap();
    let result = recommendations(&app, &viewer).await;
    assert_eq!(result["origin"], "fallback");
    assert_eq!(
        result["groups"][0]["items"],
        json!([{"title":"Film","year":"2023","cover":format!("{base}/art"),"rating":"7.5","source_id":7,"id":"4"}])
    );
    assert_eq!(result["groups"][1]["items"][0]["title"], "Show");
    feeds(&app, "http://127.0.0.1:0/movie", "http://127.0.0.1:0/tv");
    assert_eq!(recommendations(&app, &viewer).await, result);
    assert_eq!(
        app.request("GET", "/api/catalog/recommend", None, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}
