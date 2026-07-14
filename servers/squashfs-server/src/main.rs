//! # SquashFS server
//!
//! A userspace (ring 3) filesystem server that reads the read-only SquashFS
//! root image. It reads disk blocks from the kernel block server over IPC +
//! shared memory, parses the SquashFS 4.0 on-disk format, decompresses metadata
//! and data with `miniz_oxide`, and answers filesystem requests from clients.
//!
//! Running in ring 3 is the whole point of the Phase 3 hybrid microkernel: a
//! malformed or malicious SquashFS image can at worst crash this process — it
//! cannot touch kernel memory, other servers, or the capability system.
//!
//! ## On-disk format (the parts we parse)
//!
//! - **Superblock** (96 bytes at offset 0): magic, counts, block size, and the
//!   byte offsets of the inode / directory / fragment / id tables.
//! - **Metadata blocks**: the inode and directory tables are sequences of up to
//!   8 KiB chunks, each prefixed with a 2-byte length header (bit 15 set =
//!   stored uncompressed). A *metadata reference* packs a block location (high
//!   48 bits) and a byte offset within the decompressed block (low 16 bits).
//! - **Inodes**: a 16-byte common header (type, mode, uid/gid idx, mtime, inode
//!   number) followed by a type-specific body (directory or regular file, basic
//!   or extended forms).
//! - **Directories**: headers (count, inode-table block, base inode number)
//!   each followed by entries (offset, inode delta, type, name).
//! - **File data**: a list of full data blocks (each independently compressed)
//!   plus an optional tail *fragment* packed with other small files.
//!
//! ## IPC protocol (this server)
//!
//! Requests arrive as four-word IPC calls; paths and file data travel through
//! the *client* shared region (offset 0). See `libthemelios::fs_proto`.
//! - OPEN  `[OP_OPEN, path_len]`            → `[status, fd]`
//! - READ  `[OP_READ, fd, file_off, len]`   → `[status, n]` (data at client[0..n])
//! - CLOSE `[OP_CLOSE, fd]`                  → `[status]`
//! - STAT  `[OP_STAT, path_len]`             → `[status, size, is_dir]`
//! - READDIR `[OP_READDIR, fd, max]`         → `[status, count]` (entries at client[0..])

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libthemelios::fs_proto::{self, FsError};
use libthemelios::{block_proto, boot_info, ipc, BootInfo};

/// Toggle verbose parser tracing over the debug-print syscall.
const DEBUG: bool = false;

macro_rules! dbg {
    ($($arg:tt)*) => {
        if DEBUG {
            libthemelios::debug_print(&alloc::format!($($arg)*));
        }
    };
}

// ---------- little-endian readers ----------

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

// ---------- SquashFS constants ----------

const SQUASHFS_MAGIC: u32 = 0x7371_7368;
/// Bit set in a block/fragment size word meaning "stored uncompressed".
const SIZE_UNCOMPRESSED_BIT: u32 = 1 << 24;
/// Bit set in a metadata-block length header meaning "stored uncompressed".
const META_UNCOMPRESSED_BIT: u16 = 1 << 15;
/// "No fragment" sentinel in a file inode.
const NO_FRAGMENT: u32 = 0xFFFF_FFFF;

// Inode types.
const TYPE_DIR: u16 = 1;
const TYPE_FILE: u16 = 2;
const TYPE_SYMLINK: u16 = 3;
const TYPE_LDIR: u16 = 8; // extended directory
const TYPE_LREG: u16 = 9; // extended file
const TYPE_LSYMLINK: u16 = 10; // extended symlink

/// Map a native SquashFS inode type to the canonical `fs_proto` readdir type
/// code, so clients see a filesystem-independent notion of file vs. directory.
fn canonical_dt(sqfs_type: u16) -> u16 {
    match sqfs_type {
        TYPE_DIR | TYPE_LDIR => fs_proto::DT_DIR,
        TYPE_FILE | TYPE_LREG => fs_proto::DT_FILE,
        TYPE_SYMLINK | TYPE_LSYMLINK => fs_proto::DT_SYMLINK,
        _ => fs_proto::DT_UNKNOWN,
    }
}

