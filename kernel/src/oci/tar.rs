//! # USTAR tar reader (Phase 5.4)
//!
//! A minimal, dependency-light (`alloc`-only) reader for the POSIX USTAR tar
//! format — enough to walk a `docker save` bundle's outer tar and its
//! (uncompressed) layer tars. It has no kernel dependencies so it lifts into the
//! ring-3 `oci-server` unchanged (5.5).
//!
//! Handles regular files and directories; GNU long-name (`L`) records are
//! resolved into the following entry's name. Other special types are skipped.

use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

/// One entry read from a tar archive.
pub struct TarEntry<'a> {
    /// Normalized absolute path (`/`-prefixed, no `./`).
    pub name: String,
    /// USTAR type flag: `0`/`\0` = file, `5` = directory (others skipped).
    pub typeflag: u8,
    /// The entry's file data (empty for directories).
    pub data: &'a [u8],
}

impl TarEntry<'_> {
    /// Whether this entry is a regular file.
    pub fn is_file(&self) -> bool {
        self.typeflag == b'0' || self.typeflag == 0
    }
    /// Whether this entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.typeflag == b'5'
    }
}

/// Parse a base-8 (octal) ASCII field, tolerating trailing spaces/NULs — the tar
/// encoding for sizes and modes.
fn parse_octal(field: &[u8]) -> u64 {
    let mut v: u64 = 0;
    for &b in field {
        if (b'0'..=b'7').contains(&b) {
            v = v * 8 + (b - b'0') as u64;
        } else if b == b' ' || b == 0 {
            // ignore padding; a space/NUL ends the number
            if v != 0 {
                break;
            }
        }
    }
    v
}

/// Trim a NUL-padded fixed field to its string content.
fn field_str(field: &[u8]) -> &str {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).unwrap_or("")
}

/// Normalize a tar member path to an absolute `/`-prefixed path with no `./`.
fn normalize(name: &str) -> String {
    let n = name.strip_prefix("./").unwrap_or(name);
    let n = n.trim_start_matches('/');
    let mut out = String::from("/");
    out.push_str(n);
    // Drop any trailing slash except for the root itself.
    if out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

/// Read every entry of a tar archive into a `Vec`. Stops at the end-of-archive
/// marker (a zero block) or when the data runs out. Returns `None` on a malformed
/// header (bad length), never panicking.
pub fn read_all(archive: &[u8]) -> Option<Vec<TarEntry<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    // A pending GNU long name applies to the next real entry.
    let mut long_name: Option<String> = None;

    while pos + 512 <= archive.len() {
        let header = &archive[pos..pos + 512];
        // Two consecutive zero blocks (or a single one) end the archive; a header
        // whose name starts with NUL is the terminator.
        if header[0] == 0 {
            break;
        }
        let size = parse_octal(&header[124..136]) as usize;
        let typeflag = header[156];
        let data_start = pos + 512;
        let data_end = data_start.checked_add(size)?;
        if data_end > archive.len() {
            return None; // truncated
        }
        let data = &archive[data_start..data_end];

        // GNU long name: this record's data is the name of the *next* entry.
        if typeflag == b'L' {
            long_name = Some(String::from(field_str(data)));
        } else {
            let raw = match long_name.take() {
                Some(n) => n,
                None => {
                    // USTAR: optional `prefix` (345..500) joins with `name` (0..100).
                    let name = field_str(&header[0..100]);
                    let prefix = field_str(&header[345..500]);
                    if prefix.is_empty() {
                        String::from(name)
                    } else {
                        let mut s = String::from(prefix);
                        s.push('/');
                        s.push_str(name);
                        s
                    }
                }
            };
            out.push(TarEntry {
                name: normalize(&raw),
                typeflag,
                data,
            });
        }

        // Advance past the data, rounded up to the 512-byte block.
        let advance = 512 + ((size + 511) & !511);
        pos += advance;
    }
    Some(out)
}
