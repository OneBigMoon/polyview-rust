use super::{browse, native, signed_in, stream, tdl_command};
use crate::{
    AppError, Shared,
    download_settings::{DownloadSettings, download_settings},
    library::is_video,
    network::{Download, download_by_id, enqueue_telegram},
};
use axum::{
    Json, Router,
    extract::{Path as RoutePath, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STREAM_PREFIX: &str = "/api/telegram/stream/";

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/downloads/telegram", post(create))
        .route(
            "/api/telegram/chats/{chat_id}/messages/{id}/download",
            post(queue_message),
        )
}

#[derive(Deserialize)]
struct TelegramInput {
    url: String,
    title: Option<String>,
}

// Check the original text before any URL parser can normalize whitespace, slashes,
// dot segments or credentials. Only a public post or a private channel post is valid.
fn message_id(url: &str) -> Option<&str> {
    if url.len() > 128 {
        return None;
    }
    let path = url
        .strip_prefix("https://t.me/")
        .or_else(|| url.strip_prefix("https://telegram.me/"))?;
    let mut parts = path.split('/');
    let channel = parts.next()?;
    if channel == "c" {
        let private_id = parts.next()?;
        if !decimal_id(private_id) || private_id.parse::<i64>().is_err() {
            return None;
        }
    } else if channel.len() > 32
        || !channel.as_bytes().first()?.is_ascii_alphabetic()
        || !channel
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || [
            "share",
            "joinchat",
            "addstickers",
            "addemoji",
            "proxy",
            "socks",
            "login",
            "invoice",
            "iv",
            "s",
            "boost",
            "addlist",
            "giftcode",
            "contact",
            "nft",
            "bg",
            "m",
            "addtheme",
            "setlanguage",
            "confirmphone",
            "addvoice",
            "a",
            "k",
            "c",
        ]
        .iter()
        .any(|name| channel.eq_ignore_ascii_case(name))
    {
        return None;
    }
    let post = parts.next()?;
    (parts.next().is_none() && decimal_id(post) && post.parse::<i32>().is_ok()).then_some(post)
}

fn decimal_id(value: &str) -> bool {
    !value.is_empty() && !value.starts_with('0') && value.bytes().all(|b| b.is_ascii_digit())
}

async fn create(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(input): Json<TelegramInput>,
) -> Result<(StatusCode, Json<Download>), AppError> {
    let conn = state.conn()?;
    let user = signed_in(&conn, &headers)?;
    let id = message_id(&input.url)
        .ok_or_else(|| AppError::bad_request("Provide an HTTPS Telegram message link"))?;
    let title = input.title.unwrap_or_else(|| format!("Telegram {id}"));
    if title.chars().any(char::is_control) {
        return Err(AppError::bad_request("Invalid download title"));
    }
    let settings = download_settings(&conn)?;
    tdl_command(&conn, &settings, user.id)?;
    let job = enqueue_telegram(&conn, &state, user.id, &title, &input.url)?;
    Ok((
        StatusCode::CREATED,
        Json(download_by_id(&conn, job, user.id)?),
    ))
}

#[derive(Default, Deserialize)]
struct Selection {
    #[serde(default)]
    single: bool,
}

async fn queue_message(
    State(state): State<Shared>,
    headers: HeaderMap,
    RoutePath((chat_id, id)): RoutePath<(i64, i64)>,
    input: Option<Json<Selection>>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let user = signed_in(&*state.conn()?, &headers)?;
    let message = browse::message(&state, user.id, chat_id, id).await?;
    let messages =
        if input.is_some_and(|selection| selection.single) || message.media_group_id.is_none() {
            vec![message]
        } else {
            browse::album(&state, user.id, chat_id, &message).await?
        };
    let mut conn = state.conn()?;
    signed_in(&conn, &headers)?;
    let tx = conn.transaction()?;
    let mut downloads = Vec::new();
    for message in messages
        .iter()
        .filter(|message| message.is_video && message.chat_id == chat_id)
    {
        let id = enqueue_message(&tx, &state, user.id, message)?;
        downloads.push(download_by_id(&tx, id, user.id)?);
    }
    if downloads.is_empty() {
        return Err(AppError::bad_request(
            "The selected Telegram post contains no video",
        ));
    }
    tx.commit()?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"queued":downloads.len(),"downloads":downloads})),
    ))
}

