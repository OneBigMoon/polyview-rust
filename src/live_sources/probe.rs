use crate::{
    AppError, Shared,
    auth::{AuthUser, require_admin},
    network::{live_headers, live_url},
    now,
    playback::fetch_media,
};
use axum::{Json, extract::State, http::HeaderMap};
use futures_util::{StreamExt, stream};
use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};
use tokio::{sync::Mutex, time::Instant};

const CONCURRENCY: usize = 8;
const DEADLINE: Duration = Duration::from_secs(5);
const CLEANUP_THRESHOLD: i64 = 3;
// Serializing batches bounds total network load and preserves consecutive failure order.
static BATCH: Mutex<()> = Mutex::const_new(());

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filter {
    #[serde(alias = "group_name")]
    group: Option<String>,
}

struct Line {
    id: i64,
    channel_id: i64,
    url: String,
    headers: String,
}

fn lines(conn: &Connection, filter: &Filter) -> Result<Vec<Line>, AppError> {
    let mut stmt = conn.prepare(
        "SELECT s.id,s.channel_id,s.url,s.headers_json FROM channel_sources s
         JOIN channels c ON c.id=s.channel_id WHERE ?1 IS NULL OR c.group_name=?1
         ORDER BY s.channel_id,s.position,s.id",
    )?;
    Ok(stmt
        .query_map([&filter.group], |row| {
            Ok(Line {
                id: row.get(0)?,
                channel_id: row.get(1)?,
                url: row.get(2)?,
                headers: row.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

// Drop the response as soon as the first body bytes arrive, including for endless live streams.
async fn first_byte(line: &Line) -> Option<i64> {
    let url = live_url(&line.url).ok()?;
    let headers = live_headers(&line.headers).ok()?;
    let start = Instant::now();
    tokio::time::timeout(DEADLINE, async {
        let (mut response, _) = fetch_media(url, Some(&headers), &HeaderMap::new())
            .await
            .ok()?;
        if response.status().is_success() {
            while let Some(chunk) = response.chunk().await.ok()? {
                if !chunk.is_empty() {
                    return Some(start.elapsed().as_millis() as i64);
                }
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

fn rank_lines(tx: &Transaction<'_>, channel_id: i64) -> Result<(), AppError> {
    tx.execute(
        "WITH ranked AS MATERIALIZED (
            SELECT id,ROW_NUMBER() OVER (ORDER BY
                CASE WHEN fail_count=0 AND latency_ms IS NOT NULL THEN 0
                     WHEN fail_count=0 THEN 1 ELSE 2 END, latency_ms,position,id)-1 AS pos
            FROM channel_sources WHERE channel_id=?1
         ) UPDATE channel_sources SET position=ranked.pos FROM ranked
           WHERE channel_sources.id=ranked.id AND channel_sources.position!=ranked.pos",
        [channel_id],
    )?;
    // The legacy import identity is unique. A shared alternate can lead multiple playback
    // lists, but can be the legacy URL of only one channel.
    tx.execute(
        "UPDATE channels SET url=COALESCE((
            SELECT s.url FROM channel_sources s WHERE s.channel_id=?1 AND NOT EXISTS (
                SELECT 1 FROM channels owner WHERE owner.url=s.url AND owner.id!=?1
            ) ORDER BY s.position,s.id LIMIT 1
         ),url) WHERE id=?1",
        [channel_id],
    )?;
    Ok(())
}

#[derive(Default, Serialize)]
pub(super) struct Report {
    checked: usize,
    healthy: usize,
    failed: usize,
}

pub(super) async fn run(
    State(state): State<Shared>,
    user: AuthUser,
    input: Option<Json<Filter>>,
) -> Result<Json<Report>, AppError> {
    require_admin(&user)?;
    let filter = input.map(|Json(input)| input).unwrap_or_default();
    let _batch = BATCH.lock().await;
    let targets = lines(&*state.conn()?, &filter)?;
    let results = stream::iter(targets)
        .map(|line| async move {
            let latency = first_byte(&line).await;
            (line, latency, now())
        })
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;

    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let mut affected = BTreeSet::new();
    let mut report = Report::default();
    {
        let mut update = tx.prepare(
            "UPDATE channel_sources SET latency_ms=?1,
             last_ok_at=CASE WHEN ?1 IS NOT NULL THEN ?2 ELSE last_ok_at END,
             fail_count=CASE WHEN ?1 IS NULL THEN fail_count+1 ELSE 0 END
             WHERE id=?3 AND channel_id=?4 AND url=?5 AND headers_json=?6 AND
             (?7 IS NULL OR EXISTS (SELECT 1 FROM channels WHERE id=?4 AND group_name=?7))",
        )?;
        for (line, latency, at) in results {
            if update.execute(params![
                latency,
                at,
                line.id,
                line.channel_id,
                line.url,
                line.headers,
                filter.group,
            ])? == 0
            {
                continue;
            }
            affected.insert(line.channel_id);
            report.checked += 1;
            if latency.is_some() {
                report.healthy += 1;
            } else {
                report.failed += 1;
            }
        }
    }
    for channel_id in affected {
        rank_lines(&tx, channel_id)?;
    }
    tx.commit()?;
    Ok(Json(report))
}

#[derive(Serialize)]
pub(super) struct Cleanup {
    removed: usize,
}

pub(super) async fn cleanup(
    State(state): State<Shared>,
    user: AuthUser,
    input: Option<Json<Filter>>,
) -> Result<Json<Cleanup>, AppError> {
    require_admin(&user)?;
    let filter = input.map(|Json(input)| input).unwrap_or_default();
    let mut conn = state.conn()?;
    let tx = conn.transaction()?;
    let mut affected = BTreeSet::new();
    let mut removed = 0;
    {
        let mut delete = tx.prepare(
            "DELETE FROM channel_sources WHERE fail_count>=?1 AND (?2 IS NULL OR
             channel_id IN (SELECT id FROM channels WHERE group_name=?2)) RETURNING channel_id",
        )?;
        let mut rows = delete.query(params![CLEANUP_THRESHOLD, filter.group])?;
        while let Some(row) = rows.next()? {
            affected.insert(row.get::<_, i64>(0)?);
            removed += 1;
        }
    }
    for channel_id in affected {
        if tx.execute(
            "DELETE FROM channels WHERE id=?1 AND NOT EXISTS
             (SELECT 1 FROM channel_sources WHERE channel_id=?1)",
            [channel_id],
        )? == 0
        {
            rank_lines(&tx, channel_id)?;
        }
    }
    tx.commit()?;
    Ok(Json(Cleanup { removed }))
}
