use super::*;
use axum::{Json, extract::Query, routing::get};
use std::collections::HashMap;

#[tokio::test]
async fn suggestions_filter_deduplicate_and_cap_visible_titles() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    let cms = Router::new()
        .route(
            "/cms",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                if query["ac"] == "detail" {
                    return Json(
                        json!({"list":[{"vod_id":1,"vod_name":"Story","vod_area":"日本"}]}),
                    );
                }
                assert_eq!(query["ac"], "list");
                assert_eq!(query["wd"], "story");
                let mut items = vec![
                    json!({"vod_name":"Unrelated"}),
                    json!({"vod_name":"Story 0","vod_year":2025,"type_name":"剧情片"}),
                ];
                items.extend((0..12).map(|i| json!({"vod_name":format!("STORY {i}")})));
                Json(json!({"list":items}))
            }),
        )
        .route(
            "/private",
            get(|| async { Json(json!({"list":[{"vod_name":"Private Story"}]})) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, cms).await.unwrap() });
    for (route, adult) in [("cms", false), ("private", true)] {
        app.state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO catalog_sources(name,url,adult) VALUES(?,?,?)",
                rusqlite::params![route, format!("http://{addr}/{route}"), adult],
            )
            .unwrap();
    }
    let route = "/api/catalog/suggest?q=story";
    assert_eq!(
        app.request("GET", route, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let reply = app.request("GET", route, Some(&cookie), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let results = reply.json();
    assert_eq!(results.as_array().unwrap().len(), 8);
    assert_eq!(
        results[0],
        json!({"title":"Story 0","year":"2025","kind":"movie"})
    );
    assert_eq!(results[7]["title"], "STORY 7");
    let search = app
        .request("GET", "/api/catalog/search?q=story", Some(&cookie), None)
        .await;
    assert_eq!(search.json()["results"][0]["area"], "日本");
    server.abort();
}

#[tokio::test]
async fn suggestions_keep_fast_results_when_another_source_stalls() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    let cms = Router::new()
        .route(
            "/fast",
            get(|| async { Json(json!({"list":[{"vod_name":"Signal"}]})) }),
        )
        .route(
            "/filtered",
            get(|| async { Json(json!({"list":[{"vod_name":"Hidden Signal"}]})) }),
        )
        .route("/stall", get(std::future::pending::<StatusCode>));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, cms).await.unwrap() });
    for route in ["stall", "fast"] {
        app.state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO catalog_sources(name,url) VALUES(?,?)",
                rusqlite::params![route, format!("http://{addr}/{route}")],
            )
            .unwrap();
    }
    for (name, enabled, adult) in [("disabled", false, false), ("adult", true, true)] {
        app.state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO catalog_sources(name,url,enabled,adult) VALUES(?,?,?,?)",
                rusqlite::params![
                    name,
                    format!("http://{addr}/filtered?name={name}"),
                    enabled,
                    adult
                ],
            )
            .unwrap();
    }
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        app.request("GET", "/api/catalog/suggest?q=sign", Some(&cookie), None),
    )
    .await
    .expect("suggestions should have a shorter deadline than full catalog searches");
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!([{"title":"Signal","year":"","kind":"other"}])
    );
    server.abort();
}
