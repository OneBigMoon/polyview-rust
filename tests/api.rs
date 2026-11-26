#[path = "cases/backup.rs"]
mod backup;
#[path = "cases/calendar.rs"]
mod calendar;
#[path = "cases/cloud_accounts.rs"]
mod cloud_accounts;
#[path = "cases/cloud_bot.rs"]
mod cloud_bot;
#[path = "cases/cloud_search.rs"]
mod cloud_search;
#[path = "cases/download_metadata.rs"]
mod download_metadata;
#[path = "cases/download_settings.rs"]
mod download_settings;
#[path = "cases/live_formats.rs"]
mod live_formats;
#[path = "cases/live_probes.rs"]
mod live_probes;
#[path = "cases/live_relay.rs"]
mod live_relay;
#[path = "cases/live_sources.rs"]
mod live_sources;
#[path = "cases/member_feeds.rs"]
mod member_feeds;
#[path = "cases/notify.rs"]
mod notify;
#[path = "cases/parsers.rs"]
mod parsers;
#[path = "cases/play_settings.rs"]
mod play_settings;
#[path = "cases/playback_fallback.rs"]
mod playback_fallback;
#[path = "cases/recommend.rs"]
mod recommend;
#[path = "cases/resolved_download.rs"]
mod resolved_download;
#[path = "cases/source_ops.rs"]
mod source_ops;
#[path = "cases/source_subscriptions.rs"]
mod source_subscriptions;
#[path = "cases/suggest.rs"]
mod suggest;
#[path = "cases/telegram_browse.rs"]
mod telegram_browse;
#[path = "cases/telegram_media.rs"]
mod telegram_media;
#[path = "cases/telegram_queue.rs"]
mod telegram_queue;
#[path = "cases/telegram_stream.rs"]
mod telegram_stream;
#[path = "cases/telegram_web_login.rs"]
mod telegram_web_login;
#[path = "cases/tracking_resolve.rs"]
mod tracking_resolve;
#[path = "cases/user_data.rs"]
mod user_data;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header},
};
use polyview::{AppState, Shared, init_schema, router};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    sync::{Arc, Mutex},
};
use tempfile::TempDir;
use tower::ServiceExt;

const PASSWORD: &str = "polyview-test-password";

struct TestApp {
    _temp: TempDir,
    state: Shared,
    app: Router,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON response")
    }

    fn cookie(&self) -> String {
        self.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }
}

impl TestApp {
    fn new() -> Self {
        Self::new_with_native(Default::default())
    }

    fn new_with_native(telegram_native: polyview::telegram::native::NativeRuntime) -> Self {
        Self::with_services(telegram_native, None)
    }

    fn with_cloud_fixture(url: reqwest::Url) -> Self {
        Self::with_services(Default::default(), Some(url))
    }

    fn with_services(
        telegram_native: polyview::telegram::native::NativeRuntime,
        cloud_fixture: Option<reqwest::Url>,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let media_dir = temp.path().join("media");
        fs::create_dir_all(media_dir.join("downloads")).unwrap();
        let media_dir = fs::canonicalize(media_dir).unwrap();
        let download_dir = media_dir.join("downloads");
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            playback_grants: Default::default(),
            telegram_native,
            cloud_fixture,
            download_runtime: Default::default(),
            media_dir,
            download_dir,
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        Self {
            _temp: temp,
            app: router(state.clone()),
            state,
        }
    }

    async fn request(
        &self,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        self.request_with_headers(method, uri, cookie, body, &[("x-polyview-request", "1")])
            .await
    }

    async fn request_with_headers(
        &self,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&body).unwrap())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: to_bytes(body, 5 * 1024 * 1024).await.unwrap().to_vec(),
        }
    }

    async fn setup(&self) -> String {
        let reply = self
            .request(
                "POST",
                "/api/auth/setup",
                None,
                Some(json!({"username":"admin", "password":PASSWORD})),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        reply.cookie()
    }

    async fn create_viewer(&self, admin: &str) -> i64 {
        let reply = self
            .request(
                "POST",
                "/api/users",
                Some(admin),
                Some(json!({"username":"viewer", "password":PASSWORD, "role":"user"})),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED);
        reply.json()["id"].as_i64().unwrap()
    }

    async fn login(&self, username: &str, password: &str) -> Reply {
        self.request(
            "POST",
            "/api/auth/login",
            None,
            Some(json!({"username":username,"password":password})),
        )
        .await
    }
}