pub(super) fn enqueue_message(
    conn: &Connection,
    state: &Shared,
    user_id: i64,
    message: &browse::Message,
) -> Result<i64, AppError> {
    let title: String = message
        .file_name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let title = if title.trim().is_empty() {
        format!("Telegram {}", message.id)
    } else {
        title
    };
    let url = format!("{STREAM_PREFIX}{}/{}", message.chat_id, message.id);
    enqueue_telegram(conn, state, user_id, &title, &url)
}

// The queue owns work_dir (private, fresh per attempt). Dropping this future on
// cancellation terminates tdl before the queue discards its staging directory.
pub(crate) async fn download(
    state: &Shared,
    user_id: i64,
    settings: &DownloadSettings,
    url: &str,
    work_dir: &Path,
    mut progress: impl FnMut(i64, Option<i64>) -> Result<(), String>,
) -> Result<PathBuf, String> {
    let mut legacy_url = Some(url);
    let mut manifest = None;
    let message;
    let mut known_size = None;
    if let Some(reference) = url.strip_prefix(STREAM_PREFIX) {
        let (chat, post) = reference
            .split_once('/')
            .ok_or("Invalid Telegram media reference")?;
        let chat = chat.parse().map_err(|_| "Invalid Telegram chat")?;
        let post = post.parse().map_err(|_| "Invalid Telegram message")?;
        if let Some(client) = native::authorized(state, user_id)
            .await
            .map_err(|_| "Telegram authorization failed")?
        {
            let msg = browse::native_message(&client, chat, post)
                .await
                .map_err(|_| "Telegram video is unavailable")?;
            let file_id = msg
                .file_id
                .filter(|_| msg.is_video && msg.file_size > 0)
                .ok_or("Telegram video is unavailable")?;
            let name = Path::new(&msg.file_name);
            let ext = name
                .extension()
                .and_then(|s| s.to_str())
                .filter(|_| is_video(name))
                .unwrap_or("mp4");
            let path = work_dir.join(format!("telegram.{ext}"));
            let mut file = tokio::fs::File::create_new(&path)
                .await
                .map_err(|_| "Cannot create Telegram video file")?;
            let mut written = 0;
            progress(0, Some(msg.file_size))?;
            while written < msg.file_size {
                let remaining = (msg.file_size - written) as u64;
                let bytes = stream::read_chunk(&client, file_id, written as u64, remaining)
                    .await
                    .map_err(|_| "Telegram download interrupted")?;
                if bytes.is_empty() || bytes.len() as u64 > remaining {
                    return Err("Telegram returned an incomplete video".into());
                }
                file.write_all(&bytes)
                    .await
                    .map_err(|_| "Cannot write Telegram video file")?;
                written += bytes.len() as i64;
                progress(written, Some(msg.file_size))?;
            }
            file.flush()
                .await
                .map_err(|_| "Cannot flush Telegram video file")?;
            return Ok(path);
        }
        message = browse::message(state, user_id, chat, post)
            .await
            .map_err(|_| "Telegram video is unavailable")?;
        if !message.is_video {
            return Err("Telegram post contains no video".into());
        }
        if message.url.is_empty() {
            // Basic groups have no t.me/c permalink. tdl accepts its chat export
            // format and resolves the peer through this owner's session.
            let path = work_dir.join("message.json");
            let data = json!({"id": chat.unsigned_abs(), "messages": [{"type":"message", "id":post, "file":"video"}]});
            tokio::fs::write(&path, data.to_string())
                .await
                .map_err(|_| "Cannot stage Telegram message")?;
            manifest = Some(path);
            legacy_url = None;
        } else {
            legacy_url = Some(&message.url);
        }
        known_size = (message.file_size > 0).then_some(message.file_size);
    }
    if legacy_url.is_some_and(|url| message_id(url).is_none()) {
        return Err("Invalid Telegram message link".into());
    }
    let mut command = {
        let conn = state.conn().map_err(|e| e.1)?;
        tdl_command(&conn, settings, user_id).map_err(|_| "Telegram authorization is required")?
    };
    command.arg("dl");
    if let Some(path) = manifest {
        command.arg("-f").arg(path);
    } else if let Some(url) = legacy_url {
        command.args(["-u", url]);
    }
    let mut child = command
        .args(["--restart", "-d"])
        .arg(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| "Cannot start Telegram downloader")?;
    let stdout = child.stdout.take().ok_or("Missing downloader output")?;
    let stderr = child
        .stderr
        .take()
        .ok_or("Missing downloader error stream")?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(i64, f64)>(8);
    let report = async {
        while let Some((written, percent)) = rx.recv().await {
            let total = known_size.or_else(|| {
                (percent > 0.0).then(|| (written as f64 * 100.0 / percent).round() as i64)
            });
            progress(written, total)?;
        }
        Ok(())
    };
    let (status, (), (), ()) = tokio::try_join!(
        async {
            child
                .wait()
                .await
                .map_err(|_| "Cannot wait for Telegram downloader".to_owned())
        },
        read_progress(stdout, tx.clone()),
        read_progress(stderr, tx),
        report,
    )?;
    if !status.success() {
        return Err("Telegram download failed".into());
    }
    let path = find_video(work_dir)?;
    if let Some(size) = known_size
        && fs::metadata(&path)
            .map_err(|_| "Cannot inspect Telegram video")?
            .len()
            != size as u64
    {
        return Err("Telegram returned an incomplete video".into());
    }
    Ok(path)
}