/// Parsed superblock fields we use.
struct Superblock {
    block_size: u32,
    root_ref: u64,
    inode_table_start: u64,
    dir_table_start: u64,
    fragment_table_start: u64,
}

/// A parsed inode (directory or regular file).
struct Inode {
    is_dir: bool,
    file_size: u64,
    // Regular file:
    blocks_start: u64,
    block_sizes: Vec<u32>,
    frag_index: u32,
    frag_offset: u32,
    // Directory:
    dir_start_block: u32,
    dir_offset: u16,
}

/// A directory entry produced by listing a directory.
struct DirEntry {
    name: String,
    inode_ref: u64,
    type_id: u16,
}

/// Disk access via the kernel block server + the block shared region.
struct Disk {
    block_endpoint: u64,
    buf: *mut u8,
    buf_len: usize,
}

impl Disk {
    /// Read `len` bytes from absolute disk byte offset `offset` into a heap Vec.
    ///
    /// Issues block-server READ requests (sector-granular) into the shared
    /// region and copies the requested byte range out, looping for transfers
    /// larger than the region.
    fn read(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut cur = offset;
        let mut remaining = len;
        let max_sectors = (self.buf_len / block_proto::BLOCK_SIZE as usize) as u64;
        while remaining > 0 {
            let sector = cur / block_proto::BLOCK_SIZE;
            let skew = (cur % block_proto::BLOCK_SIZE) as usize;
            // Sectors needed to cover skew + remaining, capped to the region.
            let want = skew + remaining;
            let mut count = (want as u64).div_ceil(block_proto::BLOCK_SIZE);
            if count > max_sectors {
                count = max_sectors;
            }
            let reply = ipc::call(
                self.block_endpoint,
                [block_proto::OP_READ, sector, count, 0],
                0,
            );
            if reply.words[0] != block_proto::STATUS_OK {
                // I/O error: return what we have (caller treats short reads as
                // corruption). Avoids panicking the server on a bad device.
                break;
            }
            // SAFETY: the block server filled [0, count*512) of the shared
            // region, which is mapped read/write into our address space.
            let filled = (count * block_proto::BLOCK_SIZE) as usize;
            let slice = unsafe { core::slice::from_raw_parts(self.buf, filled) };
            let avail = filled - skew;
            let take = avail.min(remaining);
            out.extend_from_slice(&slice[skew..skew + take]);
            cur += take as u64;
            remaining -= take;
        }
        out
    }
}

/// Inflate a zlib stream (SquashFS "gzip" compression is zlib-wrapped).
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec_zlib(data).ok()
}

/// Read a single metadata block at disk `offset`, returning its decompressed
/// bytes and the number of on-disk bytes consumed (2 + stored size).
fn read_metadata_block(disk: &Disk, offset: u64) -> Option<(Vec<u8>, u64)> {
    let header = disk.read(offset, 2);
    if header.len() < 2 {
        return None;
    }
    let h = rd_u16(&header, 0);
    let stored = (h & !META_UNCOMPRESSED_BIT) as usize;
    let uncompressed = h & META_UNCOMPRESSED_BIT != 0;
    let body = disk.read(offset + 2, stored);
    if body.len() < stored {
        return None;
    }
    let data = if uncompressed { body } else { inflate(&body)? };
    Some((data, 2 + stored as u64))
}

/// Read a logical metadata *stream* starting at `disk_offset`, skipping the
/// first `skip` decompressed bytes, returning at least `min_len` bytes after the
/// skip (concatenating consecutive metadata blocks as needed).
fn read_metadata_stream(disk: &Disk, disk_offset: u64, skip: usize, min_len: usize) -> Vec<u8> {
    let mut acc = Vec::new();
    let mut off = disk_offset;
    while acc.len() < skip + min_len {
        match read_metadata_block(disk, off) {
            Some((data, consumed)) => {
                if data.is_empty() {
                    break;
                }
                acc.extend_from_slice(&data);
                off += consumed;
            }
            None => break,
        }
    }
    if skip >= acc.len() {
        return Vec::new();
    }
    acc[skip..].to_vec()
}

