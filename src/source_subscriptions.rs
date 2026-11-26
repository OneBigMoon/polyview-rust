mod formats;

use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    network::http_url,
    now,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
};
use formats::Candidate;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::task::JoinHandle;

const REFRESH_SECS: i64 = 12 * 60 * 60;
const RETRY_SECS: i64 = 15 * 60;
const FETCH_SECS: u64 = 180;
const KINDS: [&str; 3] = ["cms", "live", "parser"];

type SourceKey = (String, String);

// Identifiers come exclusively from this closed list, never from feed data.
fn target(kind: &str) -> Result<(&'static str, &'static str), AppError> {
    match kind {
        "cms" => Ok(("catalog_sources", "url")),
        "live" => Ok(("live_sources", "url")),
        "parser" => Ok(("parsers", "url_template")),
        _ => Err(AppError::bad_request("Unknown subscription source kind")),
    }
}

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS source_subscriptions (
            id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, url TEXT NOT NULL UNIQUE,
            enabled INTEGER NOT NULL DEFAULT 1, selected TEXT, updated_at INTEGER, error TEXT,
            next_sync_at INTEGER NOT NULL DEFAULT 0, claim TEXT, claim_until INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS parsers (
            id INTEGER PRIMARY KEY, name TEXT NOT NULL, url_template TEXT NOT NULL UNIQUE,
            headers_json TEXT NOT NULL DEFAULT '{}', flags TEXT NOT NULL DEFAULT '*',
            priority INTEGER NOT NULL DEFAULT 100, timeout_ms INTEGER NOT NULL DEFAULT 10000,
            enabled INTEGER NOT NULL DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS source_subscription_items (
            subscription_id INTEGER NOT NULL REFERENCES source_subscriptions(id) ON DELETE CASCADE,
            kind TEXT NOT NULL, url TEXT NOT NULL, name TEXT NOT NULL,
            PRIMARY KEY(subscription_id,kind,url)
        );
        CREATE INDEX IF NOT EXISTS subscription_items_source
            ON source_subscription_items(kind,url,subscription_id);
        CREATE TABLE IF NOT EXISTS subscription_owned_sources (
            kind TEXT NOT NULL, url TEXT NOT NULL, missing INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(kind,url)
        );",
    )?;
    for kind in KINDS {
        let (table, url) = target(kind)?;
        // A manually replaced row at a previously imported URL starts with fresh ownership.
        let cleanup = format!(
            "DELETE FROM subscription_owned_sources WHERE kind='{kind}' AND url=OLD.{url};
             DELETE FROM source_subscription_items WHERE kind='{kind}' AND url=OLD.{url};"
        );
        conn.execute_batch(&format!(
            "CREATE TRIGGER IF NOT EXISTS feed_delete_{kind} AFTER DELETE ON {table}
                BEGIN {cleanup} END;
             CREATE TRIGGER IF NOT EXISTS feed_move_{kind} AFTER UPDATE OF {url} ON {table}
                WHEN NEW.{url} != OLD.{url} BEGIN {cleanup} END;"
        ))?;
    }
    Ok(())
}

pub fn router() -> Router<Shared> {
    Router::new()
        .route("/api/source-subscriptions", get(list).post(create))
        .route("/api/source-subscriptions/{id}", put(update).delete(remove))
        .route("/api/source-subscriptions/preview", post(preview))
        .route("/api/source-subscriptions/sync", post(sync))
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Deserialize, Serialize)]
pub(crate) struct SubscriptionConfig {
    pub name: String,
    pub url: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub selected: Option<Vec<String>>,
}

fn validate_name(name: &str) -> Result<(), AppError> {
    if name.trim().is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(AppError::bad_request("Invalid subscription source name"));
    }
    Ok(())
}

fn validate_key(kind: &str, url: &str) -> Result<(), AppError> {
    target(kind)?;
    if url.len() > 4096 || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(AppError::bad_request("Invalid subscription source URL"));
    }
    http_url(url)?;
    Ok(())
}

