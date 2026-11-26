//! Explicit, non-destructive import from the original PolyView database.
use crate::{AppError, Shared, now};
use argon2::PasswordHash;
use axum::http::StatusCode;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path, time::Duration};
use url::Url;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImportReport {
    pub already_imported: bool,
    pub users_imported: usize,
    pub users_matched: usize,
    pub disabled_users_skipped: usize,
    pub unsupported_passwords_skipped: usize,
    pub sources_imported: usize,
    pub sources_matched: usize,
    pub sources_skipped: usize,
    pub subscriptions_imported: usize,
    pub subscriptions_skipped: usize,
    pub auto_downloads_held: usize,
    pub tv_tokens_imported: usize,
    pub tv_tokens_skipped: usize,
    #[serde(default)]
    pub progress_imported: usize,
    #[serde(default)]
    pub catalog_favorites_imported: usize,
    #[serde(default)]
    pub channel_favorites_imported: usize,
    #[serde(default)]
    pub search_history_imported: usize,
    #[serde(default)]
    pub skip_configs_imported: usize,
    #[serde(default)]
    pub skip_configs_retained_by_reason: BTreeMap<String, i64>,
    pub legacy_jobs: i64,
    pub legacy_unfinished_jobs: i64,
    pub legacy_rows_retained: BTreeMap<String, i64>,
    pub telegram_namespace: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct LegacySource {
    key: String,
    name: String,
    kind: String,
    url: String,
    enabled: bool,
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, AppError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table],
        |row| row.get(0),
    )?)
}

fn valid_url(raw: &str) -> Option<String> {
    if raw.len() > 4096 || raw.chars().any(char::is_control) {
        return None;
    }
    let mut url = Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

fn supported_password(raw: &str) -> bool {
    PasswordHash::new(raw).is_ok_and(|hash| {
        matches!(hash.algorithm.as_str(), "argon2id" | "argon2i" | "argon2d")
            && hash.salt.is_some()
            && hash.hash.is_some()
    })
}

fn config_sources(config: &Value) -> Vec<LegacySource> {
    config
        .get("sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|source| {
            let text = |key: &str| {
                source
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned()
            };
            LegacySource {
                key: text("key"),
                name: text("name"),
                kind: source
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("cms")
                    .into(),
                url: text("api"),
                enabled: !source
                    .get("disabled")
                    .and_then(|value| {
                        value
                            .as_bool()
                            .or_else(|| value.as_i64().map(|value| value != 0))
                    })
                    .unwrap_or(false),
            }
        })
        .collect()
}

fn init_import_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS legacy_imports (
            path TEXT PRIMARY KEY, imported_at INTEGER NOT NULL, report_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS legacy_user_map (
            legacy_id INTEGER PRIMARY KEY,
            user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS legacy_source_map (
            legacy_key TEXT PRIMARY KEY,
            source_id INTEGER NOT NULL REFERENCES catalog_sources(id) ON DELETE CASCADE
         );",
    )?;
    crate::ensure_column(
        conn,
        "legacy_imports",
        "version",
        "INTEGER NOT NULL DEFAULT 1",
    )
}

/// Supplements a previous explicit import when its original backup is still available.
pub fn supplement_legacy(state: &Shared) -> Result<Option<ImportReport>, AppError> {
    let path: Option<String> = {
        let conn = state.conn()?;
        if !table_exists(&conn, "legacy_imports")? {
            return Ok(None);
        }
        init_import_schema(&conn)?;
        conn.query_row(
            "SELECT path FROM legacy_imports WHERE version < 3",
            [],
            |r| r.get(0),
        )
        .optional()?
    };
    if let Some(path) = path
        && Path::new(&path).try_exists()?
    {
        return import_legacy(state, Path::new(&path), None).map(Some);
    }
    Ok(None)
}

