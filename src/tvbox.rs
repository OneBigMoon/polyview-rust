use crate::{AppError, Shared, auth::AuthUser};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tower_http::services::ServeFile;
use url::Url;

const PAGE_SIZE: usize = 40;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tvbox_tokens (
            user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
            token TEXT NOT NULL UNIQUE
         );",
    )?;
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/tvbox", get(account_feed))
        .route("/api/tvbox/token", post(rotate_token))
        .route("/tvbox/{token}/config.json", get(config))
        .route("/tvbox/{token}/vod", get(catalog))
        .route("/tvbox/{token}/source/{source_id}", get(source_catalog))
        .route("/tvbox/{token}/live.m3u", get(live))
        .route("/tvbox/{token}/media/{id}", get(stream).head(stream))
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn feed_paths(token: &str) -> Value {
    json!({
        "token": token,
        "config_path": format!("/tvbox/{token}/config.json"),
        "catalog_path": format!("/tvbox/{token}/vod"),
        "live_path": format!("/tvbox/{token}/live.m3u"),
    })
}

async fn account_feed(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    let conn = state.conn()?;
    conn.execute(
        "INSERT INTO tvbox_tokens(user_id,token) VALUES(?1,?2) ON CONFLICT(user_id) DO NOTHING",
        params![user.id, new_token()],
    )?;
    let token: String = conn.query_row(
        "SELECT token FROM tvbox_tokens WHERE user_id=?1",
        [user.id],
        |row| row.get(0),
    )?;
    Ok(Json(feed_paths(&token)))
}

async fn rotate_token(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    let token = new_token();
    state.conn()?.execute(
        "INSERT INTO tvbox_tokens(user_id,token) VALUES(?1,?2)
         ON CONFLICT(user_id) DO UPDATE SET token=excluded.token",
        params![user.id, token],
    )?;
    Ok(Json(feed_paths(&token)))
}

