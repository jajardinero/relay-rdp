//! Files copied on the remote computer, pasted here.
//!
//! Linux file managers paste files by path, so remote files need one: they appear in a small
//! read-only FUSE filesystem, mounted on first use under `$XDG_RUNTIME_DIR/relay`. Reading a
//! file there fetches that range from the server, so nothing is transferred until something
//! reads it, and the file manager's own copy dialog shows the progress.
//!
//! Each remote copy gets its own numbered folder, so pasting from an earlier copy keeps working
//! while the server still holds it (it does while its clipboard lock is held).
//!
//! FUSE requests run on fuser's thread; the replies to reads are sent from the session thread
//! when the server's data arrives, so several reads can be in flight at once.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use ironrdp_cliprdr::backend::ClipboardMessage;
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse,
    FileDescriptor,
};
use parking_lot::Mutex;
use tokio::sync::mpsc::UnboundedSender;

use super::local_files::FILETIME_UNIX_EPOCH;
use crate::session::SessionCmd;

const ROOT: u64 = 1;
/// Remote copies kept; older folders disappear.
const KEEP_COPIES: usize = 8;
/// Listed entries never change, so the kernel may cache them.
const TTL: Duration = Duration::from_secs(60);
/// Larger reads mean fewer round trips to the server.
const MAX_READAHEAD: u32 = 1 << 20;

/// A file list from the server, ready to show once every size is known.
#[derive(Debug)]
pub struct Listing {
    files: Vec<FileDescriptor>,
    lock: Option<u32>,
    conn: u64,
    serial: u64,
}

pub struct RemoteFiles {
    shared: Arc<Shared>,
    mount: Mutex<Option<Mount>>,
}

impl std::fmt::Debug for RemoteFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFiles")
            .field("mount", &self.mount.lock().as_ref().map(|m| &m.path))
            .finish_non_exhaustive()
    }
}

