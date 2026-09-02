// Project:   factbook
// File:      src/geoip/download/testkit.rs
// Purpose:   Bodies the transfer tests serve, built once for all three suites
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Bodies a transfer test serves, and the digest a provider publishes beside
//! one.
//!
//! The geo suite, the table suite and the fetch module's own tests all serve
//! the same shapes -- a MaxMind DB, a gzip stream, a tar carrying one, and a
//! sha256 of any of them -- so they are built here rather than three times.

use std::io::Write;

/// Marker that opens the metadata section of a MaxMind DB file.
const MMDB_MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

/// Directory the MaxMind archives carry their database under.
const ARCHIVE_DIRECTORY: &str = "GeoLite2-City_20241231";

/// Bytes shaped like a MaxMind DB: a payload, then the metadata marker the
/// format ends with.
pub(crate) fn mmdb_body(payload: &[u8]) -> Vec<u8> {
    let mut body = payload.to_vec();
    body.extend_from_slice(MMDB_MARKER);
    body.extend_from_slice(b"binary metadata section");
    body
}

/// SHA-256 of a body as lowercase hex, the way a provider publishes it.
pub(crate) fn sha256_hex(body: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write as _;

    sha2::Sha256::digest(body)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A gzip stream wrapping `payload`.
pub(crate) fn gzip(payload: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap()
}

/// A gzip-compressed tar holding `payload` as `member`, under the dated
/// directory MaxMind ships it in.
pub(crate) fn tar_gz(member: &str, payload: &[u8]) -> Vec<u8> {
    let path = format!("{ARCHIVE_DIRECTORY}/{member}");
    tar_gz_of(&[(path.as_str(), tar::EntryType::Regular, payload)])
}

/// A gzip-compressed tar of `entries`, each a path, an entry type, and the
/// bytes a file entry carries.
pub(crate) fn tar_gz_of(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));

    for (path, kind, payload) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(*kind);
        header.set_cksum();
        builder.append_data(&mut header, path, *payload).unwrap();
    }

    let mut encoder = builder.into_inner().unwrap();
    encoder.flush().unwrap();
    encoder.finish().unwrap()
}