fn require_token(state: &Shared, token: &str) -> Result<i64, AppError> {
    if token.len() != 64 || !token.bytes().all(|value| value.is_ascii_hexdigit()) {
        return Err(AppError::not_found("Feed not found"));
    }
    state
        .conn()?
        .query_row(
            "SELECT t.user_id FROM tvbox_tokens t JOIN users u ON u.id=t.user_id WHERE t.token=?1",
            [token],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Feed not found"))
}

fn validate_origin(raw: &str) -> Result<String, AppError> {
    let url = Url::parse(raw).map_err(|_| AppError::bad_request("Invalid public origin"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::bad_request(
            "Public origin must be an HTTP(S) origin without a path or credentials",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn origin(state: &Shared, headers: &HeaderMap) -> Result<String, AppError> {
    if let Ok(configured) = std::env::var("POLYVIEW_PUBLIC_URL")
        && !configured.is_empty()
    {
        return validate_origin(&configured);
    }
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .filter(|host| !host.is_empty())
        .ok_or_else(|| AppError::bad_request("Host header is required"))?;
    validate_origin(&format!(
        "{}://{host}",
        if state.secure_cookie { "https" } else { "http" }
    ))
}

async fn config(
    State(state): State<Shared>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_token(&state, &token)?;
    let origin = origin(&state, &headers)?;
    let mut sites = vec![json!({
        "key": "polyview_local", "name": "PolyView 媒体库", "type": 1,
        "api": format!("{origin}/tvbox/{token}/vod"),
        "searchable": 1, "quickSearch": 1, "filterable": 0
    })];
    sites.extend(
        crate::catalog::sources(&state)?
            .into_iter()
            .filter(|source| source.enabled)
            .map(|source| {
                json!({
                    "key": format!("polyview_source_{}", source.id), "name": source.name, "type": 1,
                    "api": format!("{origin}/tvbox/{token}/source/{}", source.id),
                    "searchable": 1, "quickSearch": 1, "filterable": 0
                })
            }),
    );
    Ok(Json(json!({
        "sites": sites,
        "lives": [{"name":"PolyView 直播", "type":0, "url":format!("{origin}/tvbox/{token}/live.m3u")}],
        "wallpaper": ""
    })))
}

#[derive(Default, Deserialize)]
struct CatalogQuery {
    ac: Option<String>,
    t: Option<String>,
    wd: Option<String>,
    ids: Option<String>,
    pg: Option<usize>,
}

/// TVBox clients default to the first play line, so put directly playable
/// lines ahead of share/parser pages that need a spider.
fn prefer_direct_play_lines(item: &Value) -> Value {
    let (Some(from), Some(urls)) = (
        item.get("vod_play_from").and_then(Value::as_str),
        item.get("vod_play_url").and_then(Value::as_str),
    ) else {
        return item.clone();
    };
    let froms = from.split("$$$").collect::<Vec<_>>();
    let urls = urls.split("$$$").collect::<Vec<_>>();
    if froms.len() < 2 || froms.len() != urls.len() {
        return item.clone();
    }
    let mut order = (0..froms.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| !group_is_direct(urls[index]));
    if order.iter().copied().eq(0..froms.len()) {
        return item.clone();
    }
    let mut ordered = item.clone();
    ordered["vod_play_from"] = json!(
        order
            .iter()
            .map(|&index| froms[index])
            .collect::<Vec<_>>()
            .join("$$$")
    );
    ordered["vod_play_url"] = json!(
        order
            .iter()
            .map(|&index| urls[index])
            .collect::<Vec<_>>()
            .join("$$$")
    );
    ordered
}

fn group_is_direct(playlist: &str) -> bool {
    let mut found = false;
    for episode in playlist.split('#') {
        let url = episode
            .split_once('$')
            .map_or(episode, |(_, url)| url)
            .trim();
        if url.is_empty() {
            continue;
        }
        found = true;
        if !crate::catalog::is_direct_media_url(url) {
            return false;
        }
    }
    found
}
async fn source_catalog(
    State(state): State<Shared>,
    Path((token, source_id)): Path<(String, i64)>,
    Query(query): Query<CatalogQuery>,
) -> Result<Json<Value>, AppError> {
    require_token(&state, &token)?;
    let page = query.pg.map(|page| page.to_string());
    let mut parameters = Vec::new();
    for (key, value) in [
        ("ac", query.ac.as_deref()),
        ("t", query.t.as_deref()),
        ("wd", query.wd.as_deref()),
        ("ids", query.ids.as_deref()),
        ("pg", page.as_deref()),
    ] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            parameters.push((key, value));
        }
    }
    let upstream = crate::catalog::cms_query(&state, source_id, &parameters).await?;
    let list = upstream
        .get("list")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError(
                StatusCode::BAD_GATEWAY,
                "Source JSON does not contain a CMS list".into(),
            )
        })?;
    if upstream
        .get("code")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
        .is_some_and(|code| code == 0)
    {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            "Source rejected the catalog query".into(),
        ));
    }
    let number = |key: &str, fallback: u64| {
        upstream
            .get(key)
            .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
            .unwrap_or(fallback)
    };
    Ok(Json(json!({
        "code": 1, "msg": "ok", "page": number("page", query.pg.unwrap_or(1) as u64),
        "pagecount": number("pagecount", u64::from(!list.is_empty())),
        "limit": number("limit", PAGE_SIZE as u64), "total": number("total", list.len() as u64),
        "class": upstream.get("class").and_then(Value::as_array).cloned().unwrap_or_default(),
        "list": list.iter().map(prefer_direct_play_lines).collect::<Vec<_>>(),
    })))
}

#[derive(Serialize)]
struct VodItem {
    vod_id: String,
    vod_name: String,
    vod_pic: String,
    type_id: String,
    type_name: &'static str,
    vod_remarks: String,
    vod_content: String,
    vod_play_from: &'static str,
    vod_play_url: String,
}

fn protocol_label(raw: &str) -> String {
    raw.replace(['$', '#', '\n', '\r'], " ")
}

