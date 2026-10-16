//! Files copied here, pasted on the remote computer.
//!
//! The server first gets a list of names, sizes and times (copied folders are walked, so their
//! contents come along), then reads whatever it pastes in ranges, straight from the files on
//! disk.

use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor,
};

/// Entries offered at most (each is 592 bytes on the wire).
const MAX_ENTRIES: usize = 20_000;
/// Names are 260 UTF-16 units on the wire, including the terminator.
const MAX_NAME_UNITS: usize = 259;
/// Largest range served at once; the server asks again for the rest.
const MAX_CHUNK: u64 = 16 << 20;

#[derive(Debug, Default)]
pub struct LocalFiles {
    /// What the server sees, in order; its requests refer to entries by position.
    pub descriptors: Vec<FileDescriptor>,
    /// The file behind each descriptor (`None` for folders).
    paths: Vec<Option<PathBuf>>,
}

/// The file the last range was read from, kept open for the next one.
pub type OpenFile = Option<(PathBuf, File)>;

impl LocalFiles {
    pub fn collect(roots: &[PathBuf]) -> Self {
        let mut files = Self::default();
        for root in roots {
            let Some(name) = root.file_name() else {
                continue;
            };
            files.add(root, &windows_name(name), None, true);
        }
        if MAX_ENTRIES <= files.descriptors.len() {
            tracing::warn!(
                max = MAX_ENTRIES,
                "too many copied files; only the first ones go to the remote computer"
            );
        }
        files
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    /// `follow_dir`: whether to descend if `path` is a folder. Symlinked folders inside a copied
    /// folder are skipped, as they can loop.
    fn add(&mut self, path: &Path, name: &str, parent: Option<&str>, follow_dir: bool) {
        if MAX_ENTRIES <= self.descriptors.len() {
            return;
        }
        let wire_name = match parent {
            Some(parent) => format!("{parent}\\{name}"),
            None => name.to_owned(),
        };
        if MAX_NAME_UNITS < wire_name.encode_utf16().count() {
            tracing::warn!(path = %path.display(), "path too long to copy to the remote computer; skipping");
            return;
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "can't copy file");
                return;
            }
        };

        let mut descriptor = FileDescriptor::new(name);
        if let Some(parent) = parent {
            descriptor = descriptor.with_relative_path(parent);
        }
        if let Ok(modified) = meta.modified() {
            descriptor = descriptor.with_last_write_time(filetime(modified));
        }
        if meta.is_dir() && follow_dir {
            self.descriptors
                .push(descriptor.with_attributes(ClipboardFileAttributes::DIRECTORY));
            self.paths.push(None);
            let Ok(entries) = std::fs::read_dir(path) else {
                return;
            };
            let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let follow = entry.file_type().is_ok_and(|t| !t.is_symlink());
                self.add(
                    &entry.path(),
                    &windows_name(&entry.file_name()),
                    Some(&wire_name),
                    follow,
                );
            }
        } else if meta.is_file() {
            self.descriptors.push(
                descriptor
                    .with_attributes(ClipboardFileAttributes::NORMAL)
                    .with_file_size(meta.len()),
            );
            self.paths.push(Some(path.to_path_buf()));
        }
    }

    /// Answers the server's request for a file's size or a range of its contents.
    pub fn respond(
        &self,
        request: &FileContentsRequest,
        open: &mut OpenFile,
    ) -> FileContentsResponse<'static> {
        let stream = request.stream_id;
        let path = usize::try_from(request.index)
            .ok()
            .and_then(|i| self.paths.get(i))
            .and_then(Option::as_ref);
        let Some(path) = path else {
            return FileContentsResponse::new_error(stream);
        };
        let result = if request.flags.contains(FileContentsFlags::SIZE) {
            std::fs::metadata(path)
                .map(|m| FileContentsResponse::new_size_response(stream, m.len()))
        } else {
            read_range(
                path,
                request.position,
                u64::from(request.requested_size),
                open,
            )
            .map(|data| FileContentsResponse::new_data_response(stream, data))
        };
        result.unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), error = %e, "couldn't read a copied file");
            *open = None;
            FileContentsResponse::new_error(stream)
        })
    }
}

