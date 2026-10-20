//! `file://` URI ↔ path conversion, replacing `tower_lsp_server::UriExt` (a
//! heph addition; upstream depended on `tower-lsp-server` for these two calls).
//!
//! Paths are percent-encoded byte by byte: everything but the RFC 3986
//! unreserved set and `/` is escaped, so spaces and non-ASCII names survive the
//! round trip in both directions.

use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr as _;

use lsp_types::Uri;

/// The `file://` URI for an absolute path, or `None` for a relative one.
pub fn file_path_to_uri(path: &Path) -> Option<Uri> {
    if !path.is_absolute() {
        return None;
    }
    let raw = path.to_str()?;
    let mut out = String::with_capacity(raw.len() + 7);
    out.push_str("file://");
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(hex(b >> 4));
                out.push(hex(b & 0xf));
            }
        }
    }
    Uri::from_str(&out).ok()
}

/// The path a `file://` URI names, or `None` for another scheme, a remote
/// authority, or a path that does not decode to UTF-8.
pub fn uri_to_file_path(uri: &Uri) -> Option<PathBuf> {
    if uri.scheme().map(|s| s.as_str()) != Some("file") {
        return None;
    }
    if let Some(authority) = uri.authority() {
        let host = authority.host().as_str();
        if !host.is_empty() && host != "localhost" {
            return None;
        }
    }
    let decoded = percent_decode(uri.path().as_str())?;
    Some(PathBuf::from(decoded))
}

fn hex(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16)
        .map(|c| c.to_ascii_uppercase())
        .unwrap_or('0')
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = (*bytes.get(i + 1)? as char).to_digit(16)?;
            let lo = (*bytes.get(i + 2)? as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_and_non_ascii_round_trip() {
        let path = Path::new("/tmp/a dir/café/BUILD");
        let uri = file_path_to_uri(path).unwrap();
        assert_eq!(uri.as_str(), "file:///tmp/a%20dir/caf%C3%A9/BUILD");
        assert_eq!(uri_to_file_path(&uri).unwrap(), path);
    }

    #[test]
    fn relative_paths_and_other_schemes_are_refused() {
        assert!(file_path_to_uri(Path::new("rel/BUILD")).is_none());
        let uri = Uri::from_str("starlark:/x.star").unwrap();
        assert!(uri_to_file_path(&uri).is_none());
    }
}