#[tokio::test]
async fn setup_login_logout_and_expiry_enforce_sessions() {
    let app = TestApp::new();
    let status = app.request("GET", "/api/auth/status", None, None).await;
    assert_eq!(status.json(), json!({"needs_setup":true,"user":null}));
    for route in [
        "/api/library",
        "/api/series",
        "/api/channels",
        "/api/downloads",
        "/api/users",
        "/api/library/1/stream",
    ] {
        assert_eq!(
            app.request("GET", route, None, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{route}"
        );
    }
    let forbidden = app
        .request_with_headers(
            "POST",
            "/api/auth/setup",
            None,
            Some(json!({"username":"admin","password":PASSWORD})),
            &[],
        )
        .await;
    assert_eq!(forbidden.status, StatusCode::FORBIDDEN);
    let cookie = app.setup().await;
    let status = app
        .request("GET", "/api/auth/status", Some(&cookie), None)
        .await;
    assert_eq!(status.json()["user"]["role"], "admin");
    assert_eq!(status.json()["needs_setup"], false);
    assert_eq!(
        app.request(
            "POST",
            "/api/auth/setup",
            None,
            Some(json!({"username":"second","password":PASSWORD}))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.login("admin", "incorrect-password").await.status,
        StatusCode::UNAUTHORIZED
    );
    let login = app.login("admin", PASSWORD).await;
    assert_eq!(login.status, StatusCode::OK);
    let cookie_header = login.headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie_header.contains("HttpOnly"));
    assert!(cookie_header.contains("SameSite=Lax"));
    let second_cookie = login.cookie();
    let logout = app
        .request("POST", "/api/auth/logout", Some(&second_cookie), None)
        .await;
    assert_eq!(logout.status, StatusCode::OK);
    assert!(
        logout.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    assert_eq!(
        app.request("GET", "/api/library", Some(&second_cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request("GET", "/api/library", Some(&cookie), None)
            .await
            .status,
        StatusCode::OK
    );
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE sessions SET expires_at=0", [])
        .unwrap();
    assert_eq!(
        app.request("GET", "/api/library", Some(&cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn administrators_manage_users_and_regular_users_cannot_manage_sources() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let viewer_id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let users = app.request("GET", "/api/users", Some(&admin), None).await;
    assert_eq!(users.status, StatusCode::OK);
    assert_eq!(users.json().as_array().unwrap().len(), 2);
    assert_eq!(users.json()[1]["id"], viewer_id);
    assert!(users.json()[1].get("password_hash").is_none());
    assert_eq!(
        app.request("GET", "/api/users", Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for (route, body) in [
        (
            "/api/users",
            json!({"username":"intruder","password":PASSWORD,"role":"admin"}),
        ),
        (
            "/api/sources",
            json!({"name":"intruder","url":"https://example.org/cms"}),
        ),
        ("/api/library/scan", json!({})),
        (
            "/api/channels/import",
            json!({"playlist":"#EXTM3U\n#EXTINF:-1,Channel\nhttps://example.org/live.m3u8"}),
        ),
    ] {
        assert_eq!(
            app.request("POST", route, Some(&viewer), Some(body))
                .await
                .status,
            StatusCode::FORBIDDEN,
            "{route}"
        );
    }
    for (method, route, body) in [
        (
            "PUT",
            "/api/sources/1",
            Some(json!({"name":"intruder","url":"https://example.org/cms","enabled":true})),
        ),
        ("DELETE", "/api/sources/1", None),
        (
            "PUT",
            "/api/catalog/settings",
            Some(json!({"adult_sources_enabled":true})),
        ),
    ] {
        assert_eq!(
            app.request(method, route, Some(&viewer), body).await.status,
            StatusCode::FORBIDDEN,
            "{method} {route}"
        );
    }
    assert_eq!(
        app.request("GET", "/api/catalog/settings", Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request(
            "POST",
            "/api/users",
            Some(&admin),
            Some(json!({"username":"viewer","password":PASSWORD,"role":"user"}))
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        app.request("DELETE", "/api/users/1", Some(&admin), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn library_state_is_per_user_and_media_supports_ranges_and_head() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    fs::write(app.state.media_dir.join("Series.S01E01.mp4"), b"0123456789").unwrap();
    let scan = app
        .request("POST", "/api/library/scan", Some(&admin), None)
        .await;
    assert_eq!(scan.status, StatusCode::OK);
    assert_eq!(scan.json()["total"], 1);
    let catalog = app
        .request("GET", "/api/library", Some(&admin), None)
        .await
        .json();
    let id = catalog[0]["id"].as_i64().unwrap();
    let series_id = catalog[0]["series_id"].as_i64().unwrap();
    let favorite = format!("/api/library/{id}/favorite");
    let progress = format!("/api/library/{id}/progress");
    assert_eq!(
        app.request(
            "PUT",
            &favorite,
            Some(&admin),
            Some(json!({"favorite":true}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request(
            "PUT",
            &progress,
            Some(&admin),
            Some(json!({"position":9.0,"duration":10.0,"watched":true}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request(
            "PUT",
            &progress,
            Some(&admin),
            Some(json!({"position":11.0,"duration":10.0,"watched":false}))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.request(
            "PUT",
            "/api/library/99999/favorite",
            Some(&admin),
            Some(json!({"favorite":true}))
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.request(
            "PUT",
            &format!("/api/series/{series_id}/subscription"),
            Some(&admin),
            Some(json!({"subscribed":true}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let admin_catalog = app
        .request("GET", "/api/library?q=Series", Some(&admin), None)
        .await
        .json();
    let viewer_catalog = app
        .request("GET", "/api/library", Some(&viewer), None)
        .await
        .json();
    assert_eq!(admin_catalog[0]["favorite"], true);
    assert_eq!(admin_catalog[0]["position"], 9.0);
    assert_eq!(admin_catalog[0]["watched"], true);
    assert_eq!(viewer_catalog[0]["favorite"], false);
    assert_eq!(viewer_catalog[0]["position"], 0.0);
    assert_eq!(viewer_catalog[0]["watched"], false);
    let admin_series = app
        .request("GET", "/api/series", Some(&admin), None)
        .await
        .json();
    let viewer_series = app
        .request("GET", "/api/series", Some(&viewer), None)
        .await
        .json();
    assert_eq!(admin_series[0]["watched_episodes"], 1);
    assert_eq!(admin_series[0]["subscribed"], true);
    assert_eq!(viewer_series[0]["watched_episodes"], 0);
    assert_eq!(viewer_series[0]["subscribed"], false);
    let stream = format!("/api/library/{id}/stream");
    let range = app
        .request_with_headers(
            "GET",
            &stream,
            Some(&viewer),
            None,
            &[("range", "bytes=2-5")],
        )
        .await;
    assert_eq!(range.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.headers[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(range.body, b"2345");
    let head = app.request("HEAD", &stream, Some(&admin), None).await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.headers[header::CONTENT_LENGTH], "10");
    assert!(head.body.is_empty());
    let invalid_range = app
        .request_with_headers(
            "GET",
            &stream,
            Some(&viewer),
            None,
            &[("range", "bytes=20-30")],
        )
        .await;
    assert_eq!(invalid_range.status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn password_change_keeps_current_session_and_revokes_other_sessions() {
    let app = TestApp::new();
    let current = app.setup().await;
    let other = app.login("admin", PASSWORD).await.cookie();
    assert_eq!(
        app.request(
            "POST",
            "/api/auth/password",
            Some(&current),
            Some(
                json!({"current_password":"incorrect-password","password":"new-polyview-password"})
            )
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request(
            "POST",
            "/api/auth/password",
            Some(&current),
            Some(json!({"current_password":PASSWORD,"password":"new-polyview-password"}))
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", "/api/library", Some(&current), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", "/api/library", Some(&other), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.login("admin", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.login("admin", "new-polyview-password").await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn deleting_user_with_queued_downloads_removes_jobs_and_sessions() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let viewer_id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let download = app
        .request(
            "POST",
            "/api/downloads",
            Some(&viewer),
            Some(json!({"title":"Video","url":"https://example.org/video.mp4"})),
        )
        .await;
    assert_eq!(download.status, StatusCode::CREATED);
    let deleted = app
        .request(
            "DELETE",
            &format!("/api/users/{viewer_id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(
        deleted.status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&deleted.body)
    );
    assert_eq!(
        app.request("GET", "/api/library", Some(&viewer), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let conn = app.state.conn().unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM downloads WHERE user_id=?1",
            [viewer_id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE user_id=?1",
            [viewer_id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn imported_unicode_account_with_short_password_can_login_but_new_accounts_stay_strict() {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};

    let app = TestApp::new();
    for (username, password) in [("旧管理员", PASSWORD), ("admin", "123456")] {
        assert_eq!(
            app.request(
                "POST",
                "/api/auth/setup",
                None,
                Some(json!({"username":username,"password":password}))
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    let legacy_path = app._temp.path().join("original-polyview.db");
    let legacy = Connection::open(&legacy_path).unwrap();
    legacy.execute_batch(
        "CREATE TABLE users(id INTEGER PRIMARY KEY,username TEXT,password_hash TEXT,role TEXT,disabled INTEGER);
         CREATE TABLE source_config(key TEXT PRIMARY KEY,name TEXT,kind TEXT,api TEXT,disabled INTEGER,priority INTEGER);",
    ).unwrap();
    let hash = Argon2::default()
        .hash_password(
            b"123456",
            &SaltString::encode_b64(b"legacy-login-test-salt").unwrap(),
        )
        .unwrap()
        .to_string();
    legacy.execute(
        "INSERT INTO users(id,username,password_hash,role,disabled) VALUES(42,'旧管理员',?,'admin',0)",
        [hash],
    ).unwrap();
    let report = polyview::legacy::import_legacy(&app.state, &legacy_path, None).unwrap();
    assert_eq!(report.users_imported, 1);
    let login = app.login("旧管理员", "123456").await;
    assert_eq!(login.status, StatusCode::OK);
    assert_eq!(login.json()["id"], 42);
    assert_eq!(login.json()["username"], "旧管理员");
    let cookie = login.cookie();
    assert_eq!(
        app.request("GET", "/api/library", Some(&cookie), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.login("旧管理员", "654321").await.status,
        StatusCode::UNAUTHORIZED
    );
    for (username, password) in [("新用户", PASSWORD), ("new_user", "123456")] {
        assert_eq!(
            app.request(
                "POST",
                "/api/users",
                Some(&cookie),
                Some(json!({"username":username,"password":password,"role":"user"}))
            )
            .await
            .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        app.request(
            "POST",
            "/api/users",
            Some(&cookie),
            Some(json!({"username":"new_user","password":PASSWORD,"role":"user"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
}