impl RemoteFiles {
    pub fn new(session: UnboundedSender<SessionCmd>) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                session,
            }),
            mount: Mutex::new(None),
        }
    }

    /// A CLIPRDR channel opened; returns its id. Only files from the live connection can be read.
    pub fn connect(&self) -> u64 {
        let mut state = self.shared.state.lock();
        state.next_conn += 1;
        state.conn = state.next_conn;
        state.conn
    }

    /// The channel closed: reads waiting on it fail.
    pub fn disconnect(&self, conn: u64) {
        let mut state = self.shared.state.lock();
        if state.conn == conn {
            state.conn = 0;
        }
        let waiting: Vec<u32> = state
            .pending
            .iter()
            .filter(|(_, p)| p.conn() == conn)
            .map(|(&id, _)| id)
            .collect();
        for id in waiting {
            if let Some(pending) = state.pending.remove(&id) {
                pending.fail(&mut state);
            }
        }
    }

    /// The remote clipboard changed. Unlocked file lists stop being readable, as the server only
    /// serves its current clipboard.
    pub fn clipboard_changed(&self) {
        let mut state = self.shared.state.lock();
        state.serial += 1;
        state.staged = None;
    }

    /// The server listed its copied files. Returns the listing if it's ready to show; otherwise
    /// the server is asked for the sizes it left out, and [`Self::on_response`] returns it later.
    pub fn stage(&self, files: &[FileDescriptor], lock: Option<u32>) -> Option<Listing> {
        let mut state = self.shared.state.lock();
        let listing = Listing {
            files: files.to_vec(),
            lock,
            conn: state.conn,
            serial: state.serial,
        };
        let missing: Vec<usize> = (listing.files.iter().enumerate())
            .filter(|(_, f)| !is_dir(f) && f.file_size.is_none())
            .map(|(i, _)| i)
            .collect();
        state.staged = None;
        if missing.is_empty() {
            return Some(listing);
        }
        state.next_staged += 1;
        let id = state.next_staged;
        let conn = listing.conn;
        state.staged = Some(Staged {
            id,
            missing: missing.len(),
            listing,
        });
        for index in missing {
            let request = FileContentsRequest {
                stream_id: 0,
                index: i32::try_from(index).unwrap_or(i32::MAX),
                flags: FileContentsFlags::SIZE,
                position: 0,
                requested_size: 8,
                data_id: lock,
            };
            self.shared.send(
                &mut state,
                request,
                Pending::Size {
                    staged: id,
                    index,
                    conn,
                },
            );
        }
        None
    }

    /// The server answered one of our requests. Returns a listing once its last size arrives.
    pub fn on_response(&self, response: &FileContentsResponse<'_>) -> Option<Listing> {
        let mut state = self.shared.state.lock();
        let pending = state.pending.remove(&response.stream_id())?;
        if response.is_error() {
            pending.fail(&mut state);
            return None;
        }
        match pending {
            Pending::Read(mut read) => {
                let data = response.data();
                let n = data.len().min(read.remaining as usize);
                read.data.extend_from_slice(&data[..n]);
                read.position += n as u64;
                read.remaining -= n as u32;
                if read.remaining == 0 {
                    read.reply.data(&read.data);
                } else if n == 0 {
                    tracing::warn!("a remote file ended before its listed size");
                    read.reply.error(Errno::EIO);
                } else {
                    // The server sent less than asked; ask for the rest.
                    self.shared.request(&mut state, read);
                }
                None
            }
            Pending::Size { staged, index, .. } => {
                let size = response.data_as_size().ok();
                let staged = state.staged.as_mut().filter(|s| s.id == staged)?;
                let Some(size) = size else {
                    tracing::warn!("the server sent a malformed file size");
                    state.staged = None;
                    return None;
                };
                staged.listing.files[index].file_size = Some(size);
                staged.missing -= 1;
                if staged.missing == 0 {
                    state.staged.take().map(|s| s.listing)
                } else {
                    None
                }
            }
        }
    }

    /// A request couldn't be sent to the server.
    pub fn fail(&self, stream_id: u32) {
        let mut state = self.shared.state.lock();
        if let Some(pending) = state.pending.remove(&stream_id) {
            pending.fail(&mut state);
        }
    }

    /// Adds the files to the filesystem (mounting it on first use) and returns the paths of the
    /// top-level entries, or `None` if the remote clipboard has changed since.
    pub fn publish(&self, listing: &Listing) -> std::io::Result<Option<Vec<PathBuf>>> {
        let root = self.mount_point()?;
        let mut state = self.shared.state.lock();
        if listing.serial != state.serial || listing.conn != state.conn {
            return Ok(None);
        }
        let (folder, names) = state.tree.add_copy(listing, SystemTime::now());
        Ok(Some(
            names
                .iter()
                .map(|name| root.join(&folder).join(name))
                .collect(),
        ))
    }

    /// Whether the path is one of ours (the local clipboard holds remote files).
    pub fn owns(&self, path: &Path) -> bool {
        (self.mount.lock().as_ref()).is_some_and(|mount| path.starts_with(&mount.path))
    }

    fn mount_point(&self) -> std::io::Result<PathBuf> {
        let mut mount = self.mount.lock();
        if let Some(mount) = mount.as_ref() {
            return Ok(mount.path.clone());
        }
        let mounted = Mount::new(self.shared.clone())?;
        let path = mounted.path.clone();
        *mount = Some(mounted);
        Ok(path)
    }
}

struct Shared {
    state: Mutex<State>,
    session: UnboundedSender<SessionCmd>,
}