/// Imports once per canonical database path. The source is opened read-only and never modified.
pub fn import_legacy(
    state: &Shared,
    db_path: &Path,
    config_path: Option<&Path>,
) -> Result<ImportReport, AppError> {
    let path = match db_path.canonicalize() {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let absolute = std::path::absolute(db_path)?;
            match (
                absolute.parent().and_then(|p| p.canonicalize().ok()),
                absolute.file_name(),
            ) {
                (Some(parent), Some(name)) => parent.join(name),
                _ => absolute,
            }
        }
        Err(e) => return Err(e.into()),
    };
    let path = path
        .to_str()
        .ok_or_else(|| AppError::bad_request("Legacy database path is not valid UTF-8"))?;
    let mut conn = state.conn()?;
    init_import_schema(&conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous = tx
        .query_row(
            "SELECT report_json,version FROM legacy_imports WHERE path=?",
            [path],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let previous = if let Some((raw, version)) = previous {
        let mut report: ImportReport = serde_json::from_str(&raw)
            .map_err(|_| AppError::internal("Legacy import report is invalid"))?;
        if version >= 3 || !Path::new(path).try_exists()? {
            report.already_imported = true;
            return Ok(report);
        }
        Some((report, version))
    } else {
        None
    };
    if previous.is_none()
        && tx.query_row("SELECT EXISTS(SELECT 1 FROM legacy_imports)", [], |row| {
            row.get::<_, bool>(0)
        })?
    {
        return Err(AppError(
            StatusCode::CONFLICT,
            "A different legacy database was already imported".into(),
        ));
    }
    let legacy = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    legacy.busy_timeout(Duration::from_secs(5))?;
    legacy.execute_batch("BEGIN")?;
    if !table_exists(&legacy, "users")? || !table_exists(&legacy, "source_config")? {
        return Err(AppError::bad_request(
            "This is not an original PolyView database",
        ));
    }
    if let Some((mut report, version)) = previous {
        if version < 2 {
            import_user_data(&legacy, &tx, &mut report)?;
        }
        import_skip_configs(&legacy, &tx, &mut report)?;
        save_report(&tx, path, &report)?;
        tx.commit()?;
        return Ok(report);
    }
    let config = match config_path {
        Some(path) => {
            let file = std::fs::read(path)?;
            if file.len() > 8 * 1024 * 1024 {
                return Err(AppError::bad_request("Legacy config exceeds 8 MiB"));
            }
            serde_json::from_slice::<Value>(&file)
                .map_err(|_| AppError::bad_request("Legacy config is not valid JSON"))?
        }
        None => Value::Null,
    };
    let mut report = ImportReport::default();
    import_users(&legacy, &tx, &mut report)?;
    import_sources(&legacy, &tx, &config, &mut report)?;
    import_subscriptions(&legacy, &tx, &mut report)?;
    import_tokens(&legacy, &tx, &mut report)?;
    import_user_data(&legacy, &tx, &mut report)?;
    import_skip_configs(&legacy, &tx, &mut report)?;
    for table in [
        "play_progress",
        "favorites",
        "live_favorites",
        "search_history",
        "sessions",
    ] {
        if table_exists(&legacy, table)? {
            let count: i64 =
                legacy.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?;
            report.legacy_rows_retained.insert(table.into(), count);
        }
    }
    if table_exists(&legacy, "download_jobs")? {
        report.legacy_jobs =
            legacy.query_row("SELECT COUNT(*) FROM download_jobs", [], |row| row.get(0))?;
        report.legacy_unfinished_jobs = legacy.query_row(
            "SELECT COUNT(*) FROM download_jobs WHERE status NOT IN ('completed','cancelled')",
            [],
            |row| row.get(0),
        )?;
    }
    report.telegram_namespace = config
        .get("downloads")
        .and_then(|downloads| downloads.get("telegram_namespace"))
        .and_then(Value::as_str)
        .filter(|namespace| {
            !namespace.is_empty()
                && namespace.len() <= 100
                && namespace
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        })
        .map(str::to_owned);
    report.warnings.push("Original database, completed media files, and Telegram storage are retained; this import does not move or delete them.".into());
    if report.auto_downloads_held > 0 {
        report.warnings.push("Imported automatic downloads are held off to avoid redownloading existing episodes; review each subscription before enabling.".into());
    }
    if report.legacy_jobs > 0 {
        report.warnings.push("Legacy download job history remains in the original database; unfinished jobs are not automatically resumed by this importer.".into());
    }
    save_report(&tx, path, &report)?;
    tx.commit()?;
    Ok(report)
}

// The original CMS media identity is either source_key:vod_id, or a bare vod_id
// accompanied by a separate source_key. Destination IDs come only from our import maps.
fn catalog_identity<'a>(source: &'a str, media: &'a str) -> Option<(&'a str, &'a str)> {
    let (key, id) = match media.split_once(':') {
        Some((key, id)) if source.is_empty() || source == key => (key, id),
        None if !source.is_empty() => (source, media),
        _ => return None,
    };
    (!key.is_empty()
        && !id.is_empty()
        && id.chars().count() <= 256
        && !id.chars().any(char::is_control))
    .then_some((key, id))
}

// Original releases added metadata columns over time. Only the user and media
// identities are mandatory. All identifiers passed here are application constants.
fn legacy_media_projection(
    legacy: &Connection,
    table: &str,
    optional: &[&str],
) -> Result<String, AppError> {
    let query = legacy.prepare(&format!("SELECT * FROM {table} LIMIT 0"))?;
    let mut projection = String::from("SELECT user_id,media_id");
    for &column in optional {
        projection.push(',');
        if query.column_index(column).is_err() {
            projection.push_str("NULL AS ");
        }
        projection.push_str(column);
    }
    Ok(format!("{projection} FROM {table}"))
}

