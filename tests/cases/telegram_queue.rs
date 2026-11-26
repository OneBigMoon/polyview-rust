use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use polyview::telegram::{native::NativeRuntime, subscriptions};
use std::{sync::Condvar, time::Duration};

const CHAT: i64 = -1_000_000_000_123;
const UNIT: i64 = 1 << 20;
const RULES: &str = "/api/telegram/subscriptions";

struct Feed {
    rows: Mutex<Vec<Value>>,
    hold_file: Mutex<Option<i64>>,
    resume: Condvar,
}

fn video(id: i64, album: i64, name: &str, caption: &str) -> Value {
    json!({"@type":"message", "chat_id":CHAT, "id":id*UNIT,"media_album_id":album.to_string(),
           "content":{"@type":"messageVideo","caption":{"text":caption},
           "video":{"file_name":name, "video":{"id":id, "size":10}}}})
}

fn text(id: i64) -> Value {
    json!({"@type":"message", "chat_id":CHAT, "id":id*UNIT,
           "content":{"@type":"messageText", "text":{"text":"ordinary message"}}})
}

impl Feed {
    fn reply(&self, owner: i64, request: Value) -> Vec<Value> {
        let denied = || json!({"@type":"error","code":404,"message":"unavailable"});
        let response = match request["@type"].as_str().unwrap() {
            "getAuthorizationState" => json!({"@type":"authorizationStateReady"}),
            "getMe" => json!({"@type":"user","id":8000+owner}),
            "getMessage" if owner == 1 && request["chat_id"] == CHAT => self
                .rows
                .lock()
                .unwrap()
                .iter()
                .find(|row| row["id"] == request["message_id"])
                .cloned()
                .unwrap_or_else(denied),
            "getChatHistory" if owner == 1 && request["chat_id"] == CHAT => {
                let from = request["from_message_id"].as_i64().unwrap();
                let rows = self
                    .rows
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .filter(|r| from == 0 || r["id"].as_i64().unwrap() <= from)
                    .take(2)
                    .cloned()
                    .collect::<Vec<_>>();
                json!({"@type":"messages","messages":rows})
            }
            "downloadFile" if owner == 1 => json!({"@type":"file","id":request["file_id"]}),
            "readFilePart" if owner == 1 => {
                let id = request["file_id"].as_i64().unwrap();
                let offset = request["offset"].as_u64().unwrap();
                if offset > 0 {
                    let _guard = self
                        .resume
                        .wait_timeout_while(
                            self.hold_file.lock().unwrap(),
                            Duration::from_secs(10),
                            |held| *held == Some(id),
                        )
                        .unwrap();
                }
                let count = request["count"].as_u64().unwrap().min(5);
                let data: Vec<_> = (offset..offset + count)
                    .map(|n| id as u8 + n as u8)
                    .collect();
                json!({"@type":"data","data":STANDARD.encode(data)})
            }
            "close" | "cancelDownloadFile" => json!({"@type":"ok"}),
            _ => denied(),
        };
        vec![response]
    }
}

fn fixture(rows: Vec<Value>) -> (TempDir, TestApp, Arc<Feed>) {
    let root = tempfile::tempdir().unwrap();
    let feed = Arc::new(Feed {
        rows: Mutex::new(rows),
        hold_file: Mutex::new(None),
        resume: Condvar::new(),
    });
    let transport = feed.clone();
    let runtime = NativeRuntime::with_transport(root.path().to_owned(), move |id, req| {
        transport.reply(id, req)
    });
    (root, TestApp::new_with_native(runtime), feed)
}

