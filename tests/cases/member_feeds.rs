use super::*;

async fn update(app: &TestApp, cookie: &str, id: i64, body: Value) -> StatusCode {
    app.request("PUT", &format!("/api/users/{id}"), Some(cookie), Some(body))
        .await
        .status
}

async fn reset(app: &TestApp, cookie: &str, id: i64, password: &str) -> StatusCode {
    app.request(
        "POST",
        &format!("/api/users/{id}/password"),
        Some(cookie),
        Some(json!({"password":password})),
    )
    .await
    .status
}

#[tokio::test]
async fn disabled_members_lose_sessions_login_and_every_tvbox_feed() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let feed = app
        .request("GET", "/api/tvbox", Some(&viewer), None)
        .await
        .json();
    let token = feed["token"].as_str().unwrap();
    let members = app
        .request("GET", "/api/users", Some(&admin), None)
        .await
        .json();
    assert_eq!(members[1]["disabled"], false);

    assert_eq!(
        update(&app, &admin, id, json!({"disabled":true})).await,
        StatusCode::NO_CONTENT
    );
    let members = app
        .request("GET", "/api/users", Some(&admin), None)
        .await
        .json();
    assert_eq!(
        members[1],
        json!({"id":id,"username":"viewer","role":"user","disabled":true})
    );
    let denied = app.login("viewer", PASSWORD).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(denied.json()["error"], "Account is disabled");
    assert!(!denied.headers.contains_key(header::SET_COOKIE));
    assert_eq!(
        app.login("viewer", "incorrect-password").await.status,
        StatusCode::UNAUTHORIZED
    );
    {
        let conn = app.state.conn().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions WHERE user_id=?", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
        // The extractor must also reject a retained session row for a disabled account.
        conn.execute(
            "INSERT INTO sessions VALUES('retained',?,?)",
            (id, polyview::now() + 300),
        )
        .unwrap();
    }
    let retained = Some("polyview_session=retained");
    assert_eq!(
        app.request("GET", "/api/tvbox", retained, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        app.request("GET", "/api/auth/status", retained, None)
            .await
            .json()["user"]
            .is_null()
    );
    assert_eq!(
        app.request("POST", "/api/tvbox/token", Some(&viewer), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    for tail in [
        "config.json",
        "config.json?encode=base58",
        "vod",
        "live.m3u",
        "source/1",
        "media/1",
    ] {
        let denied = app
            .request("GET", &format!("/tvbox/{token}/{tail}"), None, None)
            .await;
        assert_eq!(denied.status, StatusCode::NOT_FOUND, "{tail}");
        assert_eq!(denied.json()["error"], "Feed not found", "{tail}");
    }
    assert_eq!(
        update(&app, &admin, id, json!({"disabled":false})).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", "/api/tvbox", Some(&viewer), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(app.login("viewer", PASSWORD).await.status, StatusCode::OK);
    assert_eq!(
        app.request_with_headers(
            "GET",
            feed["live_path"].as_str().unwrap(),
            None,
            None,
            &[("host", "nas.test")]
        )
        .await
        .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn role_changes_and_deletion_protect_the_current_and_last_active_admin() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    assert_eq!(
        update(&app, &viewer, id, json!({"role":"admin"})).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        reset(&app, &viewer, 1, PASSWORD).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        update(&app, &admin, id, json!({"role":"owner"})).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        update(&app, &admin, id + 1, json!({"role":"user"})).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        update(&app, &admin, id, json!({"role":"admin"})).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", "/api/users", Some(&viewer), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        update(&app, &admin, id, json!({"role":"user"})).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", "/api/users", Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        update(&app, &admin, id, json!({"role":"admin"})).await,
        StatusCode::NO_CONTENT
    );
    for body in [json!({"role":"user"}), json!({"disabled":true})] {
        assert_eq!(update(&app, &admin, 1, body).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        app.request("DELETE", "/api/users/1", Some(&admin), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    // Each admin may remove the other until one removal makes the other request unauthorized.
    let (disable, delete) = tokio::join!(
        update(&app, &admin, id, json!({"disabled":true})),
        app.request("DELETE", "/api/users/1", Some(&viewer), None)
    );
    assert!(matches!(
        (disable, delete.status),
        (StatusCode::NO_CONTENT, StatusCode::UNAUTHORIZED)
            | (StatusCode::UNAUTHORIZED, StatusCode::NO_CONTENT)
    ));
    let conn = app.state.conn().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM users WHERE role='admin' AND disabled=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn password_reset_invalidates_existing_sessions_and_in_flight_password_logins() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    assert_eq!(
        reset(&app, &admin, 1, PASSWORD).await,
        StatusCode::BAD_REQUEST
    );
    for password in ["短".repeat(9), "长".repeat(129)] {
        assert_eq!(
            reset(&app, &admin, id, &password).await,
            StatusCode::BAD_REQUEST
        );
    }
    let replacement = "合".repeat(10);
    // Start authentication, then leave its request suspended while the reset commits.
    let pending = app.login("viewer", PASSWORD);
    tokio::pin!(pending);
    let early_login = tokio::select! {
        biased;
        reply = &mut pending => Some(reply),
        _ = std::future::ready(()) => None,
    };
    assert_eq!(
        reset(&app, &admin, id, &replacement).await,
        StatusCode::NO_CONTENT
    );
    if let Some(early) = early_login {
        assert_eq!(early.status, StatusCode::OK);
        assert_eq!(
            app.request("GET", "/api/tvbox", Some(&early.cookie()), None)
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
    } else {
        assert_eq!(pending.await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        app.request("GET", "/api/tvbox", Some(&viewer), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.login("viewer", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.login("viewer", &replacement).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", "/api/users", Some(&admin), None)
            .await
            .status,
        StatusCode::OK
    );
}

fn decode_base58(text: &[u8]) -> Vec<u8> {
    let alphabet = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut bytes = Vec::<u8>::new();
    for char in text {
        let mut carry = alphabet
            .iter()
            .position(|c| c == char)
            .expect("Base58 digit") as u16;
        for byte in &mut bytes {
            carry += u16::from(*byte) * 58;
            *byte = carry as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.push(carry as u8);
            carry >>= 8;
        }
    }
    bytes.resize(
        bytes.len() + text.iter().take_while(|&&c| c == b'1').count(),
        0,
    );
    bytes.reverse();
    bytes
}

#[tokio::test]
async fn base58_subscription_decodes_to_the_exact_standard_configuration() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let feed = app
        .request("GET", "/api/tvbox", Some(&admin), None)
        .await
        .json();
    let path = feed["config_path"].as_str().unwrap();
    assert_eq!(feed["config_b58_path"], format!("{path}?encode=base58"));
    let encoded = feed["config_b58_path"].as_str().unwrap();
    let headers = &[("host", "polyview.test")];
    let plain = app
        .request_with_headers("GET", path, None, None, headers)
        .await;
    let b58 = app
        .request_with_headers("GET", encoded, None, None, headers)
        .await;
    assert_eq!(plain.status, StatusCode::OK);
    assert!(plain.json()["sites"].is_array());
    assert_eq!(b58.status, StatusCode::OK);
    assert_eq!(
        b58.headers[header::CONTENT_TYPE],
        "text/plain; charset=utf-8"
    );
    assert_eq!(decode_base58(&b58.body), plain.body);
    let rotated = app
        .request("POST", "/api/tvbox/token", Some(&admin), None)
        .await
        .json();
    assert_ne!(feed["config_b58_path"], rotated["config_b58_path"]);
    assert_eq!(
        app.request_with_headers("GET", encoded, None, None, headers)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}
