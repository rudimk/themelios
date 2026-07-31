//! # Minimal JSON parser (Phase 5.4)
//!
//! A small, dependency-light (`alloc`-only) recursive-descent JSON parser — just
//! enough to read a `docker save` bundle's `manifest.json` and image `config`
//! (strings, string arrays, objects, numbers, bools, null). No `serde`, no kernel
//! dependencies, so it lifts into the ring-3 `oci-server` unchanged (5.5).
//!
//! It is deliberately permissive about numbers (stored as `f64`) — the OCI
//! metadata we consume is strings and arrays of strings.

use alloc::string::String;
use alloc::vec::Vec;

extern crate alloc;

/// A parsed JSON value.
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Array(Vec<Value>),
    /// Object as ordered key/value pairs (insertion order; small objects).
    Object(Vec<(String, Value)>),
}

impl Value {
    /// If this is a string, its contents.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    /// If this is an array, its elements.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(v) => Some(v),
            _ => None,
        }
    }
    /// Look up a key on an object value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    /// Collect an array of strings (skipping non-string elements).
    pub fn as_string_vec(&self) -> Vec<String> {
        match self {
            Value::Array(v) => v
                .iter()
                .filter_map(|e| e.as_str().map(String::from))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Serialize this value to a compact JSON byte string (Phase 6.0). Object keys
    /// keep insertion order (the reader preserves it), so `parse` → `to_bytes`
    /// round-trips a compact document byte-for-byte. Used to build Docker Engine
    /// API responses in the ring-3 `api-server`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.write(&mut out);
        out
    }

    /// Append this value's JSON encoding to `out`.
    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.extend_from_slice(b"null"),
            Value::Bool(true) => out.extend_from_slice(b"true"),
            Value::Bool(false) => out.extend_from_slice(b"false"),
            Value::Num(n) => write_number(*n, out),
            Value::Str(s) => write_json_string(s, out),
            Value::Array(items) => {
                out.push(b'[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    item.write(out);
                }
                out.push(b']');
            }
            Value::Object(pairs) => {
                out.push(b'{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    write_json_string(k, out);
                    out.push(b':');
                    v.write(out);
                }
                out.push(b'}');
            }
        }
    }
}

