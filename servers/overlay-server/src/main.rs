//! # Overlay server
//!
//! A userspace (ring 3) filesystem server implementing the overlayfs model: a
//! RAM-backed writable **upper** layer merged over a read-only **lower** layer.
//! The lower layer is the SquashFS server, reached over IPC; the upper layer
//! lives in this server's heap. This is exactly how container runtimes stack a
//! writable layer over a read-only image — and how ThemeliOS gives its immutable
//! SquashFS root a writable runtime view without touching the underlying image.
//!
//! ## Merge semantics
//!
//! - **Lookup/open/read**: the upper layer wins. A file present in the upper
//!   layer hides the lower one. A *whiteout* in the upper layer hides a lower
//!   file entirely (it appears deleted).
//! - **Write**: if the target exists only in the lower layer, it is first
//!   *copied up* (read in full from the lower layer into the upper layer, up to
//!   a 1 MiB limit), then modified in the upper layer. The lower image is never
//!   written.
//! - **Create / mkdir**: lands in the upper layer only.
//! - **Unlink**: drops any upper copy and records a whiteout so the name
//!   disappears from the merged view even if it exists below.
//! - **Readdir**: merges lower entries with upper entries, upper winning, with
//!   whiteouted names removed.
//!
//! The upper layer is ephemeral — it lives only in RAM and is lost on reboot,
//! which is the desired behaviour for an immutable node's runtime state. Total
//! upper-layer memory is bounded (8 MiB budget); a single copy-up is capped at
//! 1 MiB.
//!
//! ## IPC wiring
//!
//! - Receives client requests on its own `fs_endpoint`, using its own *client*
//!   shared region (`client_shared_vaddr`) for paths and data.
//! - Forwards to the lower SquashFS server on the endpoint passed in `arg0`,
//!   using the *block-slot* shared region (`shared_vaddr`) — which is mapped to
//!   the SquashFS server's client region — for paths and data.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use libthemelios::fs_proto::{self, FsError};
use libthemelios::{boot_info, ipc, BootInfo};

/// Per-file copy-up limit (bytes). Files larger than this in the lower layer
/// cannot be copied up for modification.
const COPYUP_LIMIT: usize = 1024 * 1024;
/// Total upper-layer memory budget (bytes).
const UPPER_BUDGET: usize = 8 * 1024 * 1024;

// ---------- upper-layer node tree ----------

/// A node in the RAM-backed upper layer, keyed by absolute normalized path.
enum Node {
    /// A regular file with its full contents in RAM.
    File(Vec<u8>),
    /// A directory created in the upper layer.
    Dir,
    /// A whiteout: hides a same-named entry in the lower layer.
    Whiteout,
}

// ---------- lower-layer client (SquashFS over IPC) ----------

/// Client handle to the lower (SquashFS) filesystem server.
struct Lower {
    endpoint: u64,
    buf: *mut u8,
    len: usize,
}

impl Lower {
    fn write_path(&self, path: &str) -> u64 {
        let n = path.len().min(self.len);
        // SAFETY: the shared region is mapped read/write; we hold the only
        // outstanding lower request (synchronous).
        let s = unsafe { core::slice::from_raw_parts_mut(self.buf, n) };
        s.copy_from_slice(&path.as_bytes()[..n]);
        n as u64
    }

    fn read_region(&self, len: usize) -> Vec<u8> {
        let n = len.min(self.len);
        let s = unsafe { core::slice::from_raw_parts(self.buf, n) };
        s.to_vec()
    }

    /// Open a path in the lower layer, returning its fd (or None).
    fn open(&self, path: &str) -> Option<u64> {
        let plen = self.write_path(path);
        let r = ipc::call(self.endpoint, [fs_proto::OP_OPEN, plen, 0, 0], 0);
        if r.words[0] == fs_proto::STATUS_OK {
            Some(r.words[1])
        } else {
            None
        }
    }

    /// Stat a path in the lower layer → (size, is_dir).
    fn stat(&self, path: &str) -> Option<(u64, bool)> {
        let plen = self.write_path(path);
        let r = ipc::call(self.endpoint, [fs_proto::OP_STAT, plen, 0, 0], 0);
        if r.words[0] == fs_proto::STATUS_OK {
            Some((r.words[1], r.words[2] != 0))
        } else {
            None
        }
    }

    /// Read [offset, offset+len) of an open lower file.
    fn read(&self, fd: u64, offset: u64, len: usize) -> Vec<u8> {
        let r = ipc::call(self.endpoint, [fs_proto::OP_READ, fd, offset, len as u64], 0);
        if r.words[0] != fs_proto::STATUS_OK {
            return Vec::new();
        }
        self.read_region(r.words[1] as usize)
    }

