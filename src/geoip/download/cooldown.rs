// Project:   factbook
// File:      src/geoip/download/cooldown.rs
// Purpose:   How long to leave a provider alone after it refused us
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Next-attempt stamps.
//!
//! A refused or rate-limited download leaves the copy on disk stale, so the next
//! freshness check asks again, and the one after that. Providers cap the
//! downloads per account and ban a client that ignores `Retry-After`, so the
//! refusal is written beside the lock file as the earliest time to try again.
//! The stamp is in the data directory rather than in memory, so a restart does
//! not forget it.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info};

use super::GeoIpDownloadError;

/// Extension of the stamp file, beside the lock.
const STAMP_EXT: &str = "retry";

/// Wait imposed when a refusal comes with no instruction of its own.
pub(super) const FLOOR_SECS: u64 = 60 * 60;

/// Longest wait any provider can impose, whatever it asks for.
pub(super) const CEILING_SECS: u64 = 24 * 60 * 60;

/// Stamps already announced, so a waiting database logs once, not per call.
static ANNOUNCED: Mutex<Option<HashSet<(PathBuf, u64)>>> = Mutex::new(None);

/// The stamp file for a database.
pub(super) fn stamp_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".");
    name.push(STAMP_EXT);
    PathBuf::from(name)
}

/// Seconds since the epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Time left before `dest` may be fetched again, when it may not be yet.
///
/// A stamp that cannot be read, or that lies further ahead than any provider can
/// ask for, is bounded or ignored rather than trusted: a clock that has gone
/// backwards must not lock a database out.
pub(super) fn wait_for(dest: &Path, kind: &'static str) -> Option<Duration> {
    let stamp = stamp_path(dest);
    let until: u64 = fs::read_to_string(&stamp).ok()?.trim().parse().ok()?;
    let now = now_secs();
    let until = until.min(now + CEILING_SECS);
    let remaining = until.checked_sub(now).filter(|left| *left > 0)?;

    let first = ANNOUNCED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .insert((stamp, until));
    if first {
        info!(
            kind,
            dest = %dest.display(),
            wait_secs = remaining,
            "GeoIP provider asked us to wait, serving the copy on disk until then"
        );
    }

    Some(Duration::from_secs(remaining))
}

/// Start a cool-down when `error` is a refusal or a rate limit.
///
/// A rate limit waits as long as the provider asked, a refusal waits the floor,
/// and anything else -- the network, another process, a local fault -- starts
/// nothing, because a later attempt can succeed.
pub(super) fn begin(dest: &Path, error: &GeoIpDownloadError) {
    let wait = match error {
        GeoIpDownloadError::RateLimited {
            retry_after_secs, ..
        } => retry_after_secs.unwrap_or(FLOOR_SECS),
        other if other.outcome() == "refused" => FLOOR_SECS,
        _ => return,
    };

    let until = now_secs() + wait.min(CEILING_SECS);
    if let Err(e) = fs::write(stamp_path(dest), until.to_string()) {
        debug!(dest = %dest.display(), error = %e, "could not record the cool-down");
    }
}

/// Forget any cool-down after a download that passed.
pub(super) fn clear(dest: &Path) {
    let _ = fs::remove_file(stamp_path(dest));
}
