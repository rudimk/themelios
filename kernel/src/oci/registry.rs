//! # Registry client (Phase 5.6)
//!
//! Pulls a container image from a registry speaking the **Docker Registry HTTP
//! API v2**: `GET /v2/<name>/manifests/<ref>` for the manifest, then
//! `GET /v2/<name>/blobs/<digest>` for the config and each layer blob. Blobs are
//! digest-verified and gzipped layers gunzipped by [`super::unpack_registry`].
//!
//! The HTTP transport is abstracted behind [`Connection`] so the whole pull
//! pipeline (request → response parse → digest-verify → gunzip → assemble) is
//! testable with canned responses. The **live** `Connection` over the kernel TCP
//! client (`net::socket::ksocket_*`) reachable through a slirp `guestfwd` registry
//! is a thin, documented follow-up — the pipeline value is fully exercised here.

use super::{json, unpack_registry, Image, OciError};
use crate::http::{content_length, find_sub};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

extern crate alloc;

/// An HTTP transport: send a raw request, return the raw response bytes (or
/// `None` on a transport error). One request/response per call (`Connection:
/// close`). The live impl opens a TCP socket; the test impl returns canned bytes.
pub trait Connection {
    /// Send `request` and return the full HTTP response.
    fn request(&mut self, request: &[u8]) -> Option<Vec<u8>>;
}

/// Parse an HTTP/1.1 response into `(status_code, body)`. Uses `Content-Length`
/// when present, else the bytes after the header terminator. Testable directly.
/// The byte-scanning helpers (`find_sub`, `content_length`) are shared with the
/// Phase 6 request parser in [`crate::http`].
///
/// LIMITATION (documented, not a safety issue): only `Content-Length` framing is
/// handled — a `Transfer-Encoding: chunked` response with no `Content-Length`
/// returns the raw chunked framing as the body (JSON parse / digest then fails
/// cleanly, no panic). Some registries/CDNs chunk blob responses, so the live
/// puller will need chunked decoding; the offline pipeline here does not.
pub fn http_body(resp: &[u8]) -> Option<(u16, Vec<u8>)> {
    let sep = find_sub(resp, b"\r\n\r\n")?;
    let head = &resp[..sep];
    let body_start = sep + 4;

    // Status line: "HTTP/1.1 200 OK" → the second whitespace-separated token.
    let line_end = find_sub(head, b"\r\n").unwrap_or(head.len());
    let status_line = core::str::from_utf8(&head[..line_end]).ok()?;
    let status: u16 = status_line.split(' ').nth(1)?.parse().ok()?;

    // Content-Length (case-insensitive header search). `n` is attacker-controlled,
    // so compute the end with `checked_add` — a huge Content-Length would otherwise
    // overflow `body_start + n` (an arithmetic-overflow panic under debug/test
    // builds where `overflow-checks` is on). A truncated body (end past the buffer)
    // falls out as `None` via `.get(..)`.
    let clen = content_length(head);
    let body = match clen {
        Some(n) => {
            let end = body_start.checked_add(n)?;
            resp.get(body_start..end)?.to_vec()
        }
        None => resp[body_start..].to_vec(),
    };
    Some((status, body))
}

/// Issue a GET and return the 200 response body (or `None`).
fn get<C: Connection>(conn: &mut C, host: &str, path: &str) -> Option<Vec<u8>> {
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.manifest.v1+json, */*\r\nConnection: close\r\n\r\n"
    );
    let resp = conn.request(req.as_bytes())?;
    let (status, body) = http_body(&resp)?;
    if status != 200 {
        return None;
    }
    Some(body)
}

/// Pull image `name`@`reference` from the registry reachable via `conn` (with the
/// given `Host`), returning the assembled [`Image`]. Fetches the manifest, then
/// the config + layer blobs it names, then digest-verifies and assembles them.
pub fn pull<C: Connection>(
    conn: &mut C,
    host: &str,
    name: &str,
    reference: &str,
) -> Result<Image, OciError> {
    // 1. Manifest.
    let manifest = get(conn, host, &format!("/v2/{name}/manifests/{reference}"))
        .ok_or(OciError::MissingBlob)?;
    let m = json::parse(&manifest).ok_or(OciError::BadJson)?;

    // 2. Determine the blob digests to fetch (config + layers).
    let config_digest = m
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|v| v.as_str())
        .ok_or(OciError::BadManifest)?
        .to_string();
    let layer_digests: Vec<String> = m
        .get("layers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("digest").and_then(|d| d.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // 3. Fetch every blob.
    let mut blobs: Vec<(String, Vec<u8>)> = Vec::new();
    for digest in core::iter::once(config_digest).chain(layer_digests.into_iter()) {
        let blob = get(conn, host, &format!("/v2/{name}/blobs/{digest}"))
            .ok_or(OciError::MissingBlob)?;
        blobs.push((digest, blob));
    }

    // 4. Digest-verify + assemble (the blobs are looked up from what we fetched).
    unpack_registry(&manifest, &|dig| {
        blobs.iter().find(|(k, _)| k == dig).map(|(_, v)| v.clone())
    })
}
