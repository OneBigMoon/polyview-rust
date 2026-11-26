use super::{MediaItem, media_row};
use crate::{
    AppError, Shared,
    auth::AuthUser,
    scraper::{Credit, Season},
};
use axum::{
    Json,
    extract::{Path, State},
};
use rusqlite::{Connection, ToSql, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct DisplayMetadata {
    #[serde(skip_serializing)]
    title: String,
    pub(crate) provider: String,
    pub(crate) original_title: String,
    pub(crate) rating: Option<f64>,
    pub(crate) genres: Vec<String>,
    pub(crate) backdrop: String,
    pub(crate) cast: Vec<Credit>,
    pub(crate) crew: Vec<Credit>,
    pub(crate) seasons: Vec<Season>,
}

pub(crate) enum Selection<'a> {
    Search(&'a str),
    Media(i64),
    Series(i64),
}

pub(crate) fn items(
    conn: &Connection,
    user_id: i64,
    selection: Selection<'_>,
) -> Result<Vec<MediaItem>, AppError> {
    let (filter, value): (&str, &dyn ToSql) = match &selection {
        Selection::Search(query) => (
            "(?2='' OR instr(lower(m.title || ' ' || COALESCE(s.title,'') || ' ' || COALESCE(json_extract(md.metadata_json,'$.title'),'') || ' ' || COALESCE(json_extract(md.metadata_json,'$.original_title'),'')),lower(?2))>0)",
            query,
        ),
        Selection::Media(id) => ("m.id=?2", id),
        Selection::Series(id) => ("m.series_id=?2", id),
    };
    let mut stmt = conn.prepare(&format!("SELECT m.id,m.title,m.relative_path,m.kind,s.title,m.series_id,m.season,m.episode,m.size,
            COALESCE(u.favorite,0),COALESCE(u.position,0),COALESCE(u.duration,0),COALESCE(u.watched,0),
            m.cover,m.year,m.description,md.metadata_json
        FROM library_media m LEFT JOIN library_series s ON s.id=m.series_id
        LEFT JOIN library_user_media u ON u.media_id=m.id AND u.user_id=?1
        LEFT JOIN media_metadata md ON md.media_id=m.id
        WHERE m.available=1 AND {filter} ORDER BY s.title COLLATE NOCASE,m.season,m.episode,m.title COLLATE NOCASE,m.id"))?;
    let mut rows = stmt.query(params![user_id, value])?;
    let mut items = Vec::new();
    while let Some(row) = rows.next()? {
        let mut item = media_row(row)?;
        if let Some(raw) = row
            .get_ref(16)?
            .as_str_or_null()
            .map_err(|_| AppError::internal("Invalid media metadata"))?
        {
            item.metadata = serde_json::from_str(raw)
                .map_err(|_| AppError::internal("Invalid media metadata"))?;
            if !item.metadata.title.is_empty() {
                let title = std::mem::take(&mut item.metadata.title);
                if item.series_id.is_some() {
                    item.series = Some(title);
                    if let Some(episode) = item
                        .metadata
                        .seasons
                        .iter()
                        .find(|s| Some(s.number) == item.season)
                        .and_then(|s| s.episodes.iter().find(|e| Some(e.number) == item.episode))
                        .filter(|e| !e.title.is_empty())
                    {
                        item.title.clone_from(&episode.title);
                    }
                } else {
                    item.title = title;
                }
            }
        }
        items.push(item);
    }
    Ok(items)
}

#[derive(Serialize)]
pub(super) struct Detail {
    #[serde(flatten)]
    item: MediaItem,
    episodes: Vec<MediaItem>,
    automation_status: Option<String>,
}

pub(super) async fn detail(
    State(state): State<Shared>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Detail>, AppError> {
    let conn = state.conn()?;
    let item = items(&conn, user.id, Selection::Media(id))?
        .pop()
        .ok_or_else(|| AppError::not_found("Media not found"))?;
    let episodes = match item.series_id {
        Some(series) => items(&conn, user.id, Selection::Series(series))?,
        None => Vec::new(),
    };
    let automation_status = conn.query_row(
        "SELECT (SELECT status FROM library_automation WHERE media_id=?)",
        [id],
        |row| row.get(0),
    )?;
    Ok(Json(Detail {
        item,
        episodes,
        automation_status,
    }))
}
