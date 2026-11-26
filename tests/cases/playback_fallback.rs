use super::*;
use axum::{Json, extract::Path, extract::Query, routing::get};
use std::{collections::HashMap, time::Duration};

const FALLBACK: &str = "/api/catalog/fallback?source_id=1&vod_id=original&episode=2&flag=main";

fn media(id: &str, title: &str, year: i32, kind: &str, flag: &str, playlist: &str) -> Value {
    json!({
        "vod_id":id, "vod_name":title, "vod_year":year, "type_name":kind,
        "vod_play_from":flag, "vod_play_url":playlist
    })
}

struct Fixture {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let cms = Router::new().route(
            "/{source}",
            get(move |Path(source): Path<String>, Query(q): Query<HashMap<String, String>>| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(source.clone());
                    assert_eq!(q["ac"], "detail");
                    let rows = match (source.as_str(), q.get("ids").map(String::as_str)) {
                        ("origin", Some("original")) => vec![media(
                            "original", "Story (2025)", 2025, "电视剧", "other$$$main",
                            "E02$https://a.test/other2.mp4#E01$https://a.test/other1.mp4$$$第1集$https://a.test/1.mp4#第０２集$https://a.test/2.mp4",
                        )],
                        ("best", None) => {
                            assert_eq!(q["wd"], "Story");
                            tokio::time::sleep(Duration::from_millis(40)).await;
                            vec![
                                media("remake", "Story", 2022, "电视剧", "hls", "EP02$https://b.test/remake.mp4"),
                                media("film", "Story", 2025, "动作片", "hls", "EP02$https://b.test/film.mp4"),
                                media("sequel", "Story 2", 2025, "电视剧", "hls", "EP02$https://b.test/sequel.mp4"),
                                media("missing", "Story", 2025, "电视剧", "hls", "EP01$https://b.test/1.mp4#EP03$https://b.test/3.mp4"),
                                json!({"vod_id":"imposter", "vod_name":"Story"}),
                                json!({"vod_id":"good", "vod_name":"STORY"}),
                            ]
                        }
                        ("best", Some("imposter")) => vec![media(
                            "imposter", "Other", 2025, "电视剧", "hls", "EP02$https://b.test/wrong.mp4",
                        )],
                        ("best", Some("good")) => vec![media(
                            "good", "STORY", 2025, "国产剧", "hls$$$qq$$$repeat",
                            "S01E02$https://b.test/2.m3u8#S01E01$https://b.test/share/1$$$第二集$https://qq.test/share/2$$$02$https://b.test/2.m3u8",
                        )],
                        (name, None) => vec![media(
                            name, "Story", 2025, "电视剧", "hls",
                            &format!("EP02$https://cdn.test/{name}/2.mp4"),
                        )],
                        _ => vec![],
                    };
                    Json(json!({"list":rows}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, cms).await.unwrap() });
        Self {
            addr,
            requests,
            server,
        }
    }

