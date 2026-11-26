use super::tdl_command;
use crate::{
    AppError, Shared,
    auth::current_user,
    download_settings::download_settings,
    library::is_video,
    network::{Download, download_by_id, enqueue_telegram},
};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

pub(crate) fn router() -> Router<Shared> {
    Router::new().route("/api/downloads/telegram", post(create))
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
    let user = current_user(&conn, &headers)?
        .ok_or_else(|| AppError(StatusCode::UNAUTHORIZED, "Sign in required".into()))?;
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

// The queue owns work_dir (private, fresh per attempt). Dropping this future on
// cancellation terminates tdl before the queue discards its staging directory.
pub(crate) async fn download(
    mut command: tokio::process::Command,
    url: &str,
    work_dir: &Path,
) -> Result<PathBuf, String> {
    message_id(url).ok_or("Invalid Telegram message link")?;
    let status = command
        .args(["dl", "-u", url, "-d"])
        .arg(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|_| "Cannot start Telegram downloader")?;
    if !status.success() {
        return Err("Telegram download failed".into());
    }
    find_video(work_dir)
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