fn read_range(
    path: &Path,
    position: u64,
    len: u64,
    open: &mut OpenFile,
) -> std::io::Result<Vec<u8>> {
    if open.as_ref().is_none_or(|(p, _)| p != path) {
        *open = Some((path.to_path_buf(), File::open(path)?));
    }
    let (_, file) = open.as_ref().expect("opened above");
    let available = file.metadata()?.len().saturating_sub(position);
    let mut buf = vec![0; usize::try_from(len.min(MAX_CHUNK).min(available)).unwrap_or(0)];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read_at(&mut buf[filled..], position + filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Windows can't create names containing these, and `\` would read as a folder separator.
fn windows_name(name: &OsStr) -> String {
    name.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_control() || r#"\/:*?"<>|"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// 100 ns intervals between 1601-01-01 (the FILETIME epoch) and 1970-01-01.
pub const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

/// A Windows FILETIME.
fn filetime(time: SystemTime) -> u64 {
    let ticks = |d: std::time::Duration| u64::try_from(d.as_nanos() / 100).unwrap_or(u64::MAX);
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => FILETIME_UNIX_EPOCH.saturating_add(ticks(after)),
        Err(before) => FILETIME_UNIX_EPOCH.saturating_sub(ticks(before.duration())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn range(
        files: &LocalFiles,
        index: i32,
        position: u64,
        size: u32,
    ) -> FileContentsResponse<'static> {
        let request = FileContentsRequest {
            stream_id: 7,
            index,
            flags: FileContentsFlags::RANGE,
            position,
            requested_size: size,
            data_id: None,
        };
        files.respond(&request, &mut None)
    }

    #[test]
    fn folders_are_walked_with_relative_paths() {
        let dir = scratch("local-files-walk");
        std::fs::create_dir_all(dir.join("docs/sub")).unwrap();
        std::fs::write(dir.join("docs/a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("docs/sub/b:c.txt"), b"").unwrap();
        std::fs::write(dir.join("top.bin"), [1, 2, 3]).unwrap();
        std::os::unix::fs::symlink(dir.join("docs"), dir.join("docs/sub/loop")).unwrap();

        let files = LocalFiles::collect(&[dir.join("docs"), dir.join("top.bin")]);
        let listed: Vec<_> = files
            .descriptors
            .iter()
            .map(|d| (d.relative_path.as_deref(), d.name.as_str(), d.file_size))
            .collect();
        assert_eq!(
            listed,
            [
                (None, "docs", None),
                (Some("docs"), "a.txt", Some(5)),
                (Some("docs"), "sub", None),
                (Some("docs\\sub"), "b_c.txt", Some(0)),
                (None, "top.bin", Some(3)),
            ]
        );
        assert!(
            files.descriptors[0]
                .attributes
                .unwrap()
                .contains(ClipboardFileAttributes::DIRECTORY)
        );

        assert_eq!(range(&files, 1, 1, 100).data(), b"ello");
        assert_eq!(range(&files, 4, 0, 3).data(), [1, 2, 3]);
        // Folders and unknown entries have no contents.
        assert!(range(&files, 0, 0, 1).is_error());
        assert!(range(&files, 9, 0, 1).is_error());

        let size = FileContentsRequest {
            stream_id: 1,
            index: 1,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: None,
        };
        assert_eq!(files.respond(&size, &mut None).data_as_size().unwrap(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overlong_paths_are_skipped() {
        let dir = scratch("local-files-long");
        let long = "x".repeat(200);
        std::fs::create_dir_all(dir.join(&long)).unwrap();
        std::fs::write(dir.join(&long).join("y".repeat(100)), b"").unwrap();
        std::fs::write(dir.join(&long).join("short"), b"").unwrap();

        let files = LocalFiles::collect(&[dir.join(&long)]);
        let names: Vec<_> = files.descriptors.iter().map(|d| d.name.len()).collect();
        assert_eq!(names, [200, 5]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn filetime_epochs() {
        assert_eq!(filetime(UNIX_EPOCH), FILETIME_UNIX_EPOCH);
        assert_eq!(
            filetime(UNIX_EPOCH + std::time::Duration::from_secs(1)),
            FILETIME_UNIX_EPOCH + 10_000_000
        );
    }
}