impl Shared {
    fn read(&self, ino: u64, offset: u64, size: u32, reply: ReplyData) {
        let mut state = self.state.lock();
        let Some(file) = state.tree.file(ino) else {
            return reply.error(Errno::ENOENT);
        };
        // Without a lock the server only serves its current clipboard, where an older list's
        // entries may now mean other files.
        let live = file.conn == state.conn && (file.lock.is_some() || file.serial == state.serial);
        if !live {
            return reply.error(Errno::EIO);
        }
        let end = file.size.min(offset.saturating_add(u64::from(size)));
        if end <= offset {
            return reply.data(&[]);
        }
        let remaining = u32::try_from(end - offset).expect("at most `size`");
        let read = Read {
            reply,
            file,
            position: offset,
            remaining,
            data: Vec::with_capacity(remaining as usize),
        };
        self.request(&mut state, read);
    }

    fn request(&self, state: &mut State, read: Read) {
        let request = FileContentsRequest {
            stream_id: 0,
            index: read.file.index,
            flags: FileContentsFlags::RANGE,
            position: read.position,
            requested_size: read.remaining,
            data_id: read.file.lock,
        };
        self.send(state, request, Pending::Read(read));
    }

    fn send(&self, state: &mut State, mut request: FileContentsRequest, pending: Pending) {
        let stream_id = state.next_stream;
        state.next_stream = state.next_stream.wrapping_add(1);
        request.stream_id = stream_id;
        state.pending.insert(stream_id, pending);
        let message = SessionCmd::Clipboard(ClipboardMessage::SendFileContentsRequest(request));
        if self.session.send(message).is_err()
            && let Some(pending) = state.pending.remove(&stream_id)
        {
            pending.fail(state);
        }
    }
}

#[derive(Default)]
struct State {
    tree: Tree,
    /// The live connection (0 while there is none).
    conn: u64,
    next_conn: u64,
    /// Bumped whenever the remote clipboard changes.
    serial: u64,
    next_stream: u32,
    pending: HashMap<u32, Pending>,
    /// A list waiting for the sizes the server left out.
    staged: Option<Staged>,
    next_staged: u64,
}

struct Staged {
    id: u64,
    missing: usize,
    listing: Listing,
}

enum Pending {
    Read(Read),
    Size {
        staged: u64,
        index: usize,
        conn: u64,
    },
}

impl Pending {
    fn conn(&self) -> u64 {
        match self {
            Self::Read(read) => read.file.conn,
            Self::Size { conn, .. } => *conn,
        }
    }

    fn fail(self, state: &mut State) {
        match self {
            Self::Read(read) => read.reply.error(Errno::EIO),
            Self::Size { staged, .. } => {
                if state.staged.as_ref().is_some_and(|s| s.id == staged) {
                    tracing::warn!("the server couldn't say how big a copied file is");
                    state.staged = None;
                }
            }
        }
    }
}

/// A read being served, possibly over several requests.
struct Read {
    reply: ReplyData,
    file: RemoteFile,
    position: u64,
    remaining: u32,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RemoteFile {
    size: u64,
    /// The entry's position in the server's list.
    index: i32,
    /// The server's clipboard lock covering the list, if locking was negotiated.
    lock: Option<u32>,
    /// The connection and remote clipboard the list came from.
    conn: u64,
    serial: u64,
}

enum NodeKind {
    Dir {
        children: Vec<u64>,
        by_name: HashMap<OsString, u64>,
    },
    File(RemoteFile),
}

struct Node {
    parent: u64,
    name: OsString,
    mtime: SystemTime,
    kind: NodeKind,
}

impl Node {
    fn dir(parent: u64, name: OsString, mtime: SystemTime) -> Self {
        Self {
            parent,
            name,
            mtime,
            kind: NodeKind::Dir {
                children: Vec::new(),
                by_name: HashMap::new(),
            },
        }
    }

