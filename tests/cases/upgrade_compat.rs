use super::*;
use axum::{Json, routing::get};
use rusqlite::types::Value as SqlValue;

const NOVEMBER_SCHEMA: &str = include_str!("../fixtures/old-schema.sql");
const COOKIE: &str = "polyview_session=fixture-session";

// Positional inserts deliberately use the original column counts. All values are synthetic.
const SEED: &str = "
    INSERT INTO users VALUES(17,'fixture-admin','fixture-hash','admin',101);
    INSERT INTO users VALUES(18,'fixture-viewer','fixture-viewer-hash','user',102);
    INSERT INTO sessions VALUES('fixture-session',17,4102444800);
    INSERT INTO login_attempts VALUES('fixture-other',2,102);
    INSERT INTO library_series VALUES(23,'Archive');
    INSERT INTO library_media VALUES(31,'Archive Episode','Archive/ep2.mp4','media','episode',23,1,2,12345,1);
    INSERT INTO library_user_media VALUES(17,31,1,37.5,122.5,0,103);
    INSERT INTO library_user_media VALUES(18,31,0,122.5,122.5,1,106);
    INSERT INTO library_subscriptions VALUES(17,23);
    INSERT INTO channels VALUES(41,'Archive TV','Fixture','http://127.0.0.1/logo','http://127.0.0.1/live');
    INSERT INTO downloads VALUES(53,17,'Archive download','http://127.0.0.1/file','failed',13,200,'fixture failure','archive.mp4',104);";

// The real snapshot predates online sources and tracking. This second starting point adds
// only their original tables from db27f27 (src/catalog.rs and src/tracking.rs).
const EARLY_CATALOG_AND_TRACKING: &str = "
    CREATE TABLE catalog_sources (
        id INTEGER PRIMARY KEY, name TEXT NOT NULL, url TEXT NOT NULL UNIQUE,
        enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0,1))
    );
    CREATE TABLE tracking_subscriptions (
        id INTEGER PRIMARY KEY,
        user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE RESTRICT,
        media_id TEXT NOT NULL, title TEXT NOT NULL, cover TEXT NOT NULL DEFAULT '',
        total_episodes INTEGER NOT NULL DEFAULT 0, watched_episodes INTEGER NOT NULL DEFAULT 0,
        auto_download INTEGER NOT NULL DEFAULT 0 CHECK(auto_download IN (0,1)),
        last_checked INTEGER, error TEXT, UNIQUE(user_id,source_id,media_id)
    );
    CREATE TABLE tracking_downloads (
        subscription_id INTEGER NOT NULL REFERENCES tracking_subscriptions(id) ON DELETE CASCADE,
        episode_key TEXT NOT NULL,
        download_id INTEGER NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
        PRIMARY KEY(subscription_id,episode_key)
    );
    CREATE INDEX tracking_user ON tracking_subscriptions(user_id,id);
    INSERT INTO catalog_sources VALUES(61,'Archive CMS','http://127.0.0.1/cms',1);
    INSERT INTO tracking_subscriptions VALUES(73,17,61,'m-7','Archive','http://127.0.0.1/cover',8,2,1,105,'fixture issue');
    INSERT INTO tracking_downloads VALUES(73,'ep-3',53);";

