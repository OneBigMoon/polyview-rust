use super::{
    AppError, AuthUser, HeaderMap, REQUEST_TIMEOUT, Response, Shared, State, Url, header, http_url,
    play_settings, playlist_url, relay_response, rewrite_links, source_error,
};
use axum::{Router, extract::Path, routing::get};
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use uuid::Uuid;

const GRANT_ROUTE: &str = "/api/playback/grants";
const GRANT_LIFETIME: Duration = Duration::from_secs(6 * 3600);
const MAX_SESSIONS: usize = 128;
const MAX_USER_SESSIONS: usize = 16;
const MAX_PLAYLIST_URL_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESOURCES: usize = 32_768;

pub(crate) fn no_redirect_client() -> Result<&'static reqwest::Client, AppError> {
    static CLIENT: LazyLock<Result<reqwest::Client, reqwest::Error>> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent("Mozilla/5.0")
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .connect_timeout(Duration::from_secs(10))
            .build()
    });
    CLIENT
        .as_ref()
        .map_err(|_| AppError::internal("Could not initialize protected HTTP client"))
}

pub(super) fn same_origin(first: &Url, second: &Url) -> bool {
    first.scheme() == second.scheme()
        && first.host_str() == second.host_str()
        && first.port_or_known_default() == second.port_or_known_default()
}

/// Follow redirects ourselves because custom media credentials can be carried in any header.
/// Once a request leaves the original origin, even a redirect back cannot restore its credentials.
pub(crate) async fn fetch_media(
    mut url: Url,
    private: Option<&HeaderMap>,
    range: &HeaderMap,
) -> Result<(reqwest::Response, bool), AppError> {
    let client = no_redirect_client()?;
    let mut trusted = private.is_some();
    let follow = async {
        for _ in 0..=5 {
            let mut request = client.get(url.clone());
            if trusted && let Some(headers) = private {
                request = request.headers(headers.clone());
            }
            request = request.header(header::ACCEPT_ENCODING, "identity");
            for name in [header::RANGE, header::IF_RANGE] {
                if let Some(value) = range.get(&name) {
                    request = request.header(name, value);
                }
            }
            let response = request
                .send()
                .await
                .map_err(|_| source_error("Could not connect to the media source"))?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok((response, trusted));
            }
            let next = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|target| playlist_url(&url, target).ok())
                .ok_or_else(|| source_error("Invalid media redirect"))?;
            trusted &= same_origin(&url, &next);
            url = next;
        }
        Err(source_error("Too many media redirects"))
    };
    tokio::time::timeout(REQUEST_TIMEOUT, follow)
        .await
        .map_err(|_| source_error("Media source did not respond within 30 seconds"))?
}

#[derive(Hash, Eq, PartialEq)]
struct Resource {
    url: Url,
    trusted: bool,
}

#[derive(Default)]
struct Resources {
    by_index: Vec<Arc<Resource>>,
    by_url: HashMap<Arc<Resource>, usize>,
    url_bytes: usize,
}

struct Session {
    token: Uuid,
    route: Cow<'static, str>,
    live_root: Option<Url>,
    headers: HeaderMap,
    resources: Mutex<Resources>,
}

impl Session {
    fn empty(headers: HeaderMap, route: Cow<'static, str>, live_root: Option<Url>) -> Arc<Self> {
        Arc::new(Self {
            token: Uuid::new_v4(),
            route,
            live_root,
            headers,
            resources: Mutex::default(),
        })
    }

    fn new(url: Url, headers: HeaderMap) -> Result<(Arc<Self>, String), AppError> {
        let session = Self::empty(headers, GRANT_ROUTE.into(), None);
        let path = session.register(url, true)?;
        Ok((session, path))
    }

    fn register(&self, url: Url, trusted: bool) -> Result<String, AppError> {
        let resource = Resource { url, trusted };
        let mut resources = self.resources.lock().unwrap();
        let index = if let Some(index) = resources.by_url.get(&resource) {
            *index
        } else {
            if resources.by_index.len() >= MAX_RESOURCES
                || resources.url_bytes + resource.url.as_str().len() > MAX_PLAYLIST_URL_BYTES
            {
                return Err(source_error("Playlist contains too many media resources"));
            }
            let index = resources.by_index.len();
            resources.url_bytes += resource.url.as_str().len();
            let resource = Arc::new(resource);
            resources.by_url.insert(resource.clone(), index);
            resources.by_index.push(resource);
            index
        };
        // Downloaders use the suffix to select a demuxer. Never expose a source filename or query.
        let ext = resources.by_index[index]
            .url
            .path()
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, ext)| ext)
            .filter(|ext| {
                (1..=6).contains(&ext.len()) && ext.bytes().all(|ch| ch.is_ascii_alphanumeric())
            })
            .unwrap_or("ts");
        Ok(format!(
            "{}/{}-{index}.{ext}",
            self.route,
            self.token.simple()
        ))
    }

    async fn forward(
        &self,
        index: usize,
        mut range: HeaderMap,
        ad_block: bool,
    ) -> Result<Response, AppError> {
        let resource = self
            .resources
            .lock()
            .unwrap()
            .by_index
            .get(index)
            .cloned()
            .ok_or_else(missing_grant)?;
        // Tooling may send Range even for a manifest. Rewriting requires its complete contents.
        let requested_playlist = super::playlist_path(&resource.url);
        if requested_playlist {
            range.remove(header::RANGE);
            range.remove(header::IF_RANGE);
        }
        let (response, trusted) = fetch_media(
            resource.url.clone(),
            resource.trusted.then_some(&self.headers),
            &range,
        )
        .await?;
        relay_response(response, requested_playlist, |bytes, base| {
            self.rewrite(bytes, base, trusted, ad_block)
        })
        .await
    }

    fn rewrite(
        &self,
        bytes: &[u8],
        base: &Url,
        trusted: bool,
        ad_block: bool,
    ) -> Result<String, AppError> {
        rewrite_links(bytes, ad_block, true, |raw| {
            let url = playlist_url(base, raw)?;
            let trusted = trusted && same_origin(base, &url);
            self.register(url, trusted)
        })
    }
}

