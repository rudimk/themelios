//! # Build script for the ThemeliOS kernel
//!
//! This script runs on the host at compile time (not on bare metal).
//! It generates a ULID (Universally Unique Lexicographically Sortable
//! Identifier) for each build and passes it to the kernel source code
//! via the `BUILD_ULID` environment variable, accessible with `env!("BUILD_ULID")`.
//!
//! ## What is a ULID?
//!
//! A ULID is a 26-character identifier that encodes:
//! - A 48-bit millisecond timestamp (first 10 characters)
//! - 80 bits of randomness (last 16 characters)
//!
//! Both parts are encoded in Crockford Base32 (0-9, A-Z minus I, L, O, U).
//! Because the timestamp comes first, ULIDs sort chronologically as strings.
//!
//! Example: `01J5B3GK7H9QR4XZVM0CWEJN3P`
//!          |----------|----------------|
//!           timestamp    randomness

use std::fs::File;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Crockford Base32 alphabet — 32 characters, excluding I, L, O, U to
/// avoid visual ambiguity (e.g., I vs 1, O vs 0).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Encode a 48-bit timestamp as 10 Crockford Base32 characters.
///
/// Each character encodes 5 bits. 10 characters × 5 bits = 50 bits,
/// which is enough for a 48-bit millisecond timestamp (good until year 10889).
fn encode_timestamp(mut ms: u64) -> String {
    let mut chars = [b'0'; 10];

    // Encode from least significant to most significant digit
    for i in (0..10).rev() {
        chars[i] = CROCKFORD[(ms & 0x1F) as usize];
        ms >>= 5;
    }

    String::from_utf8(chars.to_vec()).unwrap()
}

/// Encode 80 bits of randomness (10 bytes) as 16 Crockford Base32 characters.
///
/// 16 characters × 5 bits = 80 bits, which exactly fits 10 random bytes.
fn encode_random(bytes: &[u8; 10]) -> String {
    // Pack the 10 bytes into a u128 for easier bit manipulation.
    // Only the lower 80 bits are used.
    let mut value: u128 = 0;
    for &b in bytes.iter() {
        value = (value << 8) | b as u128;
    }

    let mut chars = [b'0'; 16];

    // Encode from least significant to most significant digit
    for i in (0..16).rev() {
        chars[i] = CROCKFORD[(value & 0x1F) as usize];
        value >>= 5;
    }

    String::from_utf8(chars.to_vec()).unwrap()
}

fn main() {
    // Get the current timestamp in milliseconds since Unix epoch
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System clock is before Unix epoch")
        .as_millis() as u64;

    // Read 10 random bytes (80 bits) from the OS random source
    let mut random_bytes = [0u8; 10];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut random_bytes))
        .expect("Failed to read from /dev/urandom");

    // Compose the ULID: 10 chars timestamp + 16 chars random = 26 chars
    let ulid = format!("{}{}", encode_timestamp(timestamp), encode_random(&random_bytes));

    // Pass the ULID to the kernel source via an environment variable.
    // In kernel code, use env!("BUILD_ULID") to access it at compile time.
    println!("cargo:rustc-env=BUILD_ULID={ulid}");

    // Write the ULID to a well-known file in the workspace target directory.
    // CI workflows and tooling read this to tag releases with the build identifier.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let ulid_path = std::path::Path::new(&manifest_dir).join("../target/build-ulid");
    let _ = std::fs::write(&ulid_path, &ulid);
}
