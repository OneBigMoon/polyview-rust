use super::*;
use axum::{Json, http::Uri, routing::get};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn calendar_cleans_weekdays_and_observes_mirror_changes_expiry_and_disable() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let calls = Arc::new(AtomicUsize::new(0));
    let mirror = Router::new().fallback(get({
        let calls = calls.clone();
        move |uri: Uri| {
            calls.fetch_add(1, Ordering::Relaxed);
            async move {
                Json(json!([
                    {"weekday":{"id":8}, "items":[{"id":4,"name":"ignored"}]},
                    {"weekday":{"id":7}, "items":[{"id":3,"name":" Original "}]},
                    {"weekday":{"id":2}, "items":[{"id":1,"name":"Raw","name_cn":uri.path(),
                        "rating":{"score":8.7},"air_date":"2026-10-01","images":{"common":"https://img.invalid/cover.jpg"}},
                        {"id":2,"name":"  "}]}
                ]))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, mirror).await.unwrap() });
    let config = |enabled, path: &str| json!({"enabled": enabled, "api_url": format!("http://{addr}/{path}")});
    let settings = "/api/settings/calendar";
    let route = "/api/calendar";
    assert_eq!(
        app.request("GET", settings, Some(&admin), None)
            .await
            .json(),
        json!({"enabled":true,"api_url":"https://api.bgm.tv/calendar"})
    );
    assert_eq!(
        app.request("GET", route, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    for (method, body) in [("GET", None), ("PUT", Some(config(true, "first")))] {
        assert_eq!(
            app.request(method, settings, Some(&viewer), body)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request(
            "PUT",
            settings,
            Some(&admin),
            Some(json!({"enabled":true,"api_url":"file:///tmp/calendar"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let first = config(true, "first");
    assert_eq!(
        app.request("PUT", settings, Some(&admin), Some(first.clone()))
            .await
            .json(),
        first
    );
    let data = app.request("GET", route, Some(&viewer), None).await.json();
    assert_eq!(data["enabled"], true);
    let days = data["days"].as_array().unwrap();
    assert_eq!(days.len(), 7);
    for (index, day) in days.iter().enumerate() {
        assert_eq!(day["weekday"], index + 1);
        if !matches!(index, 1 | 6) {
            assert_eq!(day["items"], json!([]));
        }
    }
    assert_eq!(
        days[1],
        json!({"weekday":2,"label_cn":"星期二","items":[{"id":1,"title_cn":"/first","title_raw":"Raw","cover":"https://img.invalid/cover.jpg","score":8.7,"air_date":"2026-10-01"}]})
    );
    assert_eq!(days[6]["items"][0]["title_cn"], "Original");
    assert_eq!(
        app.request("GET", route, Some(&admin), None).await.json(),
        data
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE remote_cache SET fetched_at=? WHERE cache_key='calendar'",
            [polyview::now() - 12 * 60 * 60],
        )
        .unwrap();
    assert_eq!(
        app.request("GET", route, Some(&viewer), None).await.json(),
        data
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let second = config(true, "second");
    assert_eq!(
        app.request("PUT", settings, Some(&admin), Some(second.clone()))
            .await
            .json(),
        second
    );
    assert_eq!(
        app.request("GET", route, Some(&viewer), None).await.json()["days"][1]["items"][0]["title_cn"],
        "/second"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(
        app.request("PUT", settings, Some(&admin), Some(config(false, "second")))
            .await
            .status,
        StatusCode::OK
    );
    let disabled = app.request("GET", route, Some(&viewer), None).await.json();
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["days"].as_array().unwrap().len(), 7);
    assert!(
        disabled["days"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["items"] == json!([]))
    );
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    server.abort();
}
