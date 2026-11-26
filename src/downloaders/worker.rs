use super::*;
use std::{
    path::{Component, Path as FsPath, PathBuf},
    time::Duration,
};
use tokio::task::JoinHandle;

pub(super) fn clean_absolute(value: &str) -> bool {
    let path = FsPath::new(value);
    path.is_absolute()
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::Prefix(_)))
}

struct Completion {
    id: i64,
    directory: String,
    file: String,
    size: u64,
    local_root: String,
    remote_root: String,
}

impl Completion {
    fn local_file(&self, state: &Shared) -> Result<PathBuf, AppError> {
        let invalid =
            || AppError::bad_request("Completed file is outside the configured mount or library");
        let name = FsPath::new(&self.file);
        if !clean_absolute(&self.directory)
            || !clean_absolute(&self.remote_root)
            || !clean_absolute(&self.local_root)
            || self.file.contains('\\')
            || self.file.chars().any(char::is_control)
            || self.file.is_empty()
            || !name.components().all(|c| matches!(c, Component::Normal(_)))
            || !crate::library::is_video(name)
        {
            return Err(invalid());
        }
        let suffix = FsPath::new(&self.directory)
            .strip_prefix(&self.remote_root)
            .map_err(|_| invalid())?;
        let mount = std::fs::canonicalize(&self.local_root)?;
        let candidate = mount.join(suffix).join(name);
        // A move-mode organizer can publish the file before its daemon reports completion.
        // Resolve existing ancestors and reject dangling symlinks before consulting its journal.
        let mut ancestor = candidate.as_path();
        let file = loop {
            match ancestor.symlink_metadata() {
                Ok(_) => {
                    let base = ancestor.canonicalize()?;
                    let tail = candidate.strip_prefix(ancestor).map_err(|_| invalid())?;
                    break if tail.as_os_str().is_empty() {
                        base
                    } else {
                        base.join(tail)
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && ancestor != mount => {
                    ancestor = ancestor.parent().ok_or_else(invalid)?;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let permitted = [&state.media_dir, &state.download_dir]
            .into_iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .any(|root| file.starts_with(root));
        if !permitted {
            return Err(invalid());
        }
        if !file.starts_with(&mount) || !crate::library::is_video(&file) {
            return Err(invalid());
        }
        Ok(file)
    }

    fn process(&self, state: &Shared) -> Result<(), AppError> {
        let file = self.local_file(state)?;
        match file.metadata() {
            Ok(meta) if meta.is_file() && meta.len() == self.size => {
                crate::library::index_external(state, &file)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if crate::library::acknowledge_organized_external(state, &file, self.size)? {
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
            Err(error) => Err(error.into()),
            _ => Err(AppError::bad_request(
                "Completed file is not available at its reported size",
            )),
        }
    }
}

fn ingest(state: &Shared) -> Result<(), AppError> {
    let events = {
        let conn = state.conn()?;
        let mut stmt = conn.prepare("SELECT e.id,e.directory,e.file,e.size,d.local_root,d.remote_root FROM external_completions e JOIN downloaders d ON d.id=e.downloader_id WHERE NOT e.done AND e.retry_at<=? AND d.local_root!='' ORDER BY e.retry_at,e.id LIMIT 128")?;
        stmt.query_map([crate::now()], |r| {
            Ok(Completion {
                id: r.get(0)?,
                directory: r.get(1)?,
                file: r.get(2)?,
                size: r.get(3)?,
                local_root: r.get(4)?,
                remote_root: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for event in events {
        let result = event.process(state);
        let conn = state.conn()?;
        match result {
            Ok(_) => {
                conn.execute(
                    "UPDATE external_completions SET done=1,last_error=NULL WHERE id=?",
                    [event.id],
                )?;
            }
            Err(_) => {
                conn.execute("UPDATE external_completions SET attempts=attempts+1,retry_at=?+min(3600,15*(1<<min(attempts,8))),last_error='Mounted file could not be indexed' WHERE id=?",params![crate::now(),event.id])?;
            }
        }
    }
    Ok(())
}

pub fn start_worker(state: Shared) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match account_ids(&state,true) {
                Ok(ids) => stream::iter(ids).for_each_concurrent(4,|id| {
                    let state = &state;
                    async move { if let Err(error) = refresh(state,id,None).await { tracing::warn!(downloader_id=id,status=%error.0,"Downloader polling failed"); } }
                }).await,
                Err(error) => tracing::warn!(status=%error.0,"Cannot read downloader configuration"),
            }
            let indexing = state.clone();
            if !matches!(
                tokio::task::spawn_blocking(move || ingest(&indexing)).await,
                Ok(Ok(()))
            ) {
                tracing::warn!("External completion ingestion failed");
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    })
}
