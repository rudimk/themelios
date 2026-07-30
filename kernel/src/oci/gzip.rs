//! # gzip decompression (Phase 5.6)
//!
//! OCI **registry** image layers are gzip-compressed tarballs (unlike `docker
//! save`, whose layers are plain tar — Phase 5.4). `miniz_oxide` decompresses raw
//! DEFLATE and zlib but not gzip, so this parses the RFC-1952 gzip wrapper (magic
//! + header + optional fields + 8-byte trailer) and inflates the DEFLATE body.

use alloc::vec::Vec;

extern crate alloc;

// gzip header FLG bits (RFC 1952 §2.3.1).
const FTEXT: u8 = 1 << 0;
const FHCRC: u8 = 1 << 1;
const FEXTRA: u8 = 1 << 2;
const FNAME: u8 = 1 << 3;
const FCOMMENT: u8 = 1 << 4;

/// Decompress a gzip member, returning the inflated bytes. Returns `None` on a
/// bad magic/header or a DEFLATE error — never panics on malformed input.
pub fn decompress(data: &[u8]) -> Option<Vec<u8>> {
    // Fixed 10-byte header: magic (0x1f 0x8b), CM=8 (deflate), FLG, MTIME(4),
    // XFL, OS.
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return None;
    }
    let flg = data[3];
    let mut pos = 10usize;

    // FEXTRA: 2-byte length then that many bytes.
    if flg & FEXTRA != 0 {
        if pos + 2 > data.len() {
            return None;
        }
        let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + xlen;
    }
    // FNAME / FCOMMENT: NUL-terminated strings.
    if flg & FNAME != 0 {
        pos = skip_cstr(data, pos)?;
    }
    if flg & FCOMMENT != 0 {
        pos = skip_cstr(data, pos)?;
    }
    // FHCRC: 2-byte header CRC.
    if flg & FHCRC != 0 {
        pos += 2;
    }
    // FTEXT is advisory; nothing to skip.
    let _ = FTEXT;

    // The DEFLATE stream runs to 8 bytes before the end (CRC32 + ISIZE trailer).
    if pos + 8 > data.len() {
        return None;
    }
    let deflate = &data[pos..data.len() - 8];
    miniz_oxide::inflate::decompress_to_vec(deflate).ok()
}

/// Advance past a NUL-terminated string starting at `pos`.
fn skip_cstr(data: &[u8], mut pos: usize) -> Option<usize> {
    while pos < data.len() {
        let b = data[pos];
        pos += 1;
        if b == 0 {
            return Some(pos);
        }
    }
    None
}
