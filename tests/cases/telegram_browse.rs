use super::*;
use polyview::telegram::native::NativeRuntime;
use rusqlite::params;
use uuid::Uuid;

const CHAT: i64 = -1_000_000_000_123;
const OTHER: i64 = -1_000_000_000_124;
const UNIT: i64 = 1 << 20;

pub(super) fn seed_native(app: &TestApp, user_id: i64, telegram_id: i64) {
    let conn = app.state.conn().unwrap();
    polyview::settings::set_setting_json(
        &conn,
        "telegram_native",
        &json!({
            "api_id":1234,"api_hash":"11111111111111111111111111111111"
        }),
    )
    .unwrap();
    conn.execute("INSERT INTO native_telegram_accounts(user_id,account_key,attempt,phase,telegram_id) VALUES(?,?,?,'authorized',?)",
        params![user_id,Uuid::new_v4().to_string(),Uuid::new_v4().to_string(),telegram_id]).unwrap();
}

fn channel(id: i64, title: &str) -> Value {
    json!({"@type":"chat","id":id,"title":title,"type":{"@type":"chatTypeSupergroup"}})
}
fn video() -> Value {
    json!({"@type":"message", "id":17*UNIT,"chat_id":CHAT,"date":1001,"media_album_id":"9223372036854775806",
        "content":{"@type":"messageVideo","caption":{"text":"电影：https://pan.quark.cn/s/movie，https://pan.quark.cn.evil/s/wrong","entities":[{"type":{"@type":"textEntityTypeTextUrl","url":"https://www.alipan.com/s/hidden"}}]},
        "video":{"file_name":"movie.mp4","duration":92,"minithumbnail":{"data":"/9j/AA=="},"video":{"id":888,"size":8457,"local":{"path":"/private/session/do-not-return"},"remote":{"id":"secret-file-token"}}}}})
}
fn fixture(user: i64, req: Value) -> Vec<Value> {
    let reply = match req["@type"].as_str().unwrap() {
        "getAuthorizationState" => json!({"@type":"authorizationStateReady"}),
        "getMe" => json!({"@type":"user","id":9000+user}),
        "getChats" if req["chat_list"]["@type"] == "chatListArchive" => {
            json!({"@type":"chats","chat_ids":[]})
        }
        "getChats" => json!({"@type":"chats","chat_ids":if user==1 {vec![CHAT,357]} else {vec![]}}),
        "getChat" if req["chat_id"] == CHAT && user == 1 => channel(CHAT, "影视频道"),
        "searchPublicChat" if req["username"] == "extra_films" => channel(OTHER, "资源频道"),
        "getChatHistory" if user == 1 && req["chat_id"] == CHAT => {
            let videos = if req["from_message_id"] == 0 {
                vec![
                    json!({"@type":"message","id":18*UNIT,"chat_id":CHAT,"content":{"@type":"messageText","text":{"text":"ordinary message"}}}),
                    video(),
                    json!({"@type":"message","id":16*UNIT,"chat_id":CHAT,"content":{"@type":"messageText","text":{"text":"https://cloud.189.cn/t/album"}}}),
                    json!({"@type":"message","id":20*UNIT,"chat_id":OTHER,"content":{"@type":"messageVideo"}}),
                ]
            } else {
                vec![]
            };
            json!({"@type":"messages","messages":videos})
        }
        "searchChatMessages" if req["chat_id"] == CHAT && req["query"] == "movie" && user == 1 => {
            json!({"@type":"foundChatMessages","messages":[video()],"next_from_message_id":0})
        }
        "close" => json!({"@type":"ok"}),
        _ => json!({"@type":"error","code":404,"message":"secret upstream error"}),
    };
    vec![reply]
}