    fn close(&self, fd: u64) {
        let _ = ipc::call(self.endpoint, [fs_proto::OP_CLOSE, fd, 0, 0], 0);
    }

    /// Read a whole lower file by path (for copy-up). Returns its bytes.
    fn read_all(&self, path: &str) -> Option<Vec<u8>> {
        let (size, is_dir) = self.stat(path)?;
        if is_dir {
            return None;
        }
        let fd = self.open(path)?;
        let mut out = Vec::with_capacity(size as usize);
        let mut off = 0u64;
        // Read in chunks bounded by the shared region size.
        while off < size {
            let want = ((size - off) as usize).min(self.len);
            let chunk = self.read(fd, off, want);
            if chunk.is_empty() {
                break;
            }
            off += chunk.len() as u64;
            out.extend_from_slice(&chunk);
        }
        self.close(fd);
        Some(out)
    }

    /// List a lower directory by fd → (name, type) entries.
    fn readdir(&self, fd: u64, max: u64) -> Vec<(String, u16)> {
        let r = ipc::call(self.endpoint, [fs_proto::OP_READDIR, fd, max, 0], 0);
        let mut entries = Vec::new();
        if r.words[0] != fs_proto::STATUS_OK {
            return entries;
        }
        let count = r.words[1] as usize;
        let data = self.read_region(self.len);
        let mut pos = 0usize;
        for _ in 0..count {
            if pos + 4 > data.len() {
                break;
            }
            let nlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            let type_id = u16::from_le_bytes([data[pos + 2], data[pos + 3]]);
            pos += 4;
            if pos + nlen > data.len() {
                break;
            }
            entries.push((String::from_utf8_lossy(&data[pos..pos + nlen]).into_owned(), type_id));
            pos += nlen;
        }
        entries
    }
}

// ---------- path helpers ----------

/// Normalize an absolute path: ensure a leading '/', drop a trailing '/'
/// (except for the root), and collapse empty components.
fn normalize(path: &str) -> String {
    let mut out = String::from("/");
    for comp in path.split('/') {
        if comp.is_empty() {
            continue;
        }
        if out.len() > 1 {
            out.push('/');
        }
        out.push_str(comp);
    }
    out
}

/// The parent directory of a normalized path ("/a/b" -> "/a", "/a" -> "/").
fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => String::from("/"),
        Some(i) => path[..i].into(),
        None => String::from("/"),
    }
}

/// The final component of a path ("/a/b" -> "b").
fn basename(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}

// ---------- open file handles ----------

/// What an overlay fd is backed by.
enum Backing {
    /// Backed by an upper-layer file at this path.
    Upper,
    /// Backed by an open lower-layer fd.
    Lower(u64),
}

/// An open overlay file handle.
struct Handle {
    path: String,
    backing: Backing,
    is_dir: bool,
}

// ---------- server ----------

struct Overlay {
    lower: Lower,
    upper: BTreeMap<String, Node>,
    upper_bytes: usize,
    handles: Vec<Option<Handle>>,
    client_buf: *mut u8,
    client_len: usize,
}

impl Overlay {
    fn read_client_path(&self, path_len: usize) -> String {
        let n = path_len.min(self.client_len);
        let s = unsafe { core::slice::from_raw_parts(self.client_buf, n) };
        normalize(&String::from_utf8_lossy(s))
    }

    fn write_client(&self, data: &[u8]) -> usize {
        let n = data.len().min(self.client_len);
        let dst = unsafe { core::slice::from_raw_parts_mut(self.client_buf, n) };
        dst.copy_from_slice(&data[..n]);
        n
    }