/// Parse the superblock from the first 96 bytes of the device.
fn read_superblock(disk: &Disk) -> Option<Superblock> {
    let b = disk.read(0, 96);
    if b.len() < 96 || rd_u32(&b, 0) != SQUASHFS_MAGIC {
        return None;
    }
    Some(Superblock {
        block_size: rd_u32(&b, 12),
        root_ref: rd_u64(&b, 32),
        inode_table_start: rd_u64(&b, 64),
        dir_table_start: rd_u64(&b, 72),
        fragment_table_start: rd_u64(&b, 80),
    })
}

/// Read and parse the inode referenced by `inode_ref` (block in high 48 bits,
/// byte offset within the decompressed metadata block in low 16 bits).
fn read_inode(disk: &Disk, sb: &Superblock, inode_ref: u64) -> Option<Inode> {
    let block = inode_ref >> 16;
    let offset = (inode_ref & 0xFFFF) as usize;
    // Read generously: header + body + block-size list. 8 KiB covers files with
    // up to ~2000 data blocks, far beyond our test images.
    let buf = read_metadata_stream(disk, sb.inode_table_start + block, offset, 8192);
    if buf.len() < 16 {
        return None;
    }
    let type_id = rd_u16(&buf, 0);
    // Common header is 16 bytes; bodies follow.
    let body = &buf[16..];

    match type_id {
        TYPE_DIR => {
            // basic dir: start_block u32, nlink u32, file_size u16, offset u16, parent u32
            if body.len() < 16 {
                return None;
            }
            Some(Inode {
                is_dir: true,
                file_size: rd_u16(body, 8) as u64,
                blocks_start: 0,
                block_sizes: Vec::new(),
                frag_index: NO_FRAGMENT,
                frag_offset: 0,
                dir_start_block: rd_u32(body, 0),
                dir_offset: rd_u16(body, 10),
            })
        }
        TYPE_LDIR => {
            // ext dir: nlink u32, file_size u32, start_block u32, parent u32,
            //          index_count u16, offset u16, xattr u32
            if body.len() < 20 {
                return None;
            }
            Some(Inode {
                is_dir: true,
                file_size: rd_u32(body, 4) as u64,
                blocks_start: 0,
                block_sizes: Vec::new(),
                frag_index: NO_FRAGMENT,
                frag_offset: 0,
                dir_start_block: rd_u32(body, 8),
                dir_offset: rd_u16(body, 18),
            })
        }
        TYPE_FILE => {
            // basic file: blocks_start u32, frag_index u32, frag_offset u32,
            //             file_size u32, block_sizes[n] u32
            if body.len() < 16 {
                return None;
            }
            let blocks_start = rd_u32(body, 0) as u64;
            let frag_index = rd_u32(body, 4);
            let frag_offset = rd_u32(body, 8);
            let file_size = rd_u32(body, 12) as u64;
            let n = block_count(file_size, sb.block_size, frag_index);
            let block_sizes = parse_block_sizes(&body[16..], n)?;
            Some(Inode {
                is_dir: false,
                file_size,
                blocks_start,
                block_sizes,
                frag_index,
                frag_offset,
                dir_start_block: 0,
                dir_offset: 0,
            })
        }
        TYPE_LREG => {
            // ext file: blocks_start u64, file_size u64, sparse u64, nlink u32,
            //           frag_index u32, frag_offset u32, xattr u32, block_sizes[n]
            if body.len() < 40 {
                return None;
            }
            let blocks_start = rd_u64(body, 0);
            let file_size = rd_u64(body, 8);
            let frag_index = rd_u32(body, 28);
            let frag_offset = rd_u32(body, 32);
            let n = block_count(file_size, sb.block_size, frag_index);
            let block_sizes = parse_block_sizes(&body[40..], n)?;
            Some(Inode {
                is_dir: false,
                file_size,
                blocks_start,
                block_sizes,
                frag_index,
                frag_offset,
                dir_start_block: 0,
                dir_offset: 0,
            })
        }
        _ => None,
    }
}

