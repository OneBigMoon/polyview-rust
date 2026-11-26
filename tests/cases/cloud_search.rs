use super::*;
use axum::{Json, extract::Query, routing::get};
use polyview::telegram::native::NativeRuntime;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

const CHANNELS: &str = "/api/cloud/channels";
const IMPORT: &str = "/api/cloud/channels/import";
const EXPORT: &str = "/api/cloud/channels/export";
const SEARCH: &str = "/api/cloud/search?q=movie%20%26%20TV";

#[tokio::test]
async fn channels_are_admin_only_batch_import_is_atomic_and_backups_restore_ids() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let input = json!({"name":"本地搜索","kind":"api","target":"https://search.invalid/resources?key=admin-secret"});
    for (method, route, body) in [
        ("GET", CHANNELS, None),
        ("GET", EXPORT, None),
        ("POST", CHANNELS, Some(input.clone())),
        ("PUT", "/api/cloud/channels/1", Some(input.clone())),
        ("DELETE", "/api/cloud/channels/1", None),
        ("POST", IMPORT, Some(json!([input.clone()]))),
    ] {
        assert_eq!(
            app.request(method, route, None, body.clone()).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.request(method, route, Some(&viewer), body).await.status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request_with_headers("POST", CHANNELS, Some(&admin), Some(input.clone()), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let row = app
        .request("POST", CHANNELS, Some(&admin), Some(input))
        .await;
    assert_eq!(row.status, StatusCode::CREATED);
    let id = row.json()["id"].as_i64().unwrap();
    let item = format!("{CHANNELS}/{id}");
    let change = json!({"name":"关闭的 API","kind":"api","target":"https://search.invalid/v2?token=admin-secret","enabled":false});
    assert_eq!(
        app.request("PUT", &item, Some(&admin), Some(change))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    let batch = json!([{ "id":"@Movie_Telegram", "name":"电影" },{ "name":"订阅", "channelId":"more_films" }]);
    for _ in 0..2 {
        assert_eq!(
            app.request("POST", IMPORT, Some(&admin), Some(batch.clone()))
                .await
                .json(),
            json!({"imported":2})
        );
    }
    let channels = app.request("GET", EXPORT, Some(&admin), None).await.json();
    assert_eq!(channels["channels"].as_array().unwrap().len(), 3);
    assert_eq!(channels["channels"][0]["enabled"], false);
    assert_eq!(channels["channels"][1]["target"], "movie_telegram");
    let bad = json!({"channels":[{"target":"another_channel","name":"合法"},{"target":"invalid/channel?q=bad","name":"无效"}]});
    assert_eq!(
        app.request("POST", IMPORT, Some(&admin), Some(bad))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.request("GET", EXPORT, Some(&admin), None).await.json(),
        channels
    );

    let backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(backup["cloud"]["channels"], channels);
    assert_eq!(
        app.request("DELETE", &item, Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("DELETE", &item, Some(&admin), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let mut invalid = backup.clone();
    invalid["cloud"]["channels"]["channels"][1]["target"] = json!("invalid/public");
    assert_eq!(
        app.request("POST", "/api/settings/import", Some(&admin), Some(invalid))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.request("GET", CHANNELS, Some(&admin), None)
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        app.request("POST", "/api/settings/import", Some(&admin), Some(backup))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", EXPORT, Some(&admin), None).await.json(),
        channels
    );
}

#[tokio::test]
async fn public_and_api_search_extract_canonical_shares_and_keep_source_secrets_private() {
    let upstream = Router::new()
        .route("/s/films", get(|Query(q): Query<HashMap<String,String>>| async move {
            assert_eq!(q.get("q").map(String::as_str), Some("movie & TV"));
            axum::response::Html(r##"<html><a href="https://115.com/s/advert">outside the post</a>
            <div class="tgme_widget_message_text js-message_text"><b>电影 &amp; TV</b><br/>
            <a href='http://pan.quark.cn/s/q1?utm_campaign=junk&amp;pwd=Q123'>夸克</a><br>
            https://115cdn.com/s/one 提取码：A555<br/>
            https://cloud.189.cn/web/share?code=tys&amp;accessCode=TY99<br/>
            <a href="https://www.aliyundrive.com/s/cloud">云盘</a><br/>
            https://pan.baidu.com/share/init?surl=baid&amp;pwd=B234<br/>
            https://drive.uc.cn/s/uc1<br/>
            https://pan.xunlei.com/s/xun#X666<br/>
            https://evil.invalid/?redirect=https://115.com/s/embedded
            https://pan.quark.cn.evil.invalid/s/fake
            https://pan.quark.cn@evil.invalid/s/fake
            https://user:secret@115.com/s/fake
            https://115.com:9001/s/wrong
            https://pan.quark.cn/s/other?fid=only_one_file
            https://115.com/s/root/unknown-subfolder
            https://115.com/login
            </div></html>"##)
        }))
        .route("/resources", get(|Query(q): Query<HashMap<String,String>>| async move {
            assert_eq!(q.get("key").map(String::as_str), Some("source-secret"));
            assert_eq!(q.get("q").map(String::as_str), Some("movie & TV"));
            Json(json!({"results":[
                {"provider":"bogus","url":"https://www.alipan.com/s/cloud?tracking=remove","code":"A111","title":"Duplicate"},
                {"url":"https://h5.cloud.189.cn/share.html#/t/tys","title":"Mobile duplicate"},
                {"title":"第二部","url":"https://anxia.com/s/two","code":"T444","private":"api-response-secret"},
                {"title":"Bogus","url":"https://115.com.evil.invalid/s/x"}
            ]}))
        }))
        .route("/redirect", get(|| async { axum::response::Redirect::temporary("/unexpected") }))
        .route("/unexpected", get(|| async { Json(json!([{"url":"https://115.com/s/redirected"}])) }))
        .route("/failure", get(|| async { (StatusCode::BAD_GATEWAY, "private-upstream-error") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let app = TestApp::with_cloud_fixture(base.parse().unwrap());
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let batch = json!({"channels":[
        {"name":"电影频道","target":"@films"},
        {"name":"影片搜索","kind":"api","target":format!("{base}/resources?key=source-secret")},
        {"name":"跳转的源","kind":"api","target":format!("{base}/redirect?key=source-secret")},
        {"name":"失败的源","kind":"api","target":format!("{base}/failure")}
    ]});
    assert_eq!(
        app.request("POST", IMPORT, Some(&admin), Some(batch))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", SEARCH, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request("GET", "/api/cloud/search?q=%20", Some(&viewer), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let reply = app.request("GET", SEARCH, Some(&viewer), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let data = reply.json();
    let rows = data.as_array().unwrap();
    assert_eq!(rows.len(), 8, "{data}");
    for (row, (provider, url, code)) in rows.iter().zip([
        ("quark", "https://pan.quark.cn/s/q1", Some("Q123")),
        ("115", "https://115.com/s/one", Some("A555")),
        ("tianyi", "https://cloud.189.cn/t/tys", Some("TY99")),
        ("ali", "https://www.alipan.com/s/cloud", Some("A111")),
        ("baidu", "https://pan.baidu.com/s/1baid", Some("B234")),
        ("uc", "https://drive.uc.cn/s/uc1", None),
        ("xunlei", "https://pan.xunlei.com/s/xun", Some("X666")),
        ("115", "https://115.com/s/two", Some("T444")),
    ]) {
        assert_eq!(row["provider"], provider);
        assert_eq!(row["url"], url);
        assert_eq!(row["code"].as_str(), code);
        assert_eq!(row.as_object().unwrap().len(), 5);
    }
    assert_eq!(rows[0]["title"], "电影 & TV");
    assert_eq!(rows[0]["channel"], "电影频道");
    assert_eq!(rows[7]["title"], "第二部");
    let serialized = data.to_string();
    for secret in [
        "source-secret",
        "api-response-secret",
        "private-upstream-error",
        "127.0.0.1",
        "tracking",
        "advert",
    ] {
        assert!(!serialized.contains(secret));
    }
    server.abort();
}

#[tokio::test]
async fn private_search_uses_only_the_requesting_members_telegram_account() {
    const CHAT: i64 = -1_000_000_000_123;
    let root = tempfile::tempdir().unwrap();
    let app = TestApp::new_with_native(NativeRuntime::with_transport(
        root.path().to_owned(),
        |uid, request| {
            let response = match request["@type"].as_str().unwrap() {
                "getAuthorizationState" => json!({"@type":"authorizationStateReady"}),
                "getMe" => json!({"@type":"user","id":9000+uid}),
                "searchChatMessages"
                    if request["query"] == "movie & TV" && request["chat_id"] == CHAT =>
                {
                    let text = format!("Member {uid}\nhttps://115.com/s/only{uid} 提取码：M{uid}");
                    json!({"@type":"foundChatMessages","messages":[{"@type":"message","id":42<<20,"chat_id":CHAT,"content":{"@type":"messageText","text":{"text":text}}}],"next_from_message_id":0})
                }
                "close" => json!({"@type":"ok"}),
                _ => json!({"@type":"error","code":404,"message":"private upstream information"}),
            };
            vec![response]
        },
    ));
    let admin = app.setup().await;
    let uid = app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    super::telegram_browse::seed_native(&app, 1, 9001);
    let source = json!({"name":"私有订阅","kind":"private","target":CHAT.to_string()});
    assert_eq!(
        app.request("POST", CHANNELS, Some(&admin), Some(source))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        app.request("GET", SEARCH, Some(&viewer), None).await.status,
        StatusCode::BAD_GATEWAY
    );
    super::telegram_browse::seed_native(&app, uid, 9000 + uid);
    for (cookie, owner) in [(&admin, 1), (&viewer, uid)] {
        let result = app.request("GET", SEARCH, Some(cookie), None).await.json();
        assert_eq!(
            result,
            json!([{"provider":"115","channel":"私有订阅","url":format!("https://115.com/s/only{owner}"),"code":format!("M{owner}"),"title":format!("Member {owner}")} ])
        );
    }
}

#[tokio::test]
async fn search_bounds_concurrent_sources_and_skips_disabled_channels() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let visits = Arc::new(AtomicUsize::new(0));
    let upstream = Router::new().route(
        "/s/{name}",
        get({
            let peak = peak.clone();
            let visits = visits.clone();
            move || {
                let (active, peak, visits) = (active.clone(), peak.clone(), visits.clone());
                async move {
                    visits.fetch_add(1, Ordering::SeqCst);
                    let n = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    "<html></html>"
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let app = TestApp::with_cloud_fixture(base.parse().unwrap());
    let admin = app.setup().await;
    let channels: Vec<_> = (0..11)
        .map(|i| json!({"target":format!("film_{i}"),"name":format!("电影 {i}"),"enabled":i!=0}))
        .collect();
    assert_eq!(
        app.request("POST", IMPORT, Some(&admin), Some(json!(channels)))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.request("GET", SEARCH, Some(&admin), None).await.json(),
        json!([])
    );
    assert_eq!(visits.load(Ordering::SeqCst), 10);
    assert!((2..=6).contains(&peak.load(Ordering::SeqCst)));
    server.abort();
}