    fn alloc_handle(&mut self, h: Handle) -> u64 {
        for (i, slot) in self.handles.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(h);
                return i as u64;
            }
        }
        self.handles.push(Some(h));
        (self.handles.len() - 1) as u64
    }

    /// Account `delta` bytes against the upper budget; false if it would exceed.
    fn reserve(&mut self, delta: usize) -> bool {
        if self.upper_bytes + delta > UPPER_BUDGET {
            return false;
        }
        self.upper_bytes += delta;
        true
    }

    fn handle(&mut self, req: &libthemelios::IpcMessage) -> [u64; 4] {
        match req.words[0] {
            fs_proto::OP_OPEN => self.op_open(req.words[1] as usize),
            fs_proto::OP_READ => self.op_read(req.words[1], req.words[2], req.words[3] as usize),
            fs_proto::OP_WRITE => self.op_write(req.words[1], req.words[2], req.words[3] as usize),
            fs_proto::OP_CLOSE => {
                let fd = req.words[1] as usize;
                if let Some(slot) = self.handles.get_mut(fd) {
                    if let Some(Handle { backing: Backing::Lower(lfd), .. }) = slot {
                        self.lower.close(*lfd);
                    }
                    *slot = None;
                    [fs_proto::STATUS_OK, 0, 0, 0]
                } else {
                    [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0]
                }
            }
            fs_proto::OP_STAT => self.op_stat(req.words[1] as usize),
            fs_proto::OP_READDIR => self.op_readdir(req.words[1], req.words[2]),
            fs_proto::OP_CREATE => self.op_create(req.words[1] as usize),
            fs_proto::OP_MKDIR => self.op_mkdir(req.words[1] as usize),
            fs_proto::OP_UNLINK => self.op_unlink(req.words[1] as usize),
            _ => [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
        }
    }

    fn op_open(&mut self, path_len: usize) -> [u64; 4] {
        let path = self.read_client_path(path_len);
        match self.upper.get(&path) {
            Some(Node::Whiteout) => return [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
            Some(Node::File(_)) => {
                let fd = self.alloc_handle(Handle { path, backing: Backing::Upper, is_dir: false });
                return [fs_proto::STATUS_OK, fd, 0, 0];
            }
            Some(Node::Dir) => {
                let fd = self.alloc_handle(Handle { path, backing: Backing::Upper, is_dir: true });
                return [fs_proto::STATUS_OK, fd, 0, 0];
            }
            None => {}
        }
        // Not in upper: forward to the lower layer.
        match self.lower.stat(&path) {
            Some((_, is_dir)) => match self.lower.open(&path) {
                Some(lfd) => {
                    let fd = self.alloc_handle(Handle { path, backing: Backing::Lower(lfd), is_dir });
                    [fs_proto::STATUS_OK, fd, 0, 0]
                }
                None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
            },
            None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
        }
    }

    fn op_read(&mut self, fd: u64, offset: u64, len: usize) -> [u64; 4] {
        let len = len.min(self.client_len);
        let h = match self.handles.get(fd as usize).and_then(|s| s.as_ref()) {
            Some(h) if !h.is_dir => h,
            Some(_) => return [fs_proto::encode_error(FsError::IsADirectory), 0, 0, 0],
            None => return [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
        };
        let data = match &h.backing {
            Backing::Upper => {
                if let Some(Node::File(bytes)) = self.upper.get(&h.path) {
                    let start = (offset as usize).min(bytes.len());
                    let end = (start + len).min(bytes.len());
                    bytes[start..end].to_vec()
                } else {
                    Vec::new()
                }
            }
            Backing::Lower(lfd) => self.lower.read(*lfd, offset, len),
        };
        let n = self.write_client(&data);
        [fs_proto::STATUS_OK, n as u64, 0, 0]
    }

    fn op_write(&mut self, fd: u64, offset: u64, len: usize) -> [u64; 4] {
        let len = len.min(self.client_len);
        // Read the incoming data from the client region up front.
        let incoming = {
            let s = unsafe { core::slice::from_raw_parts(self.client_buf, len) };
            s.to_vec()
        };
        // Determine the target path and whether a copy-up is needed.
        let (path, need_copyup) = match self.handles.get(fd as usize).and_then(|s| s.as_ref()) {
            Some(h) if h.is_dir => return [fs_proto::encode_error(FsError::IsADirectory), 0, 0, 0],
            Some(h) => (h.path.clone(), matches!(h.backing, Backing::Lower(_))),
            None => return [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
        };

        // Copy-up: pull the lower file into the upper layer (bounded).
        if need_copyup && !matches!(self.upper.get(&path), Some(Node::File(_))) {
            let lower_data = self.lower.read_all(&path).unwrap_or_default();
            if lower_data.len() > COPYUP_LIMIT {
                return [fs_proto::encode_error(FsError::NoSpace), 0, 0, 0];
            }
            if !self.reserve(lower_data.len()) {
                return [fs_proto::encode_error(FsError::NoSpace), 0, 0, 0];
            }
            self.upper.insert(path.clone(), Node::File(lower_data));
            // Re-point the fd at the upper copy and close the lower fd.
            if let Some(Some(h)) = self.handles.get_mut(fd as usize) {
                if let Backing::Lower(lfd) = h.backing {
                    self.lower.close(lfd);
                }
                h.backing = Backing::Upper;
            }
        }

        // Ensure an upper file exists (created files already have one).
        if !matches!(self.upper.get(&path), Some(Node::File(_))) {
            self.upper.insert(path.clone(), Node::File(Vec::new()));
        }

        // Apply the write, growing the file and tracking the budget delta.
        let end = offset as usize + incoming.len();
        let grow = if let Some(Node::File(bytes)) = self.upper.get(&path) {
            end.saturating_sub(bytes.len())
        } else {
            0
        };
        if grow > 0 && !self.reserve(grow) {
            return [fs_proto::encode_error(FsError::NoSpace), 0, 0, 0];
        }
        if let Some(Node::File(bytes)) = self.upper.get_mut(&path) {
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[offset as usize..end].copy_from_slice(&incoming);
            [fs_proto::STATUS_OK, incoming.len() as u64, 0, 0]
        } else {
            [fs_proto::encode_error(FsError::IoError), 0, 0, 0]
        }
    }

    fn op_stat(&mut self, path_len: usize) -> [u64; 4] {
        let path = self.read_client_path(path_len);
        match self.upper.get(&path) {
            Some(Node::Whiteout) => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
            Some(Node::File(bytes)) => [fs_proto::STATUS_OK, bytes.len() as u64, 0, 0],
            Some(Node::Dir) => [fs_proto::STATUS_OK, 0, 1, 0],
            None => match self.lower.stat(&path) {
                Some((size, is_dir)) => [fs_proto::STATUS_OK, size, is_dir as u64, 0],
                None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
            },
        }
    }

    fn op_create(&mut self, path_len: usize) -> [u64; 4] {
        let path = self.read_client_path(path_len);
        self.upper.insert(path.clone(), Node::File(Vec::new()));
        let fd = self.alloc_handle(Handle { path, backing: Backing::Upper, is_dir: false });
        [fs_proto::STATUS_OK, fd, 0, 0]
    }

    fn op_mkdir(&mut self, path_len: usize) -> [u64; 4] {
        let path = self.read_client_path(path_len);
        self.upper.insert(path, Node::Dir);
        [fs_proto::STATUS_OK, 0, 0, 0]
    }

    fn op_unlink(&mut self, path_len: usize) -> [u64; 4] {
        let path = self.read_client_path(path_len);
        // Drop any upper copy, then record a whiteout so the lower entry (if
        // any) is hidden from the merged view.
        if let Some(Node::File(bytes)) = self.upper.get(&path) {
            self.upper_bytes = self.upper_bytes.saturating_sub(bytes.len());
        }
        self.upper.insert(path, Node::Whiteout);
        [fs_proto::STATUS_OK, 0, 0, 0]
    }

    fn op_readdir(&mut self, fd: u64, max: u64) -> [u64; 4] {
        let (dir_path, lower_fd) = match self.handles.get(fd as usize).and_then(|s| s.as_ref()) {
            Some(h) if h.is_dir => (
                h.path.clone(),
                match h.backing {
                    Backing::Lower(lfd) => Some(lfd),
                    Backing::Upper => None,
                },
            ),
            Some(_) => return [fs_proto::encode_error(FsError::NotADirectory), 0, 0, 0],
            None => return [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
        };

        // Gather merged (name, type), upper winning, whiteouts removed.
        let mut names: BTreeMap<String, u16> = BTreeMap::new();

        // Lower entries first (so upper can override).
        if let Some(lfd) = lower_fd {
            for (name, type_id) in self.lower.readdir(lfd, max) {
                names.insert(name, type_id);
            }
        }

        // Upper entries that are direct children of this directory.
        for (p, node) in self.upper.iter() {
            if parent_of(p) != dir_path || p == &dir_path {
                continue;
            }
            let name = basename(p).to_string();
            match node {
                Node::Whiteout => {
                    names.remove(&name);
                }
                Node::File(_) => {
                    names.insert(name, fs_proto::OP_READ as u16 /*file marker=2*/);
                }
                Node::Dir => {
                    names.insert(name, 1 /*dir marker*/);
                }
            }
        }

        // Pack: [u16 name_len, u16 type, name]*.
        let mut packed = Vec::new();
        let mut count = 0u64;
        for (name, type_id) in names.iter().take(max as usize) {
            let nb = name.as_bytes();
            packed.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            packed.extend_from_slice(&type_id.to_le_bytes());
            packed.extend_from_slice(nb);
            count += 1;
        }
        self.write_client(&packed);
        [fs_proto::STATUS_OK, count, 0, 0]
    }
}

#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info: BootInfo = boot_info();

    let mut overlay = Overlay {
        lower: Lower {
            endpoint: info.arg0, // lower (SquashFS) fs endpoint
            buf: info.shared_vaddr as *mut u8,
            len: info.shared_size as usize,
        },
        upper: BTreeMap::new(),
        upper_bytes: 0,
        handles: Vec::new(),
        client_buf: info.client_shared_vaddr as *mut u8,
        client_len: info.client_shared_size as usize,
    };

    loop {
        let req = ipc::receive(info.fs_endpoint);
        let reply = overlay.handle(&req);
        ipc::reply(info.fs_endpoint, req.reply_token, reply);
    }
}

/// Silence an unused warning for `vec!` import in builds where no literal is used.
#[allow(dead_code)]
fn _use_vec() -> Vec<u8> {
    vec![0u8; 0]
}
