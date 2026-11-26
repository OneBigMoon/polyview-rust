use super::*;
use axum::routing::get;

const PRIMARY: &str = "https://media.invalid/path-secret/live.M3U8?token=stream-secret";
const TEMPLATE: &str =
    "https://archive.invalid/${(b)yyyyMMddHHmmss}/${(e)yyyyMMddHHmmss}/clip.ts?key=archive-secret";

fn mixed_playlist() -> String {
    format!(
        "\u{feff}#EXTM3U\r\n\
         #EXTINF:-1 tvg-id=\"epg.news\" tvg-logo=\"https://logo.invalid/logo-secret.png\" group-title=\"News, local\" catchup=\"default\" catchup-source=\"{TEMPLATE}\",Evening\r\n\
         #EXTVLCOPT:http-user-agent=Original\r\n\
         #EXTVLCOPT:http-referrer=https://refer.invalid/ref-secret\r\n\
         {PRIMARY}|user-agent=Device%201&Cookie=token%3Dcookie-secret\r\n\
         News, local,#genre#\r\n\
         Evening,https://alt.invalid/backup.flv|Referer=https%3A%2F%2Fref.invalid%2F%3Fa%3D1%26b%3D2#https://alt.invalid/last.ts\r\n\
         #EXTINF:-1,Duplicate\r\n\
         {PRIMARY}|Cookie=ignored\r\n"
    )
}

async fn import(app: &TestApp, admin: &str, playlist: &str) -> Reply {
    app.request(
        "POST",
        "/api/channels/import",
        Some(admin),
        Some(json!({"playlist":playlist})),
    )
    .await
}