fn import_user_data(
    legacy: &Connection,
    tx: &Transaction<'_>,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    if table_exists(legacy, "play_progress")? {
        let projection = legacy_media_projection(
            legacy,
            "play_progress",
            &[
                "source_key",
                "title",
                "episode",
                "position_sec",
                "duration_sec",
                "updated_at",
                "cover",
            ],
        )?;
        let mut stmt = legacy.prepare(&format!("SELECT user_id,COALESCE(source_key,''),media_id,COALESCE(NULLIF(title,''),media_id),
            MAX(1,COALESCE(episode,1)),position_sec,duration_sec,COALESCE(unixepoch(updated_at,'auto'),0),COALESCE(cover,'')
            FROM ({projection}) WHERE position_sec >= 0 AND duration_sec > 0"))?;
        let mut rows = stmt.query([])?;
        let mut insert = tx.prepare("INSERT INTO online_progress(user_id,source_id,media_id,title,episode_index,episode_name,position,duration,updated_at,cover)
            SELECT u.user_id,s.source_id,?3,?4,?5-1,'第'||?5||'集',?6,?7,?8,?9
            FROM legacy_user_map u CROSS JOIN legacy_source_map s WHERE u.legacy_id=?1 AND s.legacy_key=?2 ON CONFLICT DO NOTHING")?;
        while let Some(row) = rows.next()? {
            let source = row.get::<_, String>(1)?;
            let media = row.get::<_, String>(2)?;
            if let Some((key, id)) = catalog_identity(&source, &media) {
                report.progress_imported += insert.execute(params![
                    row.get::<_, i64>(0)?,
                    key,
                    id,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, f64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?
                ])?;
            }
        }
    }
    if table_exists(legacy, "favorites")? {
        let projection = legacy_media_projection(
            legacy,
            "favorites",
            &["title", "search_title", "cover", "year", "created_at"],
        )?;
        let mut stmt = legacy.prepare(&format!("SELECT user_id,media_id,COALESCE(NULLIF(title,''),NULLIF(search_title,''),media_id),
            COALESCE(cover,''),CASE WHEN year>0 THEN CAST(year AS TEXT) ELSE '' END,COALESCE(unixepoch(created_at,'auto'),0) FROM ({projection})"))?;
        let mut rows = stmt.query([])?;
        let mut insert = tx.prepare("INSERT INTO catalog_favorites(user_id,source_id,media_id,title,cover,year,created_at)
            SELECT u.user_id,s.source_id,?3,?4,?5,?6,?7 FROM legacy_user_map u CROSS JOIN legacy_source_map s
            WHERE u.legacy_id=?1 AND s.legacy_key=?2 ON CONFLICT DO NOTHING")?;
        while let Some(row) = rows.next()? {
            let media = row.get::<_, String>(1)?;
            if let Some((key, id)) = catalog_identity("", &media) {
                report.catalog_favorites_imported += insert.execute(params![
                    row.get::<_, i64>(0)?,
                    key,
                    id,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?
                ])?;
            }
        }
    }
    if table_exists(legacy, "live_favorites")? {
        let mut stmt = legacy.prepare("SELECT user_id,TRIM(url),TRIM(name),TRIM(grp),COALESCE(unixepoch(created_at,'auto'),0) FROM live_favorites")?;
        let mut rows = stmt.query([])?;
        let mut insert = tx.prepare("WITH candidates AS (
            SELECT c.id,CASE WHEN ?2<>'' AND (c.url=?2 OR EXISTS(SELECT 1 FROM channel_sources s WHERE s.channel_id=c.id AND s.url=?2)) THEN 1
                WHEN c.name=?3 AND c.group_name=?4 THEN 2 WHEN c.name=?3 THEN 3 END priority FROM channels c),
            matched AS (SELECT min(id) AS id FROM candidates WHERE priority=(SELECT min(priority) FROM candidates) HAVING COUNT(*)=1)
            INSERT INTO channel_favorites(user_id,channel_id,created_at) SELECT u.user_id,m.id,?5 FROM legacy_user_map u CROSS JOIN matched m
            WHERE u.legacy_id=?1 ON CONFLICT DO NOTHING")?;
        while let Some(row) = rows.next()? {
            report.channel_favorites_imported += insert.execute(params![
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?
            ])?;
        }
    }
    if table_exists(legacy, "search_history")? {
        report.search_history_imported += import_search_history(legacy, tx)?;
    }
    Ok(())
}

fn import_search_history(legacy: &Connection, tx: &Transaction<'_>) -> Result<usize, AppError> {
    let mut users = tx.prepare("SELECT legacy_id,user_id FROM legacy_user_map")?;
    let mut history = legacy.prepare("SELECT TRIM(kw),MAX(COALESCE(ts,0)) FROM search_history WHERE user_id=? AND TRIM(kw)<>'' GROUP BY TRIM(kw) ORDER BY 2 DESC,1")?;
    let mut users = users.query([])?;
    let mut count = 0;
    while let Some(user) = users.next()? {
        let user_id: i64 = user.get(1)?;
        let slots: usize = tx.query_row(
            "SELECT MAX(0,20-COUNT(*)) FROM search_history WHERE user_id=?",
            [user_id],
            |r| r.get(0),
        )?;
        let mut entries = Vec::new();
        let mut rows = history.query([user.get::<_, i64>(0)?])?;
        while entries.len() < slots {
            let Some(row) = rows.next()? else { break };
            let term: String = row.get(0)?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM search_history WHERE user_id=? AND keyword=?)",
                params![user_id, term],
                |r| r.get(0),
            )?;
            if !exists {
                let ts: i64 = row.get(1)?;
                // Original clients write Date.now() in milliseconds. Older backups can use seconds.
                entries.push((term, if ts >= 100_000_000_000 { ts / 1000 } else { ts }));
            }
        }
        // rowid breaks same-second ties in the history API, so insert oldest first.
        for (term, ts) in entries.into_iter().rev() {
            count += tx.execute(
                "INSERT INTO search_history(user_id,keyword,updated_at) VALUES(?,?,?)",
                params![user_id, term, ts],
            )?;
        }
    }
    Ok(count)
}

fn import_skip_configs(
    legacy: &Connection,
    tx: &Transaction<'_>,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    if !table_exists(legacy, "skip_configs")? {
        return Ok(());
    }
    let mut select = legacy.prepare(
        "SELECT user_id,media_id,enabled,intro_sec,outro_sec FROM skip_configs ORDER BY user_id,media_id",
    )?;
    let mut users = tx.prepare("SELECT user_id FROM legacy_user_map WHERE legacy_id=?")?;
    let mut sources = tx.prepare("SELECT source_id FROM legacy_source_map WHERE legacy_key=?")?;
    let mut insert = tx.prepare(
        "INSERT INTO skip_configs(user_id,media_key,enabled,intro_sec,outro_sec) VALUES(?,?,?,?,?)
         ON CONFLICT(user_id,media_key) DO NOTHING",
    )?;
    let mut rows = select.query([])?;
    while let Some(row) = rows.next()? {
        let mut retain = |reason: &str| {
            *report
                .skip_configs_retained_by_reason
                .entry(reason.into())
                .or_default() += 1;
        };
        let owner: Option<i64> = users
            .query_row([row.get::<_, rusqlite::types::Value>(0)?], |r| r.get(0))
            .optional()?;
        let Some(owner) = owner else {
            retain("unmapped user");
            continue;
        };
        // The legacy importer has no local library ID map. A matching numeric ID
        // in the destination cannot establish that it identifies the same file.
        let media = row.get_ref(1)?.as_str().unwrap_or("");
        let Some((source, media_id)) = catalog_identity("", media) else {
            retain("unmapped media");
            continue;
        };
        let mapped: Option<i64> = sources.query_row([source], |r| r.get(0)).optional()?;
        let Some(source_id) = mapped else {
            retain("unmapped source");
            continue;
        };
        let (Ok(enabled @ (0 | 1)), Ok(intro), Ok(outro)) = (
            row.get::<_, i64>(2),
            row.get::<_, f64>(3),
            row.get::<_, f64>(4),
        ) else {
            retain("invalid skip values");
            continue;
        };
        if !(0.0..=600.0).contains(&intro) || !(0.0..=600.0).contains(&outro) {
            retain("invalid skip values");
            continue;
        }
        let key = format!("online:{source_id}:{media_id}");
        if insert.execute(params![owner, key, enabled, intro, outro])? == 0 {
            retain("existing destination setting");
        } else {
            report.skip_configs_imported += 1;
        }
    }
    report.legacy_rows_retained.insert(
        "skip_configs".into(),
        report.skip_configs_retained_by_reason.values().sum(),
    );
    Ok(())
}

fn save_report(tx: &Transaction<'_>, path: &str, report: &ImportReport) -> Result<(), AppError> {
    let encoded = serde_json::to_string(report)
        .map_err(|_| AppError::internal("Could not save legacy import report"))?;
    tx.execute(
        "INSERT INTO legacy_imports(path,imported_at,report_json,version) VALUES(?,?,?,3)
         ON CONFLICT(path) DO UPDATE SET report_json=excluded.report_json,version=excluded.version",
        params![path, now(), encoded],
    )?;
    Ok(())
}

fn import_users(
    legacy: &Connection,
    tx: &Transaction<'_>,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    let mut users =
        legacy.prepare("SELECT id,username,password_hash,role,disabled FROM users ORDER BY id")?;
    let rows = users.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, bool>(4)?,
        ))
    })?;
    for row in rows {
        let (legacy_id, username, hash, role, disabled) = row?;
        if disabled {
            report.disabled_users_skipped += 1;
            continue;
        }
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM users WHERE username=?",
                [&username],
                |row| row.get(0),
            )
            .optional()?;
        let user_id = if let Some(id) = existing {
            report.users_matched += 1;
            id
        } else {
            if !supported_password(&hash) {
                report.unsupported_passwords_skipped += 1;
                continue;
            }
            if username.is_empty()
                || username.chars().count() > 128
                || username.chars().any(char::is_control)
            {
                report.warnings.push(format!("Legacy user {legacy_id} has an invalid username and was retained only in the original database."));
                continue;
            }
            let role = if role == "admin" { "admin" } else { "user" };
            let occupied: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE id=?)",
                [legacy_id],
                |row| row.get(0),
            )?;
            if legacy_id > 0 && !occupied {
                tx.execute("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(?,?,?,?,?)", params![legacy_id,username,hash,role,now()])?;
            } else {
                tx.execute(
                    "INSERT INTO users(username,password_hash,role,created_at) VALUES(?,?,?,?)",
                    params![username, hash, role, now()],
                )?;
            }
            report.users_imported += 1;
            tx.last_insert_rowid()
        };
        tx.execute(
            "INSERT INTO legacy_user_map(legacy_id,user_id) VALUES(?,?)",
            params![legacy_id, user_id],
        )?;
    }
    Ok(())
}

