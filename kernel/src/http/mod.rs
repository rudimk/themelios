//! # Minimal HTTP/1.1 primitives (Phase 6.0)
//!
//! Shared, dependency-light (`alloc`-only) HTTP helpers plus an HTTP **request**
//! parser for the Phase 6 management API. The registry client (Phase 5.6) already
//! parses HTTP *responses*; this adds the request side and factors the shared
//! byte-scanning helpers (`find_sub`, header lookup, `Content-Length`) out of
//! `oci::registry` so both directions share one implementation. It lifts unchanged
//! into the ring-3 `api-server`.
//!
//! Everything here consumes **untrusted bytes off a socket**, so it is
//! fail-closed: bounded request/header sizes, `checked_add` on lengths, and
//! `None` on anything malformed — never a panic or an unbounded allocation.

use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

/// Ceiling on the total request bytes we will parse (a hostile client can't make
/// us scan an unbounded buffer). The management API's requests are tiny.
pub const MAX_REQUEST: usize = 64 * 1024;
/// Ceiling on the number of header lines.
pub const MAX_HEADERS: usize = 64;
/// Ceiling on a request body (`Content-Length`).
pub const MAX_BODY: usize = 32 * 1024;

/// Find the first occurrence of `needle` in `haystack`. Shared by the request
/// parser and the registry client's response parser.
pub fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

/// Strip a trailing `\r` from a line (CRLF-split lines keep the `\r`).
fn strip_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', init)) => init,
        _ => line,
    }
}

/// Trim leading/trailing spaces and tabs from a byte slice.
fn trim_ws(mut s: &[u8]) -> &[u8] {
    while let Some((&f, rest)) = s.split_first() {
        if f == b' ' || f == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    while let Some((&l, rest)) = s.split_last() {
        if l == b' ' || l == b'\t' {
            s = rest;
        } else {
            break;
        }
    }
    s
}

/// Case-insensitively find a header's value within a head block (the bytes before
/// the `\r\n\r\n` terminator). The first line — the request or status line — is
/// skipped, so a header name can't be matched against the target. `name` is given
/// without the trailing colon. Returns the trimmed value bytes.
pub fn header_value<'a>(head: &'a [u8], name: &str) -> Option<&'a [u8]> {
    for line in head.split(|&b| b == b'\n').skip(1) {
        let line = strip_cr(line);
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let (k, v) = (&line[..colon], &line[colon + 1..]);
            if k.eq_ignore_ascii_case(name.as_bytes()) {
                return Some(trim_ws(v));
            }
        }
    }
    None
}

/// Parse a `Content-Length` header value, if present and valid.
pub fn content_length(head: &[u8]) -> Option<usize> {
    let v = header_value(head, "content-length")?;
    core::str::from_utf8(v).ok()?.trim().parse().ok()
}

/// A parsed HTTP/1.1 request.
pub struct Request {
    /// The request method (`GET`, `POST`, …), verbatim.
    pub method: String,
    /// The request path with any Docker `/v1.NN` API-version prefix stripped
    /// (e.g. `/v1.43/containers/json` → `/containers/json`).
    pub path: String,
    /// The stripped API-version string (e.g. `1.43`), if the path carried one.
    pub api_version: Option<String>,
    /// The raw query string after `?` (empty if none).
    pub query: String,
    /// Header (name, value) pairs in wire order; names verbatim.
    pub headers: Vec<(String, String)>,
    /// The request body (empty if no `Content-Length`).
    pub body: Vec<u8>,
}

impl Request {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// First value of a query parameter, e.g. `?all=1&x=2` → `query_param("all")`
    /// is `Some("1")`. A bare `?flag` yields `Some("")`.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query.split('&').find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            let k = it.next()?;
            if k == key {
                Some(it.next().unwrap_or(""))
            } else {
                None
            }
        })
    }
}

/// Strip a leading Docker API-version prefix `/v<major>.<minor>` (or `/v<n>`) from
/// a path. `/v1.43/containers/json` → `(Some("1.43"), "/containers/json")`; a path
/// without the prefix is returned unchanged with `None`.
fn strip_version_prefix(path: &str) -> (Option<String>, &str) {
    // Must start with "/v" immediately followed by a digit.
    let rest = match path.strip_prefix("/v") {
        Some(r) if r.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) => r,
        _ => return (None, path),
    };
    // The version token runs to the next '/', or the end of the path.
    match rest.find('/') {
        Some(slash) => {
            let ver = &rest[..slash];
            if ver.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                (Some(String::from(ver)), &rest[slash..])
            } else {
                (None, path)
            }
        }
        None => {
            if rest.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                (Some(String::from(rest)), "/")
            } else {
                (None, path)
            }
        }
    }
}

/// Parse a complete HTTP/1.1 request from `bytes`. Returns `None` on anything
/// malformed, oversized, or using an unsupported framing.
///
/// Assumes the full request (headers + any `Content-Length` body) is present in
/// `bytes`; a truncated body fails closed. `Transfer-Encoding` (chunked) is
/// rejected — the same single-`Content-Length` limitation as the response parser.
pub fn parse_request(bytes: &[u8]) -> Option<Request> {
    if bytes.len() > MAX_REQUEST {
        return None;
    }
    let sep = find_sub(bytes, b"\r\n\r\n")?;
    let head = &bytes[..sep];
    let body_start = sep + 4;

    // Request line: METHOD SP request-target SP HTTP/x.y
    let line_end = find_sub(head, b"\r\n").unwrap_or(head.len());
    let request_line = core::str::from_utf8(&head[..line_end]).ok()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
        return None;
    }

    // Split the target into path + query, then strip a Docker version prefix.
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let (api_version, path) = strip_version_prefix(raw_path);

    // Header lines (skip the request line).
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.split(|&b| b == b'\n').skip(1) {
        let line = strip_cr(line);
        if line.is_empty() {
            continue;
        }
        if headers.len() >= MAX_HEADERS {
            return None;
        }
        let colon = line.iter().position(|&b| b == b':')?;
        let k = core::str::from_utf8(&line[..colon]).ok()?;
        let v = core::str::from_utf8(trim_ws(&line[colon + 1..])).ok()?;
        headers.push((String::from(k), String::from(v)));
    }

    // Body. Chunked transfer-encoding is unsupported (fail closed).
    if header_value(head, "transfer-encoding").is_some() {
        return None;
    }
    let body = match content_length(head) {
        Some(n) => {
            if n > MAX_BODY {
                return None;
            }
            let end = body_start.checked_add(n)?;
            bytes.get(body_start..end)?.to_vec()
        }
        None => Vec::new(),
    };

    Some(Request {
        method: String::from(method),
        path: String::from(path),
        api_version,
        query: String::from(query),
        headers,
        body,
    })
}