/// Number of full data blocks a file has: all blocks if no fragment, otherwise
/// only the whole blocks (the tail lives in the fragment).
fn block_count(file_size: u64, block_size: u32, frag_index: u32) -> usize {
    let bs = block_size as u64;
    if frag_index == NO_FRAGMENT {
        file_size.div_ceil(bs) as usize
    } else {
        (file_size / bs) as usize
    }
}

/// Parse `n` u32 block-size entries from the start of `buf`.
fn parse_block_sizes(buf: &[u8], n: usize) -> Option<Vec<u32>> {
    if buf.len() < n * 4 {
        return None;
    }
    Some((0..n).map(|i| rd_u32(buf, i * 4)).collect())
}

/// List a directory inode's entries.
fn list_dir(disk: &Disk, sb: &Superblock, inode: &Inode) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    if inode.file_size <= 3 {
        return entries; // empty (the +3 quirk: size 3 means no entries)
    }
    let listing_len = (inode.file_size - 3) as usize;
    let listing = read_metadata_stream(
        disk,
        sb.dir_table_start + inode.dir_start_block as u64,
        inode.dir_offset as usize,
        listing_len,
    );
    let listing = &listing[..listing_len.min(listing.len())];

    let mut pos = 0usize;
    while pos + 12 <= listing.len() {
        let count = rd_u32(listing, pos) as usize + 1;
        let start_block = rd_u32(listing, pos + 4);
        pos += 12; // header: count, start_block, inode_number
        for _ in 0..count {
            if pos + 8 > listing.len() {
                return entries;
            }
            let offset = rd_u16(listing, pos);
            let type_id = rd_u16(listing, pos + 4);
            let name_size = rd_u16(listing, pos + 6) as usize + 1;
            pos += 8;
            if pos + name_size > listing.len() {
                return entries;
            }
            let name = String::from_utf8_lossy(&listing[pos..pos + name_size]).into_owned();
            pos += name_size;
            let inode_ref = ((start_block as u64) << 16) | offset as u64;
            entries.push(DirEntry { name, inode_ref, type_id });
        }
    }
    entries
}

/// Look up a fragment entry by index, returning (disk_start, stored_size, compressed).
fn fragment_entry(disk: &Disk, sb: &Superblock, index: u32) -> Option<(u64, usize, bool)> {
    // The fragment table is an array of u64 metadata-block locations; each
    // metadata block holds up to 512 fragment entries (16 bytes each).
    let meta_idx = index / 512;
    let entry_idx = (index % 512) as usize;
    let ptr_bytes = disk.read(sb.fragment_table_start + (meta_idx as u64) * 8, 8);
    if ptr_bytes.len() < 8 {
        return None;
    }
    let meta_block = rd_u64(&ptr_bytes, 0);
    let (entries, _) = read_metadata_block(disk, meta_block)?;
    let base = entry_idx * 16;
    if base + 16 > entries.len() {
        return None;
    }
    let start = rd_u64(&entries, base);
    let size_word = rd_u32(&entries, base + 8);
    let size = (size_word & !SIZE_UNCOMPRESSED_BIT) as usize;
    let compressed = size_word & SIZE_UNCOMPRESSED_BIT == 0;
    Some((start, size, compressed))
}

/// Read one full data block `i` of a file, returning its decompressed bytes.
fn read_data_block(disk: &Disk, sb: &Superblock, inode: &Inode, i: usize) -> Vec<u8> {
    let bs = sb.block_size as u64;
    let logical_len = (inode.file_size - (i as u64) * bs).min(bs) as usize;
    let size_word = inode.block_sizes[i];
    let stored = (size_word & !SIZE_UNCOMPRESSED_BIT) as usize;
    if stored == 0 {
        // Sparse block: all zeros.
        return vec![0u8; logical_len];
    }
    // Disk offset = blocks_start + sum of stored sizes of preceding blocks.
    let mut disk_off = inode.blocks_start;
    for j in 0..i {
        disk_off += (inode.block_sizes[j] & !SIZE_UNCOMPRESSED_BIT) as u64;
    }
    let raw = disk.read(disk_off, stored);
    let compressed = size_word & SIZE_UNCOMPRESSED_BIT == 0;
    if compressed {
        inflate(&raw).unwrap_or_default()
    } else {
        raw
    }
}

