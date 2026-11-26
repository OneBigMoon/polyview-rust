use axum::{Router, http::header};
use polyview::{AppState, Shared, init_schema, network, router};
use reqwest::{Client, Response, StatusCode};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tower_http::services::ServeDir;
use url::Url;

struct Fixture {
    _temp: TempDir,
    state: Shared,
    base: Url,
    source: Url,
    client: Client,
    cookie: String,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn tools_available() -> bool {
    let missing: Vec<_> = ["ffmpeg", "ffprobe"]
        .into_iter()
        .filter(|tool| {
            !Command::new(tool)
                .arg("-version")
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .collect();
    if missing.is_empty() {
        true
    } else {
        eprintln!(
            "SKIPPED hls_flow: {} unavailable; install FFmpeg and FFprobe to run this integration test",
            missing.join(", ")
        );
        false
    }
}

fn generate_hls(root: &Path) {
    fs::create_dir_all(root.join("variant")).unwrap();
    let output = Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "1.2",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "5",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
            "-b:a",
            "32k",
            "-hls_time",
            "0.5",
            "-hls_list_size",
            "0",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_filename",
        ])
        .arg(root.join("variant/segment-%03d.ts"))
        .arg(root.join("variant/list.m3u8"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "FFmpeg fixture generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(
        root.join("master.m3u8"),
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=200000,RESOLUTION=160x90\nvariant/list.m3u8?token=one%26two\n",
    )
    .unwrap();
}

async fn start_server(app: Router) -> (Url, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, task)
}

impl Fixture {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("source");
        generate_hls(&source_dir);
        let media_dir = temp.path().join("media");
        fs::create_dir_all(media_dir.join("downloads")).unwrap();
        let media_dir = media_dir.canonicalize().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            playback_grants: Default::default(),
            telegram_native: Default::default(),
            cloud_fixture: None,
            download_runtime: Default::default(),
            download_dir: media_dir.join("downloads"),
            media_dir,
            client: Client::new(),
            secure_cookie: false,
        });
        let (source, source_task) =
            start_server(Router::new().fallback_service(ServeDir::new(source_dir))).await;
        let (base, app_task) = start_server(router(state.clone())).await;
        let setup = client
            .post(base.join("api/auth/setup").unwrap())
            .header("x-polyview-request", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(json!({"username":"hlsviewer","password":"hls-integration-password"}).to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(setup.status(), StatusCode::OK);
        let cookie = setup.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let worker = network::start_worker(state.clone());
        Self {
            _temp: temp,
            state,
            base,
            source,
            client,
            cookie,
            tasks: vec![source_task, app_task, worker],
        }
    }

    async fn get(&self, url: Url) -> Response {
        self.client
            .get(url)
            .header(header::COOKIE, &self.cookie)
            .send()
            .await
            .unwrap()
    }

    async fn json(&self, path: &str) -> Value {
        let response = self.get(self.base.join(path).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
    }
}

fn first_media_uri(playlist: &str) -> &str {
    playlist
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .expect("playlist media URI")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_hls_proxy_download_library_progress_and_tvbox_flow() {
    if !tools_available() {
        return;
    }
    let fixture = Fixture::new().await;
    let source = fixture.source.join("master.m3u8").unwrap();

    // Follow the same nested manifest and segment URLs that a browser HLS player follows.
    let mut proxy = fixture.base.join("api/playback").unwrap();
    proxy.query_pairs_mut().append_pair("url", source.as_str());
    let response = fixture.get(proxy).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("mpegurl")
    );
    let master = response.text().await.unwrap();
    let variant_uri = first_media_uri(&master);
    assert!(variant_uri.starts_with("/api/playback?url="));
    let response = fixture.get(fixture.base.join(variant_uri).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let variant = response.text().await.unwrap();
    assert!(variant.starts_with("#EXTM3U\n"));
    let segment_uri = first_media_uri(&variant);
    assert!(segment_uri.starts_with("/api/playback?url="));
    let response = fixture.get(fixture.base.join(segment_uri).unwrap()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let segment = response.bytes().await.unwrap();
    assert!(!segment.is_empty());
    assert_eq!(segment[0], 0x47, "MPEG-TS packet sync byte");
    assert_eq!(segment.len() % 188, 0, "complete MPEG-TS packets");

    let response = fixture
        .client
        .post(fixture.base.join("api/downloads").unwrap())
        .header(header::COOKIE, &fixture.cookie)
        .header("x-polyview-request", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"title":"集成测试剧集 S01E01","url":source.as_str()}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let id = created["id"].as_i64().unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let jobs = fixture.json("api/downloads").await;
            let job = jobs
                .as_array()
                .unwrap()
                .iter()
                .find(|job| job["id"] == id)
                .unwrap();
            assert_ne!(job["status"], "failed", "HLS job failed: {}", job["error"]);
            if job["status"] == "completed" {
                break job.clone();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("HLS download completes within 20 seconds");
    let relative = completed["relative_path"].as_str().unwrap();
    assert!(relative.ends_with(".mp4"));
    let file = fixture.state.media_dir.join(relative);
    let bytes = fs::read(&file).unwrap();
    assert!(!bytes.is_empty());
    assert_eq!(
        completed["bytes_written"].as_u64().unwrap(),
        bytes.len() as u64
    );
    assert_eq!(completed["total_bytes"], completed["bytes_written"]);

    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_streams",
            "-show_format",
            "-of",
            "json",
        ])
        .arg(&file)
        .output()
        .unwrap();
    assert!(probe.status.success(), "FFprobe can read the completed MP4");
    let probe: Value = serde_json::from_slice(&probe.stdout).unwrap();
    let streams = probe["streams"].as_array().unwrap();
    assert!(
        streams
            .iter()
            .any(|stream| stream["codec_type"] == "video" && stream["codec_name"] == "h264")
    );
    assert!(
        streams
            .iter()
            .any(|stream| stream["codec_type"] == "audio" && stream["codec_name"] == "aac")
    );
    let duration: f64 = probe["format"]["duration"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1.0..3.0).contains(&duration));

    let media = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let library = fixture.json("api/library").await;
            if let Some(media) = library
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["relative_path"] == relative)
            {
                break media.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("completed HLS download is automatically scanned into the library");
    assert!(
        fs::read_dir(&fixture.state.download_dir)
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "part")),
        "completed download leaves no .part file"
    );
    let media_id = media["id"].as_i64().unwrap();
    let response = fixture
        .client
        .put(
            fixture
                .base
                .join(&format!("api/library/{media_id}/progress"))
                .unwrap(),
        )
        .header(header::COOKIE, &fixture.cookie)
        .header("x-polyview-request", "1")
        .header(header::CONTENT_TYPE, "application/json")
        .body(json!({"position":0.5,"duration":duration,"watched":false}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let library = fixture.json("api/library").await;
    let saved = library
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == media_id)
        .unwrap();
    assert_eq!(saved["position"], 0.5);
    let response = fixture
        .client
        .get(
            fixture
                .base
                .join(&format!("api/library/{media_id}/stream"))
                .unwrap(),
        )
        .header(header::COOKIE, &fixture.cookie)
        .header(header::RANGE, "bytes=0-31")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        format!("bytes 0-31/{}", bytes.len())
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), &bytes[..32]);

    // A TV client uses its feed token rather than the browser's login cookie.
    let feed = fixture.json("api/tvbox").await;
    let response = fixture
        .client
        .get(
            fixture
                .base
                .join(feed["catalog_path"].as_str().unwrap())
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let catalog: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    let item = &catalog["list"][0];
    let stream_url = item["vod_play_url"]
        .as_str()
        .unwrap()
        .split_once('$')
        .unwrap()
        .1;
    let response = fixture
        .client
        .get(stream_url)
        .header(header::RANGE, "bytes=0-31")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.bytes().await.unwrap().as_ref(), &bytes[..32]);
}
