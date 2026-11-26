use super::*;
use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use std::time::Duration;

const ADMIN_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const USER_HASH: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const TASKS: &str = "/api/external-tasks";
const ACCOUNTS: &str = "/api/downloaders";
const MAGNET: &str = "magnet:?xt=urn:btih:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[derive(Default)]
struct Daemon {
    added: bool,
    tag: String,
    paused: bool,
    stopped: usize,
    started: usize,
    removed: usize,
    handshakes: usize,
    complete: bool,
}
type Fixture = Arc<Mutex<Daemon>>;

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move {
            axum::serve(socket, app).await.unwrap();
        }),
    )
}

async fn qb(State(state): State<Fixture>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let body = String::from_utf8(to_bytes(body, 65536).await.unwrap().to_vec()).unwrap();
    let path = parts.uri.path().strip_prefix("/api/v2/").unwrap();
    let mut d = state.lock().unwrap();
    if path == "auth/login" {
        assert_eq!(parts.method, "POST");
        assert_eq!(body, "username=user&password=pass");
        d.handshakes += 1;
        return (
            [(header::SET_COOKIE, "SID=test-session; HttpOnly; path=/")],
            "Ok.",
        )
            .into_response();
    }
    assert_eq!(parts.headers[header::COOKIE], "SID=test-session");
    match path {
        "auth/logout" => StatusCode::OK.into_response(),
        "torrents/info" => {
            let mut rows = vec![
                json!({"hash":ADMIN_HASH,"name":"Admin torrent","state":"stalledDL","progress":0.2,"save_path":"/remote/torrents","tags":""}),
            ];
            if d.added {
                rows.push(json!({"hash":USER_HASH,"name":"Member torrent","state":if d.paused {"stoppedDL"} else {"downloading"},"progress":0.5,"save_path":"/remote/torrents","tags":d.tag}));
            }
            Json(rows).into_response()
        }
        "torrents/files" => {
            assert!(matches!(
                parts.uri.query().unwrap().strip_prefix("hash="),
                Some(USER_HASH | ADMIN_HASH)
            ));
            Json(json!([{"name":"Movie.mkv","size":10,"progress":0.5,"priority":1}]))
                .into_response()
        }
        "torrents/add" => {
            assert!(
                parts.headers[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("multipart/form-data; boundary=")
            );
            assert!(body.contains(MAGNET));
            d.tag = body
                .lines()
                .find(|line| line.starts_with("polyview-"))
                .unwrap()
                .to_owned();
            d.added = true;
            "Ok.".into_response()
        }
        "torrents/stop" | "torrents/start" => StatusCode::NOT_FOUND.into_response(),
        "torrents/pause" | "torrents/resume" => {
            assert_eq!(body, format!("hashes={USER_HASH}"));
            d.paused = path.ends_with("pause");
            if d.paused {
                d.stopped += 1;
            } else {
                d.started += 1;
            }
            StatusCode::OK.into_response()
        }
        "torrents/delete" => {
            assert_eq!(body, format!("hashes={USER_HASH}&deleteFiles=false"));
            d.removed += 1;
            d.added = false;
            StatusCode::OK.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn transmission(State(state): State<Fixture>, req: Request<Body>) -> Response {
    assert_eq!(req.uri().path(), "/transmission/rpc");
    assert_eq!(req.method(), "POST");
    assert_eq!(req.headers()[header::AUTHORIZATION], "Basic dXNlcjpwYXNz");
    if req.headers().get("x-transmission-session-id").is_none() {
        state.lock().unwrap().handshakes += 1;
        return (
            StatusCode::CONFLICT,
            [("x-transmission-session-id", "rpc-token")],
        )
            .into_response();
    }
    assert_eq!(req.headers()["x-transmission-session-id"], "rpc-token");
    let body: Value =
        serde_json::from_slice(&to_bytes(req.into_body(), 65536).await.unwrap()).unwrap();
    let args = &body["arguments"];
    let mut d = state.lock().unwrap();
    let arguments = match body["method"].as_str().unwrap() {
        "torrent-get" => {
            assert!(
                args["fields"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("fileStats"))
            );
            let mut rows = vec![];
            for (hash, title) in [(ADMIN_HASH, "Admin torrent"), (USER_HASH, "Member torrent")] {
                if hash == USER_HASH && !d.added {
                    continue;
                }
                let files = if d.complete {
                    vec![
                        "Movie.mkv",
                        "../outside.mkv",
                        "link.mkv",
                        "missing.mkv",
                        "skip.mkv",
                    ]
                } else {
                    vec!["Movie.mkv"]
                };
                rows.push(json!({"hashString":hash,"name":title,"error":0,"status":if d.paused {0} else if d.complete {6} else {4},"percentDone":if d.complete {1.0} else {0.5},"downloadDir":"/remote/torrents","files":files.iter().map(|name|json!({"name":name,"length":10,"bytesCompleted":if d.complete {10} else {5}})).collect::<Vec<_>>(),"fileStats":files.iter().map(|name|json!({"wanted":*name!="skip.mkv"})).collect::<Vec<_>>() }));
            }
            json!({"torrents":rows})
        }
        "torrent-add" => {
            assert_eq!(args["filename"], MAGNET);
            if d.added {
                json!({"torrent-duplicate":{"hashString":USER_HASH}})
            } else {
                d.added = true;
                json!({"torrent-added":{"hashString":USER_HASH}})
            }
        }
        "torrent-stop" | "torrent-start" | "torrent-remove" => {
            assert_eq!(args["ids"], json!([USER_HASH]));
            match body["method"].as_str().unwrap() {
                "torrent-stop" => {
                    d.paused = true;
                    d.stopped += 1;
                }
                "torrent-start" => {
                    d.paused = false;
                    d.started += 1;
                }
                _ => {
                    assert_eq!(args["delete-local-data"], false);
                    d.removed += 1;
                    d.added = false;
                }
            }
            json!({})
        }
        method => panic!("unexpected RPC {method}"),
    };
    Json(json!({"result":"success","arguments":arguments})).into_response()
}

fn config(provider: &str, endpoint: &str) -> Value {
    json!({"provider":provider,"endpoint":endpoint,"name":"NAS","username":"user","password":"pass","members_allowed":true})
}

async fn exercise(provider: &str) {
    let daemon = Fixture::default();
    let router = if provider == "qbittorrent" {
        Router::new().fallback(qb)
    } else {
        Router::new().fallback(transmission)
    };
    let (url, server) = serve(router.with_state(daemon.clone())).await;
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let member = app.login("viewer", PASSWORD).await.cookie();
    let account = app
        .request("POST", ACCOUNTS, Some(&admin), Some(config(provider, &url)))
        .await;
    assert_eq!(account.status, StatusCode::CREATED);
    assert!(account.json().get("password").is_none() && account.json().get("username").is_none());
    let id = account.json()["id"].as_i64().unwrap();
    assert_eq!(
        app.request("GET", ACCOUNTS, Some(&member), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request("GET", "/api/downloaders/choices", Some(&member), None)
            .await
            .json(),
        json!([{"id":id,"name":"NAS","provider":provider}])
    );
    assert_eq!(
        app.request("GET", TASKS, Some(&member), None).await.json(),
        json!([])
    );
    let preexisting = app.request("GET", TASKS, Some(&admin), None).await.json()[0]["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        app.request(
            "POST",
            &format!("{TASKS}/{preexisting}/pause"),
            Some(&member),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let input = json!({"downloader_id":id,"url":MAGNET});
    assert_eq!(
        app.request_with_headers("POST", TASKS, Some(&member), Some(input.clone()), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request("POST", TASKS, Some(&member), Some(input.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    if provider == "transmission" {
        assert_eq!(
            app.request("POST", TASKS, Some(&admin), Some(input))
                .await
                .status,
            StatusCode::CONFLICT
        );
    }
    let rows = app.request("GET", TASKS, Some(&member), None).await.json();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let task = &rows[0];
    assert_eq!(task["name"], "Member torrent");
    assert_eq!(task["status"], "running");
    assert_eq!(task["files"], json!([{"name":"Movie.mkv","size":10}]));
    assert_eq!(task["progress"], 0.5);
    let item = format!("{TASKS}/{}", task["id"].as_i64().unwrap());
    assert_eq!(
        app.request(
            "POST",
            "/api/users",
            Some(&admin),
            Some(json!({"username":"second","password":PASSWORD,"role":"user"}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    let second = app.login("second", PASSWORD).await.cookie();
    assert_eq!(
        app.request("GET", TASKS, Some(&second), None).await.json(),
        json!([])
    );
    for (method, target) in [
        ("POST", format!("{item}/pause")),
        ("POST", format!("{item}/resume")),
        ("DELETE", item.clone()),
    ] {
        assert_eq!(
            app.request(method, &target, Some(&second), None)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        app.request("POST", &format!("{item}/pause"), Some(&member), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", TASKS, Some(&member), None).await.json()[0]["status"],
        "paused"
    );
    assert_eq!(
        app.request("POST", &format!("{item}/resume"), Some(&member), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("DELETE", &item, Some(&member), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", TASKS, Some(&member), None).await.json(),
        json!([])
    );
    let d = daemon.lock().unwrap();
    assert_eq!((d.stopped, d.started, d.removed), (1, 1, 1));
    assert!(d.handshakes > 0);
    server.abort();
}

#[tokio::test]
async fn qbittorrent_session_uses_tagged_add_and_keeps_files_on_remove() {
    exercise("qbittorrent").await;
}
#[tokio::test]
async fn transmission_replays_rpc_after_409_and_keeps_files_on_remove() {
    exercise("transmission").await;
}

#[tokio::test]
async fn downloader_backup_restores_secrets_atomically_and_preserves_private_settings() {
    let original = TestApp::new();
    let admin = original.setup().await;
    let mut input = config("transmission", "http://nas.test");
    input["members_allowed"] = false.into();
    assert_eq!(
        original
            .request("POST", ACCOUNTS, Some(&admin), Some(input.clone()))
            .await
            .status,
        StatusCode::CREATED
    );
    input.as_object_mut().unwrap().remove("password");
    input.as_object_mut().unwrap().remove("username");
    assert_eq!(
        original
            .request("PUT", "/api/downloaders/1", Some(&admin), Some(input))
            .await
            .json()["has_credentials"],
        true
    );
    let backup = original
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(backup["automation"]["downloaders"][0]["password"], "pass");
    let fresh = TestApp::new();
    let root = fresh.setup().await;
    assert_eq!(
        fresh
            .request(
                "POST",
                "/api/settings/import",
                Some(&root),
                Some(backup.clone())
            )
            .await
            .status,
        StatusCode::OK
    );
    let mut broken = backup.clone();
    broken["automation"]["downloaders"][0]["local_root"] = "../invalid".into();
    broken["epg_config"]["url"] = "https://wrong.test/guide".into();
    assert_eq!(
        fresh
            .request("POST", "/api/settings/import", Some(&root), Some(broken))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let after = fresh
        .request("GET", "/api/settings/export", Some(&root), None)
        .await
        .json();
    assert_eq!(
        after["automation"]["downloaders"],
        backup["automation"]["downloaders"]
    );
    assert_eq!(after["epg_config"], backup["epg_config"]);
    fresh.create_viewer(&root).await;
    let viewer = fresh.login("viewer", PASSWORD).await.cookie();
    assert_eq!(
        fresh
            .request("GET", "/api/downloaders/choices", Some(&viewer), None)
            .await
            .json(),
        json!([])
    );
    assert_eq!(
        fresh
            .request(
                "POST",
                TASKS,
                Some(&viewer),
                Some(json!({"downloader_id":1,"url":MAGNET}))
            )
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

async fn await_events(app: &TestApp, complete: i64, failed: i64) {
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let counts=app.state.conn().unwrap().query_row("SELECT count(*) FILTER (WHERE done=1),count(*) FILTER (WHERE done=0 AND attempts>0) FROM external_completions",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).unwrap();
            if counts==(complete,failed) {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
#[cfg(unix)]
async fn completed_mount_files_retry_offline_and_reject_traversal_and_symlink_escape() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let fixture = Fixture::default();
    fixture.lock().unwrap().complete = true;
    let (url, server) = serve(Router::new().fallback(transmission).with_state(fixture)).await;
    let mount = app.state.download_dir.join("mount");
    fs::create_dir(&mount).unwrap();
    fs::write(mount.join("Movie.mkv"), [0u8; 10]).unwrap();
    // Local discovery can win the race with the daemon's completion poll.
    assert_eq!(
        app.request("POST", "/api/library/scan", Some(&admin), None)
            .await
            .status,
        StatusCode::OK
    );
    let media_id = app
        .state
        .conn()
        .unwrap()
        .query_row("SELECT id FROM library_media", [], |r| r.get::<_, i64>(0))
        .unwrap();
    assert_eq!(
        app.request(
            "PUT",
            "/api/settings/organizer",
            Some(&admin),
            Some(json!({"mode":"move"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let organized = app
        .request(
            "POST",
            &format!("/api/library/{media_id}/organize"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(organized.status, StatusCode::OK);
    let target = organized.json()["target_path"].as_str().unwrap().to_owned();
    assert!(!mount.join("Movie.mkv").exists());
    let outside = app.state.download_dir.join("outside.mkv");
    fs::write(&outside, [0u8; 10]).unwrap();
    std::os::unix::fs::symlink(&outside, mount.join("link.mkv")).unwrap();
    let mut input = config("transmission", &url);
    input["remote_root"] = "/remote/torrents".into();
    input["local_root"] = json!(mount);
    assert_eq!(
        app.request("POST", ACCOUNTS, Some(&admin), Some(input))
            .await
            .status,
        StatusCode::CREATED
    );
    let worker = polyview::downloaders::start_worker(app.state.clone());
    await_events(&app, 1, 3).await;
    worker.abort();
    let _ = worker.await;
    server.abort();
    let _ = server.await;
    fs::write(mount.join("missing.mkv"), [1u8; 10]).unwrap();
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE external_completions SET retry_at=0 WHERE file='missing.mkv'",
            [],
        )
        .unwrap();
    let restarted = polyview::downloaders::start_worker(app.state.clone());
    await_events(&app, 2, 2).await;
    restarted.abort();
    let _ = restarted.await;
    let paths = app
        .state
        .conn()
        .unwrap()
        .prepare("SELECT relative_path FROM library_media ORDER BY relative_path")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&target));
    assert!(paths.iter().any(|path| path.ends_with("mount/missing.mkv")));
    let queued: i64 = app
        .state
        .conn()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM library_automation WHERE external=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 2);
}
