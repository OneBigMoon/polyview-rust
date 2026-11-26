use super::*;

#[tokio::test]
async fn skip_settings_are_scoped_validated_and_persisted() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let path = "/api/skip?key=online%3A1%3Aone";
    let input =
        json!({"media_key":"online:1:one","enabled":true,"intro_sec":7.5,"outro_sec":600.0});
    assert_eq!(
        app.request("PUT", "/api/skip", Some(&admin), Some(input.clone()))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", path, Some(&admin), None).await.json(),
        input
    );
    assert_eq!(
        app.request("GET", path, Some(&viewer), None).await.json()["enabled"],
        false
    );
    for (field, invalid) in [("intro_sec", -1.0), ("outro_sec", 600.1)] {
        let mut bad = input.clone();
        bad[field] = json!(invalid);
        assert_eq!(
            app.request("PUT", "/api/skip", Some(&admin), Some(bad))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        app.request("GET", path, Some(&admin), None).await.json(),
        input
    );
}

#[tokio::test]
async fn play_settings_require_admin_writes_and_validate_headers() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let path = "/api/settings/play";
    let input = json!({"ad_block":true,"play_referer":"https://example.test/","play_ua":"FixturePlayer","image_referer":""});
    assert_eq!(
        app.request("PUT", path, Some(&viewer), Some(input.clone()))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request("PUT", path, Some(&admin), Some(input.clone()))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", path, Some(&viewer), None).await.json(),
        input
    );
    assert_eq!(
        app.request(
            "PUT",
            path,
            Some(&admin),
            Some(json!({"play_ua":"bad\r\nvalue"}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.request("GET", path, Some(&admin), None).await.json(),
        input
    );
}
