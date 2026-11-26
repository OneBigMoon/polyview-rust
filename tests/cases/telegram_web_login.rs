use super::*;

const LOGIN: &str = "/api/telegram/native/login";
const CONFIG: &str = "/api/settings/telegram/native";
const HASH: &str = "aa111111111111111111111111111111";

async fn configure(app: &TestApp, admin: &str) {
    let reply = app
        .request(
            "PUT",
            CONFIG,
            Some(admin),
            Some(json!({"api_id":4567,"api_hash":HASH})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"configured":true}));
}

#[tokio::test]
async fn native_login_settings_are_privileged_and_restore_only_app_credentials() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    for method in ["GET", "POST", "DELETE"] {
        assert_eq!(
            app.request(method, LOGIN, None, None).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    for method in ["GET", "PUT"] {
        assert_eq!(
            app.request(method, CONFIG, Some(&viewer), None)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request("POST", LOGIN, Some(&viewer), None).await.json()["state"],
        "unconfigured"
    );
    assert_eq!(
        app.request_with_headers("POST", LOGIN, Some(&viewer), None, &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    configure(&app, &admin).await;
    for bad in [
        json!({"api_id":0,"api_hash":HASH}),
        json!({"api_id":55,"api_hash":"fixture-invalid-hash"}),
        json!({"api_id":55,"api_hash":HASH,"otp":"fixture-invalid-hash"}),
    ] {
        let reply = app.request("PUT", CONFIG, Some(&admin), Some(bad)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        let response = String::from_utf8(reply.body).unwrap();
        assert!(!response.contains(HASH));
        assert!(!response.contains("fixture-invalid-hash"));
    }
    assert_eq!(
        app.request("GET", CONFIG, Some(&admin), None).await.json(),
        json!({"configured":true})
    );
    let reply = app
        .request(
            "POST",
            LOGIN,
            Some(&viewer),
            Some(json!({"user_id":1,"password":"fixture-private"})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(!String::from_utf8_lossy(&reply.body).contains("fixture-private"));
    assert_eq!(
        app.request("POST", &format!("{LOGIN}?user_id=1"), Some(&viewer), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(
        backup["app_settings"]["telegram_native"],
        json!({"api_id":4567,"api_hash":HASH})
    );
    let clean = TestApp::new();
    let owner = clean.setup().await;
    assert_eq!(
        clean
            .request(
                "POST",
                "/api/settings/import",
                Some(&owner),
                Some(backup.clone())
            )
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        clean
            .request("GET", CONFIG, Some(&owner), None)
            .await
            .json(),
        json!({"configured":true})
    );
    let mut invalid = backup;
    invalid["app_settings"]["telegram_native"]["api_hash"] = json!("bad");
    assert_eq!(
        clean
            .request("POST", "/api/settings/import", Some(&owner), Some(invalid))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let after = clean
        .request("GET", "/api/settings/export", Some(&owner), None)
        .await
        .json();
    assert_eq!(after["app_settings"]["telegram_native"]["api_hash"], HASH);
}

#[cfg(unix)]
mod host {
    use super::*;
    use polyview::telegram::{login::HostPrompt, native::NativeRuntime};
    use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

    const LINK: &str = "tg://login?token=SyntheticOfflineQrToken";
    const FIXTURE_ID: i64 = 87654321;

    fn fixture() -> (TestApp, TempDir) {
        let root = tempfile::tempdir().unwrap();
        let abandoned = root.path().join("abandoned");
        let native = NativeRuntime::with_transport(root.path().join("tdlib"), move |_user, req| {
            let answer = match req["@type"].as_str().unwrap() {
                "getAuthorizationState" => json!({"@type":"authorizationStateWaitPhoneNumber"}),
                "requestQrCodeAuthentication" => {
                    return vec![
                        json!({"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateWaitOtherDeviceConfirmation","link":LINK}}),
                        json!({"@type":"ok"}),
                    ];
                }
                "getMe" => json!({"@type":"user","id":FIXTURE_ID}),
                "logOut" | "destroy" | "close" => {
                    if req["@type"] != "close" {
                        let pending = abandoned.with_extension("new");
                        fs::write(&pending, b"true").unwrap();
                        fs::rename(pending, &abandoned).unwrap();
                    }
                    return vec![
                        json!({"@type":"ok"}),
                        json!({"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateClosed"}}),
                    ];
                }
                _ => json!({"@type":"ok"}),
            };
            vec![answer]
        });
        let mut app = TestApp::new_with_native(native);
        let helper = root.path().join("local-presenter");
        // The host presenter is a separate, synthetic process; its input never
        // passes through a network handler. Each marker records one shown dialog.
        fs::write(
            &helper,
            r#"#!/bin/sh
set -eu
cd "$(dirname "$0")"
payload=$(cat)
case "$payload" in
  *'"event":"confirm"'*) event=confirm;;
  *'"event":"scan"'*) event=scan;;
  *'"event":"bind"'*) event=bind;;
  *) exit 1;;
esac
printf '%s' "$payload" > "$event.new"
mv "$event.new" "$event"
while [ ! -f "allow-$event" ] && [ ! -f "deny-$event" ]; do sleep .03; done
[ ! -f "deny-$event" ] || exit 2
"#,
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        app.app = app
            .app
            .layer(axum::Extension(HostPrompt::with_helper(helper)));
        (app, root)
    }

    async fn marker(root: &Path, event: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(bytes) = fs::read(root.join(event)) {
                    return serde_json::from_slice(&bytes).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for local presenter")
    }

    fn check_private(value: &Value) {
        let body = value.to_string();
        for secret in [
            HASH,
            "SyntheticOfflineQrToken",
            "tg://",
            "account_key",
            "attempt",
            "api_hash",
            "87654321",
        ] {
            assert!(!body.contains(secret));
        }
    }

    async fn state(app: &TestApp, cookie: &str, expected: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let reply = app.request("GET", LOGIN, Some(cookie), None).await;
                assert_eq!(reply.status, StatusCode::OK);
                let value = reply.json();
                check_private(&value);
                if value["state"] == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("expected native login status");
    }

    #[tokio::test]
    async fn native_login_needs_host_confirmation_and_scopes_cancellation_to_requesting_member() {
        let (app, root) = fixture();
        let admin = app.setup().await;
        let id = app.create_viewer(&admin).await;
        let viewer = app.login("viewer", PASSWORD).await.cookie();
        configure(&app, &admin).await;
        assert_eq!(
            app.request("POST", LOGIN, Some(&viewer), None).await.json()["state"],
            "confirm"
        );
        let shown = marker(root.path(), "confirm").await;
        assert_eq!(shown["member_id"], id);
        assert_eq!(shown["member_name"], "viewer");
        assert!(shown["value"].is_null());
        assert!(!root.path().join("scan").exists());
        assert_eq!(
            app.request("POST", LOGIN, Some(&viewer), None).await.json()["state"],
            "confirm"
        );
        assert_eq!(
            app.request("POST", LOGIN, Some(&admin), None).await.status,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            app.request("DELETE", LOGIN, Some(&admin), None)
                .await
                .json()["state"],
            "idle"
        );
        state(&app, &viewer, "confirm").await;
        fs::write(root.path().join("allow-confirm"), b"").unwrap();
        let scan = marker(root.path(), "scan").await;
        assert_eq!(scan["value"], LINK);
        assert_eq!(scan["member_id"], id);
        state(&app, &viewer, "scan").await;
        assert_eq!(
            app.request("DELETE", LOGIN, Some(&viewer), None)
                .await
                .json()["state"],
            "idle"
        );
        state(&app, &viewer, "idle").await;
        assert_eq!(
            app.request("POST", LOGIN, Some(&viewer), None).await.status,
            StatusCode::TOO_MANY_REQUESTS
        );
        marker(root.path(), "abandoned").await;
    }

    async fn scanned(app: &TestApp, root: &Path, member: &str, user_id: i64) -> uuid::Uuid {
        fs::write(root.join("allow-confirm"), b"").unwrap();
        let reply = app.request("POST", LOGIN, Some(member), None).await;
        assert_eq!(reply.status, StatusCode::OK);
        check_private(&reply.json());
        assert_eq!(marker(root, "scan").await["value"], LINK);
        let key: String = app
            .state
            .conn()
            .unwrap()
            .query_row(
                "SELECT attempt FROM native_telegram_accounts WHERE user_id=?",
                [user_id],
                |r| r.get(0),
            )
            .unwrap();
        uuid::Uuid::parse_str(&key).unwrap()
    }

    fn provider(app: &TestApp, member: i64, attempt: uuid::Uuid, phase: &str) {
        app.state
            .telegram_native
            .inject_update(
                member,
                attempt,
                json!({"@type":"updateAuthorizationState","authorization_state":{"@type":phase}}),
            )
            .unwrap();
    }

    #[tokio::test]
    async fn official_mobile_approval_and_final_local_binding_identify_the_same_member() {
        let (app, root) = fixture();
        let admin = app.setup().await;
        let id = app.create_viewer(&admin).await;
        let viewer = app.login("viewer", PASSWORD).await.cookie();
        configure(&app, &admin).await;
        let attempt = scanned(&app, root.path(), &viewer, id).await;
        provider(&app, id, attempt, "authorizationStateReady");
        let bind = marker(root.path(), "bind").await;
        assert_eq!(bind["value"], FIXTURE_ID);
        assert_eq!(bind["member_name"], "viewer");
        assert_eq!(bind["member_id"], id);
        state(&app, &viewer, "scan").await;
        fs::write(root.path().join("allow-bind"), b"").unwrap();
        state(&app, &viewer, "authorized").await;
        state(&app, &admin, "idle").await;
        let identity: i64 = app.state.conn().unwrap().query_row("SELECT telegram_id FROM native_telegram_accounts WHERE user_id=? AND phase='authorized'", [id], |r| r.get(0)).unwrap();
        assert_eq!(identity, FIXTURE_ID);
        let backup = app
            .request("GET", "/api/settings/export", Some(&admin), None)
            .await
            .json()
            .to_string();
        assert!(!backup.contains(&attempt.to_string()));
        assert!(!backup.contains(&FIXTURE_ID.to_string()));
        assert!(!backup.contains(LINK));
        provider(&app, id, attempt, "authorizationStateWaitPhoneNumber");
        state(&app, &viewer, "expired").await;
        marker(root.path(), "abandoned").await;
    }

    #[tokio::test]
    async fn stale_host_confirmation_cannot_authorize_a_recycled_member_id() {
        let (app, root) = fixture();
        let admin = app.setup().await;
        let id = app.create_viewer(&admin).await;
        let viewer = app.login("viewer", PASSWORD).await.cookie();
        configure(&app, &admin).await;
        let attempt = scanned(&app, root.path(), &viewer, id).await;
        provider(&app, id, attempt, "authorizationStateReady");
        marker(root.path(), "bind").await;
        let reply = app
            .request("DELETE", &format!("/api/users/{id}"), Some(&admin), None)
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        assert_eq!(app.create_viewer(&admin).await, id);
        let replacement = app.login("viewer", PASSWORD).await.cookie();
        fs::write(root.path().join("allow-bind"), b"").unwrap();
        // Await the old worker's revocation before asserting the replacement's state.
        marker(root.path(), "abandoned").await;
        state(&app, &replacement, "idle").await;
    }

    #[tokio::test]
    async fn final_local_refusal_revokes_an_approved_mobile_session() {
        let (app, root) = fixture();
        let admin = app.setup().await;
        configure(&app, &admin).await;
        let attempt = scanned(&app, root.path(), &admin, 1).await;
        provider(&app, 1, attempt, "authorizationStateReady");
        marker(root.path(), "bind").await;
        fs::write(root.path().join("deny-bind"), b"").unwrap();
        state(&app, &admin, "declined").await;
        marker(root.path(), "abandoned").await;
        let verified: bool = app.state.conn().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM native_telegram_accounts WHERE telegram_id IS NOT NULL)",[],|r| r.get(0)).unwrap();
        assert!(!verified);
    }

    #[tokio::test]
    async fn extra_provider_verification_ends_the_attempt_without_requesting_credentials() {
        let (app, root) = fixture();
        let admin = app.setup().await;
        configure(&app, &admin).await;
        let attempt = scanned(&app, root.path(), &admin, 1).await;
        provider(&app, 1, attempt, "authorizationStateWaitPassword");
        state(&app, &admin, "official").await;
        marker(root.path(), "abandoned").await;
        assert!(!root.path().join("bind").exists());
    }

    #[tokio::test]
    async fn expired_host_attempt_cannot_complete_or_reveal_its_old_frame() {
        let (app, root) = fixture();
        let admin = app.setup().await;
        configure(&app, &admin).await;
        scanned(&app, root.path(), &admin, 1).await;
        app.state
            .conn()
            .unwrap()
            .execute(
                "UPDATE native_telegram_accounts SET expires_at=0 WHERE user_id=1",
                [],
            )
            .unwrap();
        state(&app, &admin, "expired").await;
        marker(root.path(), "abandoned").await;
        assert!(!root.path().join("bind").exists());
    }
}
