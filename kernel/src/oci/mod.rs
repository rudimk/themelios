//! # OCI / docker-save image unpacking (Phase 5.4)
//!
//! Turns a locally-staged container image bundle into a flat rootfs file list plus
//! the image's runtime config (entrypoint/cmd/env/workdir). The input is the
//! **`docker save`** archive format — an outer tar containing `manifest.json`, an
//! image config JSON, and one or more **uncompressed** layer tars.
//!
//! ## Containment
//!
//! Everything here parses **untrusted image bytes** (tar headers, JSON, layer
//! contents), so it is written dependency-light (`alloc`-only, no kernel deps) to
//! run in the ring-3 `oci-server` — the same ring-3 containment as the FS/net
//! servers. Phase 5.4 delivers and unit-tests this library; Phase 5.5 drives it
//! from the ring-3 server and writes the assembled rootfs to disk.
//!
//! ## Layers and whiteouts
//!
//! Layers apply in order, later over earlier. OCI **whiteouts** delete lower-layer
//! entries: a file named `.wh.<name>` removes `<name>` in that directory, and
//! `.wh..wh..opq` marks a directory opaque (drops all lower entries under it).
//!
//! gzip-compressed layers (the *registry* wire format) are out of scope here —
//! `docker save` layers are uncompressed; gzip arrives with the registry in 5.6.

pub mod gzip;
pub mod json;
pub mod registry;
pub mod sha256;
pub mod tar;

use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

/// The runtime configuration extracted from an image's config JSON.
pub struct ImageConfig {
    /// `config.Entrypoint` — the fixed leading argv, if any.
    pub entrypoint: Vec<String>,
    /// `config.Cmd` — default arguments (appended to the entrypoint).
    pub cmd: Vec<String>,
    /// `config.Env` — `KEY=VALUE` environment strings.
    pub env: Vec<String>,
    /// `config.WorkingDir` — the container's initial cwd (defaults to `/`).
    pub cwd: String,
}

/// A single file (or directory) in the assembled rootfs.
pub struct OciFile {
    /// Absolute path within the rootfs (`/`-prefixed).
    pub path: String,
    /// File contents (empty for a directory).
    pub data: Vec<u8>,
    /// True if this entry is a directory.
    pub is_dir: bool,
}

/// A fully-unpacked image: the assembled rootfs file list + its runtime config.
pub struct Image {
    /// Files/dirs of the flattened rootfs, later layers already applied over
    /// earlier ones and whiteouts resolved.
    pub files: Vec<OciFile>,
    /// The image's runtime configuration.
    pub config: ImageConfig,
}

/// Reasons an image bundle could not be unpacked.
#[derive(Debug, PartialEq, Eq)]
pub enum OciError {
    /// The outer archive (or a layer) is not a valid tar.
    BadTar,
    /// `manifest.json` (or the config JSON) failed to parse.
    BadJson,
    /// No `manifest.json` in the bundle.
    NoManifest,
    /// The manifest names a config file that isn't in the bundle.
    NoConfig,
    /// The manifest names a layer that isn't in the bundle.
    NoLayer,
    /// The manifest is structurally not what we expect.
    BadManifest,
    /// A blob's content did not match its `sha256:` digest (registry pull).
    DigestMismatch,
    /// A gzip-compressed layer failed to decompress (registry pull).
    BadGzip,
    /// A referenced blob was not available (registry pull).
    MissingBlob,
}

/// Find a member of the outer tar by exact normalized path (`name` is given as it
/// appears in the manifest, e.g. `config.json` or `layer.tar`).
fn find<'a>(entries: &'a [tar::TarEntry<'a>], name: &str) -> Option<&'a [u8]> {
    let want = if name.starts_with('/') {
        String::from(name)
    } else {
        let mut s = String::from("/");
        s.push_str(name);
        s
    };
    entries.iter().find(|e| e.name == want).map(|e| e.data)
}

/// The basename (final path component) of an absolute path.
fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The directory portion of an absolute path (without trailing slash; `/` for a
/// top-level entry).
fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "/",
    }
}

/// Unpack a `docker save` bundle into an [`Image`].
pub fn unpack(bundle: &[u8]) -> Result<Image, OciError> {
    // 1. Read the outer tar.
    let entries = tar::read_all(bundle).ok_or(OciError::BadTar)?;

    // 2. Parse manifest.json → the first image's config name + layer list.
    let manifest_bytes = find(&entries, "manifest.json").ok_or(OciError::NoManifest)?;
    let manifest = json::parse(manifest_bytes).ok_or(OciError::BadJson)?;
    let first = manifest
        .as_array()
        .and_then(|a| a.first())
        .ok_or(OciError::BadManifest)?;
    let config_name = first
        .get("Config")
        .and_then(|v| v.as_str())
        .ok_or(OciError::BadManifest)?;
    let layers: Vec<String> = first
        .get("Layers")
        .map(|v| v.as_string_vec())
        .unwrap_or_default();
    if layers.is_empty() {
        return Err(OciError::BadManifest);
    }

    // 3. Parse the image config JSON.
    let config_bytes = find(&entries, config_name).ok_or(OciError::NoConfig)?;
    let cfg = json::parse(config_bytes).ok_or(OciError::BadJson)?;
    let config = parse_image_config(&cfg);

    // 4. Apply each (uncompressed) layer in order, resolving whiteouts.
    let mut files: Vec<OciFile> = Vec::new();
    for layer_name in &layers {
        let layer_bytes = find(&entries, layer_name).ok_or(OciError::NoLayer)?;
        let layer = tar::read_all(layer_bytes).ok_or(OciError::BadTar)?;
        apply_layer(&mut files, &layer);
    }

    Ok(Image { files, config })
}

