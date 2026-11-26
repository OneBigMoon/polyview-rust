use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use polyview::telegram::native::NativeRuntime;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::sync::Notify;

const CHAT: i64 = -1_000_000_000_123;
const POST: i64 = 23;
const SIZE: usize = 300_010;

fn video_data(owner: i64, start: usize, len: usize) -> Vec<u8> {
    (start..start + len)
        .map(|i| ((i + owner as usize) % 251) as u8)
        .collect()
}

#[derive(Default)]
struct Transfers {
    reads: Mutex<Vec<(i64, usize, usize)>>,
    stall: AtomicBool,
    pending: Notify,
    cancelled: Notify,
}

impl Transfers {
    fn handle(&self, owner: i64, request: Value) -> Vec<Value> {
        let video = json!({
            "@type":"message", "chat_id":CHAT, "id":POST*1048576, "date":1700000000, "media_album_id":"0",
            "content":{"@type":"messageVideo", "caption":{"text":"Movie"}, "video":{
                "file_name":format!("member-{owner}.mp4"), "mime_type":"video/mp4", "duration":30,
                "video":{"@type":"file", "id":57, "size":SIZE, "local":{"path":"/private/member-session/video"}, "remote":{"id":"upstream-secret"}}
            }}
        });
        let kind = request["@type"].as_str().unwrap();
        vec![match kind {
            "getAuthorizationState" => json!({"@type":"authorizationStateReady"}),
            "getMe" => json!({"@type":"user", "id":owner+100}),
            "getChats" => json!({"@type":"chats", "chat_ids":[CHAT]}),
            "getChat" => {
                json!({"@type":"chat", "id":CHAT, "title":format!("Library {owner}"), "type":{"@type":"chatTypeSupergroup", "supergroup_id":123, "is_channel":true}})
            }
            "getChatHistory" => json!({"@type":"messages", "total_count":1, "messages":[video]}),
            "getMessage"
                if request["chat_id"] == CHAT && request["message_id"] == POST * 1048576 =>
            {
                video
            }
            "downloadFile" => {
                assert_eq!(request["file_id"], 57);
                assert_eq!(request["synchronous"], true);
                let len = request["limit"].as_u64().unwrap();
                assert!((1..=256 * 1024).contains(&len));
                self.reads.lock().unwrap().push((
                    owner,
                    request["offset"].as_u64().unwrap() as usize,
                    len as usize,
                ));
                if self.stall.swap(false, Ordering::SeqCst) {
                    self.pending.notify_one();
                    return vec![];
                }
                json!({"@type":"file", "id":57, "size":SIZE, "local":{"path":"/private/member-session/video"}})
            }
            "readFilePart" => {
                assert_eq!(request["file_id"], 57);
                let start = request["offset"].as_u64().unwrap() as usize;
                let count = request["count"].as_u64().unwrap() as usize;
                assert!(count > 0 && start + count <= SIZE);
                json!({"@type":"data", "data":STANDARD.encode(video_data(owner, start, count))})
            }
            "cancelDownloadFile" => {
                assert_eq!(request["file_id"], 57);
                self.cancelled.notify_one();
                json!({"@type":"ok"})
            }
            "close" => {
                return vec![
                    json!({"@type":"ok"}),
                    json!({"@type":"updateAuthorizationState", "authorization_state":{"@type":"authorizationStateClosed"}}),
                ];
            }
            _ => json!({"@type":"error", "code":404, "message":"upstream-secret"}),
        }]
    }
}

fn fixture() -> (TestApp, TempDir, Arc<Transfers>) {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(Transfers::default());
    let mock = calls.clone();
    let runtime = NativeRuntime::with_transport(root.path().to_owned(), move |user, request| {
        mock.handle(user, request)
    });
    (TestApp::new_with_native(runtime), root, calls)
}