// tdl writes go-pretty terminal frames, with one per-file percentage and a
// bracketed byte count (e.g. "50.0% [2.00 MB in 1s; 2.00 MB/s]"). Drain both
// pipes concurrently. Child text can contain private paths, so expose only numbers.
async fn read_progress(
    mut pipe: impl tokio::io::AsyncRead + Unpin,
    tx: tokio::sync::mpsc::Sender<(i64, f64)>,
) -> Result<(), String> {
    let mut buf = [0; 4096];
    let mut line = Vec::new();
    loop {
        let n = pipe
            .read(&mut buf)
            .await
            .map_err(|_| "Cannot read downloader progress")?;
        for &byte in &buf[..n] {
            if matches!(byte, b'\n' | b'\r') {
                if let Some(sample) = progress_line(&line) {
                    tx.send(sample).await.map_err(|_| "Download stopped")?;
                }
                line.clear();
            } else if line.len() < 8192 {
                line.push(byte);
            }
        }
        if n == 0 {
            return Ok(());
        }
    }
}

fn progress_line(line: &[u8]) -> Option<(i64, f64)> {
    let mut plain = String::with_capacity(line.len());
    let mut bytes = line.iter().copied().peekable();
    while let Some(byte) = bytes.next() {
        if byte == 27 && bytes.next_if_eq(&b'[').is_some() {
            let _ = bytes.by_ref().find(|b| (0x40..=0x7e).contains(b));
        } else if byte.is_ascii() {
            plain.push(byte as char);
        }
    }
    let (before, stats) = plain.rsplit_once('%')?;
    let percent: f64 = before
        .rsplit(|c: char| !c.is_ascii_digit() && c != '.')
        .next()?
        .parse()
        .ok()?;
    if !(0.0..=100.0).contains(&percent) {
        return None;
    }
    for field in stats.split('[').skip(1) {
        let mut parts = field.split_whitespace();
        let Some(value) = parts.next().and_then(|s| s.parse::<f64>().ok()) else {
            continue;
        };
        let scale: u64 = match parts.next()?.trim_end_matches([']', ';']) {
            "B" => 1,
            "KB" => 1 << 10,
            "MB" => 1 << 20,
            "GB" => 1 << 30,
            "TB" => 1 << 40,
            _ => continue,
        };
        if value.is_finite() && value >= 0.0 {
            return Some(((value * scale as f64).round() as i64, percent));
        }
    }
    None
}

