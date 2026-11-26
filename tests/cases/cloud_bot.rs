use super::*;
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{Method, Uri},
    response::{IntoResponse, Response},
};
use polyview::cloud::bot;
use std::time::Duration;

const SETTINGS: &str = "/api/cloud/bot";
const TOKEN: &str = "17:offline_fixture_not_a_real_token";
const COOKIE: &str = "test=offline-provider-cookie";
const SENDER: i64 = 719;
const GROUP: i64 = -100770;
const SHARE: &str = "https://pan.quark.cn/s/film123?pwd=A1b2";

fn binding(chat: i64, user: i64) -> Value {
    json!({"sender_id":SENDER,"chat_id":chat,"user_id":user})
}

#[tokio::test]
async fn bot_credentials_and_bindings_require_admin_and_restore_by_member_name() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let user = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let settings = json!({"enabled":true,"token":TOKEN,"bindings":[binding(GROUP,user)]});
    for method in ["GET", "PUT"] {
        let body = (method == "PUT").then(|| settings.clone());
        assert_eq!(
            app.request(method, SETTINGS, None, body.clone())
                .await
                .status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.request(method, SETTINGS, Some(&viewer), body)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request_with_headers("PUT", SETTINGS, Some(&admin), Some(settings.clone()), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request("GET", SETTINGS, Some(&admin), None)
            .await
            .json(),
        json!({"enabled":false,"token_configured":false,"bindings":[]})
    );
    let saved = app
        .request("PUT", SETTINGS, Some(&admin), Some(settings.clone()))
        .await;
    assert_eq!(saved.status, StatusCode::OK);
    let public = saved.json();
    assert_eq!(
        public,
        json!({"enabled":true,"token_configured":true,"bindings":[binding(GROUP,user)]})
    );
    for bad in [
        json!({"enabled":true,"token":"invalid-secret","bindings":[]}),
        json!({"enabled":true,"bindings":[binding(GROUP,99)]}),
        json!({"enabled":true,"bindings":[binding(SENDER+1,user)]}),
        json!({"enabled":true,"bindings":[binding(GROUP,user),binding(GROUP,user)]}),
        json!({"api_base":"https://untrusted.invalid/","token":TOKEN,"enabled":true,"bindings":[]}),
    ] {
        let rejected = app.request("PUT", SETTINGS, Some(&admin), Some(bad)).await;
        assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
        assert!(!String::from_utf8_lossy(&rejected.body).contains("secret"));
        assert!(!String::from_utf8_lossy(&rejected.body).contains(TOKEN));
    }
    assert_eq!(
        app.request("GET", SETTINGS, Some(&admin), None)
            .await
            .json(),
        public
    );
    let backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(
        backup["cloud"]["bot"],
        json!({"enabled":true,"token":TOKEN,"bindings":[{"sender_id":SENDER,"chat_id":GROUP,"username":"viewer"}]})
    );
    assert_eq!(
        app.request("GET", "/api/settings/export", Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let dest = TestApp::new();
    let owner = dest.setup().await;
    assert_eq!(
        dest.request(
            "POST",
            "/api/settings/import",
            Some(&owner),
            Some(backup.clone())
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        dest.request(
            "POST",
            "/api/users",
            Some(&owner),
            Some(json!({"username":"other","password":PASSWORD,"role":"user"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    let moved = dest.create_viewer(&owner).await;
    assert_ne!(moved, user);
    assert_eq!(
        dest.request("POST", "/api/settings/import", Some(&owner), Some(backup))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        dest.request("GET", SETTINGS, Some(&owner), None)
            .await
            .json(),
        json!({"enabled":true,"token_configured":true,"bindings":[binding(GROUP,moved)]})
    );
    let cleared = dest
        .request(
            "PUT",
            SETTINGS,
            Some(&owner),
            Some(json!({"enabled":false,"token":"","bindings":[]})),
        )
        .await;
    assert_eq!(cleared.json()["token_configured"], false);
}

#[derive(Default)]
struct Fixture {
    base: String,
    updates: Vec<Value>,
    calls: Vec<(String, Value)>,
    next_message: i64,
    saves: usize,
    searches: usize,
    resolves: usize,
    downloads: usize,
}

type FixtureState = Arc<Mutex<Fixture>>;

async fn respond(
    State(state): State<FixtureState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut fixture = state.lock().unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    if let Some(action) = uri.path().strip_prefix(&format!("/bot{TOKEN}/")) {
        assert_eq!(method, Method::POST);
        assert!(!headers.contains_key(header::COOKIE));
        fixture.calls.push((action.into(), body.clone()));
        let result = match action {
            "getUpdates" => {
                assert_eq!(body["allowed_updates"], json!(["message"]));
                // Deliberately replay older IDs. Offset checkpoints must also withstand a stale response.
                if body["offset"] == -1 {
                    json!(fixture.updates.last().into_iter().collect::<Vec<_>>())
                } else {
                    json!(fixture.updates)
                }
            }
            "getMe" => json!({"id":17,"username":"FixtureBot","is_bot":true}),
            "sendMessage" => {
                fixture.next_message += 1;
                json!({"message_id":fixture.next_message})
            }
            "editMessageText" => json!({"message_id":body["message_id"]}),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        return Json(json!({"ok":true,"result":result})).into_response();
    }
    if uri.path().starts_with("/1/clouddrive/") {
        assert_eq!(headers[header::COOKIE], COOKIE);
        let data = match uri.path() {
            "/1/clouddrive/share/sharepage/token" => {
                assert_eq!(body["passcode"], "A1b2");
                if body["pwd_id"] == "challenge" {
                    return Json(
                        json!({"status":403,"code":403,"message":"test-sensitive-service-message"}),
                    )
                    .into_response();
                }
                assert_eq!(body["pwd_id"], "film123");
                json!({"stoken":"share-secret"})
            }
            "/1/clouddrive/share/sharepage/detail" => {
                json!({"share":{"title":"Offline film"},"list":[{"fid":"video1","file_name":"Offline.mp4","share_fid_token":"file-secret","dir":false,"category":1}]})
            }
            "/1/clouddrive/share/sharepage/save" => {
                assert_eq!(body["to_pdir_fid"], "library");
                assert_eq!(body["fid_token_list"], json!(["file-secret"]));
                fixture.saves += 1;
                json!({"task_id":"job123"})
            }
            "/1/clouddrive/task" => json!({"status":2,"save_as":{"save_as_top_fids":["saved123"]}}),
            _ => return StatusCode::NOT_FOUND.into_response(),
        };
        return Json(json!({"status":200,"code":0,"metadata":{"_total":1},"data":data}))
            .into_response();
    }
    assert!(!headers.contains_key(header::COOKIE));
    match uri.path() {
        "/lookup" => {
            fixture.searches += 1;
            Json(json!({"results":[{"title":"Offline hit","url":SHARE}]})).into_response()
        }
        "/resolve" => {
            let input: Vec<_> =
                url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).collect();
            assert!(input.iter().any(|(k, v)| k == "url" && v == SHARE));
            fixture.resolves += 1;
            Json(json!({"url":format!("{}/fixture.mp4?token=private-media-token",fixture.base)}))
                .into_response()
        }
        "/fixture.mp4" => {
            fixture.downloads += 1;
            (
                [(header::CONTENT_TYPE, "video/mp4")],
                b"exact fixture bytes".as_slice(),
            )
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fixture() -> (TestApp, FixtureState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Fixture {
        base: base.clone(),
        ..Fixture::default()
    }));
    let server = Router::new().fallback(respond).with_state(state.clone());
    let task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
    (
        TestApp::with_cloud_fixture(base.parse().unwrap()),
        state,
        task,
    )
}

fn message(id: i64, sender: i64, chat: i64, text: &str) -> Value {
    json!({"update_id":id,"message":{"message_id":id,"date":polyview::now(),"from":{"id":sender,"is_bot":false},
        "chat":{"id":chat,"type":if chat<0 {"supergroup"} else {"private"}},"text":text}})
}

async fn until(state: &FixtureState, predicate: impl Fn(&Fixture) -> bool) {
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if predicate(&state.lock().unwrap()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("offline Bot API did not reach the expected state");
}

fn edited(f: &Fixture, chat: i64, text: &str) -> bool {
    f.calls.iter().any(|(op, p)| {
        op == "editMessageText"
            && p["chat_id"] == chat
            && p["text"].as_str().is_some_and(|s| s.contains(text))
    })
}

#[tokio::test]
async fn bound_updates_use_real_search_transfer_and_download_queue_and_resume_progress() {
    let (app, fixture, server) = fixture().await;
    let admin = app.setup().await;
    let user = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let base = fixture.lock().unwrap().base.clone();
    for (path, body) in [
        (
            "/api/cloud/channels",
            json!({"name":"Offline","kind":"api","target":format!("{base}/lookup")}),
        ),
        (
            "/api/cloud/accounts",
            json!({"name":"Family","provider":"quark","default_dir":"library","members_allowed":true,"cookie":COOKIE}),
        ),
        (
            "/api/parsers",
            json!({"name":"Offline","url_template":format!("{base}/resolve"),"flags":"quark"}),
        ),
    ] {
        assert_eq!(
            app.request("POST", path, Some(&admin), Some(body))
                .await
                .status,
            StatusCode::CREATED
        );
    }
    let worker = bot::start_worker(app.state.clone());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fixture.lock().unwrap().calls.is_empty());
    assert_eq!(app.request("PUT",SETTINGS,Some(&admin),Some(json!({"enabled":true,"token":TOKEN,"bindings":[binding(SENDER,user),binding(GROUP,user)]}))).await.status,StatusCode::OK);
    let mut anonymous = message(3, SENDER, GROUP, "/search film");
    anonymous["message"]["sender_chat"] = json!({"id":GROUP});
    let mut stale = message(4, SENDER, GROUP, SHARE);
    stale["message"]["date"] = json!(polyview::now() - 3600);
    fixture.lock().unwrap().updates = vec![
        message(1, SENDER + 1, GROUP, SHARE),
        message(2, SENDER, GROUP - 1, SHARE),
        anonymous,
        stale,
        message(
            5,
            SENDER,
            GROUP,
            "/save@OtherBot https://pan.quark.cn/s/ignored",
        ),
        message(6, SENDER, SENDER, "/search film"),
        message(7, SENDER, GROUP, "/search@FixtureBot film"),
        message(8, SENDER, GROUP, SHARE),
        message(8, SENDER, GROUP, SHARE),
        message(9, SENDER, GROUP, &format!("/download {SHARE}")),
        message(
            10,
            SENDER,
            SENDER,
            "/download https://pan.quark.cn/s/challenge?pwd=A1b2",
        ),
    ];
    until(&fixture, |f| {
        edited(f, GROUP, "已加入队列") && edited(f, SENDER, "操作失败")
    })
    .await;
    worker.abort();
    let _ = worker.await;
    {
        let f = fixture.lock().unwrap();
        assert_eq!((f.searches, f.saves, f.resolves), (2, 1, 1));
        assert!(edited(&f, SENDER, "Offline hit"));
        assert!(edited(&f, GROUP, "私发"));
        assert!(edited(&f, GROUP, "转存"));
        assert!(f.calls.iter().any(|(op, p)| op == "sendMessage"
            && p["chat_id"] == SENDER
            && p["text"].as_str().unwrap().contains("Offline hit")));
        assert!(
            f.calls
                .iter()
                .any(|(op, p)| op == "getUpdates" && p["timeout"].as_i64().unwrap() > 0)
        );
        for (_, p) in &f.calls {
            let output = p.to_string();
            for secret in [
                TOKEN,
                COOKIE,
                "file-secret",
                "share-secret",
                "private-media-token",
                "test-sensitive-service-message",
            ] {
                assert!(!output.contains(secret));
            }
            if p["chat_id"] == GROUP {
                assert!(!output.contains("Offline hit") && !output.contains("A1b2"));
            }
        }
    }
    let jobs = app
        .request("GET", "/api/downloads", Some(&viewer), None)
        .await
        .json();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert_eq!(jobs[0]["url"], "https://pan.quark.cn/s/film123");
    assert_eq!(
        app.request("GET", "/api/downloads", Some(&admin), None)
            .await
            .json(),
        json!([])
    );
    let downloads = polyview::network::start_worker(app.state.clone());
    let restarted = bot::start_worker(app.state.clone());
    until(&fixture, |f| {
        edited(f, GROUP, "已完成")
            && f.calls
                .iter()
                .any(|(op, p)| op == "getUpdates" && p["offset"] == 11)
    })
    .await;
    let jobs = app
        .request("GET", "/api/downloads", Some(&viewer), None)
        .await
        .json();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert_eq!(jobs[0]["status"], "completed");
    assert_eq!(
        fs::read(
            app.state
                .media_dir
                .join(jobs[0]["relative_path"].as_str().unwrap())
        )
        .unwrap(),
        b"exact fixture bytes"
    );
    {
        let f = fixture.lock().unwrap();
        assert_eq!((f.saves, f.resolves, f.downloads), (1, 1, 1));
        let mut ids = f
            .calls
            .iter()
            .filter(|(op, p)| {
                op == "editMessageText" && p["text"].as_str().is_some_and(|s| s.contains("下载 #"))
            })
            .map(|(_, p)| p["message_id"].as_i64().unwrap());
        let first = ids.next().unwrap();
        assert_eq!(ids.next(), Some(first));
        assert!(ids.all(|id| id == first));
    }
    restarted.abort();
    downloads.abort();
    server.abort();
}

#[tokio::test]
async fn restored_bot_discards_existing_commands_and_disabled_members_are_ignored() {
    let source = TestApp::new();
    let admin = source.setup().await;
    let user = source.create_viewer(&admin).await;
    assert_eq!(
        source
            .request(
                "PUT",
                SETTINGS,
                Some(&admin),
                Some(json!({"enabled":true,"token":TOKEN,"bindings":[binding(SENDER,user)]}))
            )
            .await
            .status,
        StatusCode::OK
    );
    let backup = source
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    let (app, fixture, server) = fixture().await;
    let owner = app.setup().await;
    let viewer = app.create_viewer(&owner).await;
    fixture
        .lock()
        .unwrap()
        .updates
        .push(message(99, SENDER, SENDER, "/search stale"));
    assert_eq!(
        app.request("POST", "/api/settings/import", Some(&owner), Some(backup))
            .await
            .status,
        StatusCode::OK
    );
    let worker = bot::start_worker(app.state.clone());
    until(&fixture, |f| {
        f.calls
            .iter()
            .any(|(op, p)| op == "getUpdates" && p["offset"] == 100)
    })
    .await;
    assert_eq!(fixture.lock().unwrap().next_message, 0);
    assert_eq!(
        app.request(
            "PUT",
            &format!("/api/users/{viewer}"),
            Some(&owner),
            Some(json!({"disabled":true}))
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    fixture
        .lock()
        .unwrap()
        .updates
        .push(message(100, SENDER, SENDER, "/search blocked"));
    until(&fixture, |f| {
        f.calls
            .iter()
            .any(|(op, p)| op == "getUpdates" && p["offset"] == 101)
    })
    .await;
    assert_eq!(fixture.lock().unwrap().next_message, 0);
    worker.abort();
    server.abort();
}