fn local_items(state: &Shared, token: &str, origin: &str) -> Result<Vec<VodItem>, AppError> {
    let conn = state.conn()?;
    let mut query = conn.prepare(
        "SELECT m.id,m.title,m.kind,m.series_id,COALESCE(s.title,''),m.season,m.episode
         FROM library_media m LEFT JOIN library_series s ON s.id=m.series_id
         WHERE m.available=1 ORDER BY m.season,m.episode,m.title COLLATE NOCASE,m.id",
    )?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut items: BTreeMap<String, VodItem> = BTreeMap::new();
    for row in rows {
        let (id, title, kind, series_id, series, season, episode) = row?;
        let is_series = kind == "episode" && series_id.is_some();
        let key = if is_series {
            format!("s{}", series_id.unwrap_or_default())
        } else {
            format!("m{id}")
        };
        let item = items.entry(key.clone()).or_insert_with(|| VodItem {
            vod_id: key,
            vod_name: if is_series { series } else { title.clone() },
            vod_pic: String::new(),
            type_id: if is_series {
                "episode".into()
            } else {
                "movie".into()
            },
            type_name: if is_series { "剧集" } else { "电影" },
            vod_remarks: String::new(),
            vod_content: String::new(),
            vod_play_from: "PolyView",
            vod_play_url: String::new(),
        });
        if !item.vod_play_url.is_empty() {
            item.vod_play_url.push('#');
        }
        let label = if is_series {
            match (season, episode) {
                (Some(season), Some(episode)) => format!("S{season:02}E{episode:02}"),
                _ => protocol_label(&title),
            }
        } else {
            "播放".into()
        };
        item.vod_play_url
            .push_str(&format!("{label}${origin}/tvbox/{token}/media/{id}"));
    }
    let mut items: Vec<_> = items.into_values().collect();
    items.sort_by(|left, right| {
        left.vod_name
            .to_lowercase()
            .cmp(&right.vod_name.to_lowercase())
    });
    Ok(items)
}

async fn catalog(
    State(state): State<Shared>,
    Path(token): Path<String>,
    Query(query): Query<CatalogQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    require_token(&state, &token)?;
    if query
        .ac
        .as_deref()
        .is_some_and(|action| !matches!(action, "list" | "detail"))
    {
        return Err(AppError::bad_request("Unsupported catalog action"));
    }
    if query
        .wd
        .as_ref()
        .is_some_and(|word| word.chars().count() > 200)
        || query.ids.as_ref().is_some_and(|ids| ids.len() > 2000)
    {
        return Err(AppError::bad_request("Catalog query is too long"));
    }
    let origin = origin(&state, &headers)?;
    let word = query.wd.unwrap_or_default().to_lowercase();
    let ids: Vec<_> = query
        .ids
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .filter(|id| !id.is_empty())
        .collect();
    let items: Vec<_> = local_items(&state, &token, &origin)?
        .into_iter()
        .filter(|item| {
            (query
                .t
                .as_deref()
                .is_none_or(|kind| kind.is_empty() || kind == item.type_id))
                && (word.is_empty() || item.vod_name.to_lowercase().contains(&word))
                && (ids.is_empty() || ids.contains(&item.vod_id.as_str()))
        })
        .collect();
    let total = items.len();
    let page = query.pg.unwrap_or(1).max(1);
    let pagecount = total.div_ceil(PAGE_SIZE).max(1);
    let skip = page.saturating_sub(1).saturating_mul(PAGE_SIZE);
    let list: Vec<_> = items.into_iter().skip(skip).take(PAGE_SIZE).collect();
    Ok(Json(json!({
        "code":1,"msg":"ok","page":page,"pagecount":pagecount,"limit":PAGE_SIZE,"total":total,
        "class":[{"type_id":"movie","type_name":"电影"},{"type_id":"episode","type_name":"剧集"}],
        "list":list
    })))
}

fn m3u_attribute(raw: &str) -> String {
    raw.replace(['\r', '\n'], " ").replace('"', "'")
}

