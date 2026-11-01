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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::{self, ThreadId};

/// Destinations whose next materialise step waits for the test holding it.
static HELD: Mutex<Vec<(PathBuf, Hold)>> = Mutex::new(Vec::new());

/// The thread each piece of blocking file work ran on, by the file it touched.
static RAN_ON: Mutex<Vec<(PathBuf, ThreadId)>> = Mutex::new(Vec::new());

/// Record the calling thread against `path`.
pub(crate) fn ran_here(path: &Path) {
    RAN_ON
        .lock()
        .unwrap()
        .push((path.to_path_buf(), thread::current().id()));
}

/// Every thread blocking work on `path` has run on.
pub(crate) fn threads_for(path: &Path) -> Vec<ThreadId> {
    RAN_ON
        .lock()
        .unwrap()
        .iter()
        .filter(|(ran, _)| ran == path)
        .map(|&(_, thread)| thread)
        .collect()
}

/// The two ends of one held materialise step.
struct Hold {
    /// Fires once the step has started.
    entered: mpsc::Sender<()>,
    /// Lets the step go on.
    release: mpsc::Receiver<()>,
}

/// Hold the next materialise into `dest` on the blocking pool until the test
/// lets it go.
///
/// Returns a receiver that fires once the step has started, and the sender that
/// releases it.
pub(crate) fn hold_materialise(dest: &Path) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered, has_entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    HELD.lock().unwrap().push((
        dest.to_path_buf(),
        Hold {
            entered,
            release: released,
        },
    ));
    (has_entered, release)
}

/// Wait here while a test holds `dest`. Blocking, and a no-op for any
/// destination no test is holding.
pub(crate) fn materialise_point(dest: &Path) {
    let hold = {
        let mut held = HELD.lock().unwrap();
        held.iter()
            .position(|(path, _)| path == dest)
            .map(|at| held.remove(at).1)
    };
    if let Some(hold) = hold {
        let _ = hold.entered.send(());
        let _ = hold.release.recv();
    }
}

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
