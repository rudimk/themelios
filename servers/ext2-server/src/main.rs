//! # ext2 server
//!
//! A userspace (ring 3) ext2 filesystem server backing persistent data volumes.
//! It reads (and, in the write path, writes) the device through the kernel block
//! server over IPC + shared memory, doing all ext2 parsing and metadata
//! management itself. A corrupt image or an allocation bug crashes this process,
//! never the kernel — the security payoff of the hybrid microkernel.
//!
//! This file implements the **read path**: superblock and block-group
//! descriptors, inode lookup, directory listing, and file reads via direct and
//! single-indirect block pointers. The write path (block/inode bitmap
//! allocation, file write/append, create, unlink) is layered on next.
//!
//! ## On-disk format (the subset we implement)
//!
//! - **Superblock** at byte offset 1024: counts, `first_data_block`, block-size
//!   shift, blocks/inodes-per-group, magic (0xEF53), inode size, first inode.
//! - **Block group descriptors** (32 bytes each) starting at the block after the
//!   superblock: per group, the block bitmap, inode bitmap, and inode table.
//! - **Inodes** (256 bytes here): mode, size, and 15 block pointers (12 direct,
//!   1 single-indirect at index 12; double/triple indirect deferred).
//! - **Directories**: linear `{inode:u32, rec_len:u16, name_len:u8,
//!   file_type:u8, name}` records (the `filetype` feature is enabled).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libthemelios::fs_proto::{self, FsError};
use libthemelios::{block_proto, boot_info, ipc, BootInfo};

// ---------- little-endian readers ----------

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

// ---------- ext2 constants ----------

const EXT2_MAGIC: u16 = 0xEF53;
const ROOT_INO: u32 = 2;
const NDIR_BLOCKS: usize = 12; // direct block pointers
const IND_BLOCK: usize = 12; // index of the single-indirect pointer

// Inode mode type bits.
const S_IFMT: u16 = 0xF000;
const S_IFREG: u16 = 0x8000;
const S_IFDIR: u16 = 0x4000;

/// Parsed superblock fields.
struct Superblock {
    block_size: u32,
    first_data_block: u32,
    inodes_per_group: u32,
    inode_size: u32,
}

/// A parsed inode.
struct Inode {
    mode: u16,
    size: u64,
    block: [u32; 15],
}

impl Inode {
    fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    fn is_reg(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }
}

/// A directory entry from a directory listing.
struct DirEntry {
    name: String,
    inode: u32,
    file_type: u8,
}

// ---------- disk access via the block server ----------

struct Disk {
    block_endpoint: u64,
    buf: *mut u8,
    buf_len: usize,
}