/// Write a JSON string literal with the mandatory escapes (RFC 8259 §7): `"` and
/// `\`, the short escapes for the common controls, and `\u00XX` for any other
/// control character. Non-ASCII is emitted as raw UTF-8 (valid JSON).
fn write_json_string(s: &str, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            '\u{8}' => out.extend_from_slice(b"\\b"),
            '\u{c}' => out.extend_from_slice(b"\\f"),
            c if (c as u32) < 0x20 => {
                let b = c as u32;
                out.extend_from_slice(b"\\u00");
                out.push(HEX[((b >> 4) & 0xf) as usize]);
                out.push(HEX[(b & 0xf) as usize]);
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

/// Write a number. Integer-valued finite values print without a decimal point (the
/// common case — ports, counts, ids, timestamps); other finite values use the
/// default `f64` formatting; non-finite values (which JSON cannot represent) are
/// emitted as `null`.
fn write_number(n: f64, out: &mut Vec<u8>) {
    use alloc::string::ToString;
    if !n.is_finite() {
        out.extend_from_slice(b"null");
    } else if n == (n as i64) as f64 {
        out.extend_from_slice((n as i64).to_string().as_bytes());
    } else {
        out.extend_from_slice(n.to_string().as_bytes());
    }
}

/// A cursor over the input bytes.
struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

/// Maximum container-nesting depth. The manifest/config JSON is **untrusted**
/// registry input, and this is a recursive-descent parser: a document like
/// `[[[[…` nests one `array()`/`value()` stack frame per bracket, so without a
/// bound a few thousand `[` bytes overflow the kernel stack (guard-page fault →
/// kernel death). 64 levels is far beyond any real OCI manifest/config.
const MAX_DEPTH: usize = 64;

/// Parse a complete JSON document. Returns `None` on any syntax error (including
/// nesting past [`MAX_DEPTH`]).
pub fn parse(input: &[u8]) -> Option<Value> {
    let mut p = Parser { b: input, i: 0 };
    p.skip_ws();
    let v = p.value(0)?;
    Some(v)
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }
    fn expect(&mut self, c: u8) -> Option<()> {
        if self.bump()? == c {
            Some(())
        } else {
            None
        }
    }

    fn value(&mut self, depth: usize) -> Option<Value> {
        self.skip_ws();
        match self.peek()? {
            b'"' => self.string().map(Value::Str),
            b'{' => self.object(depth),
            b'[' => self.array(depth),
            b't' | b'f' => self.boolean(),
            b'n' => self.null(),
            _ => self.number(),
        }
    }

    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut s = String::new();
        loop {
            let c = self.bump()?;
            match c {
                b'"' => return Some(s),
                b'\\' => {
                    let e = self.bump()?;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'u' => {
                            // Parse \uXXXX as a BMP code point (no surrogate pairs).
                            let mut cp: u32 = 0;
                            for _ in 0..4 {
                                let h = self.bump()?;
                                let d = match h {
                                    b'0'..=b'9' => (h - b'0') as u32,
                                    b'a'..=b'f' => (h - b'a' + 10) as u32,
                                    b'A'..=b'F' => (h - b'A' + 10) as u32,
                                    _ => return None,
                                };
                                cp = cp * 16 + d;
                            }
                            s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Pass raw bytes through; UTF-8 continuation bytes accumulate
                    // into a valid char via String's byte handling.
                    // SAFETY-free path: push the byte as a char if ASCII, else
                    // buffer multi-byte UTF-8 directly.
                    if c < 0x80 {
                        s.push(c as char);
                    } else {
                        // Collect a UTF-8 multibyte sequence starting at c.
                        let extra = if c >= 0xF0 {
                            3
                        } else if c >= 0xE0 {
                            2
                        } else {
                            1
                        };
                        let mut buf = alloc::vec![c];
                        for _ in 0..extra {
                            buf.push(self.bump()?);
                        }
                        if let Ok(st) = core::str::from_utf8(&buf) {
                            s.push_str(st);
                        }
                    }
                }
            }
        }
    }

    fn object(&mut self, depth: usize) -> Option<Value> {
        // Guard container nesting before recursing (untrusted-input stack safety).
        if depth >= MAX_DEPTH {
            return None;
        }
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b'}' => return Some(Value::Object(pairs)),
                _ => return None,
            }
        }
    }

    fn array(&mut self, depth: usize) -> Option<Value> {
        // Guard container nesting before recursing (untrusted-input stack safety).
        if depth >= MAX_DEPTH {
            return None;
        }
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek()? == b']' {
            self.i += 1;
            return Some(Value::Array(items));
        }
        loop {
            let val = self.value(depth + 1)?;
            items.push(val);
            self.skip_ws();
            match self.bump()? {
                b',' => continue,
                b']' => return Some(Value::Array(items)),
                _ => return None,
            }
        }
    }

    fn boolean(&mut self) -> Option<Value> {
        if self.b[self.i..].starts_with(b"true") {
            self.i += 4;
            Some(Value::Bool(true))
        } else if self.b[self.i..].starts_with(b"false") {
            self.i += 5;
            Some(Value::Bool(false))
        } else {
            None
        }
    }

    fn null(&mut self) -> Option<Value> {
        if self.b[self.i..].starts_with(b"null") {
            self.i += 4;
            Some(Value::Null)
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<Value> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' || c == b'e' || c == b'E' {
                self.i += 1;
            } else {
                break;
            }
        }
        // If nothing was consumed the input isn't a value here — fail instead of
        // returning without advancing (which would spin the array/object loop).
        if self.i == start {
            return None;
        }
        let s = core::str::from_utf8(&self.b[start..self.i]).ok()?;
        // Integer-only parse is enough for our needs; fall back to 0.0.
        let n = s.parse::<f64>().unwrap_or(0.0);
        Some(Value::Num(n))
    }
}
