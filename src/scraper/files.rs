use super::{Media, Metadata, providers::Image};
use crate::{AppError, Shared};
use axum::http::StatusCode;
use ring::digest::{SHA256, digest};
use rusqlite::{Connection, OptionalExtension, params};
use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, renameat, unlinkat};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

fn conflict() -> AppError {
    AppError(
        StatusCode::CONFLICT,
        "An existing sidecar belongs to another file or was edited".into(),
    )
}
fn unavailable() -> AppError {
    AppError::not_found("Media file or directory unavailable")
}

// Anchor every component to an opened directory. A swapped media mount/symlink
// cannot redirect a write through a freshly resolved absolute sidecar path.
struct Directory {
    handle: File,
    relative: PathBuf,
}
impl Directory {
    fn open(root: &File, relative: &Path) -> Result<Self, AppError> {
        let mut handle = root.try_clone()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(unavailable());
            };
            handle = openat(
                &handle,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map(File::from)
            .map_err(|_| unavailable())?;
        }
        Ok(Self {
            handle,
            relative: relative.to_owned(),
        })
    }
    fn read(&self, name: &OsStr) -> io::Result<File> {
        openat(
            &self.handle,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(io::Error::from)
    }
    fn fingerprint(&self, name: &OsStr) -> Result<Option<Vec<u8>>, AppError> {
        let file = match self.read(name) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(conflict()),
        };
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > 16 * 1024 * 1024 {
            return Err(conflict());
        }
        let mut file = file.take(16 * 1024 * 1024 + 1);
        let mut context = ring::digest::Context::new(&SHA256);
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            context.update(&buf[..n]);
        }
        Ok(Some(context.finish().as_ref().to_vec()))
    }
    fn publish(&self, file: &Sidecar<'_>, previous: Option<&[u8]>) -> Result<(), AppError> {
        let temp = format!(".polyview-{}.tmp", uuid::Uuid::new_v4());
        let fd = openat(
            &self.handle,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o644),
        )
        .map_err(io::Error::from)?;
        let result = (|| {
            let mut output = File::from(fd);
            output.write_all(file.data.as_ref())?;
            output.sync_all()?;
            if previous.is_some() {
                if self.fingerprint(file.name.as_ref())?.as_deref() != previous {
                    return Err(conflict());
                }
                renameat(
                    &self.handle,
                    temp.as_str(),
                    &self.handle,
                    file.name.as_str(),
                )
                .map_err(io::Error::from)?;
            } else {
                linkat(
                    &self.handle,
                    temp.as_str(),
                    &self.handle,
                    file.name.as_str(),
                    AtFlags::empty(),
                )
                .map_err(|e| {
                    if e == rustix::io::Errno::EXIST {
                        conflict()
                    } else {
                        AppError::from(io::Error::from(e))
                    }
                })?;
            }
            Ok(())
        })();
        let _ = unlinkat(&self.handle, temp.as_str(), AtFlags::empty());
        result
    }
}

struct Sidecar<'a> {
    dir: &'a Directory,
    name: String,
    owner: &'a str,
    data: std::borrow::Cow<'a, [u8]>,
}

