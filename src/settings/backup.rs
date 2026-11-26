use super::{PlaySettings, get_setting_json, set_setting_json};
use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    calendar::CalendarSettings,
    catalog::{SourceInput, adult_sources_enabled, validate_source},
    epg::{self, ChannelAlias},
    live_sources::SourceConfig,
    notify::NotifySettings,
    source_subscriptions::{self, SubscriptionBackup},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, FromRequest, Request, State},
    http::StatusCode,
    routing::{get, post},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

const VERSION: u32 = 1;
const MAX_BACKUP_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Backup {
    version: u32,
    exported_at: i64,
    catalog_sources: Vec<SourceEntry<SourceInput>>,
    live_sources: Vec<SourceEntry<SourceConfig>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    source_subscriptions: Option<SubscriptionBackup>,
    epg_config: EpgConfig,
    app_settings: SiteSettings,
}

#[derive(Serialize, Deserialize)]
struct SourceEntry<T> {
    id: i64,
    #[serde(flatten)]
    config: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EpgConfig {
    url: String,
}

// A closed list keeps future account bindings and other private records out of backups.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SiteSettings {
    adult_sources_enabled: bool,
    epg_aliases: Vec<ChannelAlias>,
    calendar: CalendarSettings,
    notify: NotifySettings,
    play: PlaySettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    downloads: Option<crate::download_settings::DownloadSettings>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    douban_movie_url: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    douban_series_url: Option<String>,
}

fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

pub(super) fn router() -> Router<Shared> {
    Router::new()
        .route("/api/settings/export", get(export))
        .route(
            "/api/settings/import",
            post(import).layer(DefaultBodyLimit::max(MAX_BACKUP_BYTES)),
        )
}

async fn export(State(state): State<Shared>, user: AuthUser) -> Result<Json<Backup>, AppError> {
    require_admin(&user)?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let backup = Backup {
        version: VERSION,
        exported_at: crate::now(),
        catalog_sources: export_catalog(&tx)?,
        live_sources: export_live(&tx)?,
        source_subscriptions: Some(source_subscriptions::export_config(&tx)?),
        epg_config: EpgConfig {
            url: tx.query_row("SELECT value FROM settings WHERE key='epg_url'", [], |r| {
                r.get(0)
            })?,
        },
        app_settings: SiteSettings {
            adult_sources_enabled: adult_sources_enabled(&tx)?,
            epg_aliases: get_setting_json(&tx, epg::ALIASES_KEY)?,
            calendar: get_setting_json(&tx, "calendar")?,
            notify: get_setting_json(&tx, "notify")?,
            play: get_setting_json(&tx, "play")?,
            downloads: Some(crate::download_settings::download_settings(&tx)?),
            douban_movie_url: get_setting_json(&tx, "douban_movie_url")?,
            douban_series_url: get_setting_json(&tx, "douban_series_url")?,
        },
    };
    tx.commit()?;
    Ok(Json(backup))
}

