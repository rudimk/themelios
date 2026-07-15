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
    blocks_count: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u32,
}

/// A parsed inode.
#[derive(Clone, Copy)]
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

    /// Write `data` to disk at `offset`. Both must be 512-byte sector aligned
    /// (callers always write whole filesystem blocks). Loops for transfers
    /// larger than the shared region.
    fn write(&self, offset: u64, data: &[u8]) {
        let mut done = 0usize;
        while done < data.len() {
            let chunk = (data.len() - done).min(self.buf_len);
            // SAFETY: shared region is mapped read/write; we own it for the
            // duration of this synchronous request.
            let dst = unsafe { core::slice::from_raw_parts_mut(self.buf, chunk) };
            dst.copy_from_slice(&data[done..done + chunk]);
            let sector = (offset + done as u64) / block_proto::BLOCK_SIZE;
            let count = (chunk as u64) / block_proto::BLOCK_SIZE;
            let _ = ipc::call(self.block_endpoint, [block_proto::OP_WRITE, sector, count, 0], 0);
            done += chunk;
        }
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

    /// Like `resolve`, but also returns the resolved inode number.
    fn resolve_num(&self, path: &str) -> Option<(u32, Inode)> {
        let mut num = ROOT_INO;
        let mut inode = self.read_inode(num)?;
        for comp in path.split('/') {
            if comp.is_empty() {
                continue;
            }
            if !inode.is_dir() {
                return None;
            }
            let entry = self.list_dir(&inode).into_iter().find(|e| e.name == comp)?;
            num = entry.inode;
            inode = self.read_inode(num)?;
        }
        Some((num, inode))
    }

    // ------------------------------------------------------------------
    // Write path: bitmap allocation, inode writeback, dir-entry edits.
    // ------------------------------------------------------------------

    /// Number of block groups in the filesystem.
    fn num_groups(&self) -> u32 {
        self.sb.blocks_count.div_ceil(self.sb.blocks_per_group)
    }

    /// 512-byte sectors per filesystem block (for the inode `i_blocks` field).
    fn sectors_per_block(&self) -> u32 {
        self.sb.block_size / 512
    }

    /// Write a full filesystem block.
    fn write_block(&self, block: u32, data: &[u8]) {
        self.disk.write(block as u64 * self.sb.block_size as u64, data);
    }

    /// Read-modify-write `data` into the block containing `byte_offset`
    /// (`data` must not cross a block boundary).
    fn patch(&self, byte_offset: u64, data: &[u8]) {
        let bs = self.sb.block_size as u64;
        let block = (byte_offset / bs) as u32;
        let within = (byte_offset % bs) as usize;
        let mut buf = self.read_block(block);
        buf[within..within + data.len()].copy_from_slice(data);
        self.write_block(block, &buf);
    }

    /// Byte offset of group `group`'s descriptor in the descriptor table.
    fn bgd_offset(&self, group: u32) -> u64 {
        self.bgd_block * self.sb.block_size as u64 + group as u64 * 32
    }

    /// Adjust the superblock's free-block / free-inode counters.
    fn adjust_super_free(&self, blocks_delta: i64, inodes_delta: i64) {
        let bs = self.sb.block_size;
        let sb_block = 1024 / bs;
        let off = (1024 % bs) as usize;
        let mut buf = self.read_block(sb_block);
        let fb = (rd_u32(&buf, off + 12) as i64 + blocks_delta) as u32;
        let fi = (rd_u32(&buf, off + 16) as i64 + inodes_delta) as u32;
        buf[off + 12..off + 16].copy_from_slice(&fb.to_le_bytes());
        buf[off + 16..off + 20].copy_from_slice(&fi.to_le_bytes());
        self.write_block(sb_block, &buf);
    }

    /// Adjust a group descriptor's free-block / free-inode / used-dirs counters.
    fn adjust_bgd(&self, group: u32, blocks_delta: i64, inodes_delta: i64, dirs_delta: i64) {
        let off = self.bgd_offset(group);
        let bgd = self.disk.read(off, 32);
        let fb = (rd_u16(&bgd, 12) as i64 + blocks_delta) as u16;
        let fi = (rd_u16(&bgd, 14) as i64 + inodes_delta) as u16;
        let dirs = (rd_u16(&bgd, 16) as i64 + dirs_delta) as u16;
        let mut p = [0u8; 6];
        p[0..2].copy_from_slice(&fb.to_le_bytes());
        p[2..4].copy_from_slice(&fi.to_le_bytes());
        p[4..6].copy_from_slice(&dirs.to_le_bytes());
        self.patch(off + 12, &p);
    }

    /// Allocate a free data block, returning its (zeroed) block number.
    fn alloc_block(&self) -> Option<u32> {
        for group in 0..self.num_groups() {
            let bgd = self.disk.read(self.bgd_offset(group), 32);
            if rd_u16(&bgd, 12) == 0 {
                continue;
            }
            let bitmap_block = rd_u32(&bgd, 0);
            let mut bitmap = self.read_block(bitmap_block);
            for i in 0..self.sb.blocks_per_group as usize {
                if i / 8 >= bitmap.len() {
                    break;
                }
                if bitmap[i / 8] & (1 << (i % 8)) == 0 {
                    bitmap[i / 8] |= 1 << (i % 8);
                    self.write_block(bitmap_block, &bitmap);
                    self.adjust_bgd(group, -1, 0, 0);
                    self.adjust_super_free(-1, 0);
                    let block_num =
                        self.sb.first_data_block + group * self.sb.blocks_per_group + i as u32;
                    self.write_block(block_num, &vec![0u8; self.sb.block_size as usize]);
                    return Some(block_num);
                }
            }
        }
        None
    }

    /// Mark a data block free.
    fn free_block(&self, block_num: u32) {
        let rel = block_num - self.sb.first_data_block;
        let group = rel / self.sb.blocks_per_group;
        let i = (rel % self.sb.blocks_per_group) as usize;
        let bgd = self.disk.read(self.bgd_offset(group), 32);
        let bitmap_block = rd_u32(&bgd, 0);
        let mut bitmap = self.read_block(bitmap_block);
        bitmap[i / 8] &= !(1 << (i % 8));
        self.write_block(bitmap_block, &bitmap);
        self.adjust_bgd(group, 1, 0, 0);
        self.adjust_super_free(1, 0);
    }

    /// Allocate a free inode, returning its number.
    fn alloc_inode(&self, is_dir: bool) -> Option<u32> {
        for group in 0..self.num_groups() {
            let bgd = self.disk.read(self.bgd_offset(group), 32);
            if rd_u16(&bgd, 14) == 0 {
                continue;
            }
            let bitmap_block = rd_u32(&bgd, 4);
            let mut bitmap = self.read_block(bitmap_block);
            for i in 0..self.sb.inodes_per_group as usize {
                if i / 8 >= bitmap.len() {
                    break;
                }
                if bitmap[i / 8] & (1 << (i % 8)) == 0 {
                    bitmap[i / 8] |= 1 << (i % 8);
                    self.write_block(bitmap_block, &bitmap);
                    self.adjust_bgd(group, 0, -1, if is_dir { 1 } else { 0 });
                    self.adjust_super_free(0, -1);
                    return Some(group * self.sb.inodes_per_group + i as u32 + 1);
                }
            }
        }
        None
    }

    /// Mark an inode free.
    fn free_inode(&self, ino: u32, is_dir: bool) {
        let group = (ino - 1) / self.sb.inodes_per_group;
        let i = ((ino - 1) % self.sb.inodes_per_group) as usize;
        let bgd = self.disk.read(self.bgd_offset(group), 32);
        let bitmap_block = rd_u32(&bgd, 4);
        let mut bitmap = self.read_block(bitmap_block);
        bitmap[i / 8] &= !(1 << (i % 8));
        self.write_block(bitmap_block, &bitmap);
        self.adjust_bgd(group, 0, 1, if is_dir { -1 } else { 0 });
        self.adjust_super_free(0, 1);
    }

    /// Disk byte offset of inode `ino`'s on-disk record.
    fn inode_offset(&self, ino: u32) -> Option<u64> {
        if ino == 0 {
            return None;
        }
        let group = (ino - 1) / self.sb.inodes_per_group;
        let index = ((ino - 1) % self.sb.inodes_per_group) as u64;
        let bgd = self.disk.read(self.bgd_offset(group), 32);
        let inode_table = rd_u32(&bgd, 8) as u64;
        Some(inode_table * self.sb.block_size as u64 + index * self.sb.inode_size as u64)
    }

    /// Write back an inode's mutable fields (mode, size, links, i_blocks, block
    /// pointers), preserving the rest of the on-disk record.
    fn write_inode(&self, ino: u32, inode: &Inode, links: u16, i_blocks: u32) {
        let off = match self.inode_offset(ino) {
            Some(o) => o,
            None => return,
        };
        let bs = self.sb.block_size as u64;
        let block = (off / bs) as u32;
        let within = (off % bs) as usize;
        let mut buf = self.read_block(block);
        buf[within..within + 2].copy_from_slice(&inode.mode.to_le_bytes());
        buf[within + 4..within + 8].copy_from_slice(&((inode.size & 0xFFFF_FFFF) as u32).to_le_bytes());
        buf[within + 26..within + 28].copy_from_slice(&links.to_le_bytes());
        buf[within + 28..within + 32].copy_from_slice(&i_blocks.to_le_bytes());
        for (i, b) in inode.block.iter().enumerate() {
            buf[within + 40 + i * 4..within + 44 + i * 4].copy_from_slice(&b.to_le_bytes());
        }
        if inode.is_reg() {
            buf[within + 108..within + 112].copy_from_slice(&((inode.size >> 32) as u32).to_le_bytes());
        }
        self.write_block(block, &buf);
    }

    /// Read an inode's on-disk link count.
    fn inode_links(&self, ino: u32) -> u16 {
        match self.inode_offset(ino) {
            Some(off) => {
                let raw = self.disk.read(off, 28);
                rd_u16(&raw, 26)
            }
            None => 0,
        }
    }

    /// Initialise a freshly-allocated inode's on-disk record to all zeros, then
    /// set its mode and link count.
    fn init_inode(&self, ino: u32, mode: u16, links: u16) {
        let off = match self.inode_offset(ino) {
            Some(o) => o,
            None => return,
        };
        let bs = self.sb.block_size as u64;
        let block = (off / bs) as u32;
        let within = (off % bs) as usize;
        let mut buf = self.read_block(block);
        for b in &mut buf[within..within + self.sb.inode_size as usize] {
            *b = 0;
        }
        buf[within..within + 2].copy_from_slice(&mode.to_le_bytes());
        buf[within + 26..within + 28].copy_from_slice(&links.to_le_bytes());
        self.write_block(block, &buf);
    }

    /// Set logical block `logical` of a file to physical block `phys`, allocating
    /// the single-indirect table if needed. Returns extra blocks allocated for
    /// metadata (the indirect block), or None on allocation failure.
    fn set_block_ptr(&self, inode: &mut Inode, logical: u32, phys: u32) -> Option<u32> {
        let lg = logical as usize;
        if lg < NDIR_BLOCKS {
            inode.block[lg] = phys;
            return Some(0);
        }
        let per = (self.sb.block_size / 4) as usize;
        let lb = lg - NDIR_BLOCKS;
        if lb < per {
            let mut extra = 0;
            if inode.block[IND_BLOCK] == 0 {
                inode.block[IND_BLOCK] = self.alloc_block()?;
                extra = 1;
            }
            let ind = inode.block[IND_BLOCK];
            let mut table = self.read_block(ind);
            table[lb * 4..lb * 4 + 4].copy_from_slice(&phys.to_le_bytes());
            self.write_block(ind, &table);
            return Some(extra);
        }
        None // double/triple indirect unsupported
    }

    /// Write `data` at `offset` in a file, allocating data blocks as needed and
    /// updating the inode (size, block pointers, i_blocks). Returns bytes written.
    fn write_file(&self, ino: u32, inode: &mut Inode, offset: u64, data: &[u8]) -> Result<usize, FsError> {
        let bs = self.sb.block_size as u64;
        let mut newly_allocated: u32 = 0;
        let mut written = 0usize;
        while written < data.len() {
            let pos = offset + written as u64;
            let logical = (pos / bs) as u32;
            let within = (pos % bs) as usize;
            let mut phys = self.resolve_block(inode, logical);
            if phys == 0 {
                phys = self.alloc_block().ok_or(FsError::NoSpace)?;
                newly_allocated += 1;
                let extra = self.set_block_ptr(inode, logical, phys).ok_or(FsError::NoSpace)?;
                newly_allocated += extra;
            }
            let mut buf = self.read_block(phys);
            let take = ((bs as usize) - within).min(data.len() - written);
            buf[within..within + take].copy_from_slice(&data[written..written + take]);
            self.write_block(phys, &buf);
            written += take;
        }
        let end = offset + data.len() as u64;
        if end > inode.size {
            inode.size = end;
        }
        // Recompute i_blocks from scratch is expensive; instead read current and
        // add the sectors for newly allocated blocks. A fresh file starts at 0.
        let links = self.inode_links(ino).max(1);
        let cur_blocks = {
            let off = self.inode_offset(ino).unwrap();
            let raw = self.disk.read(off, 32);
            rd_u32(&raw, 28)
        };
        let i_blocks = cur_blocks + newly_allocated * self.sectors_per_block();
        self.write_inode(ino, inode, links, i_blocks);
        Ok(written)
    }

    /// Split a path into (parent, name).
    fn split_path(path: &str) -> (&str, &str) {
        let trimmed = path.trim_end_matches('/');
        match trimmed.rfind('/') {
            Some(0) => ("/", &trimmed[1..]),
            Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
            None => ("/", trimmed),
        }
    }

    /// Add a directory entry (child_ino, name, file_type) to directory `dir`.
    fn add_dir_entry(&self, dir: &Inode, name: &str, child_ino: u32, file_type: u8) -> Result<(), FsError> {
        let bs = self.sb.block_size as usize;
        let name_bytes = name.as_bytes();
        let needed = 8 + ((name_bytes.len() + 3) & !3);
        let n_blocks = (dir.size as usize).div_ceil(bs);
        for lb in 0..n_blocks {
            let phys = self.resolve_block(dir, lb as u32);
            if phys == 0 {
                continue;
            }
            let mut buf = self.read_block(phys);
            let mut pos = 0usize;
            while pos + 8 <= bs {
                let ino = rd_u32(&buf, pos);
                let rec_len = rd_u16(&buf, pos + 4) as usize;
                let nl = buf[pos + 6] as usize;
                if rec_len == 0 {
                    break;
                }
                let actual = if ino == 0 { 0 } else { 8 + ((nl + 3) & !3) };
                if rec_len - actual >= needed {
                    // Split this record: shrink it to `actual`, place the new
                    // entry in the freed tail.
                    let new_pos = pos + actual;
                    let new_rec = rec_len - actual;
                    if ino != 0 {
                        buf[pos + 4..pos + 6].copy_from_slice(&(actual as u16).to_le_bytes());
                    }
                    buf[new_pos..new_pos + 4].copy_from_slice(&child_ino.to_le_bytes());
                    buf[new_pos + 4..new_pos + 6].copy_from_slice(&(new_rec as u16).to_le_bytes());
                    buf[new_pos + 6] = name_bytes.len() as u8;
                    buf[new_pos + 7] = file_type;
                    buf[new_pos + 8..new_pos + 8 + name_bytes.len()].copy_from_slice(name_bytes);
                    self.write_block(phys, &buf);
                    return Ok(());
                }
                pos += rec_len;
            }
        }
        Err(FsError::NoSpace)
    }

    /// Remove the directory entry named `name` from `dir`, returning the child
    /// inode number it referenced.
    fn remove_dir_entry(&self, dir: &Inode, name: &str) -> Option<u32> {
        let bs = self.sb.block_size as usize;
        let n_blocks = (dir.size as usize).div_ceil(bs);
        for lb in 0..n_blocks {
            let phys = self.resolve_block(dir, lb as u32);
            if phys == 0 {
                continue;
            }
            let mut buf = self.read_block(phys);
            let mut pos = 0usize;
            let mut prev: Option<usize> = None;
            while pos + 8 <= bs {
                let ino = rd_u32(&buf, pos);
                let rec_len = rd_u16(&buf, pos + 4) as usize;
                let nl = buf[pos + 6] as usize;
                if rec_len == 0 {
                    break;
                }
                // Bound-check the name slice before comparing: a corrupt or
                // misparsed record could make `pos + 8 + nl` exceed the block and
                // panic (crashing the server). `list_dir` guards the same way.
                if ino != 0
                    && nl == name.len()
                    && pos + 8 + nl <= bs
                    && &buf[pos + 8..pos + 8 + nl] == name.as_bytes()
                {
                    match prev {
                        Some(pp) => {
                            // Merge this record into the previous one.
                            let prev_rec = rd_u16(&buf, pp + 4) as usize;
                            buf[pp + 4..pp + 6]
                                .copy_from_slice(&((prev_rec + rec_len) as u16).to_le_bytes());
                        }
                        None => {
                            // First record: mark unused.
                            buf[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes());
                        }
                    }
                    self.write_block(phys, &buf);
                    return Some(ino);
                }
                prev = Some(pos);
                pos += rec_len;
            }
        }
        None
    }

    /// Create a regular file at `path`, returning (ino, inode).
    ///
    /// If a regular file already exists at `path`, it is reused and truncated to
    /// zero (open-or-create with O_TRUNC semantics) rather than allocating a
    /// second inode with a duplicate directory entry — the latter corrupts the
    /// directory. A directory already at `path` is rejected.
    fn create_file(&self, path: &str) -> Result<(u32, Inode), FsError> {
        let (parent_path, name) = Self::split_path(path);
        let (_, parent) = self.resolve_num(parent_path).ok_or(FsError::NotFound)?;
        if !parent.is_dir() {
            return Err(FsError::NotADirectory);
        }
        if name.is_empty() || name.len() > 255 {
            return Err(FsError::InvalidArgument);
        }
        // Reuse an existing entry rather than adding a duplicate dirent.
        if let Some((existing_ino, existing)) = self.resolve_num(path) {
            if existing.is_dir() {
                return Err(FsError::IsADirectory);
            }
            // Truncate: free the file's data blocks, then reset the inode in
            // place (same inode number, so its single directory entry stays).
            self.free_inode_blocks(&existing);
            self.init_inode(existing_ino, S_IFREG | 0o644, 1);
            let inode = self.read_inode(existing_ino).ok_or(FsError::IoError)?;
            return Ok((existing_ino, inode));
        }
        let ino = self.alloc_inode(false).ok_or(FsError::NoInodes)?;
        self.init_inode(ino, S_IFREG | 0o644, 1);
        self.add_dir_entry(&parent, name, ino, 1)?;
        let inode = self.read_inode(ino).ok_or(FsError::IoError)?;
        Ok((ino, inode))
    }

    /// Free all data blocks (direct + single-indirect) referenced by `inode`,
    /// including the single-indirect table block itself. Does not touch the
    /// inode record — callers either re-init it or free the inode separately.
    fn free_inode_blocks(&self, inode: &Inode) {
        let bs = self.sb.block_size as u64;
        let n_blocks = inode.size.div_ceil(bs) as u32;
        for lb in 0..n_blocks {
            let phys = self.resolve_block(inode, lb);
            if phys != 0 {
                self.free_block(phys);
            }
        }
        if inode.block[IND_BLOCK] != 0 {
            self.free_block(inode.block[IND_BLOCK]);
        }
    }

    /// Create a directory at `path`.
    fn mkdir(&self, path: &str) -> Result<(), FsError> {
        let (parent_path, name) = Self::split_path(path);
        let (parent_ino, parent) = self.resolve_num(parent_path).ok_or(FsError::NotFound)?;
        if !parent.is_dir() {
            return Err(FsError::NotADirectory);
        }
        // Reject an existing name rather than adding a duplicate dirent.
        if self.resolve_num(path).is_some() {
            return Err(FsError::AlreadyExists);
        }
        let ino = self.alloc_inode(true).ok_or(FsError::NoInodes)?;
        self.init_inode(ino, S_IFDIR | 0o755, 2);
        // Allocate the directory's first data block and fill in "." and "..".
        let dblock = self.alloc_block().ok_or(FsError::NoSpace)?;
        let bs = self.sb.block_size as usize;
        let mut buf = vec![0u8; bs];
        // "." -> self, rec_len 12.
        buf[0..4].copy_from_slice(&ino.to_le_bytes());
        buf[4..6].copy_from_slice(&12u16.to_le_bytes());
        buf[6] = 1;
        buf[7] = 2;
        buf[8] = b'.';
        // ".." -> parent, rec_len fills the rest of the block.
        buf[12..16].copy_from_slice(&parent_ino.to_le_bytes());
        buf[16..18].copy_from_slice(&((bs - 12) as u16).to_le_bytes());
        buf[18] = 2;
        buf[19] = 2;
        buf[20] = b'.';
        buf[21] = b'.';
        self.write_block(dblock, &buf);
        // Write the dir inode: size = one block, block[0] = dblock.
        let mut inode = Inode { mode: S_IFDIR | 0o755, size: bs as u64, block: [0; 15] };
        inode.block[0] = dblock;
        self.write_inode(ino, &inode, 2, self.sectors_per_block());
        // Link it into the parent and bump the parent's link count (the new
        // directory's ".." references the parent).
        self.add_dir_entry(&parent, name, ino, 2)?;
        let plinks = self.inode_links(parent_ino);
        self.write_inode(parent_ino, &parent, plinks + 1, self.dir_i_blocks(&parent));
        Ok(())
    }

    /// `i_blocks` for a directory inode (sum of its allocated blocks).
    fn dir_i_blocks(&self, inode: &Inode) -> u32 {
        let bs = self.sb.block_size as u64;
        let nblocks = (inode.size).div_ceil(bs) as u32;
        nblocks * self.sectors_per_block()
    }

    /// Remove a file or empty directory at `path`.
    fn unlink(&self, path: &str) -> Result<(), FsError> {
        let (ino, inode) = self.resolve_num(path).ok_or(FsError::NotFound)?;
        let (parent_path, name) = Self::split_path(path);
        let (_, parent) = self.resolve_num(parent_path).ok_or(FsError::NotFound)?;

        // Refuse to remove a non-empty directory.
        if inode.is_dir() {
            let children = self.list_dir(&inode);
            if children.iter().any(|e| e.name != "." && e.name != "..") {
                return Err(FsError::NotEmpty);
            }
        }

        if self.remove_dir_entry(&parent, name).is_none() {
            return Err(FsError::NotFound);
        }

        // Decrement links; when they reach zero, free the inode and its blocks.
        let links = self.inode_links(ino);
        let new_links = links.saturating_sub(1);
        if new_links == 0 || inode.is_dir() {
            // Free all data blocks the file/dir used, then the inode itself.
            self.free_inode_blocks(&inode);
            self.free_inode(ino, inode.is_dir());
        } else {
            self.write_inode(ino, &inode, new_links, 0);
        }
        Ok(())
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
        blocks_count: rd_u32(&sb_raw, 4),
        blocks_per_group: rd_u32(&sb_raw, 32),
        inodes_per_group,
        inode_size: if inode_size == 0 { 128 } else { inode_size },
    };
    // The group-descriptor table immediately follows the superblock block.
    let bgd_block = (first_data_block + 1) as u64;
    Some(Ext2 { disk, sb, bgd_block })
}

