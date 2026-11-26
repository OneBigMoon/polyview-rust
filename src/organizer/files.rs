use super::{AppError, conflict};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::digest::{Context, SHA256};
use rustix::fs::{self, AtFlags, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

fn io_error(e: Errno) -> AppError {
    match e {
        Errno::EXIST | Errno::LOOP | Errno::NOTDIR => {
            conflict("Organizer path conflicts with an existing file or symlink")
        }
        Errno::NOENT => AppError::not_found("Organizer path is missing"),
        _ => std::io::Error::from(e).into(),
    }
}

pub(super) fn relative_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 4096
        && !s.contains('\\')
        && !s.chars().any(char::is_control)
        && s.split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && part.len() <= 255)
        && Path::new(s)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

fn sync_directory(dir: &File) -> Result<(), AppError> {
    fs::fsync(dir).map_err(io_error)
}

fn open_dir(parent: &File, name: &str, create: bool) -> Result<File, AppError> {
    let flags = OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = fs::openat(parent, name, flags, Mode::empty());
    if create && matches!(fd, Err(Errno::NOENT)) {
        match fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)) {
            Ok(()) => sync_directory(parent)?,
            Err(Errno::EXIST) => (),
            Err(e) => return Err(io_error(e)),
        }
        fs::openat(parent, name, flags, Mode::empty())
            .map(File::from)
            .map_err(io_error)
    } else {
        fd.map(File::from).map_err(io_error)
    }
}

pub(super) fn open_root(root: &Path) -> Result<File, AppError> {
    fs::open(
        root,
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io_error)
}

pub(super) struct Entry {
    dir: File,
    name: String,
}

impl Entry {
    pub(super) fn resolve(
        root: &File,
        relative: &str,
        create_parents: bool,
    ) -> Result<Self, AppError> {
        if !relative_path(relative) {
            return Err(AppError::bad_request("Unsafe organizer path"));
        }
        let mut dir = root.try_clone()?;
        let mut components = relative.split('/');
        let name = components.next_back().unwrap();
        for part in components {
            dir = open_dir(&dir, part, create_parents)?;
        }
        Ok(Self {
            dir,
            name: name.to_owned(),
        })
    }

