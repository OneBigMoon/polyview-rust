use super::*;
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Uri, header},
    response::{IntoResponse, Response},
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::{net::TcpListener, task::JoinHandle};

const SHARE_URL: &str = "https://pan.quark.cn/s/ab123?pwd=A1b2";
const COOKIE: &str = "fixture=private-cookie";

struct Call {
    method: Method,
    path: String,
    query: HashMap<String, String>,
    cookie: bool,
    body: Value,
}

#[derive(Default)]
struct Script {
    calls: Vec<Call>,
    responses: VecDeque<Response>,
}

struct Fixture {
    account: DriveAccount,
    script: Arc<Mutex<Script>>,
    worker: JoinHandle<()>,
}

impl Fixture {
    async fn new(responses: Vec<Response>) -> Self {
        let script = Arc::new(Mutex::new(Script {
            responses: responses.into(),
            ..Script::default()
        }));
        let router = Router::new().fallback(respond).with_state(script.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let worker = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            account: DriveAccount::fixture("quark", origin, COOKIE),
            script,
            worker,
        }
    }

    fn calls(&self) -> Vec<Call> {
        let mut script = self.script.lock().unwrap();
        assert!(
            script.responses.is_empty(),
            "provider workflow stopped early"
        );
        for call in &script.calls {
            assert!(call.cookie);
            assert_eq!(call.query["pr"], "ucpro");
            assert_eq!(call.query["fr"], "pc");
        }
        std::mem::take(&mut script.calls)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn respond(
    State(script): State<Arc<Mutex<Script>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
) -> Response {
    let mut script = script.lock().unwrap();
    script.calls.push(Call {
        method,
        path: uri.path().into(),
        query: url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .into_owned()
            .collect(),
        cookie: headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) == Some(COOKIE),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    });
    script
        .responses
        .pop_front()
        .unwrap_or_else(|| StatusCode::IM_A_TEAPOT.into_response())
}

fn ok(data: Value) -> Response {
    Json(json!({"status":200,"code":0,"data":data})).into_response()
}
fn token() -> Response {
    ok(json!({"stoken":"private+/=stoken"}))
}
fn entry(id: &str, token: &str) -> Value {
    json!({"fid":id,"share_fid_token":token,"file_name":"sample.mp4","dir":false,"category":1})
}
fn page(items: Vec<Value>, total: usize) -> Response {
    Json(json!({"status":200,"code":0,"metadata":{"_total":total},"data":{"share":{"title":"Fixture"},"list":items}})).into_response()
}
fn submitted() -> Response {
    ok(json!({"task_id":"t123"}))
}
fn progress(state: i32) -> Response {
    ok(json!({"status":state,"save_as":{"save_as_top_fids":["newfid"]}}))
}

#[tokio::test]
async fn lists_all_pages_before_filtering_folders() {
    let fixture = Fixture::new(vec![
        page(
            vec![json!({"fid":"video", "file_name":"movie.mp4", "dir":false})],
            3,
        ),
        page(
            vec![
                json!({"fid":"one", "file_name":"Movies", "file_type":0}),
                json!({"fid":"two", "file_name":"Series", "dir":true}),
            ],
            3,
        ),
    ])
    .await;
    let listed = folders(&fixture.account, "parent").await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|f| (f.id.as_str(), f.name.as_str()))
            .collect::<Vec<_>>(),
        [("one", "Movies"), ("two", "Series")]
    );
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    for (i, call) in calls.iter().enumerate() {
        assert_eq!(call.method, Method::GET);
        assert_eq!(call.path, FILE_SORT);
        assert_eq!(call.query["_page"], (i + 1).to_string());
        assert_eq!(call.query["pdir_fid"], "parent");
    }
}

#[tokio::test]
async fn share_check_resolves_token_and_list_without_saving() {
    let fixture = Fixture::new(vec![token(), page(vec![entry("f1", "t1")], 1)]).await;
    let result = check(&fixture.account, SHARE_URL, Some("aBcD"))
        .await
        .unwrap();
    assert!(result.valid);
    assert_eq!(result.title.as_deref(), Some("Fixture"));
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].body, json!({"pwd_id":"ab123","passcode":"aBcD"}));
    assert_eq!(calls[1].query["stoken"], "private+/=stoken");
    assert_eq!(calls[1].query["pdir_fid"], "0");
}

#[tokio::test]
async fn pairs_file_tokens_across_pages_and_polls_to_completion_once() {
    let fixture = Fixture::new(vec![
        token(),
        page(vec![entry("f1", "token-A")], 2),
        page(vec![entry("f2", "token-B")], 2),
        submitted(),
        progress(1),
        progress(2),
    ])
    .await;
    let result = save(&fixture.account, SHARE_URL, None, "chosen")
        .await
        .unwrap();
    assert_eq!(result.path, "quark://folder/chosen");
    assert_eq!(result.task_id.as_deref(), Some("t123"));
    assert!(result.media_id.is_none());
    let calls = fixture.calls();
    assert_eq!(
        calls.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        [TOKEN, DETAIL, DETAIL, SAVE, TASK, TASK]
    );
    assert_eq!(calls[0].method, Method::POST);
    assert_eq!(calls[0].body, json!({"pwd_id":"ab123", "passcode":"A1b2"}));
    assert_eq!(calls[2].query["_page"], "2");
    assert_eq!(calls[3].method, Method::POST);
    assert_eq!(
        calls[3].body,
        json!({"pwd_id":"ab123","stoken":"private+/=stoken","scene":"link","pdir_fid":"0", "to_pdir_fid":"chosen","fid_list":["f1","f2"],"fid_token_list":["token-A","token-B"]})
    );
    assert_eq!(calls[4].query["task_id"], "t123");
    assert_eq!(calls[4].query["retry_index"], "0");
    assert_eq!(calls[5].query["retry_index"], "1");
}