fn rows(conn: &Connection, select: &str) -> Vec<Vec<SqlValue>> {
    let mut statement = conn.prepare(select).unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |row| (0..count).map(|column| row.get(column)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// Save every original field and relation, including NULLs, without selecting new columns
// when we compare the migrated database. Identifiers come solely from the SQL fixtures.
fn original_records(conn: &Connection) -> Vec<(String, Vec<Vec<SqlValue>>)> {
    let mut tables = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|name| {
            let name = name.unwrap();
            let all = conn.prepare(&format!("SELECT * FROM {name}")).unwrap();
            let columns = all.column_names().join(",");
            let select = format!("SELECT {columns} FROM {name} ORDER BY rowid");
            let expected = rows(conn, &select);
            (select, expected)
        })
        .collect()
}

fn assert_sql(conn: &Connection, query: &str) {
    assert!(
        conn.query_row(query, [], |row| row.get::<_, bool>(0))
            .unwrap_or_else(|error| panic!("{query}: {error}")),
        "{query}"
    );
}

fn assert_new_values(conn: &Connection, with_catalog: bool) {
    for query in [
        "SELECT disabled=0 FROM users WHERE id=17",
        "SELECT cover='' AND year='' AND description='' FROM library_media WHERE id=31",
        "SELECT favorite_at=updated_at FROM library_user_media WHERE user_id=17 AND media_id=31",
        "SELECT favorite_at IS NULL FROM library_user_media WHERE user_id=18 AND media_id=31",
        "SELECT count(*)=1 FROM channel_sources",
        "SELECT url='http://127.0.0.1/live' AND label='' AND position=0 AND headers_json='{}'
            AND catchup_mode='' AND catchup_source='' AND latency_ms IS NULL AND last_ok_at IS NULL
            AND fail_count=0 FROM channel_sources WHERE channel_id=41",
        "SELECT kind='direct' AND attempts=0 AND cover='' AND year='' AND description=''
            AND attempt_token='' AND retry_at=0 AND resolved_media IS NULL AND bytes_per_second=0
            FROM downloads WHERE id=53",
    ] {
        assert_sql(conn, query);
    }
    if with_catalog {
        for query in [
            "SELECT adult=0 AND priority=100 AND searchable=1 AND custom_ua='' AND custom_referer=''
                AND custom_cookie='' AND proxy_url='' AND timeout_secs=12 AND last_latency_ms IS NULL
                AND last_probe_ok IS NULL AND last_probe_at IS NULL AND fail_streak=0 AND cooldown_until=0
                AND health_score=100 AND successful_probes=0 AND total_probes=0 FROM catalog_sources WHERE id=61",
            "SELECT paused=0 AND notified_episodes=0 AND rules='{}' FROM tracking_subscriptions WHERE id=73",
            "SELECT archived_completed=0 FROM tracking_downloads WHERE subscription_id=73 AND download_id=53",
            "SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('tracking_downloads') WHERE
                \"table\"='downloads' AND \"from\"='download_id' AND on_delete='SET NULL')",
        ] {
            assert_sql(conn, query);
        }
    }
}

fn upgraded_app(with_catalog: bool) -> TestApp {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("polyview.sqlite3");
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    conn.execute_batch(NOVEMBER_SCHEMA).unwrap();
    conn.execute_batch(SEED).unwrap();
    if with_catalog {
        conn.execute_batch(EARLY_CATALOG_AND_TRACKING).unwrap();
    }
    let original = original_records(&conn);
    for startup in 1..=2 {
        drop(conn);
        conn = Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "truncate");
        assert_sql(
            &conn,
            "SELECT NOT EXISTS(SELECT 1 FROM pragma_foreign_key_check)",
        );
        for (select, expected) in &original {
            assert_eq!(
                &rows(&conn, select),
                expected,
                "startup {startup}: {select}"
            );
        }
        assert_new_values(&conn, with_catalog);
        if startup == 1 {
            assert_sql(&conn, "SELECT tvg_id='' FROM channels WHERE id=41");
            conn.execute("UPDATE channels SET tvg_id='fixture-guide' WHERE id=41", [])
                .unwrap();
        }
    }
    let media_dir = temp.path().join("media");
    let download_dir = media_dir.join("downloads");
    fs::create_dir_all(&download_dir).unwrap();
    let state = Arc::new(AppState {
        db: Mutex::new(conn),
        epg_refresh: Default::default(),
        media_dir,
        download_dir,
        playback_grants: Default::default(),
        telegram_native: Default::default(),
        cloud_fixture: None,
        download_runtime: Default::default(),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        secure_cookie: false,
    });
    TestApp {
        _temp: temp,
        app: router(state.clone()),
        state,
    }
}

async fn exercise_routes(with_catalog: bool) {
    let app = upgraded_app(with_catalog);
    let providers = Router::new()
        .route(
            "/calendar",
            get(|| async {
                Json(json!([{"weekday":{"id":1},"items":[{"id":7,"name":"Archive calendar"}]}]))
            }),
        )
        .route(
            "/search",
            get(|| async {
                Json(json!([{"title":"Archive share","url":"https://pan.quark.cn/s/fixture"}]))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, providers).await.unwrap() });
    let calendar = json!({"enabled":true,"api_url":format!("http://{address}/calendar")});
    let cloud = json!({
        "kind":"api", "name":"Fixture search", "target":format!("http://{address}/search")
    });
    for (method, route, body, status) in [
        ("PUT", "/api/settings/calendar", calendar, StatusCode::OK),
        ("POST", "/api/cloud/channels", cloud, StatusCode::CREATED),
    ] {
        let reply = app.request(method, route, Some(COOKIE), Some(body)).await;
        assert_eq!(reply.status, status, "{route}: {}", reply.json());
    }
    for route in [
        "/api/sources",
        "/api/channels",
        "/api/epg",
        "/api/progress",
        "/api/favorites",
        "/api/calendar",
        "/api/cloud/search?q=archive",
        "/api/parsers",
        "/api/source-subscriptions",
        "/api/downloaders",
        "/api/settings/scraper",
        "/api/settings/organizer",
        "/api/settings/export",
    ] {
        let reply = app.request("GET", route, Some(COOKIE), None).await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{route}: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let data = reply.json();
        match route {
            "/api/sources" if with_catalog => assert_eq!(data[0]["id"], 61),
            "/api/sources" => assert_eq!(data, json!([])),
            "/api/channels" => {
                assert_eq!(data[0]["tvg_id"], "fixture-guide");
                assert_eq!(data[0]["sources"][0]["url"], "http://127.0.0.1/live");
            }
            "/api/progress" => assert_eq!(data[0]["local_id"], 31),
            "/api/favorites" => assert_eq!(data["local"][0]["title"], "Archive Episode"),
            "/api/calendar" => {
                assert_eq!(data["days"][0]["items"][0]["title_cn"], "Archive calendar");
            }
            "/api/cloud/search?q=archive" => assert_eq!(data[0]["title"], "Archive share"),
            "/api/settings/export" => assert_eq!(data["live_channels"][0]["name"], "Archive TV"),
            _ => {}
        }
    }
    server.abort();
}

#[tokio::test]
async fn november_snapshot_upgrades_and_serves_authenticated_routes() {
    exercise_routes(false).await;
}

#[tokio::test]
async fn early_catalog_and_tracking_survive_upgrade_with_download_links() {
    exercise_routes(true).await;
}