    pub(super) fn private(&self, name: &str) -> Result<Self, AppError> {
        let dir = open_dir(&self.dir, ".polyview-organizer", true)?;
        let metadata = dir.metadata()?;
        if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(conflict(
                "Organizer staging directory must be service-owned and private",
            ));
        }
        Ok(Self {
            dir,
            name: name.to_owned(),
        })
    }

    fn open(&self) -> Result<Option<File>, AppError> {
        match fs::openat(
            &self.dir,
            self.name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let f = File::from(fd);
                if !f.metadata()?.is_file() {
                    return Err(conflict("Organizer expected a regular file"));
                }
                Ok(Some(f))
            }
            Err(Errno::NOENT) => Ok(None),
            Err(e) => Err(io_error(e)),
        }
    }

    pub(super) fn exists(&self) -> Result<bool, AppError> {
        Ok(self.open()?.is_some())
    }

    pub(super) fn stamp(&self) -> Result<Option<Stamp>, AppError> {
        self.open()?.map(Stamp::read).transpose()
    }

    pub(super) fn expect(&self, stamp: &Stamp) -> Result<(), AppError> {
        if self.stamp()?.as_ref() == Some(stamp) {
            Ok(())
        } else {
            Err(conflict("Organizer file has changed or is missing"))
        }
    }

    pub(super) fn matches_identity(&self, stamp: &Stamp) -> Result<bool, AppError> {
        match self.open()? {
            Some(file) => Ok(identity(&file.metadata()?) == stamp.identity),
            None => Ok(false),
        }
    }

    fn rename(&self, dest: &Self) -> Result<(), AppError> {
        fs::renameat_with(
            &self.dir,
            self.name.as_str(),
            &dest.dir,
            dest.name.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(io_error)?;
        sync_directory(&dest.dir)?;
        sync_directory(&self.dir)
    }

    // A same-device staging link is recognizable even if the process stops before
    // the next journal commit. A copy records its unique inode before its first write.
    pub(super) fn stage(
        &self,
        dest: &Self,
        expected: &Stamp,
        copy_id: Option<(u64, u64)>,
        remember: impl FnOnce((u64, u64)) -> Result<(), AppError>,
    ) -> Result<Stamp, AppError> {
        if let Some(id) = copy_id {
            return self.copy_to(dest, expected, Some(id), remember);
        }
        match fs::linkat(
            &self.dir,
            self.name.as_str(),
            &dest.dir,
            dest.name.as_str(),
            AtFlags::empty(),
        ) {
            Ok(()) | Err(Errno::EXIST) => {
                if !dest.matches_identity(expected)? {
                    return Err(conflict("Organizer staging link was replaced"));
                }
                sync_directory(&dest.dir)?;
                Ok(expected.clone())
            }
            Err(Errno::XDEV) => self.copy_to(dest, expected, None, remember),
            Err(e) => Err(io_error(e)),
        }
    }

    fn copy_to(
        &self,
        dest: &Self,
        expected: &Stamp,
        copy_id: Option<(u64, u64)>,
        remember: impl FnOnce((u64, u64)) -> Result<(), AppError>,
    ) -> Result<Stamp, AppError> {
        let flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        let flags = if copy_id.is_some() {
            flags
        } else {
            flags | OFlags::CREATE | OFlags::EXCL
        };
        let mut output = File::from(
            fs::openat(
                &dest.dir,
                dest.name.as_str(),
                flags,
                Mode::from_raw_mode(0o600),
            )
            .map_err(io_error)?,
        );
        let output_meta = output.metadata()?;
        let actual_id = (output_meta.dev(), output_meta.ino());
        if !output_meta.is_file()
            || copy_id.is_some_and(|id| id != actual_id)
            || output_meta.nlink() != 1
        {
            return Err(conflict(
                "Organizer staging copy was replaced or linked elsewhere",
            ));
        }
        let mut input = self
            .open()?
            .ok_or_else(|| conflict("Organizer source disappeared"))?;
        if identity(&input.metadata()?) != expected.identity {
            return Err(conflict("Organizer source changed"));
        }
        if copy_id.is_none() {
            sync_directory(&dest.dir)?;
            remember(actual_id)?;
        }
        let mut digest = Context::new(&SHA256);
        compare_prefix(&mut input, &mut output, &mut digest)?;
        let mut buf = [0; 64 * 1024];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            digest.update(&buf[..n]);
        }
        output.sync_all()?;
        let stamp = Stamp {
            identity: identity(&output.metadata()?),
            digest: STANDARD.encode(digest.finish().as_ref()),
        };
        if stamp.digest != expected.digest || identity(&input.metadata()?) != expected.identity {
            return Err(conflict("Organizer source changed while copying"));
        }
        Ok(stamp)
    }

    pub(super) fn partial_stamp(
        &self,
        source: &Self,
        expected: &Stamp,
        copy_id: (u64, u64),
    ) -> Result<Stamp, AppError> {
        let mut copy = self
            .open()?
            .ok_or_else(|| conflict("Organizer staging copy disappeared"))?;
        let meta = copy.metadata()?;
        if (meta.dev(), meta.ino()) != copy_id || meta.nlink() != 1 {
            return Err(conflict("Organizer staging copy was replaced"));
        }
        let mut input = source
            .open()?
            .ok_or_else(|| conflict("Organizer source disappeared"))?;
        if identity(&input.metadata()?) != expected.identity {
            return Err(conflict("Organizer source changed"));
        }
        let mut hash = Context::new(&SHA256);
        compare_prefix(&mut input, &mut copy, &mut hash)?;
        Ok(Stamp {
            identity: identity(&meta),
            digest: STANDARD.encode(hash.finish().as_ref()),
        })
    }

    pub(super) fn publish(&self, dest: &Self, expected: &Stamp) -> Result<(), AppError> {
        if self.exists()? {
            if !self.matches_identity(expected)? {
                return Err(conflict("Organizer staging file changed"));
            }
            self.rename(dest)?;
        }
        dest.expect(expected)
    }

    // Claim the public name atomically. A replacement moved by a racing editor is
    // restored without overwriting anything. Only a verified private entry is unlinked.
    pub(super) fn retire(
        &self,
        claimed: &Self,
        expected: &Stamp,
        keep: &Self,
        keep_stamp: &Stamp,
    ) -> Result<(), AppError> {
        if !claimed.exists()? {
            if !self.exists()? {
                return keep.expect(keep_stamp);
            }
            if !self.matches_identity(expected)? {
                return Err(conflict("Organizer file changed before removal"));
            }
            self.rename(claimed)?;
        }
        // A copy on another device needs its own content check. For a hardlink the
        // verified inode is shared, so checking its surviving name is sufficient.
        let safe = claimed.expect(expected).and_then(|()| {
            if expected != keep_stamp {
                return keep.expect(keep_stamp);
            }
            if keep.matches_identity(keep_stamp)? {
                Ok(())
            } else {
                Err(conflict("Organizer surviving path was replaced"))
            }
        });
        if let Err(e) = safe {
            claimed.rename(self)?;
            return Err(e);
        }
        if self.exists()? {
            return Err(conflict("Another file appeared at the organizer path"));
        }
        fs::unlinkat(&claimed.dir, claimed.name.as_str(), AtFlags::empty()).map_err(io_error)?;
        sync_directory(&claimed.dir)
    }
}