#[tokio::test]
async fn returns_only_new_account_fid_for_single_video() {
    let fixture = Fixture::new(vec![
        token(),
        page(vec![entry("source", "sensitive")], 1),
        submitted(),
        progress(2),
    ])
    .await;
    let result = save(&fixture.account, SHARE_URL, None, "0").await.unwrap();
    assert_eq!(result.path, "/");
    assert_eq!(result.media_id.as_deref(), Some("newfid"));
    let public = serde_json::to_string(&result).unwrap();
    for private in [COOKIE, "private+/=stoken", "sensitive", "source", "newfid"] {
        assert!(!public.contains(private));
    }
    assert_eq!(fixture.calls().len(), 4);
}

#[tokio::test]
async fn rejects_bad_passcode_expired_link_and_auth_challenge_distinctly() {
    for (code, status) in [
        (41008, StatusCode::BAD_REQUEST),
        (41012, StatusCode::NOT_FOUND),
        (20002, StatusCode::FORBIDDEN),
    ] {
        let fixture = Fixture::new(vec![
            Json(json!({"code":code,"status":400,"message":COOKIE})).into_response(),
        ])
        .await;
        let result = check(&fixture.account, SHARE_URL, None).await;
        if status == StatusCode::NOT_FOUND {
            assert!(!result.unwrap().valid);
        } else {
            let error = result.err().unwrap();
            assert_eq!(error.0, status);
            assert!(!error.1.contains(COOKIE));
        }
        assert_eq!(fixture.calls().len(), 1);
    }
    let fixture = Fixture::new(vec![
        (StatusCode::FOUND, [(header::LOCATION, "/challenge")]).into_response(),
    ])
    .await;
    let error = save(&fixture.account, SHARE_URL, None, "0")
        .await
        .err()
        .unwrap();
    assert_eq!(error.0, StatusCode::FORBIDDEN);
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test]
async fn incomplete_listing_and_failed_or_unknown_task_never_report_success() {
    let fixture = Fixture::new(vec![token(), page(vec![entry("source", "")], 1)]).await;
    assert!(save(&fixture.account, SHARE_URL, None, "0").await.is_err());
    assert_eq!(fixture.calls().len(), 2);
    let fixture = Fixture::new(vec![
        token(),
        page(vec![entry("source", "s")], 2),
        page(vec![], 2),
    ])
    .await;
    assert!(save(&fixture.account, SHARE_URL, None, "0").await.is_err());
    assert_eq!(fixture.calls().len(), 3);
    for state in [3, 99] {
        let fixture = Fixture::new(vec![
            token(),
            page(vec![entry("source", "secret")], 1),
            submitted(),
            progress(state),
        ])
        .await;
        let err = save(&fixture.account, SHARE_URL, None, "0")
            .await
            .err()
            .unwrap();
        assert!(!err.1.contains("secret"));
        assert!(err.1.contains(if state == 3 { "失败" } else { "尚未确认" }));
        assert_eq!(fixture.calls().len(), 4);
    }
}

#[tokio::test]
async fn rejects_untrusted_shares_and_oversized_responses() {
    let fixture = Fixture::new(Vec::new()).await;
    for url in [
        "https://pan.quark.cn.evil.example/s/id",
        "https://pan.quark.cn@other.example/s/id",
        "https://u@pan.quark.cn/s/id",
        "http://pan.quark.cn/s/id",
        "https://pan.quark.cn/s/id/other",
    ] {
        assert!(check(&fixture.account, url, None).await.is_err());
    }
    assert!(
        save(&fixture.account, SHARE_URL, None, "../dir")
            .await
            .is_err()
    );
    assert!(fixture.calls().is_empty());
    let fixture = Fixture::new(vec!["x".repeat(BODY_LIMIT + 1).into_response()]).await;
    assert!(folders(&fixture.account, "0").await.is_err());
    assert_eq!(fixture.calls().len(), 1);
}

#[tokio::test]
async fn media_resolver_selects_only_provider_https_progressive_video() {
    let mut videos = Vec::new();
    for url in [
        "https://quark.cn.evil.example/leak.mp4",
        "http://video.quark.cn/file.mp4",
        "https://user@video.quark.cn/file.mp4",
        "https://video.quark.cn/play.m3u8",
    ] {
        videos.push(json!({"video_info":{"url":url,"format":"mp4"}}));
    }
    let invalid = ok(json!({"video_list":videos}));
    videos.push(json!({"video_info":{"url":"https://video.quark.cn/play/movie.mp4?sign=test","format":"mp4"}}));
    let fixture = Fixture::new(vec![invalid, ok(json!({"video_list":videos}))]).await;
    assert!(media_url(&fixture.account, "newfid").await.is_err());
    let media = media_url(&fixture.account, "newfid").await.unwrap();
    assert_eq!(media.host_str(), Some("video.quark.cn"));
    assert_eq!(media.query(), Some("sign=test"));
    let calls = fixture.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].path, "/1/clouddrive/file/v2/play/project");
    assert_eq!(calls[1].body["fid"], "newfid");
}
