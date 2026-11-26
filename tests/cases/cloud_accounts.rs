use super::*;
use axum::{
    Json,
    response::{IntoResponse, Redirect, Response},
};
use std::sync::atomic::{AtomicUsize, Ordering};

const ACCOUNTS: &str = "/api/cloud/accounts";
const CHOICES: &str = "/api/cloud/choices";
const CHECK: &str = "/api/cloud/check";
const SAVE: &str = "/api/cloud/save";
const SHARE: &str = "https://pan.quark.cn/s/A123";
const DRIVE_COOKIE: &str = "session=fake-provider-session";
const DRIVE_TOKEN: &str = "fake-provider-token";

fn account() -> Value {
    json!({
        "name": "Family drive", "provider": "quark", "default_dir": "films",
        "cookie": DRIVE_COOKIE, "token": DRIVE_TOKEN
    })
}

fn no_secrets(bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    assert!(!text.contains(DRIVE_COOKIE));
    assert!(!text.contains(DRIVE_TOKEN));
    assert!(!text.contains("signed-cdn-query"));
}

#[tokio::test]
async fn cloud_accounts_isolate_credentials_authorize_members_and_restore_atomically() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    assert_eq!(
        app.request("GET", CHOICES, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    for (method, path, body) in [
        ("GET", ACCOUNTS, None),
        ("POST", ACCOUNTS, Some(account())),
        ("PUT", "/api/cloud/accounts/1", Some(account())),
        ("DELETE", "/api/cloud/accounts/1", None),
        ("GET", "/api/settings/export", None),
    ] {
        assert_eq!(
            app.request(method, path, Some(&viewer), body).await.status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request_with_headers("POST", ACCOUNTS, Some(&admin), Some(account()), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for endpoint in [
        "https://attacker.invalid",
        "http://127.0.0.1:8000",
        "https://drive-pc.quark.cn@other.test",
        "https://drive-pc.quark.cn:8443",
    ] {
        let mut record = account();
        record["base_endpoint"] = json!(endpoint);
        let invalid = app
            .request("POST", ACCOUNTS, Some(&admin), Some(record))
            .await;
        assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
        no_secrets(&invalid.body);
    }
    let created = app
        .request("POST", ACCOUNTS, Some(&admin), Some(account()))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["id"].as_i64().unwrap();
    let item = format!("{ACCOUNTS}/{id}");
    no_secrets(&created.body);
    assert_eq!(created.json()["credential_configured"], true);
    assert_eq!(created.json()["members_allowed"], false);
    assert_eq!(
        app.request("GET", CHOICES, Some(&viewer), None)
            .await
            .json(),
        json!([])
    );
    let share = json!({"account_id":id,"url":SHARE});
    for path in [CHECK, SAVE] {
        assert_eq!(
            app.request("POST", path, Some(&viewer), Some(share.clone()))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        app.request(
            "GET",
            &format!("/api/cloud/folders?account_id={id}"),
            Some(&viewer),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let listed = app.request("GET", ACCOUNTS, Some(&admin), None).await;
    no_secrets(&listed.body);
    assert_eq!(listed.json()[0], created.json());
    let update = json!({"provider":"quark","name":"Shared", "members_allowed":true});
    assert_eq!(
        app.request("PUT", &item, Some(&admin), Some(update))
            .await
            .status,
        StatusCode::OK
    );
    let choices = app.request("GET", CHOICES, Some(&viewer), None).await;
    assert_eq!(
        choices.json(),
        json!([{"id":id, "provider":"quark","name":"Shared", "default_dir":"films"}])
    );
    let mut override_attempt = share.clone();
    override_attempt["base_endpoint"] = json!("http://127.0.0.1:12345");
    assert!(
        app.request("POST", SAVE, Some(&viewer), Some(override_attempt))
            .await
            .status
            .is_client_error()
    );
    let mut strm_attempt = share;
    strm_attempt["export_strm"] = json!(true);
    assert_eq!(
        app.request("POST", SAVE, Some(&viewer), Some(strm_attempt))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(backup["cloud"]["accounts"][0]["cookie"], DRIVE_COOKIE);
    assert_eq!(backup["cloud"]["accounts"][0]["token"], DRIVE_TOKEN);
    let mut broken = backup.clone();
    broken["cloud"]["accounts"][0]["base_endpoint"] = json!("https://attacker.invalid");
    broken["epg_config"]["url"] = json!("https://guide.example/bad-restore.xml");
    let rejected = app
        .request("POST", "/api/settings/import", Some(&admin), Some(broken))
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    no_secrets(&rejected.body);
    let preserved = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(preserved["epg_config"], backup["epg_config"]);
    assert_eq!(preserved["cloud"], backup["cloud"]);
    assert_eq!(
        app.request("DELETE", &item, Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", CHOICES, Some(&viewer), None)
            .await
            .json(),
        json!([])
    );
    assert_eq!(
        app.request("POST", "/api/settings/import", Some(&admin), Some(backup))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", CHOICES, Some(&viewer), None)
            .await
            .json(),
        choices.json()
    );
}

async fn serve(router: Router) -> (reqwest::Url, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}/", socket.local_addr().unwrap())
        .parse()
        .unwrap();
    (
        origin,
        tokio::spawn(async move { axum::serve(socket, router).await.unwrap() }),
    )
}

async fn drive(request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    if parts.uri.path() == "/fixture.mp4" {
        assert!(!parts.headers.contains_key(header::COOKIE));
        assert!(!parts.headers.contains_key(header::AUTHORIZATION));
        assert_eq!(parts.uri.query(), Some("signed-cdn-query"));
        assert_eq!(parts.headers[header::RANGE], "bytes=0-4");
        return (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, "video/mp4"),
                (header::CONTENT_RANGE, "bytes 0-4/20"),
            ],
            "video",
        )
            .into_response();
    }
    assert_eq!(parts.headers[header::COOKIE], DRIVE_COOKIE);
    assert_eq!(
        parts.headers[header::AUTHORIZATION],
        format!("Bearer {DRIVE_TOKEN}").as_str()
    );
    let bytes = to_bytes(body, 4096).await.unwrap();
    let input: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    let data = match parts.uri.path() {
        "/1/clouddrive/file/sort" => {
            json!({"list":[{"fid":"films","file_type":0,"file_name":"Movies"}]})
        }
        "/1/clouddrive/share/sharepage/token" => {
            assert_eq!(input["passcode"], "abcd");
            json!({"stoken":"share-access"})
        }
        "/1/clouddrive/share/sharepage/detail" => {
            json!({"list":[{"fid":"src","file_name":"clip.mp4","file_type":1,"obj_category":"video","share_fid_token":"file-proof"}]})
        }
        "/1/clouddrive/share/sharepage/save" => {
            assert_eq!(input["to_pdir_fid"], "films");
            assert_eq!(input["fid_token_list"], json!(["file-proof"]));
            json!({"task_id":"task"})
        }
        "/1/clouddrive/task" => {
            json!({"status":2,"save_as":{"save_as_top_fids":["destination-file"]}})
        }
        "/1/clouddrive/file/v2/play/project" => {
            assert_eq!(input["fid"], "destination-file");
            json!({"video_list":[{"video_info":{"format":"mp4", "url":"https://video.quark.cn/fixture.mp4?signed-cdn-query"}}]})
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    Json(json!({"status":200,"code":0,"data":data})).into_response()
}

#[tokio::test]
async fn cloud_save_and_strm_use_the_completed_destination_and_revocable_playback_grant() {
    let (fixture, server) = serve(Router::new().fallback(drive)).await;
    let app = TestApp::with_cloud_fixture(fixture.clone());
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let mut record = account();
    record["members_allowed"] = json!(true);
    let created = app
        .request("POST", ACCOUNTS, Some(&admin), Some(record))
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["id"].as_i64().unwrap();
    let input = json!({"account_id":id,"url":SHARE,"code":"abcd"});
    let folders = app
        .request(
            "GET",
            &format!("/api/cloud/folders?account_id={id}"),
            Some(&viewer),
            None,
        )
        .await;
    assert_eq!(folders.json(), json!([{"id":"films","name":"Movies"}]));
    let checked = app
        .request("POST", CHECK, Some(&viewer), Some(input.clone()))
        .await;
    assert_eq!(checked.json(), json!({"valid":true,"title":"clip.mp4"}));
    let saved = app
        .request("POST", SAVE, Some(&viewer), Some(input.clone()))
        .await;
    assert_eq!(
        saved.json(),
        json!({"path":"quark://folder/films","task_id":"task"})
    );
    let mut export = input;
    export["export_strm"] = json!(true);
    let saved = app
        .request_with_headers(
            "POST",
            SAVE,
            Some(&admin),
            Some(export),
            &[("x-polyview-request", "1"), ("host", "attacker.invalid")],
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK);
    no_secrets(&saved.body);
    let body = saved.json();
    let name = body["strm_file"].as_str().unwrap();
    assert!(name.starts_with("cloud-") && name.ends_with(".strm") && !name.contains('/'));
    let file = fs::read_to_string(app.state.media_dir.join(name)).unwrap();
    no_secrets(file.as_bytes());
    assert!(!file.contains("quark.cn"));
    let url = reqwest::Url::parse(file.trim()).unwrap();
    assert_eq!(url.origin(), fixture.origin());
    assert!(url.path().starts_with("/cloud/stream/"));
    let streamed = app
        .request_with_headers(
            "GET",
            url.path(),
            None,
            None,
            &[("range", "bytes=0-4"), ("cookie", "other=browser")],
        )
        .await;
    assert_eq!(streamed.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(streamed.headers[header::CONTENT_RANGE], "bytes 0-4/20");
    assert_eq!(streamed.body, b"video");
    assert_eq!(
        app.request("DELETE", &format!("{ACCOUNTS}/{id}"), Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", url.path(), None, None).await.status,
        StatusCode::NOT_FOUND
    );
    server.abort();
}

#[tokio::test]
async fn cloud_api_redirects_stop_at_the_provider_verification_boundary() {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let (untrusted, trap) = serve(Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    }))
    .await;
    let redirect = Redirect::temporary(untrusted.as_str());
    let (fixture, provider) = serve(Router::new().fallback(move || {
        let redirect = redirect.clone();
        async move { redirect }
    }))
    .await;
    let app = TestApp::with_cloud_fixture(fixture);
    let admin = app.setup().await;
    let id = app
        .request("POST", ACCOUNTS, Some(&admin), Some(account()))
        .await
        .json()["id"]
        .as_i64()
        .unwrap();
    let reply = app
        .request(
            "POST",
            SAVE,
            Some(&admin),
            Some(json!({"account_id":id,"url":SHARE})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    no_secrets(&reply.body);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    provider.abort();
    trap.abort();
}
