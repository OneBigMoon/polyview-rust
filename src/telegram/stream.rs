use super::{browse, login, native, tdl_command};
use crate::{AppError, Shared, auth::AuthUser, download_settings::download_settings};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
    routing::get,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::stream;
use rusqlite::Connection;
use serde_json::json;
use std::{io, ops::Range, process::Stdio, time::Duration};
use tempfile::TempDir;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    process::{Child, Command},
    sync::OwnedMutexGuard,
};

const CHUNK_SIZE: u64 = 256 * 1024;

pub(super) fn router() -> Router<Shared> {
    Router::new().route(
        "/api/telegram/stream/{chat_id}/{msg_id}",
        get(stream_message),
    )
}

pub(crate) fn has_session(conn: &Connection, user_id: i64) -> Result<bool, AppError> {
    if login::verified_account(conn, user_id)?.is_some() {
        return Ok(true);
    }
    let location = super::Location::new(&download_settings(conn)?, user_id)?;
    Ok(location
        .scope(conn, user_id)?
        .is_some_and(|s| s.phase == "verified"))
}

async fn stream_message(
    State(state): State<Shared>,
    user: AuthUser,
    Path((chat, id)): Path<(i64, i64)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    serve(&state, user.id, chat, id, &method, &headers).await
}

// Decimal positions beyond u64::MAX are beyond any supported representation.
fn decimal(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
        .then(|| value.parse().unwrap_or(u64::MAX))
}

fn byte_range(headers: &HeaderMap, size: u64) -> Option<Range<u64>> {
    let mut values = headers.get_all(header::RANGE).iter();
    let Some(value) = values.next() else {
        return Some(0..size);
    };
    if values.next().is_some() || size == 0 {
        return None;
    }
    let (unit, spec) = value.to_str().ok()?.trim().split_once('=')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    if start.is_empty() {
        let n = decimal(end)?;
        return (n > 0).then(|| size.saturating_sub(n)..size);
    }
    let start = decimal(start)?;
    let end = if end.is_empty() {
        size
    } else {
        decimal(end)?.saturating_add(1).min(size)
    };
    (start < end).then_some(start..end)
}

fn unavailable() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "Telegram video data is unavailable".into(),
    )
}

// A second downloadFile on the same file changes TDLib's active offset. Hold a
// stripe for one chunk only so simultaneous seeks and queue workers can interleave.
struct PendingDownload<'a> {
    client: &'a native::NativeClient,
    file_id: i64,
    lock: Option<OwnedMutexGuard<()>>,
}

impl Drop for PendingDownload<'_> {
    fn drop(&mut self) {
        if let Some(lock) = self.lock.take() {
            let client = self.client.clone();
            let file_id = self.file_id;
            tokio::spawn(async move {
                let _ = client.request(json!({"@type":"cancelDownloadFile", "file_id":file_id, "only_if_pending":false})).await;
                drop(lock);
            });
        }
    }
}

pub(super) async fn read_chunk(
    client: &native::NativeClient,
    file_id: i64,
    offset: u64,
    remaining: u64,
) -> Result<Vec<u8>, AppError> {
    let count = remaining.min(CHUNK_SIZE);
    if count == 0 {
        return Ok(Vec::new());
    }
    let lock = client.file_lock(file_id).lock_owned().await;
    let mut download = PendingDownload {
        client,
        file_id,
        lock: Some(lock),
    };
    client.request(json!({"@type":"downloadFile", "file_id":file_id, "offset":offset, "limit":count, "priority":32, "synchronous":true})).await.map_err(|_| unavailable())?;
    // The synchronous, limited download has finished. Keep the same offset
    // protected until the read returns, including if the response body drops.
    let _lock = download.lock.take();
    let response = client
        .request(json!({"@type":"readFilePart", "file_id":file_id, "offset":offset, "count":count}))
        .await
        .map_err(|_| unavailable())?;
    let encoded = response["data"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= count.div_ceil(3) as usize * 4)
        .ok_or_else(unavailable)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| unavailable())?;
    if bytes.is_empty() || bytes.len() as u64 > count {
        return Err(unavailable());
    }
    Ok(bytes)
}

struct Staging {
    child: Child,
    file: Option<File>,
    directory: TempDir,
}

