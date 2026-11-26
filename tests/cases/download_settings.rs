use super::*;

#[tokio::test]
async fn download_settings_are_admin_only_and_invalid_writes_preserve_the_saved_configuration() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let path = "/api/settings/downloads";
    let mut body = app.request("GET", path, Some(&admin), None).await.json();
    assert_eq!(body["concurrency"], 1);
    assert_eq!(body["tdl_path"], "tdl");
    body["concurrency"] = json!(4);
    body["telegram_proxy"] = json!("socks5h://name:example@proxy.test:1080");
    assert_eq!(
        app.request("PUT", path, Some(&admin), Some(body.clone()))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", path, Some(&viewer), None).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request("PUT", path, Some(&viewer), Some(body.clone()))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for (key, val) in [
        ("concurrency", json!(0)),
        ("max_attempts", json!(11)),
        ("ffmpeg_path", json!("a\nb")),
        ("telegram_namespace", json!("../other")),
        ("telegram_proxy", json!("file:///tmp/p")),
    ] {
        let mut invalid = body.clone();
        invalid[key] = val;
        assert_eq!(
            app.request("PUT", path, Some(&admin), Some(invalid))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let mut public = app.request("GET", path, Some(&admin), None).await.json();
    assert!(public.get("telegram_proxy").is_none());
    assert_eq!(public["telegram_proxy_configured"], true);
    public["concurrency"] = json!(2);
    assert_eq!(
        app.request("PUT", path, Some(&admin), Some(public.clone()))
            .await
            .json(),
        public
    );
    let backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(
        backup["app_settings"]["downloads"]["telegram_proxy"],
        body["telegram_proxy"]
    );
    public["telegram_proxy"] = json!("");
    assert_eq!(
        app.request("PUT", path, Some(&admin), Some(public))
            .await
            .json()["telegram_proxy_configured"],
        false
    );
}
