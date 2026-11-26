use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    settings::{get_setting_json, set_setting_json},
};
use axum::{Json, extract::State};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use url::Url;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    pub(super) enabled: bool,
    pub(super) tmdb_key: String,
    proxy: String,
    pub(super) tmdb_url: String,
    pub(super) image_url: String,
    pub(super) bangumi_url: String,
    pub(super) douban_url: String,
}

impl Default for Backup {
    fn default() -> Self {
        Self {
            enabled: false,
            tmdb_key: String::new(),
            proxy: String::new(),
            tmdb_url: "https://api.themoviedb.org/3".into(),
            image_url: "https://image.tmdb.org/t/p/original".into(),
            bangumi_url: "https://api.bgm.tv".into(),
            douban_url: "https://movie.douban.com".into(),
        }
    }
}

impl Backup {
    fn public(&self) -> Value {
        json!({"enabled":self.enabled,"has_tmdb_key":!self.tmdb_key.is_empty(),"has_proxy":!self.proxy.is_empty(),"tmdb_url":self.tmdb_url,"image_url":self.image_url,"bangumi_url":self.bangumi_url,"douban_url":self.douban_url})
    }

    fn validate(&self) -> Result<(), AppError> {
        for (raw, proxy) in [
            (&self.tmdb_url, false),
            (&self.image_url, false),
            (&self.bangumi_url, false),
            (&self.douban_url, false),
            (&self.proxy, true),
        ] {
            if raw.is_empty() {
                continue;
            }
            let valid = raw.len() <= 2048
                && Url::parse(raw).is_ok_and(|u| {
                    matches!(u.scheme(), "http" | "https")
                        && u.has_host()
                        && u.query().is_none()
                        && u.fragment().is_none()
                        && (proxy || (u.username().is_empty() && u.password().is_none()))
                });
            if !valid {
                return Err(AppError::bad_request("Invalid scraper service address"));
            }
        }
        if self.tmdb_key.len() > 4096
            || self.tmdb_key.chars().any(char::is_whitespace)
            || self.tmdb_key.chars().any(char::is_control)
        {
            return Err(AppError::bad_request("Invalid TMDB credential"));
        }
        Ok(())
    }

    pub(super) fn client(&self) -> Result<reqwest::Client, AppError> {
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("PolyView/0.1 (self-hosted media library)")
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15));
        if !self.proxy.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::all(&self.proxy)
                    .map_err(|_| AppError::bad_request("Invalid scraper proxy"))?,
            );
        }
        builder
            .build()
            .map_err(|_| AppError::internal("Could not initialize metadata client"))
    }
}

pub fn export_config(conn: &Connection) -> Result<Backup, AppError> {
    get_setting_json(conn, "scraper")
}

pub fn restore_config(conn: &Connection, config: Backup) -> Result<(), AppError> {
    config.validate()?;
    set_setting_json(conn, "scraper", &config)
}

pub fn enabled(conn: &Connection) -> Result<bool, AppError> {
    Ok(export_config(conn)?.enabled)
}

pub(super) async fn read(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    Ok(Json(export_config(&*state.conn()?)?.public()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    enabled: Option<bool>,
    tmdb_key: Option<String>,
    proxy: Option<String>,
    tmdb_url: Option<String>,
    image_url: Option<String>,
    bangumi_url: Option<String>,
    douban_url: Option<String>,
}

pub(super) async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Json(patch): Json<Update>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    let mut config = export_config(&conn)?;
    if patch
        .tmdb_url
        .as_ref()
        .is_some_and(|url| url.trim_end_matches('/') != config.tmdb_url.trim_end_matches('/'))
    {
        config.tmdb_key.clear();
    }
    if let Some(v) = patch.enabled {
        config.enabled = v;
    }
    for (dst, src) in [
        (&mut config.tmdb_key, patch.tmdb_key),
        (&mut config.proxy, patch.proxy),
        (&mut config.tmdb_url, patch.tmdb_url),
        (&mut config.image_url, patch.image_url),
        (&mut config.bangumi_url, patch.bangumi_url),
        (&mut config.douban_url, patch.douban_url),
    ] {
        if let Some(value) = src {
            *dst = value.trim().to_owned();
        }
    }
    let response = config.public();
    restore_config(&conn, config)?;
    Ok(Json(response))
}