fn compare_prefix(
    input: &mut File,
    partial: &mut File,
    hash: &mut Context,
) -> Result<(), AppError> {
    let mut src = [0; 32 * 1024];
    let mut dst = [0; 32 * 1024];
    loop {
        let n = partial.read(&mut dst)?;
        if n == 0 {
            break;
        }
        input.read_exact(&mut src[..n])?;
        if src[..n] != dst[..n] {
            return Err(conflict("Organizer partial copy was modified"));
        }
        hash.update(&dst[..n]);
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stamp {
    identity: (u64, u64, u64, i64, i64),
    digest: String,
}

fn identity(m: &std::fs::Metadata) -> (u64, u64, u64, i64, i64) {
    (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec())
}

impl Stamp {
    fn read(mut file: File) -> Result<Self, AppError> {
        let before = identity(&file.metadata()?);
        let mut digest = Context::new(&SHA256);
        let mut buf = [0; 64 * 1024];
        loop {
            let count = file.read(&mut buf)?;
            if count == 0 {
                break;
            }
            digest.update(&buf[..count]);
        }
        if before != identity(&file.metadata()?) {
            return Err(conflict("Organizer file changed while reading"));
        }
        Ok(Self {
            identity: before,
            digest: STANDARD.encode(digest.finish().as_ref()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as disk;

    #[test]
    fn copy_fallback_resumes_verified_bytes_and_refuses_modified_partial_files() {
        let tmp = tempfile::tempdir().unwrap();
        disk::write(tmp.path().join("original"), b"abcdef").unwrap();
        let root = open_root(tmp.path()).unwrap();
        let src = Entry::resolve(&root, "original", false).unwrap();
        let dst = Entry::resolve(&root, "copy", false).unwrap();
        let expected = src.stamp().unwrap().unwrap();
        let mut receipt = None;
        let result = src.copy_to(&dst, &expected, None, |id| {
            receipt = Some(id);
            Err(conflict("Interrupted before streaming"))
        });
        assert!(result.is_err());
        disk::write(tmp.path().join("copy"), b"abX").unwrap();
        assert!(
            src.copy_to(&dst, &expected, receipt, |_| panic!("duplicate receipt"))
                .is_err()
        );
        assert_eq!(disk::read(tmp.path().join("copy")).unwrap(), b"abX");
        disk::write(tmp.path().join("copy"), b"abc").unwrap();
        let copy = src
            .copy_to(&dst, &expected, receipt, |_| panic!("duplicate receipt"))
            .unwrap();
        assert_eq!(expected.digest, copy.digest);
        assert_ne!(expected.identity.1, copy.identity.1);
        assert_eq!(disk::read(tmp.path().join("copy")).unwrap(), b"abcdef");
        let public = Entry::resolve(&root, "published", false).unwrap();
        dst.publish(&public, &copy).unwrap();
        public.publish(&dst, &copy).unwrap();
        src.expect(&expected).unwrap();
        dst.expect(&copy).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn different_linux_mounts_fall_back_to_copy() {
        let temp = tempfile::tempdir().unwrap();
        let Ok(shm) = tempfile::tempdir_in("/dev/shm") else {
            return;
        };
        let root = open_root(temp.path()).unwrap();
        let other = open_root(shm.path()).unwrap();
        if root.metadata().unwrap().dev() == other.metadata().unwrap().dev() {
            return;
        }
        std::fs::write(temp.path().join("movie"), b"different devices").unwrap();
        let source = Entry::resolve(&root, "movie", false).unwrap();
        let destination = Entry::resolve(&other, "copy", false).unwrap();
        let stamp = source.stamp().unwrap().unwrap();
        let mut copied = false;
        let result = source
            .stage(&destination, &stamp, None, |_| {
                copied = true;
                Ok(())
            })
            .unwrap();
        assert!(copied);
        assert_ne!(stamp.identity.0, result.identity.0);
        destination.expect(&result).unwrap();
        source.expect(&stamp).unwrap();
    }
}