impl Staging {
    fn start(mut command: Command, message: &browse::Message) -> Result<Self, AppError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("polyview-telegram-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir().map_err(|_| unavailable())?;
        command.arg("dl");
        if message.url.is_empty() {
            // Basic groups have no t.me post URL. tdl accepts the same export
            // format used by chat export, containing the resolved bare peer ID.
            let manifest = directory.path().join("message.json");
            let record = json!({"id": message.chat_id.unsigned_abs(), "messages":[{"type":"message", "id":message.id, "file":"video"}]});
            std::fs::write(&manifest, record.to_string()).map_err(|_| unavailable())?;
            command.arg("-f").arg(manifest);
        } else {
            command.arg("-u").arg(&message.url);
        }
        // One thread writes a contiguous prefix, first to video.tmp and then
        // renames it to video. Neither the upstream name nor a public listener
        // is used. The request owns both the child and its private directory.
        let child = command
            .args(["--template", "video", "--restart", "-t", "1", "-d"])
            .arg(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| unavailable())?;
        Ok(Self {
            child,
            file: None,
            directory,
        })
    }

    async fn read(&mut self, offset: u64, remaining: u64) -> io::Result<Vec<u8>> {
        let count = remaining.min(CHUNK_SIZE) as usize;
        loop {
            let finished = self.child.try_wait()?;
            if finished.is_some_and(|exit| !exit.success()) {
                return Err(io::Error::other("Telegram download failed"));
            }
            if self.file.is_none() {
                for name in ["video.tmp", "video"] {
                    match File::open(self.directory.path().join(name)).await {
                        Ok(mut file) => {
                            file.seek(io::SeekFrom::Start(offset)).await?;
                            self.file = Some(file);
                            break;
                        }
                        Err(err) if err.kind() == io::ErrorKind::NotFound => (),
                        Err(err) => return Err(err),
                    }
                }
            }
            if let Some(file) = &mut self.file {
                let available = file
                    .metadata()
                    .await?
                    .len()
                    .saturating_sub(offset)
                    .min(count as u64) as usize;
                if available != 0 {
                    let mut data = vec![0; available];
                    file.read_exact(&mut data).await?;
                    return Ok(data);
                }
            }
            if finished.is_some() {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
    }
}

enum Source {
    Native(native::NativeClient, i64),
    Legacy(Staging),
}

pub(crate) async fn serve(
    state: &Shared,
    user_id: i64,
    chat: i64,
    id: i64,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response, AppError> {
    let client = native::authorized(state, user_id).await?;
    let message = match &client {
        Some(client) => browse::native_message(client, chat, id).await?,
        None => browse::message(state, user_id, chat, id).await?,
    };
    if !message.is_video || message.file_size <= 0 {
        return Err(AppError::not_found("Telegram video not found"));
    }
    let size = message.file_size as u64;
    let builder = Response::builder().header(header::ACCEPT_RANGES, "bytes");
    let Some(range) = byte_range(headers, size) else {
        return Ok(builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            .body(Body::empty())
            .unwrap());
    };
    let mime = match message
        .file_name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "ts" | "m2ts" => "video/mp2t",
        "avi" => "video/x-msvideo",
        "flv" => "video/x-flv",
        _ => "video/mp4",
    };
    let mut builder = builder
        .header(header::CONTENT_TYPE, mime)
        .header(header::CONTENT_LENGTH, range.end - range.start);
    if headers.contains_key(header::RANGE) {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{size}", range.start, range.end - 1),
        );
    }
    if method == Method::HEAD {
        return Ok(builder.body(Body::empty()).unwrap());
    }
    let source = if let Some(client) = client {
        Source::Native(client, message.file_id.ok_or_else(unavailable)?)
    } else {
        let conn = state.conn()?;
        let command = tdl_command(&conn, &download_settings(&conn)?, user_id)?;
        Source::Legacy(Staging::start(command, &message)?)
    };
    let chunks = stream::try_unfold((source, range), |(mut source, mut range)| async move {
        if range.is_empty() {
            return Ok(None);
        }
        let remaining = range.end - range.start;
        let data = match &mut source {
            Source::Native(client, file_id) => read_chunk(client, *file_id, range.start, remaining)
                .await
                .map_err(|_| io::Error::other("Telegram range read failed"))?,
            Source::Legacy(staging) => staging.read(range.start, remaining).await?,
        };
        range.start += data.len() as u64;
        Ok::<_, io::Error>(Some((data, (source, range))))
    });
    Ok(builder.body(Body::from_stream(chunks)).unwrap())
}