// ---------- server ----------

struct OpenFile {
    ino: u32,
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

    fn alloc_fd(&mut self, ino: u32, inode: Inode) -> u64 {
        for (i, slot) in self.open.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(OpenFile { ino, inode });
                return i as u64;
            }
        }
        self.open.push(Some(OpenFile { ino, inode }));
        (self.open.len() - 1) as u64
    }

    /// Read up to `len` bytes the client placed at the start of its region.
    fn read_client_data(&self, len: usize) -> alloc::vec::Vec<u8> {
        let n = len.min(self.client_len);
        let s = unsafe { core::slice::from_raw_parts(self.client_buf, n) };
        s.to_vec()
    }

    fn handle(&mut self, req: &libthemelios::IpcMessage) -> [u64; 4] {
        match req.words[0] {
            fs_proto::OP_OPEN => {
                let path = self.read_path(req.words[1] as usize);
                match self.fs.resolve_num(&path) {
                    Some((ino, inode)) => {
                        let fd = self.alloc_fd(ino, inode);
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
            fs_proto::OP_WRITE => {
                let fd = req.words[1] as usize;
                let off = req.words[2];
                let len = (req.words[3] as usize).min(self.client_len);
                let data = self.read_client_data(len);
                // Copy the inode out, write, then store the updated inode back.
                let (ino, mut inode) = match self.open.get(fd).and_then(|s| s.as_ref()) {
                    Some(of) if of.inode.is_reg() => (of.ino, of.inode),
                    Some(_) => return [fs_proto::encode_error(FsError::IsADirectory), 0, 0, 0],
                    None => return [fs_proto::encode_error(FsError::InvalidArgument), 0, 0, 0],
                };
                match self.fs.write_file(ino, &mut inode, off, &data) {
                    Ok(n) => {
                        if let Some(Some(of)) = self.open.get_mut(fd) {
                            of.inode = inode;
                        }
                        [fs_proto::STATUS_OK, n as u64, 0, 0]
                    }
                    Err(e) => [fs_proto::encode_error(e), 0, 0, 0],
                }
            }
            fs_proto::OP_CREATE => {
                let path = self.read_path(req.words[1] as usize);
                match self.fs.create_file(&path) {
                    Ok((ino, inode)) => {
                        let fd = self.alloc_fd(ino, inode);
                        [fs_proto::STATUS_OK, fd, 0, 0]
                    }
                    Err(e) => [fs_proto::encode_error(e), 0, 0, 0],
                }
            }
            fs_proto::OP_MKDIR => {
                let path = self.read_path(req.words[1] as usize);
                match self.fs.mkdir(&path) {
                    Ok(()) => [fs_proto::STATUS_OK, 0, 0, 0],
                    Err(e) => [fs_proto::encode_error(e), 0, 0, 0],
                }
            }
            fs_proto::OP_UNLINK => {
                let path = self.read_path(req.words[1] as usize);
                match self.fs.unlink(&path) {
                    Ok(()) => [fs_proto::STATUS_OK, 0, 0, 0],
                    Err(e) => [fs_proto::encode_error(e), 0, 0, 0],
                }
            }
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
