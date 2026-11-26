use super::{
    AppError, Mode, Shared, conflict, detect_filename,
    files::{Entry, Stamp, open_root},
    render_template, settings,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use std::{fs::File, path::Path};
use uuid::Uuid;

pub fn init_schema(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS organizer_history (
        id INTEGER PRIMARY KEY, media_id INTEGER NOT NULL, source_root TEXT NOT NULL,
        source_path TEXT NOT NULL, target_path TEXT NOT NULL,
        mode TEXT NOT NULL CHECK(mode IN ('hardlink','move')),
        status TEXT NOT NULL CHECK(status IN ('pending','completed','rolling_back','rolled_back')),
        created_at INTEGER NOT NULL, proof TEXT NOT NULL);
        CREATE UNIQUE INDEX IF NOT EXISTS organizer_active_media ON organizer_history(media_id) WHERE status!='rolled_back';
        CREATE UNIQUE INDEX IF NOT EXISTS organizer_active_target ON organizer_history(target_path) WHERE status!='rolled_back';
        CREATE INDEX IF NOT EXISTS organizer_original ON organizer_history(source_root,source_path);")?;
    Ok(())
}

pub fn skip_scan_path(conn: &Connection, root: &str, path: &str) -> Result<bool, AppError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM organizer_history
        WHERE NOT (source_root='media' AND source_path=target_path) AND
        ((status='pending' AND ?1='media' AND target_path=?2) OR
         ((status='rolling_back' OR (status='completed' AND mode='hardlink')) AND source_root=?1 AND source_path=?2)))", params![root, path], |r| r.get(0))?)
}

pub(super) fn was_rolled_back(conn: &Connection, media_id: i64) -> Result<bool, AppError> {
    Ok(conn.query_row("SELECT COALESCE((SELECT status='rolled_back' FROM organizer_history WHERE media_id=? ORDER BY id DESC LIMIT 1),0)", [media_id], |r| r.get(0))?)
}

#[derive(Debug, Serialize)]
pub struct Record {
    pub id: i64,
    pub media_id: i64,
    pub source_root: String,
    pub source_path: String,
    pub target_path: String,
    pub mode: Mode,
    pub status: String,
    pub created_at: i64,
}

const COLUMNS: &str = "id,media_id,source_root,source_path,target_path,mode,status,created_at";

fn record(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: row.get(0)?,
        media_id: row.get(1)?,
        source_root: row.get(2)?,
        source_path: row.get(3)?,
        target_path: row.get(4)?,
        mode: if row.get::<_, String>(5)? == "move" {
            Mode::Move
        } else {
            Mode::Hardlink
        },
        status: row.get(6)?,
        created_at: row.get(7)?,
    })
}

pub(super) fn history(conn: &Connection) -> Result<Vec<Record>, AppError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM organizer_history ORDER BY id DESC"
    ))?;
    Ok(stmt.query_map([], record)?.collect::<Result<_, _>>()?)
}

#[derive(Serialize, Deserialize)]
struct Staged {
    name: UuidName,
    copy_id: Option<(u64, u64)>,
    stamp: Option<Stamp>,
}

// Re-parse private names on every resume. A corrupt journal cannot supply a filesystem path.
#[derive(Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[derive(Clone)]
struct UuidName(Uuid);
impl From<UuidName> for String {
    fn from(n: UuidName) -> Self {
        n.0.to_string()
    }
}
impl TryFrom<String> for UuidName {
    type Error = uuid::Error;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Uuid::parse_str(&s).map(Self)
    }
}

#[derive(Serialize, Deserialize)]
struct Proof {
    token: UuidName,
    source: Stamp,
    output: Option<Staged>,
    restored: Option<Staged>,
    discard: Option<Stamp>,
}
struct Job {
    row: Record,
    proof: Proof,
}

fn json<T: Serialize>(value: &T) -> Result<String, AppError> {
    serde_json::to_string(value).map_err(|e| AppError::internal(e.to_string()))
}

impl Job {
    fn load(conn: &Connection, id: i64) -> Result<Self, AppError> {
        let (row, proof) = conn
            .query_row(
                &format!("SELECT {COLUMNS},proof FROM organizer_history WHERE id=?"),
                [id],
                |r| Ok((record(r)?, r.get::<_, String>(8)?)),
            )
            .optional()?
            .ok_or_else(|| AppError::not_found("Organizer history record not found"))?;
        let proof = serde_json::from_str(&proof).map_err(|e| AppError::internal(e.to_string()))?;
        Ok(Self { row, proof })
    }