    fn file_type(&self) -> FileType {
        match self.kind {
            NodeKind::Dir { .. } => FileType::Directory,
            NodeKind::File(_) => FileType::RegularFile,
        }
    }
}

/// Inodes, with one numbered folder per remote copy under the root.
struct Tree {
    nodes: HashMap<u64, Node>,
    next_ino: u64,
    copies: VecDeque<u64>,
    next_copy: u64,
}

impl Default for Tree {
    fn default() -> Self {
        let root = Node::dir(ROOT, OsString::new(), SystemTime::now());
        Self {
            nodes: HashMap::from([(ROOT, root)]),
            next_ino: ROOT + 1,
            copies: VecDeque::new(),
            next_copy: 1,
        }
    }
}

impl Tree {
    fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        match &self.nodes.get(&parent)?.kind {
            NodeKind::Dir { by_name, .. } => by_name.get(name).copied(),
            NodeKind::File(_) => None,
        }
    }

    fn file(&self, ino: u64) -> Option<RemoteFile> {
        match self.nodes.get(&ino)?.kind {
            NodeKind::File(file) => Some(file),
            NodeKind::Dir { .. } => None,
        }
    }

    fn insert(&mut self, parent: u64, node: Node) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        if let Some(Node {
            kind: NodeKind::Dir { children, by_name },
            ..
        }) = self.nodes.get_mut(&parent)
        {
            children.push(ino);
            by_name.insert(node.name.clone(), ino);
        }
        self.nodes.insert(ino, node);
        ino
    }

    /// The named folder in `parent`, created if needed; `None` if a file has that name.
    fn folder(&mut self, parent: u64, name: &str, now: SystemTime) -> Option<u64> {
        match self.child(parent, OsStr::new(name)) {
            Some(ino) => matches!(self.nodes[&ino].kind, NodeKind::Dir { .. }).then_some(ino),
            None => Some(self.insert(parent, Node::dir(parent, name.into(), now))),
        }
    }

    /// Adds a copy's folder; returns its name and the names of its top-level entries.
    fn add_copy(&mut self, listing: &Listing, now: SystemTime) -> (String, Vec<OsString>) {
        let folder_name = self.next_copy.to_string();
        self.next_copy += 1;
        let folder = self.insert(ROOT, Node::dir(ROOT, folder_name.clone().into(), now));
        self.copies.push_back(folder);

        for (index, entry) in listing.files.iter().enumerate() {
            let mtime = entry.last_write_time.map_or(now, from_filetime);
            // The library has already dropped `..`, drive letters and the like from the path.
            let mut parent = Some(folder);
            for part in (entry.relative_path.iter())
                .flat_map(|p| p.split('\\'))
                .filter(|p| !p.is_empty())
            {
                parent = parent.and_then(|p| self.folder(p, part, now));
            }
            let Some(parent) = parent else {
                tracing::debug!(name = %entry.name, "remote file's folder clashes with a file; skipping");
                continue;
            };
            if is_dir(entry) {
                if let Some(dir) = self.folder(parent, &entry.name, now) {
                    self.nodes.get_mut(&dir).expect("just found").mtime = mtime;
                }
            } else if self.child(parent, OsStr::new(&entry.name)).is_some() {
                tracing::debug!(name = %entry.name, "duplicate remote file name; skipping");
            } else if let Ok(index) = i32::try_from(index) {
                let file = RemoteFile {
                    size: entry.file_size.unwrap_or(0),
                    index,
                    lock: listing.lock,
                    conn: listing.conn,
                    serial: listing.serial,
                };
                let node = Node {
                    parent,
                    name: entry.name.clone().into(),
                    mtime,
                    kind: NodeKind::File(file),
                };
                self.insert(parent, node);
            }
        }

        while KEEP_COPIES < self.copies.len() {
            let oldest = self.copies.pop_front().expect("non-empty");
            self.remove(oldest);
        }

        let names = match &self.nodes[&folder].kind {
            NodeKind::Dir { children, .. } => children
                .iter()
                .map(|c| self.nodes[c].name.clone())
                .collect(),
            NodeKind::File(_) => Vec::new(),
        };
        (folder_name, names)
    }

    fn remove(&mut self, ino: u64) {
        let Some(node) = self.nodes.remove(&ino) else {
            return;
        };
        if let Some(Node {
            kind: NodeKind::Dir { children, by_name },
            ..
        }) = self.nodes.get_mut(&node.parent)
        {
            children.retain(|&c| c != ino);
            by_name.remove(&node.name);
        }
        if let NodeKind::Dir { children, .. } = node.kind {
            for child in children {
                self.remove(child);
            }
        }
    }
}

