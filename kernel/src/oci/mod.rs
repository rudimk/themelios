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

pub mod json;
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

    // 4. Apply each layer in order, resolving whiteouts, into a flat file set.
    let mut files: Vec<OciFile> = Vec::new();
    for layer_name in &layers {
        let layer_bytes = find(&entries, layer_name).ok_or(OciError::NoLayer)?;
        let layer = tar::read_all(layer_bytes).ok_or(OciError::BadTar)?;
        for e in &layer {
            let base = basename(&e.name);
            if base == ".wh..wh..opq" {
                // Opaque dir: drop all existing entries under this directory.
                let dir = dirname(&e.name);
                let prefix = {
                    let mut p = String::from(dir);
                    if !p.ends_with('/') {
                        p.push('/');
                    }
                    p
                };
                files.retain(|f| !f.path.starts_with(&prefix));
                continue;
            }
            if let Some(name) = base.strip_prefix(".wh.") {
                // Whiteout: remove `<dir>/<name>` (and anything beneath it).
                let dir = dirname(&e.name);
                let mut target = String::from(dir);
                if !target.ends_with('/') {
                    target.push('/');
                }
                target.push_str(name);
                files.retain(|f| f.path != target && !f.path.starts_with(&{
                    let mut p = target.clone();
                    p.push('/');
                    p
                }));
                continue;
            }
            if !e.is_file() && !e.is_dir() {
                continue; // skip symlinks/devices/etc. in 5.4
            }
            // Insert or replace (later layer wins).
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

    Ok(Image {
        files,
        config: ImageConfig { entrypoint, cmd, env, cwd },
    })
}