#[tokio::test]
async fn mixed_import_keeps_each_lines_headers_template_order_and_channel_identity() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let reply = import(&app, &admin, &mixed_playlist()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"imported":3,"new_channels":1}));
    {
        let conn = app.state.conn().unwrap();
        let mut stmt = conn.prepare(
            "SELECT url,headers_json,catchup_mode,catchup_source FROM channel_sources ORDER BY position",
        ).unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, PRIMARY);
        assert_eq!(rows[0].2, "default");
        assert_eq!(rows[0].3, TEMPLATE);
        assert_eq!(
            serde_json::from_str::<Value>(&rows[0].1).unwrap(),
            json!({"User-Agent":"Device 1","Referer":"https://refer.invalid/ref-secret","Cookie":"token=cookie-secret"})
        );
        assert_eq!(rows[1].0, "https://alt.invalid/backup.flv");
        assert_eq!(
            serde_json::from_str::<Value>(&rows[1].1).unwrap(),
            json!({"Referer":"https://ref.invalid/?a=1&b=2"})
        );
        assert!(rows[1].3.is_empty());
        assert_eq!(rows[2].0, "https://alt.invalid/last.ts");
        assert_eq!(rows[2].1, "{}");
    }
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let channels = app
        .request("GET", "/api/channels", Some(&viewer), None)
        .await
        .json();
    let channel = &channels[0];
    assert_eq!(channel["name"], "Evening");
    assert_eq!(channel["group_name"], "News, local");
    assert_eq!(channel["tvg_id"], "epg.news");
    assert_eq!(channel["catchup"], true);
    assert_eq!(
        channel["logo"],
        format!("/api/channels/{}/logo", channel["id"])
    );
    let lines = channel["sources"].as_array().unwrap();
    assert_eq!(channel["url"], lines[0]["url"]);
    assert_eq!(lines[0]["hls"], true);
    assert_eq!(lines[0]["catchup_hls"], false);
    assert_eq!(lines[1]["catchup"], false);
    for line in lines {
        assert_eq!(
            line["url"],
            format!("/api/channels/lines/{}/stream", line["id"])
        );
        assert_eq!(line["stream_url"], line["url"]);
        assert!(line["latency_ms"].is_null());
        assert!(line["last_ok_at"].is_null());
        assert_eq!(line["fail_count"], 0);
    }
    assert_eq!(
        app.request(
            "PUT",
            &format!("/api/channels/{}/favorite", channel["id"]),
            Some(&viewer),
            Some(json!({"favorite":true}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let favorites = app
        .request("GET", "/api/favorites", Some(&viewer), None)
        .await
        .json();
    assert_eq!(favorites["channels"][0]["sources"], channel["sources"]);
    for value in [channels, favorites] {
        let body = value.to_string();
        assert!(!body.contains("secret"));
        assert!(!body.contains(".invalid"));
        assert!(!body.contains("headers_json"));
        assert!(!body.contains("catchup_source"));
    }
}

#[tokio::test]
async fn refresh_replaces_line_settings_without_duplicating_or_resetting_healthy_reimports() {
    let app = TestApp::new();
    let admin = app.setup().await;
    assert_eq!(
        import(&app, &admin, &mixed_playlist()).await.status,
        StatusCode::OK
    );
    let health =
        || {
            app.state.conn().unwrap().query_row(
            "SELECT id,position,latency_ms,last_ok_at,fail_count,headers_json,catchup_source
             FROM channel_sources WHERE url=?1", [PRIMARY], |r| Ok((
                r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            )),
        ).unwrap()
        };
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE channel_sources SET latency_ms=35,last_ok_at=999,fail_count=1 WHERE url=?1",
            [PRIMARY],
        )
        .unwrap();
    let original = health();
    assert_eq!(
        import(&app, &admin, &mixed_playlist()).await.json()["new_channels"],
        0
    );
    assert_eq!(health(), original);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/playlist", listener.local_addr().unwrap());
    let upstream = Router::new().route(
        "/playlist",
        get(|| async { format!("News, local,#genre#\nEvening,{PRIMARY}|User-Agent=Replacement") }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    assert_eq!(
        app.request(
            "POST",
            "/api/live-sources",
            Some(&admin),
            Some(json!({"name":"Subscription","url":url}))
        )
        .await
        .status,
        StatusCode::CREATED
    );
    assert_eq!(
        app.request("POST", "/api/live-sources/refresh", Some(&admin), None)
            .await
            .json(),
        json!({"refreshed":1,"failed":0})
    );
    server.abort();
    let refreshed = health();
    assert_eq!((refreshed.0, refreshed.1), (original.0, original.1));
    assert_eq!((refreshed.2, refreshed.3, refreshed.4), (None, None, 0));
    assert_eq!(
        serde_json::from_str::<Value>(&refreshed.5).unwrap(),
        json!({"User-Agent":"Replacement"})
    );
    assert!(refreshed.6.is_empty());
    let list = app
        .request("GET", "/api/channels", Some(&admin), None)
        .await
        .json();
    assert_eq!(list[0]["sources"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn invalid_headers_or_urls_reject_the_entire_import_without_exposing_input() {
    let app = TestApp::new();
    let admin = app.setup().await;
    for bad in [
        "Bad,https://example.invalid/|Cookie=secret%0D%0AX-Test%3A1",
        "Bad,https://example.invalid/|Host=secret",
        "Bad,https://user:secret@example.invalid/stream",
        "#EXTINF:-1 catchup-source=\"file:///secret\",Bad\nhttps://example.invalid/",
    ] {
        let reply = import(
            &app,
            &admin,
            &format!("#EXTM3U\nOK,https://example.invalid/good\n{bad}"),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert!(!String::from_utf8_lossy(&reply.body).contains("secret"));
    }
    assert_eq!(
        app.request("GET", "/api/channels", Some(&admin), None)
            .await
            .json(),
        json!([])
    );
}

#[test]
fn existing_channels_migrate_once_with_empty_metadata_and_unmeasured_health() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE channels(id INTEGER PRIMARY KEY,name TEXT NOT NULL, group_name TEXT NOT NULL, logo TEXT NOT NULL,url TEXT NOT NULL UNIQUE);
        INSERT INTO channels VALUES(42,'Old','Group','','https://old.invalid/live');").unwrap();
    init_schema(&conn).unwrap();
    init_schema(&conn).unwrap();
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM channels c JOIN channel_sources s ON c.id=s.channel_id
        WHERE c.id=42 AND c.tvg_id='' AND s.url=c.url AND s.headers_json='{}' AND s.catchup_mode=''
        AND s.catchup_source='' AND s.latency_ms IS NULL AND s.last_ok_at IS NULL AND s.fail_count=0", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1);
}