fn export_catalog(conn: &Connection) -> Result<Vec<SourceEntry<SourceInput>>, AppError> {
    let mut stmt = conn.prepare(
        "SELECT id,name,url,enabled,adult,priority,searchable,custom_ua,custom_referer,
         custom_cookie,proxy_url,timeout_secs FROM catalog_sources ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(SourceEntry {
            id: r.get(0)?,
            config: SourceInput {
                name: r.get(1)?,
                url: r.get(2)?,
                enabled: r.get(3)?,
                adult: r.get(4)?,
                priority: r.get(5)?,
                searchable: r.get(6)?,
                custom_ua: r.get(7)?,
                custom_referer: r.get(8)?,
                custom_cookie: r.get(9)?,
                proxy_url: r.get(10)?,
                timeout_secs: r.get(11)?,
            },
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn export_live(conn: &Connection) -> Result<Vec<SourceEntry<SourceConfig>>, AppError> {
    let mut stmt = conn.prepare("SELECT id,name,url,enabled FROM live_sources ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(SourceEntry {
            id: r.get(0)?,
            config: SourceConfig {
                name: r.get(1)?,
                url: r.get(2)?,
                enabled: r.get(3)?,
            },
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

async fn import(
    State(state): State<Shared>,
    user: AuthUser,
    request: Request,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let bytes = Bytes::from_request(request, &state)
        .await
        .map_err(|err| AppError(err.status(), "Could not read configuration backup".into()))?;
    let backup: Backup = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::bad_request("Invalid configuration backup"))?;
    if backup.version != VERSION {
        return Err(AppError::bad_request(
            "Unsupported configuration backup version",
        ));
    }
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    backup.app_settings.restore(&tx)?;
    epg::store_config(&tx, &backup.epg_config.url)?;
    // Preexisting manual sources keep their ownership if a backup contains the same URL.
    let subscriptions = match backup.source_subscriptions {
        Some(config) => Some((config, source_subscriptions::existing_manual_urls(&tx)?)),
        None => None,
    };
    restore_catalog(&tx, backup.catalog_sources)?;
    restore_live(&tx, backup.live_sources)?;
    if let Some((config, manual_urls)) = subscriptions {
        source_subscriptions::restore_config(&tx, config, &manual_urls)?;
    }
    tx.commit()?;
    Ok(Json(json!({"ok": true})))
}

impl SiteSettings {
    fn restore(self, conn: &Connection) -> Result<(), AppError> {
        self.calendar.validate()?;
        self.play.validate()?;
        if let Some(settings) = &self.downloads {
            settings.validate()?;
            set_setting_json(conn, "downloads", settings)?;
        }
        self.notify.validate()?;
        if self.calendar != get_setting_json::<CalendarSettings>(conn, "calendar")? {
            conn.execute("DELETE FROM remote_cache WHERE cache_key='calendar'", [])?;
        }
        set_setting_json(conn, "calendar", &self.calendar)?;
        set_setting_json(conn, "notify", &self.notify)?;
        set_setting_json(conn, "play", &self.play)?;
        for (key, url) in [
            ("douban_movie_url", self.douban_movie_url),
            ("douban_series_url", self.douban_series_url),
        ] {
            if let Some(raw) = &url
                && (raw.len() > 4096
                    || raw.chars().any(|c| c.is_control() || c.is_whitespace())
                    || !url::Url::parse(raw).is_ok_and(|u| {
                        matches!(u.scheme(), "http" | "https")
                            && u.has_host()
                            && u.username().is_empty()
                            && u.password().is_none()
                    }))
            {
                return Err(AppError::bad_request("Invalid recommendation mirror URL"));
            }
            if get_setting_json::<Option<String>>(conn, key)? != url {
                conn.execute(
                    "DELETE FROM remote_cache WHERE cache_key='catalog_recommend_douban'",
                    [],
                )?;
                if let Some(url) = url {
                    set_setting_json(conn, key, &url)?;
                } else {
                    conn.execute("DELETE FROM app_settings WHERE key=?", [key])?;
                }
            }
        }
        conn.execute(
            "INSERT INTO settings(key,value) VALUES('adult_sources_enabled',?)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [if self.adult_sources_enabled { "1" } else { "0" }],
        )?;
        epg::store_aliases(conn, self.epg_aliases)
    }
}

#[derive(Default)]
struct SourceIdentities {
    ids: HashSet<i64>,
    urls: HashSet<String>,
}

impl SourceIdentities {
    fn record(&mut self, id: i64, url: String) -> Result<(), AppError> {
        if id <= 0 || !self.ids.insert(id) || !self.urls.insert(url) {
            return Err(AppError::bad_request(
                "Invalid or duplicate source identity",
            ));
        }
        Ok(())
    }
}

fn source_error(error: rusqlite::Error) -> AppError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        return AppError(
            StatusCode::CONFLICT,
            "Backup source ID conflicts with an existing source".into(),
        );
    }
    error.into()
}

// Match an existing URL first. A new URL uses its original ID, or fails if that
// ID belongs to another source. Never delete and reinsert rows referenced by users.
fn restore_catalog(
    conn: &Connection,
    entries: Vec<SourceEntry<SourceInput>>,
) -> Result<(), AppError> {
    let mut seen = SourceIdentities::default();
    let mut stmt = conn.prepare(
        "INSERT INTO catalog_sources(id,url,name,enabled,adult,priority,searchable,custom_ua,
         custom_referer,custom_cookie,proxy_url,timeout_secs)
         VALUES(COALESCE((SELECT id FROM catalog_sources WHERE url=?2),?1),?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
         ON CONFLICT(url) DO UPDATE SET name=excluded.name, enabled=excluded.enabled,
         adult=excluded.adult,priority=excluded.priority,searchable=excluded.searchable,
         custom_ua=excluded.custom_ua,custom_referer=excluded.custom_referer,custom_cookie=excluded.custom_cookie,
         proxy_url=excluded.proxy_url,timeout_secs=excluded.timeout_secs",
    )?;
    for SourceEntry { id, config } in entries {
        let (name, url) = validate_source(&config)?;
        stmt.execute(params![
            id,
            url,
            name,
            config.enabled,
            config.adult,
            config.priority.unwrap_or(100),
            config.searchable.unwrap_or(true),
            config.custom_ua.unwrap_or_default(),
            config.custom_referer.unwrap_or_default(),
            config.custom_cookie.unwrap_or_default(),
            config.proxy_url.unwrap_or_default(),
            config.timeout_secs.unwrap_or(12)
        ])
        .map_err(source_error)?;
        seen.record(id, url)?;
    }
    Ok(())
}

fn restore_live(
    conn: &Connection,
    entries: Vec<SourceEntry<SourceConfig>>,
) -> Result<(), AppError> {
    let mut seen = SourceIdentities::default();
    let mut stmt = conn.prepare(
        "INSERT INTO live_sources(id,url,name,enabled)
         VALUES(COALESCE((SELECT id FROM live_sources WHERE url=?2),?1),?2,?3,?4)
         ON CONFLICT(url) DO UPDATE SET name=excluded.name,enabled=excluded.enabled",
    )?;
    for SourceEntry { id, config } in entries {
        let config = config.validate()?;
        stmt.execute(params![id, config.url, config.name, config.enabled])
            .map_err(source_error)?;
        seen.record(id, config.url)?;
    }
    Ok(())
}