fn is_dir(entry: &FileDescriptor) -> bool {
    entry
        .attributes
        .is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY))
}

fn from_filetime(filetime: u64) -> SystemTime {
    let ticks = |t: u64| Duration::from_nanos(t.saturating_mul(100));
    match filetime.checked_sub(FILETIME_UNIX_EPOCH) {
        Some(after) => UNIX_EPOCH + ticks(after),
        None => UNIX_EPOCH - ticks(FILETIME_UNIX_EPOCH - filetime),
    }
}

struct Mount {
    path: PathBuf,
    session: Option<fuser::BackgroundSession>,
}

impl Mount {
    fn new(shared: Arc<Shared>) -> std::io::Result<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(std::env::temp_dir)
            .join("relay");
        std::fs::create_dir_all(&base)?;
        remove_stale_mounts(&base);
        let path = base.join(format!(
            "clipboard-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path)?;
        let owner = std::fs::metadata(&path)?;

        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoExec,
            MountOption::FSName("relay".into()),
            MountOption::Subtype("clipboard".into()),
        ];
        let fs = Fs {
            shared,
            uid: owner.uid(),
            gid: owner.gid(),
        };
        match fuser::spawn_mount(fs, &path, &config) {
            Ok(session) => {
                tracing::info!(path = %path.display(), "mounted remote clipboard files");
                Ok(Self {
                    path,
                    session: Some(session),
                })
            }
            Err(e) => {
                let _ = std::fs::remove_dir(&path);
                Err(e)
            }
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        // Unmounts (lazily, so an open file doesn't keep it).
        drop(self.session.take());
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Mounts left by an instance that crashed answer everything with ENOTCONN until unmounted.
fn remove_stale_mounts(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = (name.to_str())
            .and_then(|n| n.strip_prefix("clipboard-"))
            .and_then(|n| n.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok());
        let Some(pid) = pid else { continue };
        if pid == std::process::id() || Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let path = entry.path();
        if std::fs::remove_dir(&path).is_err() {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z", "-q"])
                .arg(&path)
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::fs::remove_dir(&path);
        }
    }
}

struct Fs {
    shared: Arc<Shared>,
    uid: u32,
    gid: u32,
}

impl Fs {
    fn attr(&self, tree: &Tree, ino: u64) -> Option<FileAttr> {
        let node = tree.nodes.get(&ino)?;
        let (size, perm, nlink) = match &node.kind {
            NodeKind::Dir { .. } => (0, 0o555, 2),
            NodeKind::File(file) => (file.size, 0o444, 1),
        };
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: node.mtime,
            mtime: node.mtime,
            ctime: node.mtime,
            crtime: node.mtime,
            kind: node.file_type(),
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 128 * 1024,
            flags: 0,
        })
    }
}