#[tokio::test]
async fn browses_the_current_native_account_extracts_resources_and_tracks_personal_favorites() {
    let root = tempfile::tempdir().unwrap();
    let app = TestApp::new_with_native(NativeRuntime::with_transport(
        root.path().to_owned(),
        fixture,
    ));
    let chats = "/api/telegram/chats";
    assert_eq!(
        app.request("GET", chats, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let admin = app.setup().await;
    let uid = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    seed_native(&app, 1, 9001);
    seed_native(&app, uid, 9000 + uid);
    let get = app.request("GET", chats, Some(&admin), None).await;
    assert_eq!(get.status, StatusCode::OK);
    assert_eq!(
        get.json(),
        json!([{"id":CHAT,"title":"影视频道","favorite":false}])
    );
    let endpoint = format!("{chats}/{CHAT}/messages");
    let page = app
        .request("GET", &endpoint, Some(&admin), None)
        .await
        .json();
    let rows = page["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(page["newest_id"], 18);
    assert_eq!(page["next_offset"], 15);
    assert_eq!(rows[0]["file_size"], 8457);
    assert_eq!(rows[0]["duration"], 92);
    assert_eq!(rows[0]["media_group_id"], "9223372036854775806");
    assert_eq!(rows[0]["thumbnail"], "data:image/jpeg;base64,/9j/AA==");
    assert_eq!(rows[0]["url"], "https://t.me/c/123/17");
    assert_eq!(
        rows[0]["cloud_urls"],
        json!([
            "https://pan.quark.cn/s/movie",
            "https://www.alipan.com/s/hidden"
        ])
    );
    assert_eq!(rows[1]["is_video"], false);
    assert!(!page.to_string().contains("secret-file-token"));
    assert!(!page.to_string().contains("/private"));
    assert!(rows[0].get("file_id").is_none());
    assert_eq!(
        app.request("GET", &format!("{endpoint}?q=movie"), Some(&admin), None)
            .await
            .json()["messages"][0]["id"],
        17
    );
    assert_eq!(
        app.request("GET", &format!("{endpoint}?offset=15"), Some(&admin), None)
            .await
            .json()["next_offset"],
        Value::Null
    );
    assert_eq!(
        app.request("GET", &format!("{endpoint}?offset=-1"), Some(&admin), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_ne!(
        app.request("GET", &endpoint, Some(&viewer), None)
            .await
            .status,
        StatusCode::OK
    );
    let add = "/api/telegram/favorites";
    let favorite = json!({"channel":"@extra_films"});
    assert_eq!(
        app.request_with_headers("PUT", add, Some(&admin), Some(favorite.clone()), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request("PUT", add, Some(&admin), Some(favorite))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("GET", chats, Some(&admin), None).await.json()[1],
        json!({"id":OTHER,"title":"资源频道","favorite":true})
    );
    assert_eq!(
        app.request("GET", chats, Some(&viewer), None).await.json(),
        json!([])
    );
    let remove = format!("{add}/{OTHER}");
    app.request("DELETE", &remove, Some(&viewer), None).await;
    assert_eq!(
        app.request("GET", chats, Some(&admin), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    app.request("DELETE", &remove, Some(&admin), None).await;
    assert_eq!(
        app.request("GET", chats, Some(&admin), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn verified_legacy_session_exports_offline_messages_with_bounded_scoped_arguments() {
    use std::os::unix::fs::PermissionsExt;
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let root = fs::canonicalize(app._temp.path()).unwrap();
    let exe = root.join("tdl");
    fs::write(&exe,r#"#!/bin/sh
set -eu
root=$(dirname "$0")
test "$1" = -n && test "$2" = polyview_u1 && test "$3" = --storage && test "$4" = "type=bolt,path=$root/legacy/key/attempt" || exit 1
shift 4
if [ "$*" = 'chat ls -o json' ]; then
  printf '%s' '[{"id":123,"type":"channel","visible_name":"Offline","username":"offline"},{"id":77,"type":"private","visible_name":"Private"}]'
  exit 0
fi
test "$1" = chat && test "$2" = export && test "$3" = -c && test "$4" = 123 && test "$5" = -T && test "$6" = last && test "$7" = -i && test "$8" = 50 || exit 1
shift 8
test "$1" = --all && test "$2" = --with-content && test "$3" = --raw && test "$4" = -o || exit 1
cat > "$5" <<'EXPORT'
{"id":123,"messages":[{"id":24,"file":"Movie.mkv","text":"https://115.com/s/film","date":500,"raw":{"PeerID":{"ChannelID":123},"GroupedID":9876543,"Media":{"Document":{"Size":127,"MimeType":"video/x-matroska","Thumbs":[{"Bytes":"AQgKAA=="}],"Attributes":[{"Duration":58.8}]}}}},{"id":23,"file":"NotOurChat.mkv","raw":{"PeerID":{"UserID":123}}},{"id":22,"file":"photo.jpg","text":"plain text","raw":{"PeerID":{"ChannelID":123}}}]}
EXPORT
"#).unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o700)).unwrap();
    let dir = root.join("legacy");
    {
        let conn = app.state.conn().unwrap();
        polyview::settings::set_setting_json(
            &conn,
            "downloads",
            &json!({"tdl_path":exe,"telegram_directory":dir}),
        )
        .unwrap();
        conn.execute("INSERT INTO telegram_accounts(user_id,account_key,attempt,root,namespace,phase) VALUES(1,'key','attempt',?,'polyview_u1','verified')",[dir.to_str().unwrap()]).unwrap();
    }
    let chats = "/api/telegram/chats";
    assert_eq!(
        app.request("GET", chats, Some(&admin), None).await.json(),
        json!([{"id":CHAT,"title":"Offline","favorite":false}])
    );
    assert_eq!(
        app.request("GET", chats, Some(&viewer), None).await.status,
        StatusCode::CONFLICT
    );
    let page = app
        .request(
            "GET",
            &format!("{chats}/{CHAT}/messages?q=MOVIE"),
            Some(&admin),
            None,
        )
        .await
        .json();
    let rows = page["messages"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["file_size"], 127);
    assert_eq!(rows[0]["duration"], 58);
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let preview = rows[0]["thumbnail"]
        .as_str()
        .unwrap()
        .strip_prefix("data:image/jpeg;base64,")
        .unwrap();
    let jpeg = STANDARD.decode(preview).unwrap();
    assert_eq!((jpeg[164], jpeg[166]), (8, 10));
    assert!(jpeg.ends_with(&[0xff, 0xd9]));
    assert_eq!(rows[0]["media_group_id"], "9876543");
    assert_eq!(rows[0]["cloud_urls"], json!(["https://115.com/s/film"]));
    assert_eq!(page["next_offset"], Value::Null);
}
