use super::{
    AppError, File, RemoteTask,
    config::{Account, Provider},
};
use axum::body::{Body, Bytes, to_bytes};
use reqwest::{Client, Method, Response, StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::LazyLock, time::Duration};

// Only the HTTP connection pool is shared. Cookies and RPC tokens belong to each session.
static TRANSPORT: LazyLock<Result<Client, reqwest::Error>> = LazyLock::new(|| {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
});

fn upstream() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "Downloader returned an invalid or unsuccessful response".into(),
    )
}

async fn read(response: Response) -> Result<Bytes, AppError> {
    if !response.status().is_success() {
        return Err(upstream());
    }
    to_bytes(Body::from_stream(response.bytes_stream()), 16 * 1024 * 1024)
        .await
        .map_err(|_| upstream())
}

pub(super) fn hash(value: &str) -> Result<String, AppError> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|v| v.is_ascii_hexdigit()) {
        return Err(upstream());
    }
    Ok(value.to_ascii_lowercase())
}

pub(super) struct Session<'a> {
    account: &'a Account,
    http: &'static Client,
    token: Option<header::HeaderValue>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Pause,
    Resume,
    Remove,
}

pub(super) enum Added {
    Tagged,
    Known(String),
    Duplicate,
}

impl<'a> Session<'a> {
    pub(super) async fn open(account: &'a Account) -> Result<Self, AppError> {
        let http = TRANSPORT.as_ref().map_err(|_| upstream())?;
        let mut session = Self {
            account,
            http,
            token: None,
        };
        if account.provider == Provider::Qbittorrent
            && (!account.username.is_empty() || !account.password.is_empty())
        {
            let res = session
                .qrequest(Method::POST, "auth/login")
                .form(&[
                    ("username", &account.username),
                    ("password", &account.password),
                ])
                .send()
                .await
                .map_err(|_| upstream())?;
            let cookie = res
                .headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|h| h.to_str().ok())
                .filter_map(|s| s.split(';').next())
                .find(|s| s.starts_with("SID=") || s.starts_with("QBT_SID="))
                .and_then(|s| header::HeaderValue::from_str(s).ok());
            if read(res).await?.as_ref() != b"Ok." || cookie.is_none() {
                return Err(upstream());
            }
            session.token = cookie;
        }
        Ok(session)
    }

    fn qrequest(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let base = self.account.endpoint.trim_end_matches('/');
        let mut req = self
            .http
            .request(method, format!("{base}/api/v2/{path}"))
            .header(header::REFERER, format!("{base}/"));
        if let Some(cookie) = &self.token {
            req = req.header(header::COOKIE, cookie);
        }
        req
    }

    pub(super) async fn close(self) {
        if self.account.provider == Provider::Qbittorrent && self.token.is_some() {
            let _ = self.qrequest(Method::POST, "auth/logout").send().await;
        }
    }

    async fn rpc<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        args: Value,
    ) -> Result<T, AppError> {
        #[derive(Deserialize)]
        struct RpcReply<T> {
            result: String,
            arguments: T,
        }
        let mut endpoint = crate::network::http_url(&self.account.endpoint)?;
        if endpoint.path() == "/" {
            endpoint.set_path("/transmission/rpc");
        }
        let body = Bytes::from(
            serde_json::to_vec(&json!({"method":method,"arguments":args}))
                .map_err(|_| upstream())?,
        );
        for attempt in 0..2 {
            let mut req = self
                .http
                .post(endpoint.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.clone());
            if !self.account.username.is_empty() || !self.account.password.is_empty() {
                req = req.basic_auth(&self.account.username, Some(&self.account.password));
            }
            if let Some(token) = &self.token {
                req = req.header("x-transmission-session-id", token);
            }
            let response = req.send().await.map_err(|_| upstream())?;
            if response.status() == StatusCode::CONFLICT && attempt == 0 {
                self.token = Some(
                    response
                        .headers()
                        .get("x-transmission-session-id")
                        .filter(|h| !h.is_empty())
                        .ok_or_else(upstream)?
                        .clone(),
                );
                continue;
            }
            let result: RpcReply<T> =
                serde_json::from_slice(&read(response).await?).map_err(|_| upstream())?;
            if result.result != "success" {
                return Err(upstream());
            }
            return Ok(result.arguments);
        }
        Err(upstream())
    }

    pub(super) async fn list(&mut self) -> Result<Vec<RemoteTask>, AppError> {
        match self.account.provider {
            Provider::Transmission => {
                #[derive(Deserialize)]
                struct Listing {
                    torrents: Vec<TransmissionTask>,
                }
                let listing: Listing = self.rpc("torrent-get",json!({"fields":["hashString","name","status","error","percentDone","downloadDir","files","fileStats"]})).await?;
                listing
                    .torrents
                    .into_iter()
                    .map(TransmissionTask::normalize)
                    .collect()
            }
            Provider::Qbittorrent => {
                let response = self
                    .qrequest(Method::GET, "torrents/info")
                    .send()
                    .await
                    .map_err(|_| upstream())?;
                let torrents: Vec<QbitTask> =
                    serde_json::from_slice(&read(response).await?).map_err(|_| upstream())?;
                let mut tasks = Vec::with_capacity(torrents.len());
                for torrent in torrents {
                    let hash = hash(&torrent.hash)?;
                    let res = self
                        .qrequest(Method::GET, "torrents/files")
                        .query(&[("hash", &hash)])
                        .send()
                        .await
                        .map_err(|_| upstream())?;
                    // An externally removed torrent can disappear between the two calls.
                    if res.status() == StatusCode::NOT_FOUND {
                        continue;
                    }
                    let files: Vec<QbitFile> =
                        serde_json::from_slice(&read(res).await?).map_err(|_| upstream())?;
                    tasks.push(torrent.normalize(hash, files));
                }
                Ok(tasks)
            }
        }
    }

    pub(super) async fn add(&mut self, url: &str, tag: &str) -> Result<Added, AppError> {
        match self.account.provider {
            Provider::Transmission => {
                let data: Value = self.rpc("torrent-add", json!({"filename":url})).await?;
                if data["torrent-duplicate"].is_object() {
                    return Ok(Added::Duplicate);
                }
                Ok(Added::Known(hash(
                    data["torrent-added"]["hashString"]
                        .as_str()
                        .ok_or_else(upstream)?,
                )?))
            }
            Provider::Qbittorrent => {
                let boundary = uuid::Uuid::new_v4().simple().to_string();
                let body = format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"urls\"\r\n\r\n{url}\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"tags\"\r\n\r\n{tag}\r\n--{boundary}--\r\n"
                );
                let res = self
                    .qrequest(Method::POST, "torrents/add")
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(body)
                    .send()
                    .await
                    .map_err(|_| upstream())?;
                if read(res).await?.as_ref() != b"Ok." {
                    return Err(upstream());
                }
                Ok(Added::Tagged)
            }
        }
    }

    pub(super) async fn control(&mut self, hash: &str, action: Action) -> Result<(), AppError> {
        match self.account.provider {
            Provider::Transmission => {
                let mut args = json!({"ids":[hash]});
                let method = match action {
                    Action::Pause => "torrent-stop",
                    Action::Resume => "torrent-start",
                    Action::Remove => {
                        args["delete-local-data"] = false.into();
                        "torrent-remove"
                    }
                };
                self.rpc::<serde::de::IgnoredAny>(method, args).await?;
            }
            Provider::Qbittorrent => {
                let (method, legacy) = match action {
                    Action::Pause => ("stop", "pause"),
                    Action::Resume => ("start", "resume"),
                    Action::Remove => ("delete", "delete"),
                };
                let mut form = vec![("hashes", hash)];
                if action == Action::Remove {
                    form.push(("deleteFiles", "false"));
                }
                let send = |name: &str| {
                    self.qrequest(Method::POST, &format!("torrents/{name}"))
                        .form(&form)
                        .send()
                };
                let mut res = send(method).await.map_err(|_| upstream())?;
                if method != legacy
                    && matches!(
                        res.status(),
                        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                    )
                {
                    res = send(legacy).await.map_err(|_| upstream())?;
                }
                read(res).await?;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct QbitTask {
    hash: String,
    name: String,
    state: String,
    progress: f64,
    save_path: String,
    #[serde(default)]
    tags: String,
}
#[derive(Deserialize)]
struct QbitFile {
    name: String,
    size: u64,
    progress: f64,
    priority: i64,
}

impl QbitTask {
    fn normalize(self, hash: String, files: Vec<QbitFile>) -> RemoteTask {
        let status = match self.state.as_str() {
            "error" | "missingFiles" | "unknown" => "failed",
            s if s.starts_with("paused") || s.starts_with("stopped") => "paused",
            _ if self.progress >= 1.0 => "completed",
            s if s.starts_with("queued") || s.starts_with("checking") => "queued",
            _ => "running",
        };
        RemoteTask {
            hash,
            name: self.name,
            status,
            progress: self.progress.clamp(0.0, 1.0),
            directory: self.save_path,
            tags: self.tags,
            files: files
                .into_iter()
                .map(|f| File {
                    name: f.name,
                    size: f.size,
                    ready: f.priority > 0 && f.progress >= 1.0,
                })
                .collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransmissionTask {
    hash_string: String,
    name: String,
    status: i64,
    error: i64,
    percent_done: f64,
    download_dir: String,
    files: Vec<TransmissionFile>,
    file_stats: Vec<FileStat>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TransmissionFile {
    name: String,
    length: u64,
    bytes_completed: u64,
}
#[derive(Deserialize)]
struct FileStat {
    wanted: bool,
}

impl TransmissionTask {
    fn normalize(self) -> Result<RemoteTask, AppError> {
        if self.files.len() != self.file_stats.len() {
            return Err(upstream());
        }
        let status = match self.status {
            _ if self.error != 0 => "failed",
            0 => "paused",
            _ if self.percent_done >= 1.0 => "completed",
            1..=3 | 5 => "queued",
            4 | 6 => "running",
            _ => "failed",
        };
        Ok(RemoteTask {
            hash: hash(&self.hash_string)?,
            name: self.name,
            status,
            progress: self.percent_done.clamp(0.0, 1.0),
            directory: self.download_dir,
            tags: String::new(),
            files: self
                .files
                .into_iter()
                .zip(self.file_stats)
                .map(|(f, s)| File {
                    name: f.name,
                    size: f.length,
                    ready: s.wanted && f.bytes_completed == f.length,
                })
                .collect(),
        })
    }
}