fn missing_grant() -> AppError {
    AppError::not_found("Playback grant was not found or has expired")
}

fn parse_token(token: &str) -> Result<(Uuid, usize), AppError> {
    let (session, item) = token.split_once('-').ok_or_else(missing_grant)?;
    let (index, _) = item.split_once('.').ok_or_else(missing_grant)?;
    Ok((
        Uuid::parse_str(session).map_err(|_| missing_grant())?,
        index.parse().map_err(|_| missing_grant())?,
    ))
}

struct BrowserGrant {
    owner: i64,
    expires: Instant,
    session: Arc<Session>,
}

#[derive(Default)]
pub struct PlaybackGrants(Mutex<VecDeque<BrowserGrant>>);

impl PlaybackGrants {
    pub fn revoke_user(&self, user_id: i64) {
        self.0
            .lock()
            .unwrap()
            .retain(|grant| grant.owner != user_id);
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub(crate) fn grant_url(
    state: &Shared,
    user_id: i64,
    url: &str,
    headers: &HeaderMap,
) -> Result<String, AppError> {
    let (session, path) = Session::new(http_url(url)?, headers.clone())?;
    store_session(
        &mut state.playback_grants.0.lock().unwrap(),
        user_id,
        session,
    );
    Ok(path)
}

fn store_session(sessions: &mut VecDeque<BrowserGrant>, user_id: i64, session: Arc<Session>) {
    let now = Instant::now();
    sessions.retain(|grant| grant.expires > now);
    if sessions
        .iter()
        .filter(|grant| grant.owner == user_id)
        .count()
        >= MAX_USER_SESSIONS
        && let Some(index) = sessions.iter().position(|grant| grant.owner == user_id)
    {
        sessions.remove(index);
    }
    if sessions.len() >= MAX_SESSIONS {
        sessions.pop_front();
    }
    sessions.push_back(BrowserGrant {
        owner: user_id,
        expires: now + GRANT_LIFETIME,
        session,
    });
}

pub(super) async fn live_stream(
    state: &Shared,
    user_id: i64,
    feed_token: Option<&str>,
    url: Url,
    private: HeaderMap,
    mut range: HeaderMap,
) -> Result<Response, AppError> {
    let requested_playlist = super::playlist_path(&url);
    if requested_playlist {
        range.remove(header::RANGE);
        range.remove(header::IF_RANGE);
    }
    let (upstream, trusted) = fetch_media(url.clone(), Some(&private), &range).await?;
    relay_response(upstream, requested_playlist, |bytes, base| {
        // Media playlists are polled frequently. Keep segment and key handles stable across reloads.
        let route = grant_route(feed_token);
        let mut sessions = state.playback_grants.0.lock().unwrap();
        let session = sessions
            .iter()
            .find(|grant| {
                grant.owner == user_id
                    && grant.expires > Instant::now()
                    && grant.session.route == route
                    && grant.session.live_root.as_ref() == Some(&url)
                    && grant.session.headers == private
            })
            .map(|grant| grant.session.clone())
            .unwrap_or_else(|| {
                let session = Session::empty(private, route, Some(url));
                store_session(&mut sessions, user_id, session.clone());
                session
            });
        drop(sessions);
        session.rewrite(bytes, base, trusted, false)
    })
    .await
}

fn grant_route(feed_token: Option<&str>) -> Cow<'static, str> {
    match feed_token {
        Some(token) => format!("/tvbox/{token}/grants").into(),
        None => GRANT_ROUTE.into(),
    }
}

pub(super) async fn proxy(
    State(state): State<Shared>,
    user: AuthUser,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    forward_grant(&state, user.id, None, &token, headers).await
}

pub(crate) async fn forward_grant(
    state: &Shared,
    user_id: i64,
    feed_token: Option<&str>,
    token: &str,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let route = grant_route(feed_token);
    let (id, index) = parse_token(token)?;
    let session = state
        .playback_grants
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|grant| {
            grant.owner == user_id
                && grant.session.token == id
                && grant.session.route == route
                && grant.expires > Instant::now()
        })
        .map(|grant| grant.session.clone())
        .ok_or_else(missing_grant)?;
    let ad_block = session.live_root.is_none() && play_settings(&*state.conn()?)?.ad_block;
    session.forward(index, headers, ad_block).await
}

/// A downloader gets an attempt-local URL. It never receives parser headers or signed media URLs.
pub(crate) struct DownloadRelay {
    pub url: String,
    _shutdown: oneshot::Sender<()>,
}

impl DownloadRelay {
    pub async fn start(url: Url, headers: HeaderMap) -> Result<Self, AppError> {
        let (session, path) = Session::new(url, headers)?;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!("http://{}{path}", listener.local_addr()?);
        let app = Router::new().route(
            "/api/playback/grants/{token}",
            get(move |Path(token): Path<String>, headers: HeaderMap| {
                let session = session.clone();
                async move {
                    let (id, index) = parse_token(&token)?;
                    if id != session.token {
                        return Err(missing_grant());
                    }
                    session.forward(index, headers, false).await
                }
            }),
        );
        let (shutdown, closed) = oneshot::channel();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = closed.await;
                })
                .await;
        });
        Ok(Self {
            url,
            _shutdown: shutdown,
        })
    }
}
