use super::*;
use crate::{AppState, settings::set_setting_json};
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use tempfile::TempDir;
use tower::ServiceExt;

const LOGIN: &str = "/api/telegram/login";
const REQUESTS: &str = "/api/telegram/login/requests";
const PROXY: &str = "socks5://alice:private-value@localhost:1080";

struct Fixture {
    temp: TempDir,
    state: Shared,
    router: Router,
    settings: DownloadSettings,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("operator's tdl");
        fs::write(
            &executable,
            r#"#!/bin/sh
if [ "$1" = '--help' ]; then exit 0; fi
printf '%s\n' 'private Telegram data' >&2
[ "$1" = '-n' ] && [ "$3" = '--storage' ] && [ "$5" = '--proxy' ] || exit 1
[ "$6" = 'socks5://alice:private-value@localhost:1080' ] || exit 1
[ "$7" = 'chat' ] && [ "$8" = 'ls' ] && [ "$#" = 8 ] || exit 1
if read -r password; then exit 1; fi
test -f "${4#type=bolt,path=}/$2.fixture"
"#,
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let settings = DownloadSettings {
            tdl_path: executable.to_str().unwrap().into(),
            telegram_directory: temp.path().join("local storage").to_str().unwrap().into(),
            telegram_proxy: PROXY.into(),
            ..Default::default()
        };
        let conn = Connection::open_in_memory().unwrap();
        crate::init_schema(&conn).unwrap();
        conn.execute_batch("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(1,'operator','unused','admin',0),(2,'viewer','unused','user',0);
            INSERT INTO sessions(token,user_id,expires_at) VALUES('operator',1,9000000000),('viewer',2,9000000000);").unwrap();
        set_setting_json(&conn, "downloads", &settings).unwrap();
        let state = Arc::new(AppState {
            db: std::sync::Mutex::new(conn),
            download_runtime: Default::default(),
            media_dir: temp.path().join("media"),
            download_dir: temp.path().join("downloads"),
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        let router = crate::router(state.clone());
        Self {
            temp,
            settings,
            state,
            router,
        }
    }

    async fn call(&self, method: Method, target: &str, session: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .method(method)
            .uri(target)
            .header("cookie", format!("polyview_session={session}"))
            .header("x-polyview-request", "1")
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        (status, value)
    }

    async fn member(&self, method: Method, token: &str) -> Value {
        let (code, body) = self.call(method, LOGIN, token).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let text = body.to_string();
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert!(body["message"].is_string());
        for secret in [
            "private-value",
            "private Telegram data",
            "operator's tdl",
            self.temp.path().to_str().unwrap(),
        ] {
            assert!(!text.contains(secret));
        }
        body["state"].clone()
    }

    fn directory(&self, user: i64) -> PathBuf {
        let location = Location::new(&self.settings, user).unwrap();
        let scope = location
            .scope(&self.state.conn().unwrap(), user)
            .unwrap()
            .unwrap();
        location.directory(&scope)
    }

    fn authorize_on_host(&self, user: i64) {
        let directory = self.directory(user);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(format!("polyview_u{user}.fixture")), b"stub").unwrap();
    }
}

#[tokio::test]
async fn host_requests_are_privileged_and_authorization_requires_an_authenticated_probe() {
    let app = Fixture::new();
    assert_eq!(
        app.call(Method::GET, LOGIN, "expired").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(app.member(Method::GET, "viewer").await, "idle");
    assert_eq!(app.member(Method::POST, "viewer").await, "waiting");
    assert_eq!(
        app.call(Method::GET, REQUESTS, "viewer").await.0,
        StatusCode::FORBIDDEN
    );
    let (code, rows) = app.call(Method::GET, REQUESTS, "operator").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["username"], "viewer");
    assert_eq!(rows[0]["user_id"], 2);
    assert_eq!(rows[0]["uses_proxy"], true);
    let cmd = rows[0]["command"].as_str().unwrap();
    assert!(cmd.contains("umask 077"));
    assert!(cmd.contains(" telegram-host-login "));
    assert!(cmd.contains("operator'\\''s tdl' -n 'polyview_u2'"));
    assert!(cmd.contains(&format!("'type=bolt,path={}'", app.directory(2).display())));
    assert!(cmd.contains("${TDL_PROXY:?"));
    assert!(cmd.ends_with("login -T qr)"));
    assert!(!cmd.contains("private-value"));
    assert!(tdl_command(&app.state.conn().unwrap(), &app.settings, 2).is_err());
    assert!(!app.directory(2).exists());
    fs::create_dir_all(app.directory(2)).unwrap();
    assert_eq!(app.member(Method::GET, "viewer").await, "waiting");
    app.authorize_on_host(2);
    assert_eq!(app.member(Method::GET, "viewer").await, "authorized");
    assert!(tdl_command(&app.state.conn().unwrap(), &app.settings, 2).is_ok());
    assert_eq!(app.member(Method::DELETE, "viewer").await, "authorized");
    assert!(
        app.call(Method::GET, REQUESTS, "operator")
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::remove_file(app.directory(2).join("polyview_u2.fixture")).unwrap();
    assert_eq!(app.member(Method::GET, "viewer").await, "error");
    assert!(tdl_command(&app.state.conn().unwrap(), &app.settings, 2).is_err());
    assert_eq!(app.member(Method::POST, "viewer").await, "waiting");
    assert_eq!(
        app.call(Method::POST, "/api/telegram/login/password", "viewer")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn requests_are_idempotent_and_cancel_or_recycled_ids_cannot_adopt_stale_logins() {
    let app = Fixture::new();
    let states = tokio::join!(
        app.member(Method::POST, "viewer"),
        app.member(Method::POST, "operator")
    );
    assert_eq!(states.0, "waiting");
    assert_eq!(states.1, "waiting");
    let old = app.directory(2);
    let admin = app.directory(1);
    assert_ne!(old.parent(), admin.parent());
    let repeated = tokio::join!(
        app.member(Method::POST, "viewer"),
        app.member(Method::POST, "viewer")
    );
    assert_eq!(repeated.0, "waiting");
    assert_eq!(repeated.1, "waiting");
    assert_eq!(old, app.directory(2));
    app.authorize_on_host(1);
    assert_eq!(app.member(Method::GET, "operator").await, "authorized");
    assert_eq!(app.member(Method::DELETE, "viewer").await, "idle");
    // A terminal that was already open may finish after the request was canceled.
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("polyview_u2.fixture"), b"late").unwrap();
    assert_eq!(app.member(Method::GET, "viewer").await, "idle");
    assert_eq!(app.member(Method::POST, "viewer").await, "waiting");
    let next = app.directory(2);
    assert_ne!(old, next);
    assert_eq!(old.parent(), next.parent());
    assert_eq!(app.member(Method::GET, "viewer").await, "waiting");
    app.authorize_on_host(2);
    assert_eq!(app.member(Method::GET, "viewer").await, "authorized");
    app.state.conn().unwrap().execute_batch("DELETE FROM users WHERE id=2;
        INSERT INTO users(id,username,password_hash,role,created_at) VALUES(2,'viewer','unused','user',0);
        INSERT INTO sessions VALUES('replacement',2,9000000000);").unwrap();
    assert_eq!(
        app.call(Method::GET, LOGIN, "viewer").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(app.member(Method::GET, "replacement").await, "idle");
    assert_eq!(app.member(Method::POST, "replacement").await, "waiting");
    assert_ne!(old.parent(), app.directory(2).parent());
    assert_eq!(app.member(Method::GET, "replacement").await, "waiting");
    assert_eq!(app.member(Method::GET, "operator").await, "authorized");
    assert_eq!(admin, app.directory(1));
}

#[tokio::test]
async fn missing_broken_and_timed_out_clients_fail_closed() {
    let app = Fixture::new();
    fs::remove_file(&app.settings.tdl_path).unwrap();
    assert_eq!(app.member(Method::GET, "viewer").await, "unconfigured");
    assert_eq!(app.member(Method::POST, "viewer").await, "unconfigured");
    assert!(
        app.call(Method::GET, REQUESTS, "operator")
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::write(&app.settings.tdl_path, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&app.settings.tdl_path, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(app.member(Method::GET, "viewer").await, "error");
    fs::write(&app.settings.tdl_path, "#!/bin/sh\nexec sleep 60\n").unwrap();
    assert!(matches!(
        run_quietly(
            &mut Command::new(&app.settings.tdl_path),
            Duration::from_millis(100)
        )
        .await,
        Outcome::Error
    ));
}

#[tokio::test]
async fn finishing_a_probe_after_account_replacement_cannot_authorize_the_replacement() {
    let app = Fixture::new();
    assert_eq!(app.member(Method::POST, "viewer").await, "waiting");
    let directory = app.directory(2);
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        &app.settings.tdl_path,
        r#"#!/bin/sh
if [ "$1" = '--help' ]; then exit 0; fi
dir=${4#type=bolt,path=}
touch "$dir/running"
while [ ! -f "$dir/finish" ]; do sleep 0.02; done
"#,
    )
    .unwrap();
    let probe = app.call(Method::GET, LOGIN, "viewer");
    let replace = async {
        timeout(Duration::from_secs(5), async {
            while !directory.join("running").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        app.state.conn().unwrap().execute_batch("DELETE FROM users WHERE id=2;
            INSERT INTO users(id,username,password_hash,role,created_at) VALUES(2,'viewer','unused','user',0);
            INSERT INTO sessions(token,user_id,expires_at) VALUES('replacement',2,9000000000);").unwrap();
        fs::write(directory.join("finish"), []).unwrap();
    };
    let ((code, _), ()) = tokio::join!(probe, replace);
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert_eq!(app.member(Method::GET, "replacement").await, "idle");
    assert!(tdl_command(&app.state.conn().unwrap(), &app.settings, 2).is_err());
}
