mod public;

use super::{Backup, Credit, Episode, Metadata, Prepared, Season};
use crate::AppError;
use axum::http::StatusCode;
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, RequestBuilder};
use serde_json::Value;
use url::Url;

fn unavailable() -> AppError {
    AppError(
        StatusCode::BAD_GATEWAY,
        "Metadata provider unavailable or returned invalid data".into(),
    )
}
fn no_match() -> AppError {
    AppError::not_found("No matching metadata found")
}

async fn body(request: RequestBuilder, limit: usize) -> Result<Vec<u8>, AppError> {
    let response = request.send().await.map_err(|_| unavailable())?;
    if response.status() == StatusCode::NOT_FOUND {
        return Err(no_match());
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|size| size > limit as u64)
    {
        return Err(unavailable());
    }
    let mut bytes = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.try_next().await.map_err(|_| unavailable())? {
        if chunk.len() > limit - bytes.len() {
            return Err(unavailable());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
async fn json(request: RequestBuilder) -> Result<Value, AppError> {
    serde_json::from_slice(&body(request, 4 * 1024 * 1024).await?).map_err(|_| unavailable())
}
fn items(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}
fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().trim().to_owned()
}
fn primary(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| value[*k].as_str().filter(|s| !s.trim().is_empty()))
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn year(date: &str) -> String {
    date.get(..4)
        .filter(|s| s.bytes().all(|c| c.is_ascii_digit()))
        .unwrap_or_default()
        .into()
}
fn endpoint(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

pub(super) struct Image {
    pub(super) data: Vec<u8>,
    pub(super) mime: &'static str,
    pub(super) ext: &'static str,
}

pub(super) async fn image(client: &Client, url: Option<&str>) -> Result<Option<Image>, AppError> {
    let Some(url) = url else { return Ok(None) };
    let data = body(client.get(url), 12 * 1024 * 1024).await?;
    decode_image(data).map(Some)
}

pub(super) fn decode_image(data: Vec<u8>) -> Result<Image, AppError> {
    let (mime, ext) = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        ("image/png", "png")
    } else if data.starts_with(&[0xff, 0xd8, 0xff]) {
        ("image/jpeg", "jpg")
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        ("image/webp", "webp")
    } else {
        return Err(AppError(
            StatusCode::BAD_GATEWAY,
            "Provider returned unsupported artwork".into(),
        ));
    };
    Ok(Image { data, mime, ext })
}

pub(super) async fn lookup(
    config: &Backup,
    title: &str,
    requested_year: &str,
    season: Option<i64>,
    id: Option<i64>,
) -> Result<Prepared, AppError> {
    let client = config.client()?;
    let mut error = no_match();
    if !config.tmdb_key.is_empty() && !config.tmdb_url.is_empty() {
        match tmdb(config, &client, title, requested_year, season, id).await {
            Ok(prepared) => return Ok(prepared),
            Err(e) if id.is_some() => return Err(e),
            Err(e) => error = e,
        }
    } else if id.is_some() {
        return Err(AppError::bad_request(
            "Configure TMDB before identifying by TMDB ID",
        ));
    }
    let sources = if season.is_some() {
        ["bangumi", "douban"]
    } else {
        ["douban", "bangumi"]
    };
    for source in sources {
        let result = match source {
            "bangumi" if !config.bangumi_url.is_empty() => {
                public::bangumi(&client, &config.bangumi_url, title, requested_year, season).await
            }
            "douban" if !config.douban_url.is_empty() => {
                public::douban(&client, &config.douban_url, title, requested_year).await
            }
            _ => continue,
        };
        match result {
            Ok(prepared) => return Ok(prepared),
            Err(e) if e.0.is_server_error() => error = e,
            Err(_) => {}
        }
    }
    Err(error)
}

struct Tmdb<'a> {
    config: &'a Backup,
    client: &'a Client,
}
impl Tmdb<'_> {
    fn get(&self, path: &str) -> RequestBuilder {
        let request = self
            .client
            .get(endpoint(&self.config.tmdb_url, path))
            .query(&[("language", "zh-CN")]);
        if self.config.tmdb_key.starts_with("eyJ") {
            request.bearer_auth(&self.config.tmdb_key)
        } else {
            request.query(&[("api_key", &self.config.tmdb_key)])
        }
    }
    fn art(&self, value: &Value) -> Option<String> {
        let name = value.as_str()?.trim_start_matches('/');
        (!self.config.image_url.is_empty()
            && !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
            && !name.starts_with('.'))
        .then(|| endpoint(&self.config.image_url, name))
    }
    async fn season(&self, path: &str, n: i64) -> Result<Season, AppError> {
        let detail = json(self.get(&format!("{path}/season/{n}"))).await?;
        let episodes = items(&detail["episodes"])
            .filter_map(|e| {
                Some(Episode {
                    number: e["episode_number"].as_i64()?,
                    title: text(e, "name"),
                    description: text(e, "overview"),
                })
            })
            .collect();
        Ok(Season {
            number: n,
            title: text(&detail, "name"),
            episodes,
        })
    }
}