impl SubscriptionConfig {
    fn validate(&mut self) -> Result<(), AppError> {
        validate_name(&self.name)?;
        self.name = self.name.trim().to_owned();
        self.url = self.url.trim().to_owned();
        formats::validate_input(&self.url)?;
        if let Some(selected) = &mut self.selected {
            for key in selected.iter() {
                let (kind, url) = key
                    .split_once(':')
                    .ok_or_else(|| AppError::bad_request("Invalid subscription selection"))?;
                validate_key(kind, url)?;
            }
            selected.sort_unstable();
            selected.dedup();
        }
        Ok(())
    }

    fn selection_json(&self) -> Result<Option<String>, AppError> {
        self.selected
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::internal("Could not serialize source selection"))
    }
}

#[derive(Serialize)]
struct Subscription {
    id: i64,
    #[serde(flatten)]
    config: SubscriptionConfig,
    updated_at: Option<i64>,
    error: Option<String>,
    item_count: i64,
}

const SUBSCRIPTIONS: &str = "SELECT id,name,url,enabled,selected,updated_at,error,
    (SELECT count(*) FROM source_subscription_items WHERE subscription_id=s.id)
    FROM source_subscriptions s";

fn subscription(row: &Row<'_>) -> rusqlite::Result<Subscription> {
    let selected: Option<String> = row.get(4)?;
    Ok(Subscription {
        id: row.get(0)?,
        config: SubscriptionConfig {
            name: row.get(1)?,
            url: row.get(2)?,
            enabled: row.get(3)?,
            selected: selected
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?,
        },
        updated_at: row.get(5)?,
        error: row.get(6)?,
        item_count: row.get(7)?,
    })
}

fn subscriptions(conn: &Connection) -> Result<Vec<Subscription>, AppError> {
    let mut stmt = conn.prepare(&format!("{SUBSCRIPTIONS} ORDER BY id"))?;
    Ok(stmt
        .query_map([], subscription)?
        .collect::<Result<_, _>>()?)
}

fn by_id(conn: &Connection, id: i64) -> Result<Subscription, AppError> {
    conn.query_row(&format!("{SUBSCRIPTIONS} WHERE id=?"), [id], subscription)
        .optional()?
        .ok_or_else(|| AppError::not_found("Source subscription not found"))
}

async fn list(
    State(state): State<Shared>,
    user: AuthUser,
) -> Result<Json<Vec<Subscription>>, AppError> {
    require_admin(&user)?;
    let conn = state.conn()?;
    Ok(Json(subscriptions(&conn)?))
}

fn write_error(error: rusqlite::Error) -> AppError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
        return AppError(StatusCode::CONFLICT, "Duplicate source subscription".into());
    }
    error.into()
}

async fn create(
    State(state): State<Shared>,
    user: AuthUser,
    Json(mut config): Json<SubscriptionConfig>,
) -> Result<(StatusCode, Json<Subscription>), AppError> {
    require_admin(&user)?;
    config.validate()?;
    let conn = state.conn()?;
    conn.execute(
        "INSERT INTO source_subscriptions(name,url,enabled,selected) VALUES(?,?,?,?)",
        params![
            config.name,
            config.url,
            config.enabled,
            config.selection_json()?
        ],
    )
    .map_err(write_error)?;
    Ok((
        StatusCode::CREATED,
        Json(by_id(&conn, conn.last_insert_rowid())?),
    ))
}

async fn update(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(mut config): Json<SubscriptionConfig>,
) -> Result<Json<Subscription>, AppError> {
    require_admin(&user)?;
    config.validate()?;
    let conn = state.conn()?;
    conn.execute(
        "UPDATE source_subscriptions SET name=?,url=?,enabled=?,selected=?,
         next_sync_at=0,error=NULL,claim=NULL,claim_until=0 WHERE id=?",
        params![
            config.name,
            config.url,
            config.enabled,
            config.selection_json()?,
            id
        ],
    )
    .map_err(write_error)?;
    Ok(Json(by_id(&conn, id)?))
}