// The pending fingerprint is committed before publishing a file. Either the old
// or the new fingerprint is ours after a power loss, so a retry can finish safely.
fn install(conn: &Connection, root: &str, files: &[Sidecar<'_>]) -> Result<(), AppError> {
    let mut plan = Vec::new();
    for file in files {
        let path = file.dir.relative.join(&file.name);
        let key = path.to_str().ok_or_else(unavailable)?;
        let record: Option<(String, Vec<u8>, Option<Vec<u8>>)> = conn
            .query_row(
                "SELECT owner,digest,pending FROM scraper_sidecars WHERE root=? AND path=?",
                params![root, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let current = file.dir.fingerprint(file.name.as_ref())?;
        if let Some(bytes) = current.as_ref()
            && !record.as_ref().is_some_and(|(owner, old, next)| {
                owner == file.owner && (bytes == old || next.as_ref() == Some(bytes))
            })
        {
            return Err(conflict());
        }
        let next = digest(&SHA256, file.data.as_ref()).as_ref().to_vec();
        plan.push((key.to_owned(), current, next));
    }
    for (file, (key, current, next)) in files.iter().zip(plan) {
        if current.as_ref() != Some(&next) {
            conn.execute("INSERT INTO scraper_sidecars(root,path,owner,digest,pending) VALUES(?,?,?,?,?) ON CONFLICT(root,path) DO UPDATE SET owner=excluded.owner,digest=excluded.digest,pending=excluded.pending",params![root,&key,file.owner,current.as_deref().unwrap_or_default(),&next])?;
            file.dir.publish(file, current.as_deref())?;
        }
        conn.execute(
            "UPDATE scraper_sidecars SET digest=?,pending=NULL WHERE root=? AND path=?",
            params![next, root, key],
        )?;
    }
    Ok(())
}

fn escaped(xml: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => xml.push_str("&amp;"),
            '<' => xml.push_str("&lt;"),
            '>' => xml.push_str("&gt;"),
            '"' => xml.push_str("&quot;"),
            '\'' => xml.push_str("&apos;"),
            '\t'
            | '\r'
            | '\n'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => xml.push(c),
            _ => {}
        }
    }
}
fn field(xml: &mut String, tag: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    xml.push('<');
    xml.push_str(tag);
    xml.push('>');
    escaped(xml, value);
    xml.push_str("</");
    xml.push_str(tag);
    xml.push('>');
}
fn nfo(tag: &str, media: &Media, m: &Metadata, art: &[(String, String)]) -> Vec<u8> {
    let mut xml = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<{tag}>");
    if tag == "episodedetails" {
        let season = media.season.unwrap_or(1);
        let episode = media.episode.unwrap_or(1);
        let detail = m
            .seasons
            .iter()
            .find(|s| s.number == season)
            .and_then(|s| s.episodes.iter().find(|e| e.number == episode));
        let fallback = format!("{} S{season:02}E{episode:02}", m.title);
        field(
            &mut xml,
            "title",
            detail
                .map(|e| e.title.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&fallback),
        );
        field(&mut xml, "showtitle", &m.title);
        field(&mut xml, "season", &season.to_string());
        field(&mut xml, "episode", &episode.to_string());
        field(
            &mut xml,
            "plot",
            detail
                .map(|e| e.description.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&m.description),
        );
    } else {
        field(&mut xml, "title", &m.title);
        field(&mut xml, "originaltitle", &m.original_title);
        field(&mut xml, "plot", &m.description);
        xml.push_str("<uniqueid type=\"");
        escaped(&mut xml, &m.provider);
        xml.push_str("\" default=\"true\">");
        escaped(&mut xml, &m.provider_id);
        xml.push_str("</uniqueid>");
    }
    field(&mut xml, "year", &m.year);
    if let Some(score) = m.rating {
        field(&mut xml, "rating", &score.to_string());
    }
    for genre in &m.genres {
        field(&mut xml, "genre", genre);
    }
    for person in &m.cast {
        xml.push_str("<actor>");
        field(&mut xml, "name", &person.name);
        field(&mut xml, "role", &person.role);
        xml.push_str("</actor>");
    }
    for person in &m.crew {
        match person.role.as_str() {
            "Director" | "导演" | "監督" => field(&mut xml, "director", &person.name),
            "Writer" | "Screenplay" | "编剧" | "脚本" => {
                field(&mut xml, "credits", &person.name)
            }
            _ => {}
        }
    }
    for (kind, name) in art {
        if kind == "poster" {
            field(&mut xml, "thumb", name);
        } else {
            xml.push_str("<fanart>");
            field(&mut xml, "thumb", name);
            xml.push_str("</fanart>");
        }
    }
    xml.push_str("</");
    xml.push_str(tag);
    xml.push_str(">\n");
    xml.into_bytes()
}

pub(super) fn write_sidecars(
    state: &Shared,
    conn: &Connection,
    media: &Media,
    id: i64,
    m: &Metadata,
    assets: &[(&str, Option<Image>)],
) -> Result<(), AppError> {
    let root = match media.root.as_str() {
        "media" => &state.media_dir,
        "download" => &state.download_dir,
        _ => return Err(unavailable()),
    };
    let file = Path::new(&media.relative);
    if !crate::library::is_video(file) {
        return Err(unavailable());
    }
    let root = File::open(root)?;
    let relative = file.parent().ok_or_else(unavailable)?;
    let dir = Directory::open(&root, relative)?;
    let source = dir
        .read(file.file_name().ok_or_else(unavailable)?)
        .map_err(|_| unavailable())?;
    if !source.metadata()?.is_file() {
        return Err(unavailable());
    }
    let stem = file
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(unavailable)?;
    let season_dir = relative
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|s| {
            s.to_ascii_lowercase()
                .strip_prefix("season")
                .is_some_and(|n| {
                    !n.trim().is_empty() && n.trim().bytes().all(|c| c.is_ascii_digit())
                })
        });
    let show_relative = if media.season.is_some() && season_dir {
        relative.parent().unwrap_or(relative)
    } else {
        relative
    };
    let show = Directory::open(&root, show_relative)?;
    let media_owner = format!("media:{id}");
    let show_owner = media
        .series_id
        .map(|n| format!("series:{n}"))
        .unwrap_or_else(|| media_owner.clone());
    let mut art = Vec::new();
    let mut files = Vec::new();
    for (kind, asset) in assets {
        let Some(asset) = asset else {
            continue;
        };
        let label = if *kind == "poster" {
            "poster"
        } else {
            "fanart"
        };
        let (name, owner) = if media.season.is_some() {
            (format!("{label}.{}", asset.ext), &show_owner)
        } else {
            (format!("{stem}-{label}.{}", asset.ext), &media_owner)
        };
        art.push((kind.to_string(), name.clone()));
        files.push(Sidecar {
            dir: &show,
            name,
            owner,
            data: asset.data.as_slice().into(),
        });
    }
    let tag = if media.season.is_some() {
        files.push(Sidecar {
            dir: &show,
            name: "tvshow.nfo".into(),
            owner: &show_owner,
            data: nfo("tvshow", media, m, &art).into(),
        });
        if relative != show_relative {
            for (_, name) in &mut art {
                name.insert_str(0, "../");
            }
        }
        "episodedetails"
    } else {
        "movie"
    };
    files.push(Sidecar {
        dir: &dir,
        name: format!("{stem}.nfo"),
        owner: &media_owner,
        data: nfo(tag, media, m, &art).into(),
    });
    install(conn, &media.root, &files)
}