async fn live(
    State(state): State<Shared>,
    Path(token): Path<String>,
) -> Result<Response, AppError> {
    require_token(&state, &token)?;
    let conn = state.conn()?;
    let mut query =
        conn.prepare("SELECT name,group_name,logo,url FROM channels ORDER BY group_name,name,id")?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut playlist = String::from("#EXTM3U\n");
    for row in rows {
        let (name, group, logo, url) = row?;
        if !Url::parse(&url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
        {
            continue;
        }
        playlist.push_str(&format!(
            "#EXTINF:-1 tvg-logo=\"{}\" group-title=\"{}\",{}\n{}\n",
            m3u_attribute(&logo),
            m3u_attribute(&group),
            m3u_attribute(&name),
            url.replace(['\r', '\n'], "")
        ));
    }
    Ok((
        [(
            header::CONTENT_TYPE,
            "application/vnd.apple.mpegurl; charset=utf-8",
        )],
        playlist,
    )
        .into_response())
}

async fn stream(
    State(state): State<Shared>,
    Path((token, id)): Path<(String, i64)>,
    request: Request,
) -> Result<Response, AppError> {
    require_token(&state, &token)?;
    let (root, relative): (String, String) = state
        .conn()?
        .query_row(
            "SELECT root,relative_path FROM library_media WHERE id=?1 AND available=1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::not_found("Media not found"))?;
    let base = match root.as_str() {
        "media" => &state.media_dir,
        "download" => &state.download_dir,
        _ => return Err(AppError::not_found("Media file is unavailable")),
    };
    let base = tokio::fs::canonicalize(base)
        .await
        .map_err(|_| AppError::not_found("Media directory is unavailable"))?;
    let path = tokio::fs::canonicalize(base.join(relative))
        .await
        .map_err(|_| AppError::not_found("Media file is unavailable"))?;
    if !path.starts_with(base) {
        return Err(AppError::not_found("Media file is unavailable"));
    }
    Ok(ServeFile::new(path)
        .try_call(request)
        .await?
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, auth, catalog, library, network};
    use axum::{
        body::{Body, to_bytes},
        http::Request as HttpRequest,
    };
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn fixture() -> (TempDir, Shared) {
        let temp = tempfile::tempdir().unwrap();
        let media_dir = temp.path().join("media");
        std::fs::create_dir_all(media_dir.join("downloads")).unwrap();
        let media_dir = media_dir.canonicalize().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        auth::init_schema(&conn).unwrap();
        library::init_schema(&conn).unwrap();
        network::init_schema(&conn).unwrap();
        catalog::init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
        conn.execute("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(1,'viewer','unused','user',0)",[]).unwrap();
        let state = Arc::new(AppState {
            db: Mutex::new(conn),
            download_dir: media_dir.join("downloads"),
            media_dir,
            client: reqwest::Client::new(),
            secure_cookie: false,
        });
        (temp, state)
    }
    fn user() -> AuthUser {
        AuthUser {
            id: 1,
            username: "viewer".into(),
            role: "user".into(),
        }
    }
    async fn request(
        app: Router,
        uri: &str,
        method: &str,
        range: Option<&str>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = HttpRequest::builder()
            .uri(uri)
            .method(method)
            .header("host", "nas.local:8080");
        if let Some(range) = range {
            request = request.header("range", range);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        (
            parts.status,
            parts.headers,
            to_bytes(body, 1024 * 1024).await.unwrap().to_vec(),
        )
    }

    #[tokio::test]
    async fn tokens_are_per_user_revocable_and_deleted_with_account() {
        let (_temp, state) = fixture();
        let Json(first) = account_feed(State(state.clone()), user()).await.unwrap();
        let Json(same) = account_feed(State(state.clone()), user()).await.unwrap();
        assert_eq!(first, same);
        let token = first["token"].as_str().unwrap();
        assert_eq!(require_token(&state, token).unwrap(), 1);
        state.conn().unwrap().execute("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(2,'second','unused','user',0)", []).unwrap();
        let Json(second) = account_feed(
            State(state.clone()),
            AuthUser {
                id: 2,
                username: "second".into(),
                role: "user".into(),
            },
        )
        .await
        .unwrap();
        let second_token = second["token"].as_str().unwrap();
        assert_ne!(token, second_token);
        assert_eq!(require_token(&state, second_token).unwrap(), 2);
        assert!(require_token(&state, "invalid").is_err());
        let Json(rotated) = rotate_token(State(state.clone()), user()).await.unwrap();
        assert_ne!(first["token"], rotated["token"]);
        assert!(require_token(&state, token).is_err());
        let token = rotated["token"].as_str().unwrap();
        state
            .conn()
            .unwrap()
            .execute("DELETE FROM users WHERE id=1", [])
            .unwrap();
        assert!(require_token(&state, token).is_err());
        assert_eq!(require_token(&state, second_token).unwrap(), 2);
    }

    #[test]
    fn origin_and_playlist_labels_reject_or_remove_delimiters() {
        assert_eq!(
            validate_origin("https://nas.example:443/").unwrap(),
            "https://nas.example"
        );
        for origin in [
            "javascript:alert(1)",
            "https://user:pass@example.org",
            "http://example.org/path",
            "http://example.org?token=a",
            "http://example.org/#a",
        ] {
            assert!(validate_origin(origin).is_err(), "{origin}");
        }
        assert_eq!(protocol_label("A$B#C\nD"), "A B C D");
        assert_eq!(m3u_attribute("A\"B\nC"), "A'B C");
    }

    #[tokio::test]
    async fn config_catalog_and_token_stream_support_series_search_range_head() {
        let (_temp, state) = fixture();
        std::fs::write(state.media_dir.join("电影<script>.mp4"), b"0123456789").unwrap();
        std::fs::write(state.media_dir.join("故事.S01E01.mp4"), b"episode-one").unwrap();
        std::fs::write(state.media_dir.join("故事.S01E02.mp4"), b"episode-two").unwrap();
        library::scan_library(&state).unwrap();
        let Json(feed) = account_feed(State(state.clone()), user()).await.unwrap();
        let token = feed["token"].as_str().unwrap();
        let app = router().with_state(state.clone());
        assert_eq!(
            request(app.clone(), "/tvbox/invalid/config.json", "GET", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let (status, _, body) = request(
            app.clone(),
            &format!("/tvbox/{token}/config.json"),
            "GET",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let config: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            config["sites"][0]["api"],
            format!("http://nas.local:8080/tvbox/{token}/vod")
        );
        let (_, _, body) = request(
            app.clone(),
            &format!("/tvbox/{token}/vod?ac=detail"),
            "GET",
            None,
        )
        .await;
        let catalog: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(catalog["total"], 2);
        let series = catalog["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type_id"] == "episode")
            .unwrap();
        assert!(
            series["vod_play_url"]
                .as_str()
                .unwrap()
                .contains("S01E01$http://nas.local:8080/")
        );
        assert!(
            series["vod_play_url"]
                .as_str()
                .unwrap()
                .contains("#S01E02$")
        );
        let movie = catalog["list"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type_id"] == "movie")
            .unwrap();
        assert_eq!(movie["vod_name"], "电影<script>");
        let media_url = movie["vod_play_url"]
            .as_str()
            .unwrap()
            .split_once('$')
            .unwrap()
            .1;
        let media_url = Url::parse(media_url).unwrap();
        let (status, headers, body) =
            request(app.clone(), media_url.path(), "GET", Some("bytes=2-5")).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(headers[header::CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(body, b"2345");
        let (status, headers, body) = request(app.clone(), media_url.path(), "HEAD", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_LENGTH], "10");
        assert!(body.is_empty());
        let (_, _, body) = request(
            app.clone(),
            &format!("/tvbox/{token}/vod?wd=%E6%95%85%E4%BA%8B&t=episode"),
            "GET",
            None,
        )
        .await;
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap()["total"], 1);
        let Json(_) = rotate_token(State(state.clone()), user()).await.unwrap();
        assert_eq!(
            request(app, media_url.path(), "GET", None).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn live_playlist_is_valid_and_cannot_inject_metadata_lines() {
        let (_temp, state) = fixture();
        state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO channels(name,group_name,logo,url) VALUES(?1,?2,'',?3)",
                params![
                    "频道\n#EXTINF:evil",
                    "新闻\"台",
                    "http://example.org/live.m3u8"
                ],
            )
            .unwrap();
        let Json(feed) = account_feed(State(state.clone()), user()).await.unwrap();
        let app = router().with_state(state);
        let (status, headers, body) =
            request(app, feed["live_path"].as_str().unwrap(), "GET", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/vnd.apple.mpegurl; charset=utf-8"
        );
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.contains("group-title=\"新闻'台\""));
    }

    #[tokio::test]
    async fn online_feed_proxies_enabled_sources_and_preserves_queries() {
        use std::collections::HashMap;
        let (_temp, state) = fixture();
        let upstream = Router::new().route("/cms", get(|Query(query): Query<HashMap<String, String>>| async move {
            assert_eq!(query.get("secret").map(String::as_str), Some("source-private"));
            assert_eq!(query.get("ac").map(String::as_str), Some("detail"));
            assert_eq!(query.get("wd").map(String::as_str), Some("故事 & test"));
            assert_eq!(query.get("pg").map(String::as_str), Some("2"));
            Json(json!({"code":1,"page":"2","pagecount":"3","limit":"20","total":"44","class":[{"type_id":"1","type_name":"电影"}],"list":[{"vod_id":7,"vod_name":"故事","vod_play_from":"HLS","vod_play_url":"第1集$https://example.org/video.m3u8"}]}))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        state.conn().unwrap().execute("INSERT INTO catalog_sources(id,name,url,enabled) VALUES(1,'Online',?1,1),(2,'Disabled','https://example.org/disabled',0)", [format!("http://{address}/cms?secret=source-private")]).unwrap();
        let Json(feed) = account_feed(State(state.clone()), user()).await.unwrap();
        let token = feed["token"].as_str().unwrap();
        let app = router().with_state(state.clone());
        let (_, _, body) = request(
            app.clone(),
            &format!("/tvbox/{token}/config.json"),
            "GET",
            None,
        )
        .await;
        let config: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(config["sites"].as_array().unwrap().len(), 2);
        assert_eq!(
            config["sites"][1]["api"],
            format!("http://nas.local:8080/tvbox/{token}/source/1")
        );
        assert!(!String::from_utf8(body).unwrap().contains("source-private"));
        let (status, _, body) = request(
            app.clone(),
            &format!("/tvbox/{token}/source/1?ac=detail&wd=%E6%95%85%E4%BA%8B%20%26%20test&pg=2"),
            "GET",
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let page: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(page["page"], 2);
        assert_eq!(page["pagecount"], 3);
        assert_eq!(page["total"], 44);
        assert_eq!(
            page["list"][0]["vod_play_url"],
            "第1集$https://example.org/video.m3u8"
        );
        assert_eq!(
            request(
                app.clone(),
                &format!("/tvbox/{token}/source/2"),
                "GET",
                None
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(app.clone(), "/tvbox/invalid/source/1", "GET", None)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                app,
                &format!("/tvbox/{token}/source/1?ac=delete"),
                "GET",
                None
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        task.abort();
    }

    #[tokio::test]
    async fn feed_management_requires_session_and_request_header() {
        let (_temp, state) = fixture();
        state
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO sessions(token,user_id,expires_at) VALUES('session-token',1,?1)",
                [crate::now() + 3600],
            )
            .unwrap();
        let app = crate::router(state);
        assert_eq!(
            request(app.clone(), "/api/tvbox", "GET", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/api/tvbox")
                    .header("cookie", "polyview_session=session-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/tvbox/token")
                    .header("cookie", "polyview_session=session-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .oneshot(
                HttpRequest::builder()
                    .method("POST")
                    .uri("/api/tvbox/token")
                    .header("cookie", "polyview_session=session-token")
                    .header("x-polyview-request", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn direct_play_lines_are_ordered_before_parser_pages() {
        let item = json!({
            "vod_play_from": "share$$$ffm3u8",
            "vod_play_url": "第1集$https://example.org/share/a$$$第1集$https://example.org/1.m3u8"
        });
        let ordered = prefer_direct_play_lines(&item);
        assert_eq!(ordered["vod_play_from"], "ffm3u8$$$share");
        assert_eq!(
            ordered["vod_play_url"],
            "第1集$https://example.org/1.m3u8$$$第1集$https://example.org/share/a"
        );
    }
}
