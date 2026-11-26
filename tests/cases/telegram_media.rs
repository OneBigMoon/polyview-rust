#![cfg(unix)]

use super::*;

#[tokio::test]
async fn telegram_link_is_validated_and_imported_with_the_owners_verified_tdl_session() {
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let endpoint = "/api/downloads/telegram";
    let body = json!({"url":"https://telegram.me/c/123456789/42"});
    assert_eq!(
        app.request("POST", endpoint, None, Some(body.clone()))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    for invalid in [
        "https://telegram.me.evil/channel/42",
        "https://t.me/channel/42?token=secret",
        "https://t.me/+invite",
        "https://t.me/c/abc/42",
    ] {
        let reply = app
            .request("POST", endpoint, Some(&admin), Some(json!({"url":invalid})))
            .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!String::from_utf8_lossy(&reply.body).contains(invalid));
    }
    let base = fs::canonicalize(app._temp.path()).unwrap();
    let stub = base.join("tdl");
    fs::write(&stub, r#"#!/bin/sh
set -eu
base=$(dirname "$0")
action= namespace= storage= proxy= dest= url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -n|--ns|--namespace) namespace=$2; shift;;
    --storage) storage=$2; shift;;
    --proxy) proxy=$2; shift;;
    -u) url=$2; shift;;
    -d) dest=$2; shift;;
    login|chat|dl) action=$1;;
  esac
  shift
done
case "$action" in
  chat) printf '%s' "$storage" > "$base/verified";;
  dl)
    test "$namespace" = polyview_u1
    test "$proxy" = 'socks5h://user:stub-secret@proxy.example:1080'
    test "$storage" = "$(cat "$base/verified")"
    case "$storage" in "type=bolt,path=$base/session/"*) ;; *) exit 12;; esac
    case "$(ls -ld "$dest")" in drwx------*) ;; *) exit 13;; esac
    printf '%s\n' "$dest" >> "$base/attempts"
    echo 'stub-secret child stdout'
    echo 'stub-secret child stderr' >&2
    if [ "$url" = 'https://t.me/channel/44' ]; then printf '%s' "$dest" > "$base/active"; exec sleep 60; fi
    if [ "$url" != 'https://telegram.me/c/123456789/42' ]; then exit 19; fi
    if [ ! -e "$base/retried" ]; then touch "$base/retried"; printf 'partial' > "$dest/unpublished.mkv"; exit 18; fi
    printf 'offline-video' > "$dest/upstream.mkv";;
esac
"#).unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();
    let settings = json!({"tdl_path":stub, "telegram_directory":base.join("session"), "telegram_proxy":"socks5h://user:stub-secret@proxy.example:1080", "max_attempts":2});
    assert_eq!(
        app.request(
            "PUT",
            "/api/settings/downloads",
            Some(&admin),
            Some(settings)
        )
        .await
        .status,
        StatusCode::OK
    );
    app.request("POST", "/api/telegram/login", Some(&admin), None)
        .await;
    let pending = app
        .request("GET", "/api/telegram/login/requests", Some(&admin), None)
        .await
        .json();
    let command = pending[0]["command"]
        .as_str()
        .expect("pending host authorization command");
    let storage = command
        .split_once(" --storage 'type=bolt,path=")
        .unwrap()
        .1
        .split_once('\'')
        .unwrap()
        .0;
    let storage = std::path::Path::new(storage);
    assert!(storage.starts_with(base.join("session")));
    fs::create_dir_all(storage).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let result = app
                .request("GET", "/api/telegram/login", Some(&admin), None)
                .await;
            assert_eq!(result.status, StatusCode::OK);
            if result.json()["state"] == "authorized" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("stub session authorized");
    assert!(
        app.request("POST", endpoint, Some(&viewer), Some(body.clone()))
            .await
            .status
            .is_client_error()
    );
    let job = app
        .request("POST", endpoint, Some(&admin), Some(body))
        .await;
    assert_eq!(job.status, StatusCode::CREATED);
    assert_eq!(job.json()["kind"], "telegram");
    assert_eq!(job.json()["title"], "Telegram 42");
    let failed = app
        .request(
            "POST",
            endpoint,
            Some(&admin),
            Some(json!({"url":"https://t.me/channel/43", "title":"Failure"})),
        )
        .await;
    assert_eq!(failed.status, StatusCode::CREATED);
    let worker = polyview::network::start_worker(app.state.clone());
    let (completed, failed) = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let jobs = app
                .request("GET", "/api/downloads", Some(&admin), None)
                .await
                .json();
            if jobs[1]["status"] == "completed" && jobs[0]["status"] == "failed" {
                let library = app
                    .request("GET", "/api/library", Some(&admin), None)
                    .await
                    .json();
                if library
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["relative_path"] == jobs[1]["relative_path"])
                {
                    break (jobs[1].clone(), jobs[0].clone());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("queue completed the stub import and indexed the video");
    assert_eq!(completed["attempts"], 2);
    assert_eq!(failed["attempts"], 2);
    assert!(!failed.to_string().contains("stub-secret"));
    let path = app
        .state
        .media_dir
        .join(completed["relative_path"].as_str().unwrap());
    assert_eq!(path.extension().unwrap(), "mkv");
    assert_eq!(fs::read(path).unwrap(), b"offline-video");
    let interrupted = app
        .request(
            "POST",
            endpoint,
            Some(&admin),
            Some(json!({"url":"https://t.me/channel/44"})),
        )
        .await;
    assert_eq!(interrupted.status, StatusCode::CREATED);
    let active = base.join("active");
    let stage = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(dir) = fs::read_to_string(&active)
                && !dir.is_empty()
            {
                break std::path::PathBuf::from(dir);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("tdl running");
    let cancel = format!("/api/downloads/{}", interrupted.json()["id"]);
    assert_eq!(
        app.request("DELETE", &cancel, Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while stage.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("cancelled attempt staging removed");
    worker.abort();
    let attempts = fs::read_to_string(app._temp.path().join("attempts")).unwrap();
    let dirs: std::collections::HashSet<_> = attempts.lines().collect();
    assert_eq!(dirs.len(), 5);
    assert!(dirs.iter().all(|dir| !std::path::Path::new(dir).exists()));
}