#[tokio::test]
async fn members_seek_their_own_video_in_bounded_tdlib_parts() {
    let (app, _root, calls) = fixture();
    let admin = app.setup().await;
    let id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    super::telegram_browse::seed_native(&app, 1, 101);
    super::telegram_browse::seed_native(&app, id, id + 100);
    let path = format!("/api/telegram/stream/{CHAT}/{POST}");
    assert_eq!(
        app.request("GET", &path, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    for (owner, cookie, range, start, end) in [
        (1, &admin, "bytes=9-12", 9, 13),
        (id, &viewer, "bytes=9-12", 9, 13),
        (id, &viewer, "bytes=-8", SIZE - 8, SIZE),
        (1, &admin, "bytes=300000-", 300000, SIZE),
        (1, &admin, "bytes=300000-500000", 300000, SIZE),
    ] {
        let response = app
            .request_with_headers("GET", &path, Some(cookie), None, &[("range", range)])
            .await;
        assert_eq!(response.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers[header::CONTENT_RANGE].to_str().unwrap(),
            format!("bytes {start}-{}/{SIZE}", end - 1)
        );
        assert_eq!(
            response.headers[header::CONTENT_LENGTH].to_str().unwrap(),
            (end - start).to_string()
        );
        assert_eq!(response.body, video_data(owner, start, end - start));
        assert_eq!(
            calls.reads.lock().unwrap().last(),
            Some(&(owner, start, end - start))
        );
    }
    let before = calls.reads.lock().unwrap().len();
    for invalid in [
        "bytes=300010-",
        "bytes=-0",
        "bytes=8-2",
        "bytes=9-10,23-24",
        "bytes=+1-3",
    ] {
        let response = app
            .request_with_headers("GET", &path, Some(&viewer), None, &[("range", invalid)])
            .await;
        assert_eq!(response.status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers[header::CONTENT_RANGE], "bytes */300010");
        assert!(response.body.is_empty());
    }
    let head = app
        .request_with_headers(
            "HEAD",
            &path,
            Some(&viewer),
            None,
            &[("range", "bytes=-21")],
        )
        .await;
    assert_eq!(head.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.headers[header::CONTENT_LENGTH], "21");
    assert!(head.body.is_empty());
    assert_eq!(calls.reads.lock().unwrap().len(), before);
    let full = app.request("GET", &path, Some(&admin), None).await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.body, video_data(1, 0, SIZE));
    assert!(!full.headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(
        &calls.reads.lock().unwrap()[before..],
        &[(1, 0, 262144), (1, 262144, SIZE - 262144)]
    );
}

#[tokio::test]
async fn disconnect_cancels_pending_download_and_releases_the_next_seek() {
    let (app, _root, calls) = fixture();
    let admin = app.setup().await;
    super::telegram_browse::seed_native(&app, 1, 101);
    let path = format!("/api/telegram/stream/{CHAT}/{POST}");
    calls.stall.store(true, Ordering::SeqCst);
    let response = app
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&path)
                .header(header::COOKIE, &admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::spawn(to_bytes(response.into_body(), SIZE));
    tokio::time::timeout(Duration::from_secs(3), calls.pending.notified())
        .await
        .unwrap();
    body.abort();
    assert!(body.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), calls.cancelled.notified())
        .await
        .unwrap();
    let next = app
        .request_with_headers(
            "GET",
            &path,
            Some(&admin),
            None,
            &[("range", "bytes=100-102")],
        )
        .await;
    assert_eq!(next.body, video_data(1, 100, 3));
}

#[tokio::test]
async fn telegram_tvbox_browse_and_play_use_the_feed_owner_and_enforce_rotation() {
    let (app, _root, _) = fixture();
    let admin = app.setup().await;
    let id = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    super::telegram_browse::seed_native(&app, id, id + 100);
    let feed = app
        .request("GET", "/api/tvbox", Some(&viewer), None)
        .await
        .json();
    let token = feed["token"].as_str().unwrap();
    let host = [("host", "nas.local")];
    let config = app
        .request_with_headers(
            "GET",
            &format!("/tvbox/{token}/config.json"),
            None,
            None,
            &host,
        )
        .await
        .json();
    let catalog = config["sites"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "polyview_telegram")
        .unwrap()["api"]
        .as_str()
        .unwrap()
        .strip_prefix("http://nas.local")
        .unwrap();
    let classes = app
        .request_with_headers("GET", catalog, None, None, &host)
        .await
        .json();
    assert_eq!(classes["class"][0]["type_name"], format!("Library {id}"));
    let list = app
        .request_with_headers(
            "GET",
            &format!("{catalog}?ac=detail&t={CHAT}"),
            None,
            None,
            &host,
        )
        .await
        .json();
    assert_eq!(list["list"][0]["vod_name"], format!("member-{id}.mp4"));
    let detail = format!(
        "{catalog}?ac=detail&ids={}",
        list["list"][0]["vod_id"].as_str().unwrap()
    );
    let detail_reply = app
        .request_with_headers("GET", &detail, None, None, &host)
        .await;
    let text = std::str::from_utf8(&detail_reply.body).unwrap();
    assert!(!text.contains("upstream-secret") && !text.contains("member-session"));
    let detail_json = detail_reply.json();
    let stream = detail_json["list"][0]["vod_play_url"]
        .as_str()
        .unwrap()
        .strip_prefix("播放$http://nas.local")
        .unwrap();
    let seek = app
        .request_with_headers(
            "GET",
            stream,
            Some(&admin),
            None,
            &[("range", "bytes=15-17")],
        )
        .await;
    assert_eq!(seek.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(seek.body, video_data(id, 15, 3));
    let new = app
        .request("POST", "/api/tvbox/token", Some(&viewer), None)
        .await
        .json();
    for path in [catalog, detail.as_str(), stream] {
        assert_eq!(
            app.request_with_headers("GET", path, Some(&viewer), None, &host)
                .await
                .status,
            StatusCode::NOT_FOUND
        );
    }
    let rotated = stream.replace(token, new["token"].as_str().unwrap());
    let seek = app
        .request_with_headers("GET", &rotated, None, None, &[("range", "bytes=10-12")])
        .await;
    assert_eq!(seek.body, video_data(id, 10, 3));
}

#[cfg(unix)]
#[tokio::test]
async fn verified_legacy_group_plays_from_a_private_partial_file_and_releases_it() {
    use std::os::unix::fs::PermissionsExt;
    let app = TestApp::new();
    let cookie = app.setup().await;
    let base = app._temp.path().canonicalize().unwrap();
    let tdl = base.join("tdl");
    fs::write(&tdl, r#"#!/bin/sh
set -eu
base=$(dirname "$0")
action= dest= output= manifest= namespace= storage=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -n) namespace=$2; shift;;
    --storage) storage=$2; shift;;
    --serve) exit 20;;
    --template) test "$2" = video; shift;;
    -t) test "$2" = 1; shift;;
    -f) manifest=$2; shift;;
    -o) output=$2; shift;;
    -d) dest=$2; shift;;
    export|dl) action=$1;;
  esac
  shift