/// Extract the runtime config from a parsed image-config JSON value (the `config`
/// object holds `Entrypoint`/`Cmd`/`Env`/`WorkingDir`). Shared by the docker-save
/// and registry paths.
fn parse_image_config(cfg: &json::Value) -> ImageConfig {
    let cfg_obj = cfg.get("config");
    let entrypoint = cfg_obj
        .and_then(|c| c.get("Entrypoint"))
        .map(|v| v.as_string_vec())
        .unwrap_or_default();
    let cmd = cfg_obj
        .and_then(|c| c.get("Cmd"))
        .map(|v| v.as_string_vec())
        .unwrap_or_default();
    let env = cfg_obj
        .and_then(|c| c.get("Env"))
        .map(|v| v.as_string_vec())
        .unwrap_or_default();
    let cwd = cfg_obj
        .and_then(|c| c.get("WorkingDir"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| String::from("/"));
    ImageConfig { entrypoint, cmd, env, cwd }
}

/// Apply one layer's tar entries over the accumulated file set, resolving OCI
/// whiteouts (`.wh.<name>`) and opaque directories (`.wh..wh..opq`). Later layers
/// win. Shared by the docker-save and registry paths.
fn apply_layer(files: &mut Vec<OciFile>, layer: &[tar::TarEntry]) {
    for e in layer {
        let base = basename(&e.name);
        if base == ".wh..wh..opq" {
            let dir = dirname(&e.name);
            let mut prefix = String::from(dir);
            if !prefix.ends_with('/') {
                prefix.push('/');
            }
            files.retain(|f| !f.path.starts_with(&prefix));
            continue;
        }
        if let Some(name) = base.strip_prefix(".wh.") {
            let dir = dirname(&e.name);
            let mut target = String::from(dir);
            if !target.ends_with('/') {
                target.push('/');
            }
            target.push_str(name);
            let mut sub = target.clone();
            sub.push('/');
            files.retain(|f| f.path != target && !f.path.starts_with(&sub));
            continue;
        }
        if !e.is_file() && !e.is_dir() {
            continue; // symlinks/devices/etc. are out of scope
        }
        let file = OciFile {
            path: e.name.clone(),
            data: e.data.to_vec(),
            is_dir: e.is_dir(),
        };
        if let Some(slot) = files.iter_mut().find(|f| f.path == file.path) {
            *slot = file;
        } else {
            files.push(file);
        }
    }
}

/// Unpack a **registry** image (OCI / Docker manifest v2): the manifest names a
/// config blob and gzipped layer blobs by `sha256:` digest, fetched via
/// `get_blob`. Each blob is **digest-verified** before use; layers are
/// gunzipped, then applied like docker-save layers. This is the assembly the
/// registry puller drives after fetching the manifest + blobs over HTTP.
pub fn unpack_registry(
    manifest_json: &[u8],
    get_blob: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> Result<Image, OciError> {
    let manifest = json::parse(manifest_json).ok_or(OciError::BadJson)?;
    let config_digest = manifest
        .get("config")
        .and_then(|c| c.get("digest"))
        .and_then(|v| v.as_str())
        .ok_or(OciError::BadManifest)?;
    let layer_digests: Vec<String> = manifest
        .get("layers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("digest").and_then(|d| d.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if layer_digests.is_empty() {
        return Err(OciError::BadManifest);
    }

    // Config blob: fetch, verify, parse.
    let config_blob = get_blob(config_digest).ok_or(OciError::MissingBlob)?;
    if !sha256::verify(&config_blob, config_digest) {
        return Err(OciError::DigestMismatch);
    }
    let cfg = json::parse(&config_blob).ok_or(OciError::BadJson)?;
    let config = parse_image_config(&cfg);

    // Layers: fetch, verify, gunzip, apply in order.
    let mut files: Vec<OciFile> = Vec::new();
    for digest in &layer_digests {
        let blob = get_blob(digest).ok_or(OciError::MissingBlob)?;
        if !sha256::verify(&blob, digest) {
            return Err(OciError::DigestMismatch);
        }
        let tar_bytes = gzip::decompress(&blob).ok_or(OciError::BadGzip)?;
        let layer = tar::read_all(&tar_bytes).ok_or(OciError::BadTar)?;
        apply_layer(&mut files, &layer);
    }

    Ok(Image { files, config })
}