    fn same_path(&self) -> bool {
        self.row.source_root == "media" && self.row.source_path == self.row.target_path
    }

    fn paths(
        &self,
        roots: &Roots,
        state: &Shared,
        rollback: bool,
    ) -> Result<(Entry, Entry), AppError> {
        let base = roots.source(state, &self.row.source_root)?;
        Ok((
            Entry::resolve(
                &base,
                &self.row.source_path,
                rollback && self.row.mode == Mode::Move,
            )?,
            Entry::resolve(&roots.media, &self.row.target_path, !rollback)?,
        ))
    }

    fn update_proof(&self, state: &Shared) -> Result<(), AppError> {
        state.conn()?.execute(
            "UPDATE organizer_history SET proof=? WHERE id=?",
            params![json(&self.proof)?, self.row.id],
        )?;
        Ok(())
    }

    fn finish(mut self, state: &Shared, rollback: bool) -> Result<Record, AppError> {
        let row = &self.row;
        let mut conn = state.conn()?;
        let tx = conn.transaction()?;
        let (root, path, status) = if rollback {
            (
                row.source_root.as_str(),
                row.source_path.as_str(),
                "rolled_back",
            )
        } else {
            ("media", row.target_path.as_str(), "completed")
        };
        let changed = tx.execute(
            "UPDATE library_media SET root=?1,relative_path=?2,available=1 WHERE id=?3
            AND ((root=?4 AND relative_path=?5) OR (root='media' AND relative_path=?6))",
            params![
                root,
                path,
                row.media_id,
                row.source_root,
                row.source_path,
                row.target_path
            ],
        )?;
        if changed != 1 {
            return Err(conflict("Library item moved independently"));
        }
        tx.execute(
            "UPDATE organizer_history SET status=? WHERE id=?",
            params![status, row.id],
        )?;
        tx.commit()?;
        self.row.status = status.into();
        Ok(self.row)
    }

    fn stage(&self, restore: bool) -> Option<&Staged> {
        if restore {
            self.proof.restored.as_ref()
        } else {
            self.proof.output.as_ref()
        }
    }

    fn stage_slot(&mut self, restore: bool) -> &mut Option<Staged> {
        if restore {
            &mut self.proof.restored
        } else {
            &mut self.proof.output
        }
    }

    fn output_stamp(&self) -> Option<&Stamp> {
        self.proof.output.as_ref().and_then(|s| s.stamp.as_ref())
    }

    fn publish(
        &mut self,
        state: &Shared,
        source: &Entry,
        target: &Entry,
        restore: bool,
    ) -> Result<(), AppError> {
        let expected = if restore {
            self.output_stamp()
                .ok_or_else(|| conflict("Missing organizer output identity"))?
        } else {
            &self.proof.source
        }
        .clone();
        let allocate = if let Some(stage) = self.stage(restore) {
            let entry = target.private(&stage.name.0.to_string())?;
            // A crash immediately after exclusive creation can leave an empty entry
            // with no inode receipt. Preserve it and restart with a fresh private name.
            stage.stamp.is_none()
                && stage.copy_id.is_none()
                && entry.exists()?
                && !entry.matches_identity(&expected)?
        } else {
            true
        };
        if allocate {
            *self.stage_slot(restore) = Some(Staged {
                name: UuidName(Uuid::new_v4()),
                copy_id: None,
                stamp: None,
            });
            self.update_proof(state)?;
        }
        let stage = self.stage(restore).unwrap();
        let private = target.private(&stage.name.0.to_string())?;
        if stage.stamp.is_none() {
            let copy_id = stage.copy_id;
            let stamp = source.stage(&private, &expected, copy_id, |id| {
                self.stage_slot(restore).as_mut().unwrap().copy_id = Some(id);
                self.update_proof(state)
            })?;
            self.stage_slot(restore).as_mut().unwrap().stamp = Some(stamp);
            self.update_proof(state)?;
        }
        private.publish(
            target,
            self.stage(restore).and_then(|s| s.stamp.as_ref()).unwrap(),
        )
    }

    fn claimed(&self, path: &Entry) -> Result<Entry, AppError> {
        path.private(&format!("retire-{}", self.proof.token.0))
    }
}

