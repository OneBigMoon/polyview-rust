use super::{AppError, Download, Shared, Url, download_extension, http_url};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

/// Store parser transport details separately from the public job row and its original catalog URL.
#[derive(Serialize, Deserialize)]
struct StoredMedia {
    url: String,
    headers: Vec<(String, String)>,
}

pub(crate) struct PreparedDownload {
    pub(super) original: Url,
    pub(super) extension: String,
    pub(super) resolved: Option<String>,
}

impl PreparedDownload {
    pub(super) fn direct(raw: &str) -> Result<Self, AppError> {
        let original = download_url(raw)?;
        let extension = download_extension(&original)?;
        Ok(Self {
            original,
            extension,
            resolved: None,
        })
    }
}

fn download_url(raw: &str) -> Result<Url, AppError> {
    if raw.chars().any(char::is_control) {
        return Err(AppError::bad_request(
            "Invalid control character in download URL",
        ));
    }
    http_url(raw.trim())
}

pub(crate) async fn prepare_catalog_download(
    state: &Shared,
    url: &str,
    flag: &str,
    direct: bool,
) -> Result<PreparedDownload, AppError> {
    if direct {
        return PreparedDownload::direct(url);
    }
    let original = download_url(url)?;
    let media = crate::parsers::resolve_media(state, original.as_str(), flag).await?;
    let extension = download_extension(&download_url(&media.url)?)?;
    let mut headers = Vec::with_capacity(media.headers.len());
    for (name, value) in &media.headers {
        let value = value
            .to_str()
            .map_err(|_| AppError::bad_request("Invalid resolved media header"))?;
        headers.push((name.to_string(), value.to_owned()));
    }
    let serialized = serde_json::to_string(&StoredMedia {
        url: media.url,
        headers,
    })
    .map_err(|_| AppError::internal("Could not store media transfer details"))?;
    Ok(PreparedDownload {
        original,
        extension,
        resolved: Some(serialized),
    })
}

pub(super) fn transfer_source(
    state: &Shared,
    job: &Download,
) -> Result<(Url, Option<HeaderMap>), String> {
    let stored: Option<String> = state
        .conn()
        .map_err(|e| e.1)?
        .query_row(
            "SELECT resolved_media FROM downloads WHERE id=?1 AND attempt_token=?2",
            rusqlite::params![job.id, job.attempt_token],
            |row| row.get(0),
        )
        .map_err(|_| "Could not read download source")?;
    let Some(stored) = stored else {
        return Ok((download_url(&job.url).map_err(|e| e.1)?, None));
    };
    let media: StoredMedia =
        serde_json::from_str(&stored).map_err(|_| "Invalid stored media source")?;
    let mut headers = HeaderMap::with_capacity(media.headers.len());
    for (name, value) in media.headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| "Invalid stored media header")?;
        let mut value = HeaderValue::from_str(&value).map_err(|_| "Invalid stored media header")?;
        value.set_sensitive(true);
        headers.append(name, value);
    }
    Ok((download_url(&media.url).map_err(|e| e.1)?, Some(headers)))
}