async fn remove(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    require_admin(&user)?;
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let previous = items(&tx, id)?;
    if tx.execute("DELETE FROM source_subscriptions WHERE id=?", [id])? == 0 {
        return Err(AppError::not_found("Source subscription not found"));
    }
    for item in previous {
        reconcile(&tx, &item.kind, &item.url)?;
    }
    tx.commit()?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PreviewInput {
    url: String,
}

#[derive(Serialize)]
struct Preview {
    items: Vec<Candidate>,
}

async fn preview(
    State(state): State<Shared>,
    user: AuthUser,
    Json(input): Json<PreviewInput>,
) -> Result<Json<Preview>, AppError> {
    require_admin(&user)?;
    Ok(Json(Preview {
        items: fetch(&state, &input.url).await?,
    }))
}

async fn fetch(state: &Shared, url: &str) -> Result<Vec<Candidate>, AppError> {
    tokio::time::timeout(
        Duration::from_secs(FETCH_SECS),
        formats::preview(&state.client, url),
    )
    .await
    .map_err(|_| {
        AppError(
            StatusCode::BAD_GATEWAY,
            "Subscription fetch timed out".into(),
        )
    })?
}

fn items(conn: &Connection, id: i64) -> Result<Vec<Candidate>, AppError> {
    let mut stmt = conn.prepare_cached(
        "SELECT kind,name,url FROM source_subscription_items WHERE subscription_id=? ORDER BY kind,url",
    )?;
    Ok(stmt
        .query_map([id], |r| {
            Ok(Candidate {
                kind: r.get(0)?,
                name: r.get(1)?,
                url: r.get(2)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

// Each feed keeps its own last complete snapshot. The lowest subscription ID
// supplies the displayed name when several feeds describe the same target.
fn reconcile(conn: &Connection, kind: &str, url: &str) -> Result<(), AppError> {
    let (table, column) = target(kind)?;
    let name: Option<String> = conn
        .prepare_cached("SELECT name FROM source_subscription_items WHERE kind=? AND url=? ORDER BY subscription_id LIMIT 1")?
        .query_row((kind, url), |r| r.get(0)).optional()?;
    let was_missing: Option<bool> = conn
        .prepare_cached("SELECT missing FROM subscription_owned_sources WHERE kind=? AND url=?")?
        .query_row((kind, url), |r| r.get(0))
        .optional()?;
    match (name, was_missing) {
        (Some(name), None) => {
            let inserted = conn.prepare_cached(&format!(
                "INSERT INTO {table}(name,{column}) VALUES(?,?) ON CONFLICT({column}) DO NOTHING"
            ))?.execute((&name, url))?;
            if inserted != 0 {
                conn.prepare_cached(
                    "INSERT INTO subscription_owned_sources(kind,url) VALUES(?,?)",
                )?
                .execute((kind, url))?;
            }
        }
        (Some(name), Some(missing)) => {
            conn.prepare_cached(&format!(
                "UPDATE {table} SET name=?,enabled=CASE WHEN ? THEN 1 ELSE enabled END WHERE {column}=?"
            ))?.execute((&name, missing, url))?;
            if missing {
                conn.prepare_cached(
                    "UPDATE subscription_owned_sources SET missing=0 WHERE kind=? AND url=?",
                )?
                .execute((kind, url))?;
            }
        }
        (None, Some(false)) => {
            // Retain the original row ID and all member history and favorites.
            conn.prepare_cached(&format!("UPDATE {table} SET enabled=0 WHERE {column}=?"))?
                .execute([url])?;
            conn.prepare_cached(
                "UPDATE subscription_owned_sources SET missing=1 WHERE kind=? AND url=?",
            )?
            .execute((kind, url))?;
        }
        (None, _) => {}
    }
    Ok(())
}

fn store_items(
    conn: &Connection,
    id: i64,
    candidates: &[Candidate],
    mut changed: impl FnMut(&str, &str) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let previous = items(conn, id)?;
    let mut old: HashMap<_, _> = previous
        .iter()
        .map(|c| ((c.kind.as_str(), c.url.as_str()), c.name.as_str()))
        .collect();
    let mut insert = conn.prepare_cached(
        "INSERT INTO source_subscription_items(subscription_id,kind,url,name) VALUES(?,?,?,?)
         ON CONFLICT(subscription_id,kind,url) DO UPDATE SET name=excluded.name",
    )?;
    for Candidate { kind, url, name } in candidates {
        if old.remove(&(kind.as_str(), url.as_str())) == Some(name.as_str()) {
            continue;
        }
        insert.execute(params![id, kind, url, name])?;
        changed(kind, url)?;
    }
    let mut remove = conn.prepare_cached(
        "DELETE FROM source_subscription_items WHERE subscription_id=? AND kind=? AND url=?",
    )?;
    for ((kind, url), _) in old {
        remove.execute(params![id, kind, url])?;
        changed(kind, url)?;
    }
    Ok(())
}

#[derive(Default, Deserialize)]
struct SyncInput {
    id: Option<i64>,
}

#[derive(Default, Serialize)]
struct SyncReport {
    synced: usize,
    failed: usize,
}

struct Claim {
    id: i64,
    token: String,
    config: SubscriptionConfig,
}

fn claim(state: &Shared, id: i64, due_only: bool) -> Result<Option<Claim>, AppError> {
    let conn = state.conn()?;
    let token = uuid::Uuid::new_v4().to_string();
    let timestamp = now();
    if conn.execute(
        "UPDATE source_subscriptions SET claim=?1,claim_until=?2 WHERE id=?3 AND enabled=1
         AND (claim IS NULL OR claim_until<=?4) AND (NOT ?5 OR next_sync_at<=?4)",
        params![
            token,
            timestamp + FETCH_SECS as i64 + 60,
            id,
            timestamp,
            due_only
        ],
    )? == 0
    {
        return Ok(None);
    }
    Ok(Some(Claim {
        id,
        token,
        config: by_id(&conn, id)?.config,
    }))
}

fn finish(
    state: &Shared,
    claim: Claim,
    result: Result<Vec<Candidate>, AppError>,
    report: &mut SyncReport,
) -> Result<(), AppError> {
    let mut conn = state.conn()?;
    let mut tx = conn.transaction()?;
    let timestamp = now();
    if tx.execute(
        "UPDATE source_subscriptions SET claim=NULL,claim_until=0,next_sync_at=? WHERE id=? AND claim=?",
        params![timestamp + RETRY_SECS, claim.id, claim.token],
    )? == 0 {
        return Ok(());
    }
    let result = result.and_then(|mut candidates| {
        if let Some(keys) = &claim.config.selected {
            let keys: HashSet<_> = keys.iter().filter_map(|key| key.split_once(':')).collect();
            candidates.retain(|c| keys.contains(&(c.kind.as_str(), c.url.as_str())));
        }
        let savepoint = tx.savepoint()?;
        store_items(&savepoint, claim.id, &candidates, |kind, url| {
            reconcile(&savepoint, kind, url)
        })?;
        savepoint.commit()?;
        Ok(())
    });
    match result {
        Ok(()) => {
            tx.execute(
                "UPDATE source_subscriptions SET updated_at=?,next_sync_at=?,error=NULL WHERE id=?",
                params![timestamp, timestamp + REFRESH_SECS, claim.id],
            )?;
            report.synced += 1;
        }
        Err(_) => {
            // Do not persist upstream messages: URLs and request errors can contain credentials.
            tx.execute(
                "UPDATE source_subscriptions SET error='Could not synchronize subscription' WHERE id=?",
                [claim.id],
            )?;
            report.failed += 1;
        }
    }
    tx.commit()?;
    Ok(())
}

async fn sync_enabled(
    state: &Shared,
    id: Option<i64>,
    due_only: bool,
) -> Result<SyncReport, AppError> {
    let ids = {
        let conn = state.conn()?;
        if let Some(id) = id {
            by_id(&conn, id)?;
            vec![id]
        } else {
            let mut stmt = conn.prepare(
                "SELECT id FROM source_subscriptions WHERE enabled=1 AND (NOT ? OR next_sync_at<=?) ORDER BY id",
            )?;
            stmt.query_map((due_only, now()), |r| r.get(0))?
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    let mut report = SyncReport::default();
    for id in ids {
        if let Some(claim) = claim(state, id, due_only)? {
            let result = fetch(state, &claim.config.url).await;
            finish(state, claim, result, &mut report)?;
        }
    }
    Ok(report)
}

async fn sync(
    State(state): State<Shared>,
    user: AuthUser,
    input: Option<Json<SyncInput>>,
) -> Result<Json<SyncReport>, AppError> {
    require_admin(&user)?;
    let id = input.and_then(|Json(input)| input.id);
    Ok(Json(sync_enabled(&state, id, false).await?))
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_secs(60));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticks.tick().await;
            if let Err(error) = sync_enabled(&state, None, true).await {
                tracing::warn!(error = %error.1, "Source subscription scheduler failed");
            }
        }
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionBackup {
    subscriptions: Vec<FeedBackup>,
    sources: Vec<SourceBackup>,
}

#[derive(Serialize, Deserialize)]
struct FeedBackup {
    #[serde(flatten)]
    config: SubscriptionConfig,
    items: Vec<Candidate>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceBackup {
    kind: String,
    url: String,
    managed: bool,
    missing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    parser: Option<ParserConfig>,
}

// Catalog and live row settings are already part of the configuration backup.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParserConfig {
    name: String,
    headers_json: String,
    flags: String,
    priority: i64,
    timeout_ms: i64,
    enabled: bool,
}

pub(crate) fn export_config(conn: &Connection) -> Result<SubscriptionBackup, AppError> {
    let subscriptions = subscriptions(conn)?
        .into_iter()
        .map(|sub| {
            Ok(FeedBackup {
                config: sub.config,
                items: items(conn, sub.id)?,
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;
    let mut stmt = conn.prepare(
        "SELECT kind,url,missing FROM subscription_owned_sources WHERE kind!='parser' ORDER BY kind,url",
    )?;
    let mut sources = stmt
        .query_map([], |r| {
            Ok(SourceBackup {
                kind: r.get(0)?,
                url: r.get(1)?,
                managed: true,
                missing: r.get(2)?,
                parser: None,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT p.url_template,o.missing,p.name,p.headers_json,p.flags,p.priority,p.timeout_ms,p.enabled
         FROM parsers p LEFT JOIN subscription_owned_sources o ON o.kind='parser' AND o.url=p.url_template
         ORDER BY p.url_template",
    )?;
    for row in stmt.query_map([], |r| {
        let missing: Option<bool> = r.get(1)?;
        Ok(SourceBackup {
            kind: "parser".into(),
            url: r.get(0)?,
            managed: missing.is_some(),
            missing: missing.unwrap_or(false),
            parser: Some(ParserConfig {
                name: r.get(2)?,
                headers_json: r.get(3)?,
                flags: r.get(4)?,
                priority: r.get(5)?,
                timeout_ms: r.get(6)?,
                enabled: r.get(7)?,
            }),
        })
    })? {
        sources.push(row?);
    }
    Ok(SubscriptionBackup {
        subscriptions,
        sources,
    })
}

pub(crate) fn existing_manual_urls(conn: &Connection) -> Result<HashSet<SourceKey>, AppError> {
    let mut keys = HashSet::new();
    for kind in KINDS {
        let (table, column) = target(kind)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {column} FROM {table} s WHERE NOT EXISTS
             (SELECT 1 FROM subscription_owned_sources WHERE kind=? AND url=s.{column})"
        ))?;
        for row in stmt.query_map([kind], |r| r.get(0))? {
            keys.insert((kind.to_owned(), row?));
        }
    }
    Ok(keys)
}

fn require_target(conn: &Connection, kind: &str, url: &str) -> Result<(), AppError> {
    let (table, column) = target(kind)?;
    let exists: bool = conn
        .prepare_cached(&format!(
            "SELECT EXISTS(SELECT 1 FROM {table} WHERE {column}=?)"
        ))?
        .query_row([url], |r| r.get(0))?;
    if !exists {
        return Err(AppError::bad_request("Backup references a missing source"));
    }
    Ok(())
}

impl ParserConfig {
    fn restore(self, conn: &Connection, url: &str) -> Result<(), AppError> {
        crate::parsers::validate_config(
            &self.name,
            url,
            &self.headers_json,
            &self.flags,
            self.timeout_ms,
        )?;
        conn.execute(
            "INSERT INTO parsers(url_template,name,headers_json,flags,priority,timeout_ms,enabled)
             VALUES(?,?,?,?,?,?,?) ON CONFLICT(url_template) DO UPDATE SET name=excluded.name,
             headers_json=excluded.headers_json,flags=excluded.flags,priority=excluded.priority,
             timeout_ms=excluded.timeout_ms,enabled=excluded.enabled",
            params![
                url,
                self.name,
                self.headers_json,
                self.flags,
                self.priority,
                self.timeout_ms,
                self.enabled
            ],
        )?;
        Ok(())
    }
}

// Called inside the same transaction that has just restored catalog/live rows.
// Local manual targets stay manual even when the incoming backup owned their URLs.
pub(crate) fn restore_config(
    conn: &Connection,
    backup: SubscriptionBackup,
    existing_manual: &HashSet<SourceKey>,
) -> Result<(), AppError> {
    let mut seen = HashSet::new();
    for source in backup.sources {
        validate_key(&source.kind, &source.url)?;
        if (source.kind == "parser") != source.parser.is_some()
            || (!source.managed && (source.missing || source.kind != "parser"))
        {
            return Err(AppError::bad_request("Invalid backup source ownership"));
        }
        if let Some(parser) = source.parser {
            parser.restore(conn, &source.url)?;
        } else {
            require_target(conn, &source.kind, &source.url)?;
        }
        let key = (source.kind, source.url);
        if source.managed && !existing_manual.contains(&key) {
            conn.execute(
                "INSERT INTO subscription_owned_sources(kind,url,missing) VALUES(?,?,?)
                 ON CONFLICT(kind,url) DO UPDATE SET missing=excluded.missing",
                params![key.0, key.1, source.missing],
            )?;
        }
        if !seen.insert(key) {
            return Err(AppError::bad_request("Duplicate backup source"));
        }
    }
    let mut feed_urls = HashSet::new();
    let mut changed = HashSet::new();
    for mut feed in backup.subscriptions {
        feed.config.validate()?;
        let config = feed.config;
        if !feed_urls.insert(config.url.clone()) {
            return Err(AppError::bad_request("Duplicate backup subscription"));
        }
        let id = conn.query_row(
            "INSERT INTO source_subscriptions(name,url,enabled,selected) VALUES(?,?,?,?)
             ON CONFLICT(url) DO UPDATE SET name=excluded.name,enabled=excluded.enabled,
             selected=excluded.selected,updated_at=NULL,error=NULL,next_sync_at=0,claim=NULL,claim_until=0
             RETURNING id",
            params![config.name, config.url, config.enabled, config.selection_json()?],
            |r| r.get(0),
        )?;
        let mut unique = HashSet::new();
        for item in &feed.items {
            validate_key(&item.kind, &item.url)?;
            validate_name(&item.name)?;
            require_target(conn, &item.kind, &item.url)?;
            if !unique.insert((&item.kind, &item.url)) {
                return Err(AppError::bad_request("Duplicate backup subscription item"));
            }
        }
        store_items(conn, id, &feed.items, |kind, url| {
            changed.insert((kind.to_owned(), url.to_owned()));
            Ok(())
        })?;
    }
    // Snapshot target rows already carry their exact display names and enabled flags.
    // Reconcile only local managed rows affected by replacing a feed's last snapshot.
    for (kind, url) in changed.difference(&seen) {
        reconcile(conn, kind, url)?;
    }
    // A feed outside the backup may still advertise a source marked absent by the backup.
    let mut stmt = conn.prepare(
        "SELECT kind,url FROM subscription_owned_sources o WHERE missing = EXISTS
         (SELECT 1 FROM source_subscription_items i WHERE i.kind=o.kind AND i.url=o.url)",
    )?;
    let mismatched = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<Vec<SourceKey>, _>>()?;
    for (kind, url) in mismatched {
        reconcile(conn, &kind, &url)?;
    }
    Ok(())
}