fn import_sources(
    legacy: &Connection,
    tx: &Transaction<'_>,
    config: &Value,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    let mut sources: BTreeMap<String, LegacySource> = config_sources(config)
        .into_iter()
        .map(|source| (source.key.clone(), source))
        .collect();
    let mut stored = legacy.prepare("SELECT key,name,COALESCE(kind,'cms'),COALESCE(api,''),COALESCE(disabled,0) FROM source_config ORDER BY priority,key")?;
    for row in stored.query_map([], |row| {
        Ok(LegacySource {
            key: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            url: row.get(3)?,
            enabled: !row.get::<_, bool>(4)?,
        })
    })? {
        let mut source = row?;
        if let Some(configured) = sources.get(&source.key) {
            if source.url.trim().is_empty() {
                source.url.clone_from(&configured.url);
            }
            if source.name.trim().is_empty() {
                source.name.clone_from(&configured.name);
            }
            if source.kind.trim().is_empty() {
                source.kind.clone_from(&configured.kind);
            }
        }
        sources.insert(source.key.clone(), source);
    }
    for (_, source) in sources {
        let Some(url) = valid_url(&source.url) else {
            report.sources_skipped += 1;
            continue;
        };
        if (!source.kind.trim().is_empty() && !source.kind.trim().eq_ignore_ascii_case("cms"))
            || source.key.is_empty()
            || source.key.len() > 256
            || source.key.chars().any(char::is_control)
        {
            report.sources_skipped += 1;
            continue;
        }
        let name = if source.name.trim().is_empty() {
            &source.key
        } else {
            &source.name
        };
        let inserted = tx.execute("INSERT INTO catalog_sources(name,url,enabled) VALUES(?,?,?) ON CONFLICT(url) DO NOTHING",params![name,url,source.enabled])?;
        if inserted > 0 {
            report.sources_imported += 1;
        } else {
            report.sources_matched += 1;
        }
        let id: i64 = tx.query_row("SELECT id FROM catalog_sources WHERE url=?", [url], |row| {
            row.get(0)
        })?;
        tx.execute(
            "INSERT INTO legacy_source_map(legacy_key,source_id) VALUES(?,?)",
            params![source.key, id],
        )?;
    }
    Ok(())
}