async fn await_rows(
    app: &TestApp,
    path: &str,
    cookie: &str,
    ready: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let res = app.request("GET", path, Some(cookie), None).await;
            assert_eq!(res.status, StatusCode::OK);
            let rows: Vec<Value> = serde_json::from_slice(&res.body).unwrap();
            if ready(&rows) {
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("expected queue or cursor update")
}

#[tokio::test]
async fn authoritative_album_queues_member_videos_reports_live_progress_and_indexes_exact_bytes() {
    let (_root, app, feed) = fixture(vec![
        video(10, 44, "first.mkv", "first"),
        text(11),
        video(12, 44, "last.mp4", "last"),
        video(13, 99, "other.mp4", "other"),
    ]);
    let owner = app.setup().await;
    let viewer_id = app.create_viewer(&owner).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    super::telegram_browse::seed_native(&app, 1, 8001);
    super::telegram_browse::seed_native(&app, viewer_id, 8000 + viewer_id);
    let endpoint = format!("/api/telegram/chats/{CHAT}/messages/12/download");
    assert_eq!(
        app.request("POST", &endpoint, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request_with_headers("POST", &endpoint, Some(&owner), None, &[])
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_ne!(
        app.request("POST", &endpoint, Some(&viewer), None)
            .await
            .status,
        StatusCode::CREATED
    );
    let submitted = app
        .request("POST", &endpoint, Some(&owner), Some(json!({})))
        .await;
    assert_eq!(submitted.status, StatusCode::CREATED);
    let result = submitted.json();
    assert_eq!(result["queued"], 2);
    assert_eq!(result["downloads"][0]["title"], "first.mkv");
    assert_eq!(result["downloads"][1]["title"], "last.mp4");
    let source = result["downloads"][0]["url"].as_str().unwrap();
    assert_ne!(
        app.request("GET", source, Some(&viewer), None).await.status,
        StatusCode::OK
    );
    let range = app
        .request_with_headers("GET", source, Some(&owner), None, &[("range", "bytes=2-4")])
        .await;
    assert_eq!(range.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.body, [12, 13, 14]);
    assert_eq!(
        app.request("GET", "/api/downloads", Some(&viewer), None)
            .await
            .json(),
        json!([])
    );

    *feed.hold_file.lock().unwrap() = Some(10);
    let worker = polyview::network::start_worker(app.state.clone());
    let jobs = await_rows(&app, "/api/downloads", &owner, |rows| {
        rows.iter().any(|r| r["bytes_written"] == 5)
    })
    .await;
    let progress = jobs.iter().find(|r| r["bytes_written"] == 5).unwrap();
    assert_eq!(progress["total_bytes"], 10);
    assert_eq!(progress["percent"], 50.0);
    assert!(progress["bytes_per_second"].as_i64().unwrap() > 0);
    *feed.hold_file.lock().unwrap() = None;
    feed.resume.notify_all();
    let jobs = await_rows(&app, "/api/downloads", &owner, |rows| {
        rows.len() == 2 && rows.iter().all(|r| r["status"] == "completed")
    })
    .await;
    let library = await_rows(&app, "/api/library", &owner, |rows| rows.len() == 2).await;
    for job in jobs {
        let item = library
            .iter()
            .find(|item| item["relative_path"] == job["relative_path"])
            .unwrap();
        let stream = format!("/api/library/{}/stream", item["id"]);
        assert_eq!(
            app.request("GET", &stream, None, None).await.status,
            StatusCode::UNAUTHORIZED
        );
        let body = app.request("GET", &stream, Some(&owner), None).await;
        assert_eq!(body.status, StatusCode::OK);
        let first = if job["title"] == "first.mkv" { 10 } else { 12 };
        assert_eq!(body.body, (first..first + 10).collect::<Vec<u8>>());
        assert_eq!(job["percent"], 100.0);
        assert_eq!(job["bytes_per_second"], 0);
    }
    worker.abort();
}

#[tokio::test]
async fn keyword_worker_resumes_across_empty_pages_and_deduplicates_overlapping_rules() {
    let (_root, app, feed) = fixture(vec![video(39, 0, "past.mp4", "news"), text(40)]);
    let owner = app.setup().await;
    app.create_viewer(&owner).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    super::telegram_browse::seed_native(&app, 1, 8001);
    let mut rule_ids = Vec::new();
    for keyword in ["news", "cinema"] {
        let created = app
            .request(
                "POST",
                RULES,
                Some(&owner),
                Some(json!({"chat_id":CHAT,"keyword":keyword})),
            )
            .await;
        assert_eq!(created.status, StatusCode::CREATED);
        assert_eq!(created.json()["last_message_id"], 40);
        rule_ids.push(created.json()["id"].as_i64().unwrap());
    }
    let rule_path = format!("{RULES}/{}", rule_ids[1]);
    assert_eq!(
        app.request("DELETE", &rule_path, Some(&viewer), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.request("GET", RULES, Some(&viewer), None).await.json(),
        json!([])
    );
    assert_eq!(
        app.request(
            "PUT",
            &rule_path,
            Some(&owner),
            Some(json!({"enabled":false}))
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    feed.rows.lock().unwrap().extend([
        video(41, 0, "cinema.mp4", "breaking NEWS"),
        video(42, 0, "ignored.mp4", "unrelated"),
        video(43, 0, "music.mp4", "cinema"),
        text(44),
        text(45),
    ]);
    let worker = subscriptions::start_worker(app.state.clone());
    await_rows(&app, RULES, &owner, |rows| rows[0]["last_message_id"] == 45).await;
    worker.abort();
    let _ = worker.await;
    let jobs = app
        .request("GET", "/api/downloads", Some(&owner), None)
        .await
        .json();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    assert_eq!(jobs[0]["title"], "cinema.mp4");
    assert_eq!(
        app.request(
            "PUT",
            &rule_path,
            Some(&owner),
            Some(json!({"enabled":true}))
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let worker = subscriptions::start_worker(app.state.clone());
    await_rows(&app, RULES, &owner, |rows| rows[1]["last_message_id"] == 45).await;
    let jobs = app
        .request("GET", "/api/downloads", Some(&owner), None)
        .await
        .json();
    assert_eq!(jobs.as_array().unwrap().len(), 2);
    assert_eq!(jobs[0]["title"], "music.mp4");
    worker.abort();
}

#[tokio::test]
async fn cancelling_a_native_single_message_stops_library_publication() {
    let (_root, app, feed) = fixture(vec![
        video(10, 44, "first.mp4", ""),
        video(11, 44, "second.mp4", ""),
    ]);
    let owner = app.setup().await;
    super::telegram_browse::seed_native(&app, 1, 8001);
    let response = app
        .request(
            "POST",
            &format!("/api/telegram/chats/{CHAT}/messages/11/download"),
            Some(&owner),
            Some(json!({"single":true})),
        )
        .await;
    assert_eq!(response.status, StatusCode::CREATED);
    let batch = response.json();
    assert_eq!(batch["queued"], 1);
    let job = &batch["downloads"][0];
    assert_eq!(job["title"], "second.mp4");
    *feed.hold_file.lock().unwrap() = Some(11);
    let worker = polyview::network::start_worker(app.state.clone());
    await_rows(&app, "/api/downloads", &owner, |rows| {
        rows[0]["bytes_written"] == 5
    })
    .await;
    let endpoint = format!("/api/downloads/{}", job["id"]);
    assert_eq!(
        app.request("DELETE", &endpoint, Some(&owner), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    *feed.hold_file.lock().unwrap() = None;
    feed.resume.notify_all();
    tokio::time::timeout(Duration::from_secs(5), async {
        while fs::read_dir(&app.state.download_dir)
            .unwrap()
            .next()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("cancelled native staging removed");
    let jobs = app
        .request("GET", "/api/downloads", Some(&owner), None)
        .await
        .json();
    assert_eq!(jobs[0]["status"], "cancelled");
    assert_eq!(jobs[0]["bytes_per_second"], 0);
    assert_eq!(
        app.request("GET", "/api/library", Some(&owner), None)
            .await
            .json(),
        json!([])
    );
    worker.abort();
}

#[tokio::test]
async fn unfinished_backfill_resumes_at_its_saved_page_after_worker_restart() {
    let (_root, app, feed) = fixture(vec![text(10)]);
    let owner = app.setup().await;
    super::telegram_browse::seed_native(&app, 1, 8001);
    let created = app
        .request(
            "POST",
            RULES,
            Some(&owner),
            Some(json!({"chat_id":CHAT, "keyword":"weekly"})),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED);
    let id = created.json()["id"].as_i64().unwrap();
    {
        let mut rows = feed.rows.lock().unwrap();
        rows.push(video(11, 0, "weekly-early.mp4", ""));
        rows.extend((12..29).map(text));
        rows.push(video(29, 0, "weekly-late.mp4", ""));
    }
    let first_worker = subscriptions::start_worker(app.state.clone());
    // The scripted history returns two entries per page. Eight pages leave
    // IDs 11–13 for the next round, including the earlier matching video.
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let cursor: (i64, i64, i64) = app.state.conn().unwrap().query_row(
                "SELECT last_message_id,scan_offset,scan_newest FROM telegram_keyword_subscriptions WHERE id=?", [id],
                |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
            ).unwrap();
            if cursor == (10,13,29) { break; }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }).await.expect("subscription saved its incomplete scan");
    first_worker.abort();
    let _ = first_worker.await;
    let worker = subscriptions::start_worker(app.state.clone());
    await_rows(&app, RULES, &owner, |rows| rows[0]["last_message_id"] == 29).await;
    let jobs = app
        .request("GET", "/api/downloads", Some(&owner), None)
        .await
        .json();
    assert_eq!(jobs.as_array().unwrap().len(), 2);
    assert_eq!(jobs[0]["title"], "weekly-early.mp4");
    assert_eq!(jobs[1]["title"], "weekly-late.mp4");
    worker.abort();
}