fn find_video(dir: &Path) -> Result<PathBuf, String> {
    let mut video = None;
    for entry in fs::read_dir(dir).map_err(|_| "Cannot inspect Telegram download")? {
        let entry = entry.map_err(|_| "Cannot inspect Telegram download")?;
        let path = entry.path();
        if !is_video(&path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| "Cannot inspect Telegram video")?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err("Invalid Telegram video file".into());
        }
        #[cfg(unix)]
        if std::os::unix::fs::MetadataExt::nlink(&metadata) != 1 {
            return Err("Invalid Telegram video file".into());
        }
        if video.replace(path).is_some() {
            return Err("Telegram message produced multiple videos".into());
        }
    }
    video.ok_or_else(|| "Telegram message contained no video".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_canonical_message_links() {
        assert_eq!(message_id("https://t.me/example_channel/17"), Some("17"));
        assert_eq!(
            message_id("https://telegram.me/c/1234567890/17"),
            Some("17")
        );
        for invalid in [
            "https://t.me",
            "https://t.me/",
            "http://t.me/channel/1",
            "https://t.me:443/channel/1",
            "https://t.me.evil/channel/1",
            "https://t.me@evil/channel/1",
            "https://evil@t.me/channel/1",
            "https://t.me/c/23",
            "https://t.me/c/abc/1",
            "https://t.me/name/+1",
            "https://t.me/name/0",
            "https://t.me/name/1/",
            "https://t.me/name/1#fragment",
            "https://t.me/name/1?token=secret",
            "https://t.me/%2e/1",
            "https://t.me/name/../1",
            "https://t.me/share/1",
            "https://t.me/na\\me/1",
            "https://t.me/chan\nnel/1",
            "https://t.me/channel/1 ",
            "https://t.me/name/1;id",
        ] {
            assert_eq!(message_id(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn parses_numeric_tdl_frames_and_ignores_aggregate_counts() {
        assert_eq!(
            progress_line(b"\x1b[2Kfile.mp4 ... \x1b[32m50.0%\x1b[0m [2.00 MB in 2s; 1.00 MB/s]"),
            Some((2 << 20, 50.0))
        );
        assert_eq!(
            progress_line(b"file.mp4 ... 0.0% [0 B in 100ms]"),
            Some((0, 0.0))
        );
        assert_eq!(
            progress_line(b"overall ... 50.0% [1/2 in 2s; 1.00 MB/s]"),
            None
        );
        assert_eq!(progress_line(b"unstructured child diagnostic"), None);
    }

    #[cfg(unix)]
    #[test]
    fn accepts_only_one_regular_nonempty_video() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        outside.as_file().set_len(1).unwrap();
        let video = dir.path().join("clip.MKV");
        std::os::unix::fs::symlink(outside.path(), &video).unwrap();
        assert!(find_video(dir.path()).is_err());
        fs::remove_file(&video).unwrap();
        fs::hard_link(outside.path(), &video).unwrap();
        assert!(find_video(dir.path()).is_err());
        fs::remove_file(&video).unwrap();
        fs::write(dir.path().join("payload.html"), "html").unwrap();
        assert!(find_video(dir.path()).is_err());
        fs::write(&video, "").unwrap();
        assert!(find_video(dir.path()).is_err());
        fs::write(&video, "clip").unwrap();
        assert_eq!(find_video(dir.path()).unwrap(), video);
        fs::write(dir.path().join("other.mp4"), "clip").unwrap();
        assert!(find_video(dir.path()).is_err());
    }
}