fn import_subscriptions(
    legacy: &Connection,
    tx: &Transaction<'_>,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    if !table_exists(legacy, "subscriptions")? {
        return Ok(());
    }
    let automatic = if table_exists(legacy, "download_subscriptions")? {
        "COALESCE((SELECT enabled FROM download_subscriptions ds WHERE ds.subscription_id=s.id),0)"
    } else {
        "0"
    };
    let mut stmt=legacy.prepare(&format!("SELECT id,user_id,title,COALESCE(source_key,''),COALESCE(media_id,''),COALESCE(total_eps,0),COALESCE(watched_eps,0),COALESCE(last_found_eps,0),COALESCE(state,0),{automatic} FROM subscriptions s ORDER BY id"))?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, bool>(9)?,
        ))
    })?;
    for row in rows {
        let (
            _legacy_id,
            owner,
            title,
            mut key,
            mut media,
            total,
            watched,
            found,
            status,
            automatic,
        ) = row?;
        if key.is_empty() {
            if let Some((prefix, id)) = media.split_once(':') {
                key = prefix.to_owned();
                media = id.to_owned();
            }
        } else if let Some(id) = media.strip_prefix(&format!("{key}:")) {
            media = id.to_owned();
        }
        let mapped:Option<(i64,i64)>=tx.query_row("SELECT u.user_id,s.source_id FROM legacy_user_map u CROSS JOIN legacy_source_map s WHERE u.legacy_id=? AND s.legacy_key=?",params![owner,key],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let Some((user_id, source_id)) = mapped else {
            report.subscriptions_skipped += 1;
            continue;
        };
        if media.is_empty() || media.chars().count() > 256 || media.chars().any(char::is_control) {
            report.subscriptions_skipped += 1;
            continue;
        }
        let total = total.max(found).max(watched).max(0);
        let error = if status == 3 {
            Some("Imported paused subscription; automatic downloads remain disabled")
        } else if automatic {
            Some("Review imported episode history before enabling automatic downloads")
        } else {
            None
        };
        let inserted=tx.execute("INSERT INTO tracking_subscriptions(user_id,source_id,media_id,title,total_episodes,watched_episodes,auto_download,error) VALUES(?,?,?,?,?,?,0,?) ON CONFLICT(user_id,source_id,media_id) DO NOTHING",params![user_id,source_id,media,title,total,watched.max(0),error])?;
        if inserted > 0 {
            report.subscriptions_imported += 1;
            if automatic {
                report.auto_downloads_held += 1;
            }
        }
    }
    Ok(())
}

