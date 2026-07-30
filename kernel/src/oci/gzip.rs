//! # gzip decompression (Phase 5.6)
//!
//! OCI **registry** image layers are gzip-compressed tarballs (unlike `docker
//! save`, whose layers are plain tar — Phase 5.4). `miniz_oxide` decompresses raw
//! DEFLATE and zlib but not gzip, so this parses the RFC-1952 gzip wrapper (magic
//! + header + optional fields + 8-byte trailer) and inflates the DEFLATE body.

use alloc::vec::Vec;

extern crate alloc;

/// Maximum inflated size for a single layer (8 MiB). The blob's `sha256` proves
/// it matches the manifest — **not** that it's benign: a hostile registry can put
/// the real digest of a decompression bomb (a tiny DEFLATE stream of zeros that
/// inflates to gigabytes) in a manifest it also serves, so digest verification
/// does not bound the output. We inflate with an explicit cap (well under the
/// kernel's 16 MiB heap ceiling) so a bomb fails cleanly instead of exhausting
/// the heap and aborting the kernel. Real OCI base layers are a few MiB.
const MAX_INFLATED: usize = 8 * 1024 * 1024;

// gzip header FLG bits (RFC 1952 §2.3.1).
const FTEXT: u8 = 1 << 0;
const FHCRC: u8 = 1 << 1;
const FEXTRA: u8 = 1 << 2;
const FNAME: u8 = 1 << 3;
const FCOMMENT: u8 = 1 << 4;

/// Decompress a gzip member, returning the inflated bytes. Returns `None` on a
/// bad magic/header, a DEFLATE error, or output exceeding [`MAX_INFLATED`] —
/// never panics on malformed input.
///
/// LIMITATION (documented): a **single** gzip member only — the trailer is
/// assumed to sit at the very end (`data[..len-8]`). OCI layers are single-member
/// gzip in practice, so this holds; concatenated multi-member streams are not
/// supported.
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
    // Bounded inflate: `decompress_to_vec` is unbounded and would let a
    // decompression bomb grow until the allocator aborts; the `_with_limit`
    // variant fails (returns `Err`) once the output would exceed `MAX_INFLATED`.
    miniz_oxide::inflate::decompress_to_vec_with_limit(deflate, MAX_INFLATED).ok()
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
