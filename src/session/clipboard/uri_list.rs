//! `text/uri-list` (RFC 2483), the format Linux file managers copy files in.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

pub const MIME: &str = "text/uri-list";
/// GNOME's variant ("copy" or "cut", then one URI per line); Nautilus and Thunar read it.
pub const GNOME_MIME: &str = "x-special/gnome-copied-files";

/// Local paths in a uri-list; other URIs and comments are skipped.
pub fn parse(data: &[u8]) -> Vec<PathBuf> {
    data.split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.starts_with(b"#"))
        .filter_map(|line| line.strip_prefix(b"file://"))
        .filter_map(|rest| {
            // An empty host or "localhost" is this machine; anything else isn't a local path.
            let path = rest.strip_prefix(b"localhost").unwrap_or(rest);
            path.starts_with(b"/").then(|| decode(path))
        })
        .collect()
}

/// A `file://` URI, percent-encoding everything but unreserved characters and `/`.
pub fn encode(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

fn decode(s: &[u8]) -> PathBuf {
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let Some(hi) = s.get(i + 1).copied().and_then(hex)
            && let Some(lo) = s.get(i + 2).copied().and_then(hex)
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    PathBuf::from(OsString::from_vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_crlf_lists_with_comments() {
        let list = b"# copied\r\nfile:///home/me/a%20b.txt\r\nfile://localhost/tmp/x\r\nhttps://example.com/\r\nfile://other-host/y\r\n";
        assert_eq!(
            parse(list),
            [PathBuf::from("/home/me/a b.txt"), PathBuf::from("/tmp/x")]
        );
    }

    #[test]
    fn round_trips_awkward_names() {
        let path = Path::new("/run/user/1000/relay/1/Ñandú #1 (50%)?.txt");
        let uri = encode(path);
        assert!(uri.is_ascii());
        assert!(!uri.contains(' ') && !uri.contains('#') && !uri.contains('?'));
        assert_eq!(parse(uri.as_bytes()), [path.to_path_buf()]);
    }

    #[test]
    fn keeps_non_utf8_bytes() {
        let path = PathBuf::from(OsString::from_vec(b"/tmp/\xff\xfe".to_vec()));
        assert_eq!(parse(encode(&path).as_bytes()), [path]);
    }
}