fn import_tokens(
    legacy: &Connection,
    tx: &Transaction<'_>,
    report: &mut ImportReport,
) -> Result<(), AppError> {
    if !table_exists(legacy, "library_tokens")? {
        return Ok(());
    }
    let mut stmt = legacy.prepare("SELECT user_id,token FROM library_tokens")?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })? {
        let (old_user, token) = row?;
        let mapped: Option<i64> = tx
            .query_row(
                "SELECT user_id FROM legacy_user_map WHERE legacy_id=?",
                [old_user],
                |row| row.get(0),
            )
            .optional()?;
        let Some(user) = mapped else {
            report.tv_tokens_skipped += 1;
            continue;
        };
        if token.len() != 64 || !token.bytes().all(|c| c.is_ascii_hexdigit()) {
            report.tv_tokens_skipped += 1;
            continue;
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO tvbox_tokens(user_id,token) VALUES(?,?)",
            params![user, token],
        )?;
        if inserted > 0 {
            report.tv_tokens_imported += 1;
        } else {
            report.tv_tokens_skipped += 1;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppState;
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn hash() -> String {
        Argon2::default()
            .hash_password(
                b"legacy-password",
                &SaltString::encode_b64(b"fixed-test-salt").unwrap(),
            )
            .unwrap()
            .to_string()
    }

    fn state(root: &Path) -> Shared {
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        crate::init_schema(&conn).unwrap();
        Arc::new(AppState {
            db: Mutex::new(conn),
            playback_grants: Default::default(),
            download_runtime: Default::default(),
            media_dir: root.to_path_buf(),
            download_dir: root.join("downloads"),
            client: reqwest::Client::new(),
            secure_cookie: false,
        })
    }

    fn legacy(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE users(id INTEGER PRIMARY KEY,username TEXT,password_hash TEXT,role TEXT,disabled INTEGER);CREATE TABLE source_config(key TEXT PRIMARY KEY,name TEXT,kind TEXT,api TEXT,disabled INTEGER,priority INTEGER);CREATE TABLE subscriptions(id INTEGER PRIMARY KEY,user_id INTEGER,title TEXT,source_key TEXT,media_id TEXT,total_eps INTEGER,watched_eps INTEGER,last_found_eps INTEGER,state INTEGER);CREATE TABLE download_subscriptions(subscription_id INTEGER,user_id INTEGER,enabled INTEGER);CREATE TABLE library_tokens(user_id INTEGER,token TEXT);CREATE TABLE download_jobs(id INTEGER,status TEXT);CREATE TABLE play_progress(user_id INTEGER,media_id TEXT);CREATE TABLE favorites(user_id INTEGER,media_id TEXT);").unwrap();
        conn
    }

    #[test]
    fn imports_accounts_sources_subscriptions_tokens_once_without_activating_downloads() {
        let temp = TempDir::new().unwrap();
        let state = state(temp.path());
        let path = temp.path().join("legacy.db");
        let old = legacy(&path);
        let password = hash();
        old.execute(
            "INSERT INTO users VALUES(7,'oldadmin',?,'admin',0)",
            [&password],
        )
        .unwrap();
        old.execute("INSERT INTO source_config VALUES('cms','Saved CMS',' Cms ','https://example.org/cms',0,1)",[]).unwrap();
        old.execute(
            "INSERT INTO subscriptions VALUES(1,7,'Old series','cms','cms:42',12,4,8,0)",
            [],
        )
        .unwrap();
        old.execute("INSERT INTO download_subscriptions VALUES(1,7,1)", [])
            .unwrap();
        old.execute("INSERT INTO library_tokens VALUES(7,?)", ["a".repeat(64)])
            .unwrap();
        old.execute_batch("INSERT INTO download_jobs VALUES(1,'completed'),(2,'paused');INSERT INTO favorites VALUES(7,'cms:42');").unwrap();
        let report = import_legacy(&state, &path, None).unwrap();
        assert_eq!(report.users_imported, 1);
        assert_eq!(report.sources_imported, 1);
        assert_eq!(report.subscriptions_imported, 1);
        assert_eq!(report.auto_downloads_held, 1);
        assert_eq!(report.tv_tokens_imported, 1);
        assert_eq!(report.legacy_jobs, 2);
        assert_eq!(report.legacy_unfinished_jobs, 1);
        assert_eq!(report.legacy_rows_retained["favorites"], 1);
        let conn = state.conn().unwrap();
        let user: (i64, String) = conn
            .query_row(
                "SELECT id,password_hash FROM users WHERE username='oldadmin'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(user, (7, password));
        let tracking: (String, i64, bool) = conn
            .query_row(
                "SELECT media_id,watched_episodes,auto_download FROM tracking_subscriptions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(tracking, ("42".into(), 4, false));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM downloads", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(conn);
        assert!(import_legacy(&state, &path, None).unwrap().already_imported);
        assert_eq!(
            old.query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            old.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='legacy_imports'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn existing_accounts_and_source_settings_survive_and_unsafe_accounts_stay_disabled() {
        let temp = TempDir::new().unwrap();
        let state = state(temp.path());
        let path = temp.path().join("legacy.db");
        let old = legacy(&path);
        let password = hash();
        old.execute(
            "INSERT INTO users VALUES(1,'same',?,'admin',0)",
            [&password],
        )
        .unwrap();
        old.execute(
            "INSERT INTO users VALUES(2,'disabled',?,'user',1)",
            [&password],
        )
        .unwrap();
        old.execute(
            "INSERT INTO users VALUES(3,'unsupported','plain-password','user',0)",
            [],
        )
        .unwrap();
        old.execute(
            "INSERT INTO users VALUES(4,'collision',?,'user',0)",
            [&password],
        )
        .unwrap();
        old.execute(
            "INSERT INTO source_config VALUES('cms','','cms','',1,1)",
            [],
        )
        .unwrap();
        state.conn().unwrap().execute_batch("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(9,'same','existing-hash','user',1),(4,'existing','other-hash','admin',1);INSERT INTO catalog_sources(name,url,enabled) VALUES('Unchanged','https://example.org/cms',0);").unwrap();
        let config = temp.path().join("config.json");
        std::fs::write(&config,serde_json::to_vec(&serde_json::json!({"sources":[{"key":"cms","name":"Import must not overwrite","api":"https://example.org/cms"},{"key":"alist","kind":"alist","api":"https://files.example.org"},{"key":"bad","api":"file:///private"}],"downloads":{"telegram_namespace":"polyview"}})).unwrap()).unwrap();
        let report = import_legacy(&state, &path, Some(&config)).unwrap();
        assert_eq!(report.users_matched, 1);
        assert_eq!(report.users_imported, 1);
        assert_eq!(report.disabled_users_skipped, 1);
        assert_eq!(report.unsupported_passwords_skipped, 1);
        assert_eq!(report.sources_matched, 1);
        assert_eq!(report.sources_skipped, 2);
        assert_eq!(report.telegram_namespace.as_deref(), Some("polyview"));
        let conn = state.conn().unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT password_hash FROM users WHERE username='same'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "existing-hash"
        );
        assert_eq!(
            conn.query_row(
                "SELECT user_id FROM legacy_user_map WHERE legacy_id=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            9
        );
        assert_ne!(
            conn.query_row(
                "SELECT user_id FROM legacy_user_map WHERE legacy_id=4",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            4
        );
        assert!(
            !conn
                .query_row(
                    "SELECT enabled FROM catalog_sources WHERE url='https://example.org/cms'",
                    [],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap()
        );
    }

    #[test]
    fn invalid_config_or_wrong_schema_does_not_partially_import_users() {
        let temp = TempDir::new().unwrap();
        let state = state(temp.path());
        let path = temp.path().join("legacy.db");
        let old = legacy(&path);
        old.execute("INSERT INTO users VALUES(1,'admin',?,'admin',0)", [hash()])
            .unwrap();
        let config = temp.path().join("bad.json");
        std::fs::write(&config, b"{").unwrap();
        assert!(import_legacy(&state, &path, Some(&config)).is_err());
        assert_eq!(
            state
                .conn()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM users", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            state
                .conn()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM legacy_imports", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn imports_legacy_media_channels_and_recent_searches_with_mapped_owners() {
        let temp = tempfile::tempdir().unwrap();
        let state = state(temp.path());
        let path = temp.path().join("original.db");
        let old = legacy(&path);
        old.execute_batch("INSERT INTO users VALUES(7,'matched','','user',0),(9,'disabled','','user',1);
            INSERT INTO source_config VALUES('cms','Original','cms','https://example.org/cms',0,1);
            DROP TABLE play_progress;
            CREATE TABLE play_progress(user_id INTEGER,media_id TEXT,source_key TEXT,title TEXT,episode INTEGER,position_sec REAL,duration_sec REAL,updated_at TEXT,cover TEXT);
            INSERT INTO play_progress VALUES(7,'cms:42','cms','Story',3,12.5,100,'2025-01-01T00:00:00Z','cover'),
                (9,'cms:42','cms','Disabled',1,2,20,NULL,NULL),(7,'unknown:44',NULL,'Missing',1,2,20,NULL,NULL);
            DROP TABLE favorites;
            CREATE TABLE favorites(user_id INTEGER,media_id TEXT,title TEXT,created_at TEXT,search_title TEXT,cover TEXT,year INTEGER);
            INSERT INTO favorites VALUES(7,'cms:42',NULL,'2025-01-01','Story','cover',2024),(7,'unknown:3','Missing',NULL,NULL,NULL,0);
            CREATE TABLE live_favorites(user_id INTEGER,channel_key TEXT,name TEXT,grp TEXT,url TEXT,created_at TEXT);
            INSERT INTO live_favorites VALUES(7,'backup','Old','','https://example.org/alt',NULL),(7,'name','News','B','',NULL),
                (7,'ambiguous','News','','',NULL),(9,'disabled','News','A','',NULL);
            CREATE TABLE search_history(user_id INTEGER,kw TEXT,ts INTEGER);
            WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<23)
            INSERT INTO search_history SELECT 7,'Word '||x,1700000000000+x FROM n;
            INSERT INTO search_history VALUES(9,'Disabled',1700000000000);").unwrap();
        state.conn().unwrap().execute_batch("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(91,'matched','existing','user',1);
            INSERT INTO catalog_sources(id,name,url) VALUES(15,'Matched source','https://example.org/cms');
            INSERT INTO channels(id,name,group_name,logo,url) VALUES(4,'News','A','','https://example.org/a'),(8,'News','B','','https://example.org/b');
            INSERT INTO channel_sources(channel_id,url) VALUES(4,'https://example.org/alt');").unwrap();

        let report = import_legacy(&state, &path, None).unwrap();
        assert_eq!(
            (
                report.progress_imported,
                report.catalog_favorites_imported,
                report.channel_favorites_imported,
                report.search_history_imported
            ),
            (1, 1, 2, 20)
        );
        let conn = state.conn().unwrap();
        let count = |query| conn.query_row(query, [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(count("SELECT COUNT(*) FROM online_progress WHERE user_id=91 AND source_id=15 AND media_id='42'
            AND title='Story' AND episode_index=2 AND episode_name='第3集' AND position=12.5 AND duration=100 AND updated_at=1735689600 AND cover='cover'"),1);
        assert_eq!(count("SELECT COUNT(*) FROM catalog_favorites WHERE user_id=91 AND source_id=15 AND media_id='42'
            AND title='Story' AND cover='cover' AND year='2024' AND created_at=1735689600"),1);
        assert_eq!(
            count("SELECT COUNT(*) FROM channel_favorites WHERE user_id=91 AND channel_id IN(4,8)"),
            2
        );
        assert_eq!(
            count(
                "SELECT COUNT(*) FROM search_history WHERE user_id=91 AND updated_at=1700000000 AND CAST(substr(keyword,6) AS INTEGER)>=4"
            ),
            20
        );
        let newest: String = conn
            .query_row(
                "SELECT keyword FROM search_history ORDER BY updated_at DESC,rowid DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(newest, "Word 23");
        drop(conn);
        assert!(supplement_legacy(&state).unwrap().is_none());
        assert!(import_legacy(&state, &path, None).unwrap().already_imported);
    }

    #[test]
    fn supplemental_import_waits_for_backup_and_preserves_existing_edits() {
        let temp = tempfile::tempdir().unwrap();
        let state = state(temp.path());
        let path = temp.path().join("original.db");
        let old = legacy(&path);
        old.execute_batch(
            "INSERT INTO users VALUES(1,'matched','','user',0);
            INSERT INTO source_config VALUES('cms','Original','cms','https://example.org/cms',0,1);
            INSERT INTO favorites VALUES(1,'cms:1'),(1,'cms:2');",
        )
        .unwrap();
        drop(old);
        state.conn().unwrap().execute_batch("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(51,'matched','existing','user',1)").unwrap();
        import_legacy(&state, &path, None).unwrap();
        state.conn().unwrap().execute_batch("UPDATE legacy_imports SET version=1,report_json=json_remove(report_json,'$.progress_imported','$.catalog_favorites_imported','$.channel_favorites_imported','$.search_history_imported');
            UPDATE catalog_favorites SET title='Edited' WHERE media_id='1'; DELETE FROM catalog_favorites WHERE media_id='2';").unwrap();
        let moved = temp.path().join("unmounted.db");
        std::fs::rename(&path, &moved).unwrap();
        assert!(supplement_legacy(&state).unwrap().is_none());
        assert!(import_legacy(&state, &path, None).unwrap().already_imported);
        std::fs::rename(moved, &path).unwrap();
        let report = supplement_legacy(&state).unwrap().unwrap();
        assert_eq!(report.catalog_favorites_imported, 1);
        let title: String = state
            .conn()
            .unwrap()
            .query_row(
                "SELECT title FROM catalog_favorites WHERE user_id=51 AND media_id='1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(title, "Edited");
        state
            .conn()
            .unwrap()
            .execute("DELETE FROM catalog_favorites", [])
            .unwrap();
        assert!(supplement_legacy(&state).unwrap().is_none());
        assert!(import_legacy(&state, &path, None).unwrap().already_imported);
        let remaining: usize = state
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM catalog_favorites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 0);
    }
    #[test]
    fn original_skip_settings_migrate_once_on_fresh_and_m2_installations() {
        for upgrade in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let state = state(temp.path());
            let path = temp.path().join("original.db");
            let old = legacy(&path);
            old.execute_batch("INSERT INTO users VALUES(7,'matched','','user',0);
                INSERT INTO source_config VALUES('cms','Original','cms','https://example.org/cms',0,1);
                INSERT INTO favorites VALUES(7,'cms:a');").unwrap();
            state.conn().unwrap().execute_batch("INSERT INTO users(id,username,password_hash,role,created_at) VALUES(91,'matched','existing','user',1);
                INSERT INTO catalog_sources(id,name,url) VALUES(15,'Matched','https://example.org/cms');
                INSERT INTO skip_configs VALUES(91,'online:15:b',1,42,42);").unwrap();
            if upgrade {
                import_legacy(&state, &path, None).unwrap();
                state.conn().unwrap().execute_batch("UPDATE legacy_imports SET version=2, report_json=json_remove(report_json,'$.skip_configs_imported','$.skip_configs_retained_by_reason');
                    DELETE FROM catalog_favorites;").unwrap();
            }
            old.execute_batch(
                "CREATE TABLE skip_configs (
                    user_id INTEGER NOT NULL DEFAULT 1, media_id TEXT NOT NULL,
                    enabled INTEGER DEFAULT 0, intro_sec REAL DEFAULT 0, outro_sec REAL DEFAULT 0,
                    PRIMARY KEY(user_id,media_id));
                INSERT INTO skip_configs VALUES(7,'cms:a',0,12.5,600),(7,'cms:b',1,10,20),
                    (9,'cms:a',1,0,0),(7,'unknown:4',1,0,0),(7,'4',1,0,0),
                    (7,'cms:c',1,601,0),(7,'cms:d',1,'invalid',0);",
            )
            .unwrap();
            drop(old);
            let before = std::fs::read(&path).unwrap();
            let report = if upgrade {
                supplement_legacy(&state).unwrap().unwrap()
            } else {
                import_legacy(&state, &path, None).unwrap()
            };
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(report.skip_configs_imported, 1);
            assert_eq!(report.legacy_rows_retained["skip_configs"], 6);
            assert_eq!(
                report.skip_configs_retained_by_reason,
                BTreeMap::from([
                    ("existing destination setting".into(), 1),
                    ("unmapped user".into(), 1),
                    ("unmapped source".into(), 1),
                    ("unmapped media".into(), 1),
                    ("invalid skip values".into(), 2),
                ])
            );
            let conn = state.conn().unwrap();
            let count = |sql| conn.query_row(sql, [], |r| r.get::<_, usize>(0)).unwrap();
            assert_eq!(
                count(
                    "SELECT COUNT(*) FROM skip_configs WHERE user_id=91 AND ((media_key='online:15:a' AND enabled=0 AND intro_sec=12.5 AND outro_sec=600) OR (media_key='online:15:b' AND intro_sec=42 AND outro_sec=42))"
                ),
                2
            );
            assert_eq!(
                count("SELECT COUNT(*) FROM catalog_favorites"),
                usize::from(!upgrade)
            );
            conn.execute("DELETE FROM skip_configs", []).unwrap();
            drop(conn);
            assert!(supplement_legacy(&state).unwrap().is_none());
            assert!(import_legacy(&state, &path, None).unwrap().already_imported);
            assert_eq!(
                state
                    .conn()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM skip_configs", [], |r| r
                        .get::<_, usize>(0))
                    .unwrap(),
                0
            );
        }
    }
}