impl Disk {
    /// Read `len` bytes from absolute disk byte offset `offset`.
    fn read(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len);
        let mut cur = offset;
        let mut remaining = len;
        let max_sectors = (self.buf_len / block_proto::BLOCK_SIZE as usize) as u64;
        while remaining > 0 {
            let sector = cur / block_proto::BLOCK_SIZE;
            let skew = (cur % block_proto::BLOCK_SIZE) as usize;
            let mut count = ((skew + remaining) as u64).div_ceil(block_proto::BLOCK_SIZE);
            if count > max_sectors {
                count = max_sectors;
            }
            let reply = ipc::call(
                self.block_endpoint,
                [block_proto::OP_READ, sector, count, 0],
                0,
            );
            if reply.words[0] != block_proto::STATUS_OK {
                break;
            }
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

/// The ext2 filesystem, holding the superblock and disk handle.
struct Ext2 {
    disk: Disk,
    sb: Superblock,
    bgd_block: u64, // block number of the block-group descriptor table
}

impl Ext2 {
    /// Read a whole filesystem block.
    fn read_block(&self, block: u32) -> Vec<u8> {
        self.disk
            .read(block as u64 * self.sb.block_size as u64, self.sb.block_size as usize)
    }

    /// Read inode `ino` (1-based).
    fn read_inode(&self, ino: u32) -> Option<Inode> {
        if ino == 0 {
            return None;
        }
        let group = (ino - 1) / self.sb.inodes_per_group;
        let index = ((ino - 1) % self.sb.inodes_per_group) as u64;
        // Block-group descriptor for this group (32 bytes each).
        let bgd = self
            .disk
            .read(self.bgd_block * self.sb.block_size as u64 + group as u64 * 32, 32);
        if bgd.len() < 32 {
            return None;
        }
        let inode_table = rd_u32(&bgd, 8) as u64;
        let inode_off = inode_table * self.sb.block_size as u64 + index * self.sb.inode_size as u64;
        let raw = self.disk.read(inode_off, self.sb.inode_size as usize);
        if raw.len() < 100 {
            return None;
        }
        let mode = rd_u16(&raw, 0);
        let mut size = rd_u32(&raw, 4) as u64;
        // For regular files, i_dir_acl (offset 108) holds the high 32 bits.
        if mode & S_IFMT == S_IFREG && raw.len() >= 112 {
            size |= (rd_u32(&raw, 108) as u64) << 32;
        }
        let mut block = [0u32; 15];
        for (i, b) in block.iter_mut().enumerate() {
            *b = rd_u32(&raw, 40 + i * 4);
        }
        Some(Inode { mode, size, block })
    }

    /// Map a file's logical block index to a physical block number (0 = sparse).
    fn resolve_block(&self, inode: &Inode, logical: u32) -> u32 {
        let logical = logical as usize;
        if logical < NDIR_BLOCKS {
            return inode.block[logical];
        }
        let per_block = (self.sb.block_size / 4) as usize;
        let lb = logical - NDIR_BLOCKS;
        if lb < per_block {
            let ind = inode.block[IND_BLOCK];
            if ind == 0 {
                return 0;
            }
            let table = self.read_block(ind);
            if (lb + 1) * 4 <= table.len() {
                return rd_u32(&table, lb * 4);
            }
            return 0;
        }
        // Double/triple indirect: deferred (not needed for Phase 3 volumes).
        0
    }

    /// Read [offset, offset+len) of a file.
    fn read_file(&self, inode: &Inode, offset: u64, len: usize) -> Vec<u8> {
        let bs = self.sb.block_size as u64;
        let mut out = Vec::with_capacity(len);
        let mut pos = offset;
        let mut remaining = len.min(inode.size.saturating_sub(offset) as usize);
        while remaining > 0 && pos < inode.size {
            let logical = (pos / bs) as u32;
            let within = (pos % bs) as usize;
            let phys = self.resolve_block(inode, logical);
            let block = if phys == 0 {
                vec![0u8; bs as usize] // sparse hole
            } else {
                self.read_block(phys)
            };
            if block.is_empty() {
                break;
            }
            let take = (block.len() - within).min(remaining);
            out.extend_from_slice(&block[within..within + take]);
            pos += take as u64;
            remaining -= take;
        }
        out
    }

    /// List a directory inode's entries (skips unused records, inode == 0).
    fn list_dir(&self, inode: &Inode) -> Vec<DirEntry> {
        let mut entries = Vec::new();
        // Read the directory's full data (entries never span block boundaries;
        // each block's records' rec_len sum to the block size).
        let data = self.read_file(inode, 0, inode.size as usize);
        let mut pos = 0usize;
        while pos + 8 <= data.len() {
            let ino = rd_u32(&data, pos);
            let rec_len = rd_u16(&data, pos + 4) as usize;
            let name_len = data[pos + 6] as usize;
            let file_type = data[pos + 7];
            if rec_len == 0 {
                break; // malformed; avoid an infinite loop
            }
            if ino != 0 && pos + 8 + name_len <= data.len() {
                let name = String::from_utf8_lossy(&data[pos + 8..pos + 8 + name_len]).into_owned();
                entries.push(DirEntry { name, inode: ino, file_type });
            }
            pos += rec_len;
        }
        entries
    }

    /// Resolve an absolute path to its inode, starting from the root.
    fn resolve(&self, path: &str) -> Option<Inode> {
        let mut inode = self.read_inode(ROOT_INO)?;
        for comp in path.split('/') {
            if comp.is_empty() {
                continue;
            }
            if !inode.is_dir() {
                return None;
            }
            let entries = self.list_dir(&inode);
            let entry = entries.iter().find(|e| e.name == comp)?;
            inode = self.read_inode(entry.inode)?;
        }
        Some(inode)
    }
}

/// Parse the superblock and construct the filesystem handle.
fn mount(disk: Disk) -> Option<Ext2> {
    // Superblock lives at byte offset 1024, regardless of block size.
    let sb_raw = disk.read(1024, 256);
    if sb_raw.len() < 100 || rd_u16(&sb_raw, 56) != EXT2_MAGIC {
        return None;
    }
    let log_block_size = rd_u32(&sb_raw, 24);
    let block_size = 1024u32 << log_block_size;
    let first_data_block = rd_u32(&sb_raw, 20);
    let inodes_per_group = rd_u32(&sb_raw, 40);
    let inode_size = rd_u16(&sb_raw, 88) as u32;
    let sb = Superblock {
        block_size,
        first_data_block,
        inodes_per_group,
        inode_size: if inode_size == 0 { 128 } else { inode_size },
    };
    // The group-descriptor table immediately follows the superblock block.
    let bgd_block = (first_data_block + 1) as u64;
    Some(Ext2 { disk, sb, bgd_block })
}

// ---------- server ----------

struct OpenFile {
    inode: Inode,
}

struct Server {
    fs: Ext2,
    open: Vec<Option<OpenFile>>,
    client_buf: *mut u8,
    client_len: usize,
}

impl Server {
    fn read_path(&self, path_len: usize) -> String {
        let n = path_len.min(self.client_len);
        let s = unsafe { core::slice::from_raw_parts(self.client_buf, n) };
        String::from_utf8_lossy(s).into_owned()
    }

    fn write_client(&self, data: &[u8]) -> usize {
        let n = data.len().min(self.client_len);
        let dst = unsafe { core::slice::from_raw_parts_mut(self.client_buf, n) };
        dst.copy_from_slice(&data[..n]);
        n
    }

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
        match req.words[0] {
            fs_proto::OP_OPEN => {
                let path = self.read_path(req.words[1] as usize);
                match self.fs.resolve(&path) {
                    Some(inode) => {
                        let fd = self.alloc_fd(inode);
                        [fs_proto::STATUS_OK, fd, 0, 0]
                    }
                    None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
                }
            }
            fs_proto::OP_READ => {
                let fd = req.words[1] as usize;
                let off = req.words[2];
                let len = (req.words[3] as usize).min(self.client_len);
                match self.open.get(fd).and_then(|s| s.as_ref()) {
                    Some(of) if of.inode.is_reg() => {
                        let data = self.fs.read_file(&of.inode, off, len);
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
                match self.fs.resolve(&path) {
                    Some(inode) => [fs_proto::STATUS_OK, inode.size, inode.is_dir() as u64, 0],
                    None => [fs_proto::encode_error(FsError::NotFound), 0, 0, 0],
                }
            }
            fs_proto::OP_READDIR => {
                let fd = req.words[1] as usize;
                let max = req.words[2] as usize;
                match self.open.get(fd).and_then(|s| s.as_ref()) {
                    Some(of) if of.inode.is_dir() => {
                        let entries = self.fs.list_dir(&of.inode);
                        let mut packed = Vec::new();
                        let mut count = 0u64;
                        for e in entries.iter().take(max) {
                            let nb = e.name.as_bytes();
                            packed.extend_from_slice(&(nb.len() as u16).to_le_bytes());
                            packed.extend_from_slice(&(e.file_type as u16).to_le_bytes());
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
            // Write operations: implemented in the ext2 write-path step.
            fs_proto::OP_WRITE
            | fs_proto::OP_CREATE
            | fs_proto::OP_MKDIR
            | fs_proto::OP_UNLINK => [fs_proto::encode_error(FsError::ReadOnlyFs), 0, 0, 0],
            _ => [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
        }
    }
}

#[no_mangle]
pub extern "C" fn server_main() -> ! {
    let info: BootInfo = boot_info();

    let disk = Disk {
        block_endpoint: info.block_endpoint,
        buf: info.shared_vaddr as *mut u8,
        buf_len: info.shared_size as usize,
    };

    let fs = match mount(disk) {
        Some(fs) => fs,
        None => {
            libthemelios::debug_print("[ext2] bad superblock\n");
            libthemelios::syscall::exit(1);
        }
    };

    let mut server = Server {
        fs,
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
