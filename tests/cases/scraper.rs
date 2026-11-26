use super::*;
use axum::{Json, extract::Request as HttpRequest, response::IntoResponse};
use base64::{Engine, engine::general_purpose::STANDARD};
use polyview::scraper::{Lookup, scrape_media};
use std::sync::atomic::{AtomicUsize, Ordering};

const SETTINGS: &str = "/api/settings/scraper";
const KEY: &str = "tmdb-private-token";
const TITLE: &str = "故事 <一> & '夜'";
fn png() -> Vec<u8> {
    STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+yKqkAAAAASUVORK5CYII=").unwrap()
}

struct Server {
    url: String,
    calls: Arc<Mutex<Vec<String>>>,
    revision: Arc<AtomicUsize>,
    worker: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let revision = Arc::new(AtomicUsize::new(0));
        let log = calls.clone();
        let rev = revision.clone();
        let routes=Router::new().fallback(move |req:HttpRequest| {
            let log=log.clone();let rev=rev.clone();
            async move {
                let path=req.uri().path().to_owned();
                let uri=req.uri().to_string();
                let auth=req.headers().get(header::AUTHORIZATION).and_then(|v|v.to_str().ok()).unwrap_or_default().to_owned();
                log.lock().unwrap().push(format!("{} {uri} {auth}",req.method()));
                let reply=match path.as_str() {
                    "/tmdb/search/movie"|"/tmdb/search/tv" => json!({"results":[{"id":42}]}),
                    "/tmdb/movie/42"|"/tmdb/tv/42"=>json!({"id":42,"title":TITLE,"name":TITLE,"original_title":"Original","overview":format!("Plot <{}> & more",rev.load(Ordering::Relaxed)),"release_date":"2024-09-09","vote_average":8.6,"genres":[{"name":"Drama"}],"poster_path":"/poster.png","backdrop_path":"/back.png","seasons":[{"season_number":1},{"season_number":2}]}),
                    "/tmdb/movie/42/credits"|"/tmdb/tv/42/credits"=>json!({"cast":[{"name":"Actor & Co.","character":"Hero"}],"crew":[{"name":"Director <A>","job":"Director"}]}),
                    "/tmdb/tv/42/season/1"|"/tmdb/tv/42/season/2"=>json!({"name":"季","episodes":[{"episode_number":2,"name":"Two & More","overview":"<Episode>"}]}),
                    "/img/poster.png"|"/img/back.png" =>return ([(header::CONTENT_TYPE,"image/png")],png()).into_response(),
                    "/unavailable/search/tv"|"/unavailable/search/movie"=>return (StatusCode::BAD_GATEWAY,format!("secret {KEY}")).into_response(),
                    "/redirect/search/movie"=>return axum::response::Redirect::temporary("/trap").into_response(),
                    "/bgm/v0/search/subjects"=>{
                        let bytes=to_bytes(req.into_body(),4096).await.unwrap();
                        let body:Value=serde_json::from_slice(&bytes).unwrap();
                        if body["keyword"]!="Story" {return StatusCode::BAD_REQUEST.into_response();}
                        json!({"data":[{"id":9,"name_cn":"Story"}]})
                    },
                    "/bgm/v0/subjects/9"=>json!({"id":9,"name":"物語","name_cn":"故事","summary":"Synopsis","date":"2024-03-01","rating":{"score":8},"tags":[{"name":"日常"}],"images":{"large":"/img/poster.png"}}),
                    "/bgm/v0/subjects/9/characters"=>json!([{"name":"Role","actors":[{"name":"Voice"}]}]),
                    "/bgm/v0/subjects/9/persons"=>json!([{"name":"Crew","relation":"导演"}]),
                    "/bgm/v0/episodes"=>json!({"total":1,"data":[{"ep":2,"name_cn":"第二话","desc":"第二话剧情"}]}),
                    "/douban/j/subject_suggest"=>json!([{"id":"17","type":"movie","title":"Story","sub_title":"Original","year":"2024"}]),
                    "/douban/subject/17/"=>return axum::response::Html(r#"<script type="application/ld+json">{"name":"Story","datePublished":"2024-06-01","genre":["Drama"],"actor":[{"name":"Actor"}],"director":[{"name":"Director"}],"aggregateRating":{"ratingValue":"7.9"},"image":"/img/poster.png"}</script><span property="v:summary">Story &amp; more</span>"#).into_response(),
                    _=>return StatusCode::NOT_FOUND.into_response(),
                };
                Json(reply).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let worker = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        Self {
            url,
            calls,
            revision,
            worker,
        }
    }
    async fn configure(&self, app: &TestApp, admin: &str, tmdb: &str) {
        let response=app.request("PUT",SETTINGS,Some(admin),Some(json!({"tmdb_key":KEY,"tmdb_url":format!("{}{tmdb}",self.url),"image_url":format!("{}/img",self.url),"bangumi_url":format!("{}/bgm",self.url),"douban_url":format!("{}/douban",self.url)}))).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.json()["has_tmdb_key"], true);
        assert!(!String::from_utf8_lossy(&response.body).contains(KEY));
    }
}

fn media(app: &TestApp, path: &str, tv: bool) -> i64 {
    let path = app.state.media_dir.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"media").unwrap();
    let conn = app.state.conn().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO library_series(id,title) VALUES(91,'Story')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO library_media(root,relative_path,title,year,kind,series_id,season,episode,size) VALUES('media',?,'Story','2024',?,?,?,?,5)",rusqlite::params![path.strip_prefix(&app.state.media_dir).unwrap().to_str().unwrap(),if tv {"episode"} else {"movie"},tv.then_some(91),tv.then_some(1),tv.then_some(2)]).unwrap();
    conn.last_insert_rowid()
}

#[tokio::test]
async fn tmdb_movie_art_nfo_cache_force_and_credentials() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let server = Server::new().await;
    server.configure(&app, &admin, "/tmdb").await;
    let id = media(&app, "Movies/Story.mkv", false);
    let m = scrape_media(&app.state, id, Lookup::default(), false)
        .await
        .unwrap();
    assert_eq!(
        (
            m.provider.as_str(),
            m.title.as_str(),
            m.original_title.as_str(),
            m.year.as_str(),
            m.rating
        ),
        ("tmdb", TITLE, "Original", "2024", Some(8.6))
    );
    assert_eq!(m.cast[0].role, "Hero");
    assert_eq!(m.crew[0].role, "Director");
    let folder = app.state.media_dir.join("Movies");
    let xml = fs::read_to_string(folder.join("Story.nfo")).unwrap();
    let doc = roxmltree::Document::parse(&xml).unwrap();
    assert_eq!(doc.root_element().tag_name().name(), "movie");
    assert_eq!(
        doc.descendants()
            .find(|n| n.has_tag_name("title"))
            .unwrap()
            .text(),
        Some(TITLE)
    );
    assert_eq!(fs::read(folder.join("Story-poster.png")).unwrap(), png());
    assert_eq!(fs::read(folder.join("Story-fanart.png")).unwrap(), png());
    let image = app.request("GET", &m.cover, Some(&viewer), None).await;
    assert_eq!(image.headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(image.body, png());
    assert_eq!(
        app.request("GET", &m.cover, None, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let detail = app
        .request(
            "GET",
            &format!("/api/library/{id}/metadata"),
            Some(&viewer),
            None,
        )
        .await
        .json();
    assert_eq!(detail["genres"], json!(["Drama"]));
    assert!(!detail.to_string().contains(KEY));
    assert!(!detail.to_string().contains(&server.url));
    let requests = server.calls.lock().unwrap().clone();
    assert!(
        requests.iter().any(|s| s.contains("/tmdb/search/movie?")
            && s.contains("year=2024")
            && s.contains(KEY))
    );
    assert!(
        requests
            .iter()
            .filter(|s| s.contains("/img/"))
            .all(|s| !s.contains(KEY))
    );
    server.revision.store(1, Ordering::Relaxed);
    assert_eq!(
        scrape_media(&app.state, id, Lookup::default(), false)
            .await
            .unwrap()
            .description,
        "Plot <0> & more"
    );
    assert_eq!(server.calls.lock().unwrap().len(), requests.len());
    let direct = Lookup {
        tmdb_id: Some(42),
        ..Default::default()
    };
    assert_eq!(
        scrape_media(&app.state, id, direct, true)
            .await
            .unwrap()
            .description,
        "Plot <1> & more"
    );
    assert!(
        fs::read_to_string(folder.join("Story.nfo"))
            .unwrap()
            .contains("Plot &lt;1&gt;")
    );
    assert_eq!(
        server
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("/search/movie"))
            .count(),
        1
    );
    fs::write(folder.join("Story.nfo"), b"my edits").unwrap();
    server.revision.store(2, Ordering::Relaxed);
    assert_eq!(
        scrape_media(&app.state, id, Lookup::default(), true)
            .await
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(fs::read(folder.join("Story.nfo")).unwrap(), b"my edits");
    assert_eq!(
        scrape_media(&app.state, id, Lookup::default(), false)
            .await
            .unwrap()
            .description,
        "Plot <1> & more"
    );
}

#[tokio::test]
async fn tv_seasons_cast_episode_sidecars_and_public_fallbacks() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let server = Server::new().await;
    server.configure(&app, &admin, "/tmdb").await;
    let episode = media(&app, "TV/Story/Season 01/Story S01E02.mkv", true);
    let series = app.state.media_dir.join("TV/Story");
    let m = scrape_media(&app.state, episode, Lookup::default(), true)
        .await
        .unwrap();
    assert_eq!(
        m.seasons.iter().map(|s| s.number).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let show = fs::read_to_string(series.join("tvshow.nfo")).unwrap();
    assert_eq!(
        roxmltree::Document::parse(&show)
            .unwrap()
            .root_element()
            .tag_name()
            .name(),
        "tvshow"
    );
    let episode_nfo = fs::read_to_string(series.join("Season 01/Story S01E02.nfo")).unwrap();
    let doc = roxmltree::Document::parse(&episode_nfo).unwrap();
    assert_eq!(doc.root_element().tag_name().name(), "episodedetails");
    for (tag, value) in [
        ("title", "Two & More"),
        ("season", "1"),
        ("episode", "2"),
        ("plot", "<Episode>"),
        ("thumb", "../poster.png"),
    ] {
        assert_eq!(
            doc.descendants()
                .find(|n| n.has_tag_name(tag))
                .unwrap()
                .text(),
            Some(value)
        );
    }
    server.configure(&app, &admin, "/unavailable").await;
    let anime = scrape_media(&app.state, episode, Lookup::default(), true)
        .await
        .unwrap();
    assert_eq!(anime.provider, "bangumi");
    assert_eq!(anime.cast[0].name, "Voice");
    assert_eq!(anime.seasons[0].episodes[0].title, "第二话");
    let movie = media(&app, "Movies/second.avi", false);
    let fallback = scrape_media(&app.state, movie, Lookup::default(), true)
        .await
        .unwrap();
    assert_eq!(fallback.provider, "douban");
    assert_eq!(fallback.rating, Some(7.9));
    assert_eq!(fallback.description, "Story & more");
    assert_eq!(fallback.crew[0].name, "Director");
    assert!(
        server
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.contains("/bgm/") || s.contains("/douban/"))
            .all(|s| !s.contains(KEY))
    );
    let requests = server.calls.lock().unwrap().len();
    fs::create_dir(app.state.media_dir.join("Organized")).unwrap();
    fs::rename(
        app.state.media_dir.join("Movies/second.avi"),
        app.state.media_dir.join("Organized/Story.avi"),
    )
    .unwrap();
    app.state
        .conn()
        .unwrap()
        .execute(
            "UPDATE library_media SET relative_path='Organized/Story.avi' WHERE id=?",
            [movie],
        )
        .unwrap();
    polyview::scraper::write_cached_sidecars(&app.state, movie)
        .await
        .unwrap();
    assert_eq!(
        fs::read(app.state.media_dir.join("Organized/Story-poster.png")).unwrap(),
        png()
    );
    assert!(
        fs::read_to_string(app.state.media_dir.join("Organized/Story.nfo"))
            .unwrap()
            .contains("Story &amp; more")
    );
    assert_eq!(server.calls.lock().unwrap().len(), requests);
}

#[tokio::test]
async fn protected_config_restore_redirect_and_unmanaged_files() {
    let app = TestApp::new();
    let admin = app.setup().await;
    app.create_viewer(&admin).await;
    let viewer = app.login("viewer", PASSWORD).await.cookie();
    let server = Server::new().await;
    server.configure(&app, &admin, "/redirect").await;
    for (method, body) in [("GET", None), ("PUT", Some(json!({"enabled":true})))] {
        assert_eq!(
            app.request(method, SETTINGS, Some(&viewer), body)
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request_with_headers(
            "PUT",
            SETTINGS,
            Some(&admin),
            Some(json!({"enabled":true})),
            &[]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    let id = media(&app, "Movies/handmade.mkv", false);
    fs::write(app.state.media_dir.join("Movies/handmade.nfo"), b"mine").unwrap();
    assert_eq!(
        scrape_media(&app.state, id, Lookup::default(), true)
            .await
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        fs::read(app.state.media_dir.join("Movies/handmade.nfo")).unwrap(),
        b"mine"
    );
    assert!(
        !app.state
            .media_dir
            .join("Movies/handmade-poster.png")
            .exists()
    );
    assert!(
        server
            .calls
            .lock()
            .unwrap()
            .iter()
            .all(|s| !s.contains("/trap"))
    );
    let saved = app
        .request(
            "PUT",
            SETTINGS,
            Some(&admin),
            Some(json!({"proxy":"http://alice:secret@localhost:1234"})),
        )
        .await
        .json();
    assert_eq!(saved["has_proxy"], true);
    assert!(saved.get("proxy").is_none());
    assert!(!saved.to_string().contains("secret"));
    let backup = app
        .request("GET", "/api/settings/export", Some(&admin), None)
        .await
        .json();
    assert_eq!(backup["automation"]["scraper"]["tmdb_key"], KEY);
    let dest = TestApp::new();
    let dest_admin = dest.setup().await;
    assert_eq!(
        dest.request(
            "POST",
            "/api/settings/import",
            Some(&dest_admin),
            Some(backup.clone())
        )
        .await
        .status,
        StatusCode::OK
    );
    let mut invalid = backup.clone();
    invalid["automation"]["scraper"]["tmdb_url"] = json!("file:///bad");
    invalid["epg_config"]["url"] = json!("https://bad.example/changed.xml");
    assert_eq!(
        dest.request(
            "POST",
            "/api/settings/import",
            Some(&dest_admin),
            Some(invalid)
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let restored = dest
        .request("GET", "/api/settings/export", Some(&dest_admin), None)
        .await
        .json();
    assert_eq!(
        restored["automation"]["scraper"],
        backup["automation"]["scraper"]
    );
    assert_eq!(restored["epg_config"], backup["epg_config"]);
    let moved = app
        .request(
            "PUT",
            SETTINGS,
            Some(&admin),
            Some(json!({"tmdb_url":format!("{}/other",server.url)})),
        )
        .await
        .json();
    assert_eq!(moved["has_tmdb_key"], false);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_media_directory_cannot_redirect_sidecars() {
    let app = TestApp::new();
    let admin = app.setup().await;
    let server = Server::new().await;
    server.configure(&app, &admin, "/tmdb").await;
    let id = media(&app, "media-link/file.mkv", false);
    let folder = app.state.media_dir.join("media-link");
    let other = app._temp.path().join("outside");
    fs::rename(&folder, &other).unwrap();
    std::os::unix::fs::symlink(&other, &folder).unwrap();
    assert_eq!(
        scrape_media(&app.state, id, Lookup::default(), true)
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(fs::read_dir(other).unwrap().count(), 1);
}