async fn tmdb(
    config: &Backup,
    client: &Client,
    title: &str,
    requested_year: &str,
    season: Option<i64>,
    id: Option<i64>,
) -> Result<Prepared, AppError> {
    let api = Tmdb { config, client };
    let kind = if season.is_some() { "tv" } else { "movie" };
    let id = match id {
        Some(id) => id,
        None => {
            let mut request = api
                .get(&format!("search/{kind}"))
                .query(&[("query", title)]);
            if !requested_year.is_empty() {
                request = request.query(&[(
                    if season.is_some() {
                        "first_air_date_year"
                    } else {
                        "year"
                    },
                    requested_year,
                )]);
            }
            let results = json(request).await?;
            items(&results["results"])
                .find_map(|r| r["id"].as_i64().filter(|n| *n > 0))
                .ok_or_else(no_match)?
        }
    };
    let path = format!("{kind}/{id}");
    let (detail, credits) = tokio::try_join!(
        json(api.get(&path)),
        json(api.get(&format!("{path}/credits")))
    )?;
    let title = primary(
        &detail,
        &["title", "name", "original_title", "original_name"],
    );
    if title.is_empty() {
        return Err(no_match());
    }
    let people = |kind: &str, role: &str| {
        items(&credits[kind])
            .filter_map(|person| {
                let name = text(person, "name");
                (!name.is_empty()).then(|| Credit {
                    name,
                    role: text(person, role),
                })
            })
            .collect()
    };
    let mut seasons = Vec::new();
    if let Some(n) = season {
        let mut numbers: Vec<_> = items(&detail["seasons"])
            .filter_map(|s| s["season_number"].as_i64())
            .collect();
        if numbers.is_empty() {
            numbers.push(n);
        }
        numbers.sort_unstable();
        numbers.dedup();
        if numbers.len() > 256 {
            return Err(unavailable());
        }
        seasons = stream::iter(numbers.into_iter().map(|n| api.season(&path, n)))
            .buffered(4)
            .try_collect()
            .await?;
    }
    Ok(Prepared {
        metadata: Metadata {
            provider: "tmdb".into(),
            provider_id: id.to_string(),
            title,
            original_title: primary(&detail, &["original_title", "original_name"]),
            year: year(&primary(&detail, &["release_date", "first_air_date"])),
            rating: detail["vote_average"].as_f64(),
            description: text(&detail, "overview"),
            genres: items(&detail["genres"])
                .filter_map(|v| v["name"].as_str().map(str::to_owned))
                .collect(),
            cast: people("cast", "character"),
            crew: people("crew", "job"),
            seasons,
            ..Default::default()
        },
        poster_url: api.art(&detail["poster_path"]),
        backdrop_url: api.art(&detail["backdrop_path"]),
    })
}

// Public metadata may point to a CDN. Only follow artwork on the configured service
// itself or the provider's image hosts. No service credentials accompany image requests.
fn public_image(base: &str, raw: &str, domains: &[&str]) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let origin = Url::parse(base).ok()?;
    let image = origin.join(raw).ok()?;
    if !matches!(image.scheme(), "http" | "https")
        || !image.username().is_empty()
        || image.password().is_some()
    {
        return None;
    }
    let host = image.host_str()?;
    (image.origin() == origin.origin()
        || (image.scheme() == "https"
            && domains
                .iter()
                .any(|d| host == *d || host.strip_suffix(d).is_some_and(|p| p.ends_with('.')))))
    .then(|| image.to_string())
}
