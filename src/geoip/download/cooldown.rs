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
//!
//! A permanent failure -- a rejected credential, a missing entitlement, a 4xx
//! about the request -- is held back in memory against a fingerprint of the
//! configuration it failed under, until that changes or a day passes. Memory,
//! because the fingerprint covers the credential and a restart is when a
//! corrected one usually arrives.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use super::GeoIpDownloadError;

/// Extension of the stamp file, beside the lock.
const STAMP_EXT: &str = "retry";

/// Wait imposed when a refusal comes with no instruction of its own.
pub(super) const FLOOR_SECS: u64 = 60 * 60;

/// Longest wait any provider can impose, whatever it asks for.
pub(super) const CEILING_SECS: u64 = 24 * 60 * 60;

/// Stamps already announced, so a waiting database logs once, not per call.
static ANNOUNCED: Mutex<Option<HashSet<(PathBuf, u64)>>> = Mutex::new(None);

/// Permanent failures held back, by the file they were for.
static SUPPRESSED: Mutex<Option<HashMap<PathBuf, Suppressed>>> = Mutex::new(None);

/// One permanent failure, and the configuration it failed under.
struct Suppressed {
    /// Fingerprint of the configuration that failed.
    fingerprint: u64,

    /// Seconds since the epoch at which the request may be sent again anyway.
    until: u64,

    /// What the failure was, for the line that says why nothing is asked.
    reason: String,

    /// Whether that line has been logged.
    announced: bool,
}

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

/// Time left before `dest` may be fetched again under the configuration
/// `fingerprint` names, when it may not be yet.
///
/// A stamp that cannot be read, or that lies further ahead than any provider can
/// ask for, is bounded or ignored rather than trusted: a clock that has gone
/// backwards must not lock a database out.
pub(super) fn wait_for(dest: &Path, kind: &'static str, fingerprint: u64) -> Option<Duration> {
    if let Some(wait) = suppressed(dest, kind, fingerprint) {
        return Some(wait);
    }

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
            "the provider asked us to wait, serving the copy on disk until then"
        );
    }

    Some(Duration::from_secs(remaining))
}

/// Time left on a permanent failure of `dest` under the same configuration.
///
/// A suppression made under another configuration is dropped: the request has
/// changed, so the answer may have too.
fn suppressed(dest: &Path, kind: &'static str, fingerprint: u64) -> Option<Duration> {
    let mut held = SUPPRESSED.lock().unwrap_or_else(PoisonError::into_inner);
    let held = held.get_or_insert_default();
    let entry = held.get_mut(dest)?;

    let remaining = entry.until.saturating_sub(now_secs());
    if entry.fingerprint != fingerprint || remaining == 0 {
        held.remove(dest);
        return None;
    }

    if !std::mem::replace(&mut entry.announced, true) {
        warn!(
            kind,
            dest = %dest.display(),
            reason = %entry.reason,
            wait_secs = remaining,
            "not asking again until the configuration changes, serving the copy on disk"
        );
    }
    Some(Duration::from_secs(remaining))
}

/// Start a cool-down when `error` is a refusal, a rate limit or a permanent
/// failure of the configuration `fingerprint` names.
///
/// A rate limit waits as long as the provider asked, a refusal waits the floor,
/// and a permanent failure is held back for the ceiling unless the
/// configuration changes first. Anything else -- the network, another process,
/// a local fault -- starts nothing, because a later attempt can succeed.
pub(super) fn begin(dest: &Path, error: &GeoIpDownloadError, fingerprint: u64) {
    if error.is_permanent() {
        SUPPRESSED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_default()
            .insert(
                dest.to_path_buf(),
                Suppressed {
                    fingerprint,
                    until: now_secs() + CEILING_SECS,
                    reason: error.to_string(),
                    announced: false,
                },
            );
        return;
    }

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
    if let Some(held) = SUPPRESSED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_mut()
    {
        held.remove(dest);
    }
}