/// Read the fragment tail bytes of a file (the bytes beyond its full blocks).
fn read_fragment_tail(disk: &Disk, sb: &Superblock, inode: &Inode) -> Vec<u8> {
    let bs = sb.block_size as u64;
    let full_bytes = inode.block_sizes.len() as u64 * bs;
    if inode.file_size <= full_bytes || inode.frag_index == NO_FRAGMENT {
        return Vec::new();
    }
    let tail_len = (inode.file_size - full_bytes) as usize;
    let (start, stored, compressed) = match fragment_entry(disk, sb, inode.frag_index) {
        Some(e) => e,
        None => return Vec::new(),
    };
    let raw = disk.read(start, stored);
    let block = if compressed { inflate(&raw).unwrap_or_default() } else { raw };
    let begin = inode.frag_offset as usize;
    let end = (begin + tail_len).min(block.len());
    if begin >= block.len() {
        Vec::new()
    } else {
        block[begin..end].to_vec()
    }
}

/// Read `len` bytes of a file starting at `file_offset`.
fn read_file(disk: &Disk, sb: &Superblock, inode: &Inode, file_offset: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let bs = sb.block_size as u64;
    let n_full = inode.block_sizes.len();
    let mut pos = file_offset;
    let mut remaining = len.min((inode.file_size.saturating_sub(file_offset)) as usize);
    while remaining > 0 && pos < inode.file_size {
        let block_index = (pos / bs) as usize;
        let (data, region_base) = if block_index < n_full {
            (read_data_block(disk, sb, inode, block_index), (block_index as u64) * bs)
        } else {
            (read_fragment_tail(disk, sb, inode), (n_full as u64) * bs)
        };
        if data.is_empty() {
            break;
        }
        let within = (pos - region_base) as usize;
        if within >= data.len() {
            break;
        }
        let take = (data.len() - within).min(remaining);
        out.extend_from_slice(&data[within..within + take]);
        pos += take as u64;
        remaining -= take;
    }
    out
}

/// Resolve an absolute path ("/a/b/c") to its inode, starting from the root.
fn resolve(disk: &Disk, sb: &Superblock, path: &str) -> Option<Inode> {
    let mut inode = read_inode(disk, sb, sb.root_ref)?;
    for comp in path.split('/') {
        if comp.is_empty() {
            continue; // leading/duplicate slashes
        }
        if !inode.is_dir {
            return None; // a path component is not a directory
        }
        let entries = list_dir(disk, sb, &inode);
        let entry = entries.iter().find(|e| e.name == comp)?;
        inode = read_inode(disk, sb, entry.inode_ref)?;
    }
    Some(inode)
}

// ---------- open-file table ----------

/// An entry in the open-file table.
struct OpenFile {
    inode: Inode,
}

/// Server state: the parsed superblock and the open-file table.
struct Server {
    disk: Disk,
    sb: Superblock,
    open: Vec<Option<OpenFile>>,
    /// Client shared region (paths in, file/dir data out).
    client_buf: *mut u8,
    client_len: usize,
}

impl Server {
    /// Read the path bytes the client placed at the start of its shared region.
    fn read_path(&self, path_len: usize) -> String {
        let n = path_len.min(self.client_len);
        // SAFETY: client region is mapped read/write; client wrote `path_len`
        // path bytes at offset 0 before the call.
        let slice = unsafe { core::slice::from_raw_parts(self.client_buf, n) };
        String::from_utf8_lossy(slice).into_owned()
    }

    /// Write `data` to the start of the client region, returning bytes written.
    fn write_client(&self, data: &[u8]) -> usize {
        let n = data.len().min(self.client_len);
        // SAFETY: client region is mapped read/write and at least client_len.
        let dst = unsafe { core::slice::from_raw_parts_mut(self.client_buf, n) };
        dst.copy_from_slice(&data[..n]);
        n
    }