impl Filesystem for Fs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        if let Err(max) = config.set_max_readahead(MAX_READAHEAD) {
            let _ = config.set_max_readahead(max);
        }
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let state = self.shared.state.lock();
        let attr = (state.tree.child(parent.0, name)).and_then(|ino| self.attr(&state.tree, ino));
        match attr {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let state = self.shared.state.lock();
        match self.attr(&state.tree, ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let state = self.shared.state.lock();
        match state.tree.nodes.get(&ino.0).map(|n| &n.kind) {
            None => reply.error(Errno::ENOENT),
            Some(NodeKind::Dir { .. }) => reply.error(Errno::EISDIR),
            Some(NodeKind::File(_)) if flags.acc_mode() != OpenAccMode::O_RDONLY => {
                reply.error(Errno::EROFS);
            }
            // A copy's contents never change, so cached pages stay valid.
            Some(NodeKind::File(_)) => reply.opened(FileHandle(0), FopenFlags::FOPEN_KEEP_CACHE),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.shared.read(ino.0, offset, size, reply);
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let state = self.shared.state.lock();
        let Some(node) = state.tree.nodes.get(&ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        let NodeKind::Dir { children, .. } = &node.kind else {
            return reply.error(Errno::ENOTDIR);
        };
        let dots = [
            (ino.0, FileType::Directory, OsStr::new(".")),
            (node.parent, FileType::Directory, OsStr::new("..")),
        ];
        let entries = dots.into_iter().chain(children.iter().map(|c| {
            let child = &state.tree.nodes[c];
            (*c, child.file_type(), child.name.as_os_str())
        }));
        for (i, (ino, kind, name)) in entries.enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let exists = self.shared.state.lock().tree.nodes.contains_key(&ino.0);
        if !exists {
            reply.error(Errno::ENOENT);
        } else if mask.contains(AccessFlags::W_OK) {
            reply.error(Errno::EROFS);
        } else {
            reply.ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry as the protocol library hands it over: `relative_path` plus `name`.
    fn named(path: &str) -> FileDescriptor {
        match path.rsplit_once('\\') {
            Some((dir, name)) => FileDescriptor::new(name).with_relative_path(dir),
            None => FileDescriptor::new(path),
        }
    }

    fn dir(path: &str) -> FileDescriptor {
        named(path).with_attributes(ClipboardFileAttributes::DIRECTORY)
    }

    fn file(path: &str, size: u64) -> FileDescriptor {
        named(path).with_file_size(size)
    }

    fn listing(files: Vec<FileDescriptor>) -> Listing {
        Listing {
            files,
            lock: Some(3),
            conn: 1,
            serial: 0,
        }
    }

    fn path_of(tree: &Tree, mut ino: u64) -> String {
        let mut parts = Vec::new();
        while ino != ROOT {
            let node = &tree.nodes[&ino];
            parts.push(node.name.to_string_lossy().into_owned());
            ino = node.parent;
        }
        parts.reverse();
        parts.join("/")
    }

    fn lookup(tree: &Tree, path: &str) -> Option<u64> {
        path.split('/')
            .try_fold(ROOT, |ino, part| tree.child(ino, OsStr::new(part)))
    }

    #[test]
    fn copies_become_numbered_folders() {
        let mut tree = Tree::default();
        let files = vec![
            dir("docs"),
            file("docs\\a.txt", 5),
            file("docs\\sub\\b.txt", 7), // its folder wasn't listed
            file("top.bin", 1),
            file("top.bin", 2), // duplicate
        ];
        let (folder, names) = tree.add_copy(&listing(files), SystemTime::now());
        assert_eq!(folder, "1");
        assert_eq!(names, [OsString::from("docs"), OsString::from("top.bin")]);

        let b = lookup(&tree, "1/docs/sub/b.txt").unwrap();
        assert_eq!(path_of(&tree, b), "1/docs/sub/b.txt");
        let b = tree.file(b).unwrap();
        assert_eq!((b.size, b.index, b.lock), (7, 2, Some(3)));
        assert_eq!(
            tree.file(lookup(&tree, "1/top.bin").unwrap())
                .unwrap()
                .index,
            3
        );
        assert!(tree.file(lookup(&tree, "1/docs").unwrap()).is_none());

        let (folder, _) = tree.add_copy(&listing(vec![file("x", 0)]), SystemTime::now());
        assert_eq!(folder, "2");
        assert!(lookup(&tree, "1/docs/a.txt").is_some());
    }

    #[test]
    fn old_copies_are_dropped() {
        let mut tree = Tree::default();
        for _ in 0..KEEP_COPIES + 2 {
            tree.add_copy(&listing(vec![file("d\\f", 1)]), SystemTime::now());
        }
        assert!(lookup(&tree, "1").is_none());
        assert!(lookup(&tree, "2").is_none());
        assert!(lookup(&tree, "3/d/f").is_some());
        // Root, then a folder, a subfolder and a file per copy kept.
        assert_eq!(tree.nodes.len(), 1 + KEEP_COPIES * 3);
    }

    #[test]
    fn filetimes_convert() {
        assert_eq!(from_filetime(FILETIME_UNIX_EPOCH), UNIX_EPOCH);
        assert_eq!(
            from_filetime(FILETIME_UNIX_EPOCH + 15_000_000),
            UNIX_EPOCH + Duration::from_millis(1500)
        );
        assert!(from_filetime(0) < UNIX_EPOCH);
    }

    /// Mounts the filesystem and reads a file through it, answering the requests the way a
    /// server would (including a short answer). Skipped where FUSE isn't available.
    #[test]
    fn reads_go_to_the_server() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let remote = Arc::new(RemoteFiles::new(tx));
        let conn = remote.connect();
        remote.clipboard_changed();

        let contents: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        // One size is missing, so it's asked for first.
        let files = [
            dir("dir"),
            named("dir\\data.bin"), // no size
            file("dir\\unread.bin", 5),
        ];
        assert!(remote.stage(&files, Some(9)).is_none());
        let Some(SessionCmd::Clipboard(ClipboardMessage::SendFileContentsRequest(size))) =
            rx.try_recv().ok()
        else {
            panic!("expected a size request");
        };
        assert_eq!(
            (size.index, size.flags, size.data_id),
            (1, FileContentsFlags::SIZE, Some(9))
        );
        let listing = remote
            .on_response(&FileContentsResponse::new_size_response(
                size.stream_id,
                contents.len() as u64,
            ))
            .expect("all sizes known");

        let paths = match remote.publish(&listing) {
            Ok(paths) => paths.unwrap(),
            Err(e) => {
                eprintln!("skipping: can't mount FUSE here ({e})");
                return;
            }
        };
        assert_eq!(paths.len(), 1);
        assert!(remote.owns(&paths[0]));
        let file = paths[0].join("data.bin");
        assert_eq!(
            std::fs::metadata(&file).unwrap().len(),
            contents.len() as u64
        );
        assert!(std::fs::OpenOptions::new().write(true).open(&file).is_err());

        let reader = std::thread::spawn({
            let file = file.clone();
            move || std::fs::read(file)
        });
        let mut served = 0;
        let mut short = true;
        while !reader.is_finished() {
            let Ok(SessionCmd::Clipboard(ClipboardMessage::SendFileContentsRequest(req))) =
                rx.try_recv()
            else {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            };
            assert_eq!(
                (req.index, req.flags, req.data_id),
                (1, FileContentsFlags::RANGE, Some(9))
            );
            let start = usize::try_from(req.position).unwrap();
            let mut end = (start + req.requested_size as usize).min(contents.len());
            if short && end - start > 10 {
                // Servers may send less than asked.
                end = start + 10;
                short = false;
            }
            served += end - start;
            let data = contents[start..end].to_vec();
            assert!(
                remote
                    .on_response(&FileContentsResponse::new_data_response(
                        req.stream_id,
                        data
                    ))
                    .is_none()
            );
        }
        assert_eq!(reader.join().unwrap().unwrap(), contents);
        assert!(served >= contents.len());

        // Once the connection is gone, reads fail instead of hanging.
        remote.disconnect(conn);
        assert!(std::fs::read(paths[0].join("unread.bin")).is_err());
        assert!(rx.try_recv().is_err());

        // Dropping it unmounts.
        let mount = paths[0].parent().unwrap().parent().unwrap().to_path_buf();
        drop(remote);
        assert!(!mount.exists());
    }
}