done
test "$namespace" = polyview_u1
test "$storage" = "type=bolt,path=$base/session/member/attempt"
case "$action" in
  export) printf '%s' '{"id":123,"messages":[{"id":23,"file":"clip.mp4","raw":{"PeerID":{"ChatID":123},"Media":{"Document":{"Size":128,"MimeType":"video/mp4"}}}}]}' > "$output";;
  dl)
    test -s "$manifest"
    case "$(ls -ld "$dest")" in drwx------*) ;; *) exit 22;; esac
    printf '%s' "$dest" > "$base/active"
    printf 'first-bytes' > "$dest/video.tmp"
    exec sleep 30;;
  *) exit 21;;
esac
"#).unwrap();
    fs::set_permissions(&tdl, fs::Permissions::from_mode(0o700)).unwrap();
    let root = base.join("session");
    let settings = json!({"tdl_path":tdl,"telegram_directory":root});
    assert_eq!(
        app.request(
            "PUT",
            "/api/settings/downloads",
            Some(&cookie),
            Some(settings)
        )
        .await
        .status,
        StatusCode::OK
    );
    app.state.conn().unwrap().execute("INSERT INTO telegram_accounts(user_id,account_key,attempt,root,namespace,phase) VALUES(1,'member','attempt',?,'polyview_u1','verified')",[root.to_str().unwrap()]).unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        app.request_with_headers(
            "GET",
            "/api/telegram/stream/-123/23",
            Some(&cookie),
            None,
            &[("range", "bytes=6-10")],
        ),
    )
    .await
    .unwrap();
    assert_eq!(reply.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(reply.body, b"bytes");
    let staging = fs::read_to_string(base.join("active")).unwrap();
    assert!(!std::path::Path::new(&staging).exists());
}
