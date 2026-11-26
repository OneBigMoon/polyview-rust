use super::*;
use axum::{
    Json,
    extract::{Path, Query},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use std::{collections::HashMap, time::Duration};
use tokio::task::JoinHandle;

const PARSERS: &str = "/api/parsers";
const RESOLVE: &str = "/api/playback/resolve";
const INPUT: &str = "https://site.invalid/watch?id=1&signature=a%2Bb";
const MEDIA: &str = "https://cdn.invalid/film.M3U8?token=signed-private-url";

async fn serve(router: Router) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), task)
}

async fn create(app: &TestApp, admin: &str, config: Value) -> i64 {
    let reply = app
        .request("POST", PARSERS, Some(admin), Some(config))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let json = reply.json();
    assert!(json.get("headers_json").is_none());
    assert!(json.get("url_template").is_none());
    json["id"].as_i64().unwrap()
}

async fn resolve(app: &TestApp, cookie: &str, flag: &str) -> Reply {
    app.request(
        "POST",
        RESOLVE,
        Some(cookie),
        Some(json!({"url":INPUT,"flag":flag})),
    )
    .await
}

#[tokio::test]
async fn parser_permissions_crud_and_write_only_fields() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let member = app.login("viewer", PASSWORD).await.cookie();
    let config = json!({"name":"Parser", "url_template":"https://parser.invalid/test?secret=parser-key&url={url}", "headers_json":"{\"Authorization\":\"private-header\"}"});
    let input = json!({"url":INPUT,"flag":"qq"});
    for (method, path, body) in [
        ("GET", PARSERS, None),
        ("POST", PARSERS, Some(config.clone())),
        ("PUT", "/api/parsers/1", Some(config.clone())),
        ("DELETE", "/api/parsers/1", None),
        ("POST", "/api/parsers/1/test", Some(input.clone())),
    ] {
        for (cookie, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(member.as_str()), StatusCode::FORBIDDEN),
        ] {
            assert_eq!(
                app.request(method, path, cookie, body.clone()).await.status,
                expected
            );
        }
        if method != "GET" {
            assert_eq!(
                app.request_with_headers(method, path, Some(&admin), body, &[])
                    .await
                    .status,
                StatusCode::FORBIDDEN
            );
        }
    }
    assert_eq!(
        app.request("POST", RESOLVE, None, Some(input.clone()))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request_with_headers("POST", RESOLVE, Some(&member), Some(input), &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for invalid in [
        json!({"name":"Bad", "url_template":"javascript:alert(1)"}),
        json!({"name":"Bad", "url_template":"https://user:password@parser.invalid"}),
        json!({"name":"Bad", "url_template":"https://parser.invalid/x", "headers_json":"{\"Content-Length\":\"3\"}"}),
        json!({"name":"Bad", "url_template":"https://parser.invalid/x", "timeout_ms":0}),
    ] {
        assert_eq!(
            app.request("POST", PARSERS, Some(&admin), Some(invalid))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let id = create(&app, &admin, config.clone()).await;
    let path = format!("{PARSERS}/{id}");
    assert_eq!(
        app.request("POST", PARSERS, Some(&admin), Some(config))
            .await
            .status,
        StatusCode::CONFLICT
    );
    let list = app.request("GET", PARSERS, Some(&admin), None).await.json();
    assert_eq!(list[0]["name"], "Parser");
    assert!(!list.to_string().contains("private-header"));
    assert!(!list.to_string().contains("parser-key"));
    let update = app
        .request(
            "PUT",
            &path,
            Some(&admin),
            Some(json!({"name":"Renamed","enabled":false})),
        )
        .await;
    assert_eq!(update.json()["enabled"], false);
    let db_headers = || {
        app.state
            .conn()
            .unwrap()
            .query_row("SELECT headers_json FROM parsers WHERE id=?", [id], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
    };
    assert!(db_headers().contains("private-header"));
    assert_eq!(
        resolve(&app, &member, "qq").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.request(
            "PUT",
            &path,
            Some(&admin),
            Some(json!({"name":"Renamed","headers_json":"{}"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(db_headers(), "{}");
    assert_eq!(
        app.request("DELETE", &path, Some(&admin), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request("DELETE", &path, Some(&admin), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn resolution_checks_flags_priority_timeout_size_media_headers_and_cache() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let member = app.login("viewer", PASSWORD).await.cookie();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = calls.clone();
    let handler = move |Path(name): Path<String>,
                        Query(params): Query<HashMap<String, String>>,
                        headers: HeaderMap| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(name.clone());
            if params.get("url").map(String::as_str) != Some(INPUT)
                || headers.get("x-parser-token").and_then(|v| v.to_str().ok())
                    != Some("private-request")
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            match name.as_str() {
                "timeout" => { tokio::time::sleep(Duration::from_secs(5)).await; StatusCode::GATEWAY_TIMEOUT.into_response() },
                "large" => Response::new(Body::from_stream(futures_util::stream::iter([Ok::<_,std::io::Error>(vec![b'x'; 256 * 1024 + 1])]))),
                "bad" => Json(json!({"url":"javascript:alert(1)"})).into_response(),
                "html" => Json(json!({"url":"https://site.invalid/page.html"})).into_response(),
                "ok" => Json(json!({"data":{"url":MEDIA,"header":{"Cookie":"media-only", "Referer":"https://site.invalid/", "Host":"untrusted", "X-Playback-Token":"playback-only", "X-Forwarded-For":"never-forward"}}})).into_response(),
                _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
    };
    let (base, server) = serve(Router::new().route("/{name}", get(handler))).await;
    let mut good_id = 0;
    for (name, flag, priority, enabled) in [
        ("unmatched", "youku", 90, true),
        ("off", "qq", 89, false),
        ("timeout", " qq , IQIYI ", 80, true),
        ("large", "*", 70, true),
        ("bad", "qq", 60, true),
        ("html", "*", 55, true),
        ("ok", "*", 50, true),
        ("lowest", "*", 1, true),
    ] {
        let template = if name == "ok" {
            format!("{base}/{name}?url=")
        } else {
            format!("{base}/{name}?url={{url}}")
        };
        let id = create(&app,&admin,json!({"name":name,"url_template":template,"flags":flag,"priority":priority,"enabled":enabled,"timeout_ms":if name == "timeout" {100} else {2000},"headers_json":"{\"X-Parser-Token\":\"private-request\"}"})).await;
        if name == "ok" {
            good_id = id
        }
    }
    let first = resolve(&app, &member, "QQ").await;
    assert_eq!(first.status, StatusCode::OK);
    let result = first.json();
    assert_eq!(result["is_hls"], true);
    assert_eq!(result["url"], result["playback_url"]);
    assert!(
        result["playback_url"]
            .as_str()
            .unwrap()
            .starts_with("/api/")
    );
    for secret in [
        "private-request",
        "media-only",
        "playback-only",
        "signed-private-url",
        "cdn.invalid",
    ] {
        assert!(!result.to_string().contains(secret));
    }
    assert_eq!(
        *calls.lock().unwrap(),
        ["timeout", "large", "bad", "html", "ok"]
    );
    let (media, headers, expires): (String, String, i64) = app
        .state
        .conn()
        .unwrap()
        .query_row("SELECT media,headers,expires FROM parser_cache", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(media, MEDIA);
    assert_eq!(
        serde_json::from_str::<Value>(&headers).unwrap(),
        json!({"cookie":"media-only","referer":"https://site.invalid/","x-playback-token":"playback-only"})
    );
    assert!((590..=600).contains(&(expires - polyview::now())));
    assert_eq!(resolve(&app, &member, "qq").await.status, StatusCode::OK);
    assert_eq!(calls.lock().unwrap().len(), 5);
    app.state.conn().unwrap().execute(
        "WITH RECURSIVE sequence(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM sequence WHERE n<512)
         INSERT INTO parser_cache(input,flag,media,headers,expires)
         SELECT printf('older-%d',n),'qq',?1,'{}',?2 FROM sequence",
        rusqlite::params![MEDIA,polyview::now()+300],
    ).unwrap();
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE parser_cache SET expires=0 WHERE input=?", [INPUT])
        .unwrap();
    assert_eq!(resolve(&app, &member, "qq").await.status, StatusCode::OK);
    assert_eq!(calls.lock().unwrap().len(), 10);
    let (count, evicted): (i64,bool) = app.state.conn().unwrap().query_row(
        "SELECT count(*), NOT EXISTS(SELECT 1 FROM parser_cache WHERE input='older-1') FROM parser_cache", [],
        |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((count, evicted), (512, true));
    let test = app
        .request(
            "POST",
            &format!("{PARSERS}/{good_id}/test"),
            Some(&admin),
            Some(json!({"url":INPUT,"flag":"different"})),
        )
        .await;
    assert_eq!(test.status, StatusCode::OK);
    assert_eq!(calls.lock().unwrap().len(), 11);
    app.request(
        "PUT",
        &format!("{PARSERS}/{good_id}"),
        Some(&admin),
        Some(json!({"name":"ok","enabled":false})),
    )
    .await;
    for _ in 0..2 {
        assert_eq!(
            resolve(&app, &member, "qq").await.status,
            StatusCode::BAD_GATEWAY
        );
    }
    assert_eq!(calls.lock().unwrap().len(), 21);
    server.abort();
}

#[tokio::test]
async fn parser_credentials_never_follow_redirects_and_direct_urls_skip_parsing() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let leaked = Arc::new(Mutex::new(false));
    let received = leaked.clone();
    let (other, sink) = serve(Router::new().route(
        "/sink",
        get(move || async move {
            *received.lock().unwrap() = true;
            Json(json!({"url":MEDIA}))
        }),
    ))
    .await;
    let (base, parser) = serve(Router::new().route(
        "/entry",
        get(move || async move { Redirect::temporary(&format!("{other}/sink")) }),
    ))
    .await;
    create(&app,&admin,json!({"name":"Redirect", "url_template":format!("{base}/entry"),"headers_json":"{\"X-Token\":\"private\"}"})).await;
    assert_eq!(
        resolve(&app, &admin, "*").await.status,
        StatusCode::BAD_GATEWAY
    );
    assert!(!*leaked.lock().unwrap());
    let direct = app
        .request(
            "POST",
            RESOLVE,
            Some(&admin),
            Some(json!({"url":"https://site.invalid/movie.mp4"})),
        )
        .await;
    assert_eq!(direct.status, StatusCode::OK);
    assert_eq!(direct.json()["is_hls"], false);
    for url in [
        "file:///movie.mp4",
        "javascript:alert(1)",
        "https://token@site.invalid/movie.mp4",
    ] {
        assert_eq!(
            app.request("POST", RESOLVE, Some(&admin), Some(json!({"url":url})))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    parser.abort();
    sink.abort();
}