    fn add(&self, app: &TestApp, name: &str, health: i64, priority: i64) -> i64 {
        let conn = app.state.conn().unwrap();
        conn.execute(
            "INSERT INTO catalog_sources(name,url,health_score,priority,custom_cookie)
             VALUES(?,?,?,?,'session=private-configuration')",
            rusqlite::params![
                name,
                format!("http://{}/{name}", self.addr),
                health,
                priority
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn fallback_matches_title_year_kind_and_episode_in_health_and_priority_order() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let cms = Fixture::start().await;
    assert_eq!(cms.add(&app, "origin", 100, 0), 1);
    let low = cms.add(&app, "low", 30, 0);
    let second = cms.add(&app, "second", 95, 10);
    let best = cms.add(&app, "best", 95, 5);
    let reply = app.request("GET", FALLBACK, Some(&viewer), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({"items":[
            {"source_id":best, "vod_id":"good", "title":"STORY", "flag":"hls", "url":"https://b.test/2.m3u8", "direct":true},
            {"source_id":best, "vod_id":"good", "title":"STORY", "flag":"qq", "url":"https://qq.test/share/2", "direct":false},
            {"source_id":second, "vod_id":"second", "title":"Story", "flag":"hls", "url":"https://cdn.test/second/2.mp4", "direct":true},
            {"source_id":low, "vod_id":"low", "title":"Story", "flag":"hls", "url":"https://cdn.test/low/2.mp4", "direct":true}
        ]})
    );
}

#[tokio::test]
async fn fallback_skips_inaccessible_sources_and_validates_the_original_episode() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    let cms = Fixture::start().await;
    cms.add(&app, "origin", 100, 0);
    for name in ["disabled", "adult", "private", "cooldown"] {
        cms.add(&app, name, 100, 0);
    }
    app.state
        .conn()
        .unwrap()
        .execute_batch(
            "UPDATE catalog_sources SET enabled=0 WHERE name='disabled';
         UPDATE catalog_sources SET adult=1 WHERE name='adult';
         UPDATE catalog_sources SET searchable=0 WHERE name='private';
         UPDATE catalog_sources SET cooldown_until=unixepoch()+120 WHERE name='cooldown';",
        )
        .unwrap();
    for (route, session, status) in [
        (FALLBACK, None, StatusCode::UNAUTHORIZED),
        (
            "/api/catalog/fallback?source_id=1&vod_id=original&episode=0",
            Some(&cookie),
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/catalog/fallback?source_id=3&vod_id=original&episode=1",
            Some(&cookie),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let reply = app
            .request("GET", route, session.map(String::as_str), None)
            .await;
        assert_eq!(reply.status, status);
    }
    assert!(cms.requests.lock().unwrap().is_empty());
    let reply = app.request("GET", FALLBACK, Some(&cookie), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"items":[]}));
    assert_eq!(*cms.requests.lock().unwrap(), ["origin"]);
    cms.add(&app, "public", 100, 0);
    let reply = app
        .request(
            "GET",
            "/api/catalog/fallback?source_id=1&vod_id=original&episode=3",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(reply.json(), json!({"items":[]}));
    assert_eq!(*cms.requests.lock().unwrap(), ["origin", "origin"]);
}

#[tokio::test]
async fn an_opened_title_recovers_offline_and_cached_identity_follows_source_lifecycle() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    let origin = Fixture::start().await;
    let backup = Fixture::start().await;
    origin.add(&app, "origin", 100, 0);
    backup.add(&app, "backup", 100, 0);
    let detail = "/api/catalog/1/detail?id=original";
    assert_eq!(
        app.request("GET", detail, Some(&cookie), None).await.status,
        StatusCode::OK
    );
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE catalog_sources SET url=? WHERE id=1",
            [format!("http://{}/replacement", backup.addr)],
        )
        .unwrap();
    assert_eq!(
        app.request("GET", FALLBACK, Some(&cookie), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let url = format!("http://{}/origin", origin.addr);
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE catalog_sources SET url=? WHERE id=1", [&url])
        .unwrap();
    assert_eq!(
        app.request("GET", detail, Some(&cookie), None).await.status,
        StatusCode::OK
    );
    origin.server.abort();
    let reply = app.request("GET", FALLBACK, Some(&cookie), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json()["items"][0]["url"],
        "https://cdn.test/backup/2.mp4"
    );
    assert_eq!(*origin.requests.lock().unwrap(), ["origin", "origin"]);
    app.state
        .conn()
        .unwrap()
        .execute("UPDATE catalog_sources SET enabled=0 WHERE id=1", [])
        .unwrap();
    assert_eq!(
        app.request("GET", FALLBACK, Some(&cookie), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    {
        let conn = app.state.conn().unwrap();
        conn.execute("DELETE FROM catalog_sources WHERE id=1", [])
            .unwrap();
        conn.execute(
            "INSERT INTO catalog_sources(id,name,url) VALUES(1,'replacement',?)",
            [url],
        )
        .unwrap();
    }
    assert_eq!(
        app.request("GET", FALLBACK, Some(&cookie), None)
            .await
            .status,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn fallback_queries_only_the_twelve_highest_ranked_eligible_sources() {
    let app = TestApp::new();
    let cookie = app.setup().await;
    let cms = Fixture::start().await;
    cms.add(&app, "origin", 100, 0);
    for i in 1..=15 {
        cms.add(&app, &format!("backup{i}"), 100, i);
    }
    let reply = app.request("GET", FALLBACK, Some(&cookie), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let body = reply.json();
    let rows = body["items"].as_array().unwrap();
    assert_eq!(rows.len(), 12);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row["vod_id"], format!("backup{}", i + 1));
    }
    assert_eq!(cms.requests.lock().unwrap().len(), 13);
}