    /// Allocate an open-file slot for `inode`, returning its fd.
    fn alloc_fd(&mut self, inode: Inode) -> u64 {
        for (i, slot) in self.open.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(OpenFile { inode });
                return i as u64;
            }
        }
        self.open.push(Some(OpenFile { inode }));
        (self.open.len() - 1) as u64
    }

    fn handle(&mut self, req: &libthemelios::IpcMessage) -> [u64; 4] {
        let op = req.words[0];
        match op {
            fs_proto::OP_OPEN => {
                let path = self.read_path(req.words[1] as usize);
                match resolve(&self.disk, &self.sb, &path) {
                    Some(inode) => {
                        let fd = self.alloc_fd(inode);
                        [fs_proto::STATUS_OK, fd, 0, 0]
                    }
                    None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
                }
            }
            fs_proto::OP_READ => {
                let fd = req.words[1] as usize;
                let file_off = req.words[2];
                let len = (req.words[3] as usize).min(self.client_len);
                match self.open.get(fd).and_then(|s| s.as_ref()) {
                    Some(of) if !of.inode.is_dir => {
                        // Borrow the inode immutably for the read, then write out.
                        let data = read_file(&self.disk, &self.sb, &of.inode, file_off, len);
                        let n = self.write_client(&data);
                        [fs_proto::STATUS_OK, n as u64, 0, 0]
                    }
                    Some(_) => [fs_proto::encode_error(FsError::IsADirectory), 0, 0, 0],
                    None => [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
                }
            }
            fs_proto::OP_CLOSE => {
                let fd = req.words[1] as usize;
                if let Some(slot) = self.open.get_mut(fd) {
                    *slot = None;
                    [fs_proto::STATUS_OK, 0, 0, 0]
                } else {
                    [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0]
                }
            }
            fs_proto::OP_STAT => {
                let path = self.read_path(req.words[1] as usize);
                match resolve(&self.disk, &self.sb, &path) {
                    Some(inode) => {
                        [fs_proto::STATUS_OK, inode.file_size, inode.is_dir as u64, 0]
                    }
                    None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
                }
            }
            fs_proto::OP_READDIR => {
                let fd = req.words[1] as usize;
                let max = req.words[2] as usize;
                match self.open.get(fd).and_then(|s| s.as_ref()) {
                    Some(of) if of.inode.is_dir => {
                        let entries = list_dir(&self.disk, &self.sb, &of.inode);
                        // Pack entries into the client region: for each entry,
                        // u16 name_len, u16 type, then the name bytes.
                        let mut packed = Vec::new();
                        let mut count = 0u64;
                        for e in entries.iter().take(max) {
                            let nb = e.name.as_bytes();
                            packed.extend_from_slice(&(nb.len() as u16).to_le_bytes());
                            // Translate the native SquashFS inode type to the
                            // canonical readdir type code the client expects.
                            packed.extend_from_slice(&canonical_dt(e.type_id).to_le_bytes());
                            packed.extend_from_slice(nb);
                            count += 1;
                        }
                        self.write_client(&packed);
                        [fs_proto::STATUS_OK, count, 0, 0]
                    }
                    Some(_) => [fs_proto::encode_error(FsError::NotADirectory), 0, 0, 0],
                    None => [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
                }
            }
            _ => [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
        }
    }
}

#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info: BootInfo = boot_info();
    dbg!("[squashfs] starting, block_ep={}\n", info.block_endpoint);

    let disk = Disk {
        block_endpoint: info.block_endpoint,
        buf: info.shared_vaddr as *mut u8,
        buf_len: info.shared_size as usize,
    };

    let sb = match read_superblock(&disk) {
        Some(sb) => sb,
        None => {
            libthemelios::debug_print("[squashfs] bad superblock\n");
            libthemelios::syscall::exit(1);
        }
    };
    dbg!("[squashfs] block_size={} root_ref={:#x}\n", sb.block_size, sb.root_ref);

    let mut server = Server {
        disk,
        sb,
        open: Vec::new(),
        client_buf: info.client_shared_vaddr as *mut u8,
        client_len: info.client_shared_size as usize,
    };

    loop {
        let req = ipc::receive(info.fs_endpoint);
        let reply = server.handle(&req);
        ipc::reply(info.fs_endpoint, req.reply_token, reply);
    }
}