/// Keep the enumerated filenames valid until their scan transaction commits.
pub fn scan_guard(state: &Shared) -> Result<File, AppError> {
    let media = open_root(&state.media_dir)?;
    flock(&media, FlockOperation::LockShared).map_err(std::io::Error::from)?;
    Ok(media)
}

struct Roots {
    media: File,
}
impl Roots {
    // Operations spanning multiple SQLite commits also serialize across app processes.
    fn lock(state: &Shared) -> Result<Self, AppError> {
        let media = open_root(&state.media_dir)?;
        flock(&media, FlockOperation::LockExclusive).map_err(std::io::Error::from)?;
        Ok(Self { media })
    }
    fn source(&self, state: &Shared, key: &str) -> Result<File, AppError> {
        match key {
            "media" => Ok(self.media.try_clone()?),
            "download" => open_root(&state.download_dir),
            _ => Err(AppError::bad_request("Unknown media root")),
        }
    }
}

fn new_job(
    state: &Shared,
    roots: &Roots,
    id: i64,
    title: Option<&str>,
    year: Option<u16>,
) -> Result<Job, AppError> {
    let (root, path, catalog_year, config) = {
        let conn = state.conn()?;
        let (root, path, year) = conn
            .query_row(
                "SELECT root,relative_path,year FROM library_media WHERE id=? AND available=1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| AppError::not_found("Library item not found"))?;
        (root, path, year, settings(&conn)?)
    };
    if !crate::library::is_video(Path::new(&path)) {
        return Err(AppError::bad_request("Organizer requires a video file"));
    }
    config.validate()?;
    let mut info = detect_filename(Path::new(&path));
    if let Some(title) = title.filter(|s| !s.trim().is_empty()) {
        info.title = title.to_owned();
    }
    info.year = year.or(info.year).or_else(|| catalog_year.parse().ok());
    let template = if info.episode.is_some() {
        &config.episode_template
    } else {
        &config.movie_template
    };
    let target_path = render_template(template, &info)?;
    let source = Entry::resolve(&roots.source(state, &root)?, &path, false)?;
    let stamp = source
        .stamp()?
        .ok_or_else(|| AppError::not_found("Organizer source file not found"))?;
    if (root != "media" || path != target_path)
        && Entry::resolve(&roots.media, &target_path, true)?.exists()?
    {
        return Err(conflict("Organizer destination already exists"));
    }
    let proof = Proof {
        source: stamp,
        token: UuidName(Uuid::new_v4()),
        output: None,
        restored: None,
        discard: None,
    };
    let conn = state.conn()?;
    let occupied: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM library_media WHERE root='media' AND relative_path=?1 AND id!=?2)
        OR EXISTS(SELECT 1 FROM organizer_history WHERE status!='rolled_back' AND (target_path=?1 OR (source_root='media' AND source_path=?1)))", params![target_path,id], |r| r.get(0))?;
    if occupied {
        return Err(conflict(
            "Organizer destination belongs to another library item",
        ));
    }
    let mode = match config.mode {
        Mode::Move => "move",
        Mode::Hardlink => "hardlink",
    };
    let history_id = conn.query_row("INSERT INTO organizer_history(media_id,source_root,source_path,target_path,mode,status,created_at,proof)
        VALUES(?,?,?,?,?,'pending',?,?) RETURNING id", params![id,root,path,target_path,mode,crate::now(),json(&proof)?], |r| r.get(0))?;
    Job::load(&conn, history_id)
}

pub(super) fn organize(
    state: &Shared,
    id: i64,
    title: Option<&str>,
    year: Option<u16>,
) -> Result<Record, AppError> {
    let roots = Roots::lock(state)?;
    organize_locked(&roots, state, id, title, year)
}

pub(super) fn automatic(
    state: &Shared,
    id: i64,
    title: Option<&str>,
    year: Option<u16>,
) -> Result<(), AppError> {
    let roots = Roots::lock(state)?;
    {
        let conn = state.conn()?;
        if !settings(&conn)?.auto_organize || was_rolled_back(&conn, id)? {
            return Ok(());
        }
    }
    organize_locked(&roots, state, id, title, year)?;
    Ok(())
}

fn organize_locked(
    roots: &Roots,
    state: &Shared,
    id: i64,
    title: Option<&str>,
    year: Option<u16>,
) -> Result<Record, AppError> {
    let active = {
        let conn = state.conn()?;
        let active = conn
            .query_row(
                "SELECT id FROM organizer_history WHERE media_id=? AND status!='rolled_back'",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        active.map(|id| Job::load(&conn, id)).transpose()?
    };
    let mut job = match active {
        Some(j) => j,
        None => new_job(state, roots, id, title, year)?,
    };
    if job.row.status == "rolling_back" {
        return Err(conflict("Organizer rollback is in progress"));
    }
    if job.row.status == "completed" {
        let target = Entry::resolve(&roots.media, &job.row.target_path, false)?;
        target.expect(job.output_stamp().unwrap_or(&job.proof.source))?;
        return Ok(job.row);
    }
    let (source, target) = job.paths(roots, state, false)?;
    if !job.same_path() {
        job.publish(state, &source, &target, false)?;
        if job.row.mode == Mode::Move {
            let output = job
                .output_stamp()
                .ok_or_else(|| conflict("Missing organizer output identity"))?;
            source.retire(&job.claimed(&source)?, &job.proof.source, &target, output)?;
        } else if !source.matches_identity(&job.proof.source)? {
            return Err(conflict("Organizer source was replaced"));
        }
    }
    job.finish(state, false)
}

pub(super) fn rollback(state: &Shared, id: i64) -> Result<Record, AppError> {
    let roots = Roots::lock(state)?;
    let mut job = {
        let conn = state.conn()?;
        Job::load(&conn, id)?
    };
    if job.row.status == "rolled_back" {
        return Ok(job.row);
    }
    let occupied: bool = state.conn()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_media WHERE root=? AND relative_path=? AND id!=?)",
        params![job.row.source_root, job.row.source_path, job.row.media_id],
        |r| r.get(0),
    )?;
    if occupied {
        return Err(conflict(
            "Original organizer path belongs to a different library item",
        ));
    }
    let (source, target) = job.paths(&roots, state, true)?;
    if job.same_path() {
        source.expect(&job.proof.source)?;
        return job.finish(state, true);
    }
    if job.row.status == "pending"
        && let Some(output) = &job.proof.output
    {
        // A private stage still present means publication has not happened. A
        // concurrent file at the public destination is outside this operation.
        let stage = target.private(&output.name.0.to_string())?;
        if stage.exists()? {
            source.expect(&job.proof.source)?;
            let stamp = match (&output.stamp, output.copy_id) {
                (Some(stamp), _) => stamp.clone(),
                (None, Some(id)) => stage.partial_stamp(&source, &job.proof.source, id)?,
                (None, None) if stage.matches_identity(&job.proof.source)? => {
                    job.proof.source.clone()
                }
                (None, None) => return job.finish(state, true),
            };
            job.proof.discard = Some(stamp);
            state.conn()?.execute(
                "UPDATE organizer_history SET status='rolling_back',proof=? WHERE id=?",
                params![json(&job.proof)?, id],
            )?;
        }
    }
    if let Some(stamp) = &job.proof.discard {
        let output = job
            .proof
            .output
            .as_ref()
            .ok_or_else(|| conflict("Missing organizer staging plan"))?;
        target.private(&output.name.0.to_string())?.retire(
            &job.claimed(&target)?,
            stamp,
            &source,
            &job.proof.source,
        )?;
        return job.finish(state, true);
    }
    let Some(stamp) = job.output_stamp().cloned() else {
        source.expect(&job.proof.source)?;
        return job.finish(state, true);
    };
    if job.row.status != "rolling_back" {
        target.expect(&stamp)?;
        if job.row.mode == Mode::Move && source.exists()? && job.row.status == "completed" {
            return Err(conflict("Original organizer path is occupied"));
        }
        if job.row.mode == Mode::Hardlink || source.exists()? {
            source.expect(&job.proof.source)?;
        }
        // Finish a forward retirement interrupted after its no-replace rename.
        if job.row.mode == Mode::Move && job.claimed(&source)?.exists()? {
            source.retire(&job.claimed(&source)?, &job.proof.source, &target, &stamp)?;
        }
        state.conn()?.execute(
            "UPDATE organizer_history SET status='rolling_back' WHERE id=?",
            [id],
        )?;
    }
    if job.row.mode == Mode::Move && (!source.exists()? || job.proof.restored.is_some()) {
        job.publish(state, &target, &source, true)?;
    }
    let surviving = job
        .proof
        .restored
        .as_ref()
        .and_then(|s| s.stamp.as_ref())
        .unwrap_or(&job.proof.source);
    target.retire(&job.claimed(&target)?, &stamp, &source, surviving)?;
    job.finish(state, true)
}
