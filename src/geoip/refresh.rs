// Project:   factbook
// File:      src/geoip/refresh.rs
// Purpose:   Pick up a replaced database without restarting the process
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Noticing that a database has been replaced.
//!
//! The provisioning half rewrites a database when it goes stale, and a process
//! holding a memory map of the old file keeps answering from it until something
//! reopens it. [`refresh_if_changed`](GeoIp::refresh_if_changed) is that
//! something: one `stat` per database, a reopen for whichever moved, and a
//! lock-free swap of the reader set.
//!
//! # Caller-driven
//!
//! Nothing here starts a timer or a task. A library that spawns its own
//! background thread imposes that thread on every consumer, so the schedule is
//! the consumer's -- a tick on its own runtime, a periodic job, a signal.
//!
//! # Off the lookup path
//!
//! The check is never made during a lookup. A cache hit is measured in tens of
//! nanoseconds and a `stat` is a system call, so folding the two together would
//! cost more than the cache saves.
//!
//! # Example
//!
//! ```rust,no_run
//! # use factbook::geoip::GeoIp;
//! # fn run(geoip: &GeoIp) -> Result<(), Box<dyn std::error::Error>> {
//! if geoip.refresh_if_changed()? {
//!     println!("reopened the databases");
//! }
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use maxminddb::{Mmap, Reader};

use super::enricher::{GeoIp, GeoIpLookupError, Readers, Source, file_mtime, open_reader};

impl GeoIp {
    /// Reopen any database whose file has changed since it was opened.
    ///
    /// Reports whether anything was swapped. A database that has not moved is
    /// left alone, so the common call costs one `stat` per database and nothing
    /// else.
    ///
    /// A swap clears the cache, because an answer produced from the old file is
    /// the only thing a new file can make wrong.
    ///
    /// # Errors
    ///
    /// [`GeoIpLookupError::Open`] when a replaced file will not open as a
    /// MaxMind DB. The reader it would have replaced stays in place and keeps
    /// answering, and the next call tries again.
    pub fn refresh_if_changed(&self) -> Result<bool, GeoIpLookupError> {
        let mut sources = match self.inner.sources.lock() {
            Ok(sources) => sources,
            // The guarded state is a path and a timestamp, neither of which a
            // panic elsewhere can have left inconsistent.
            Err(poisoned) => poisoned.into_inner(),
        };

        let current = self.inner.readers.load_full();
        let mut next = Readers {
            city: current.city.clone(),
            asn: current.asn.clone(),
        };
        let mut changed = false;
        let mut failure = None;

        match sources.city.as_mut().map(reopen) {
            Some(Ok(Some(reader))) => {
                next.city = Some(reader);
                changed = true;
            }
            Some(Err(e)) => failure = Some(e),
            Some(Ok(None)) | None => {}
        }

        match sources.asn.as_mut().map(reopen) {
            Some(Ok(Some(reader))) => {
                next.asn = Some(reader);
                changed = true;
            }
            Some(Err(e)) => failure = failure.or(Some(e)),
            Some(Ok(None)) | None => {}
        }

        // Whatever did reopen is installed before the failure is reported, so
        // one unreadable file does not hold back the database beside it.
        if changed {
            self.inner.swap_readers(next);
        }

        failure.map_or(Ok(changed), Err)
    }
}

/// Reopen one database when its file has moved, updating the recorded time.
///
/// `Ok(None)` means the file is where it was, which is the answer nearly every
/// call gets.
fn reopen(source: &mut Source) -> Result<Option<Arc<Reader<Mmap>>>, GeoIpLookupError> {
    let mtime = file_mtime(&source.path);

    // A file that cannot be stat'ed keeps the reader already mapped: a check
    // landing mid-replacement must not take enrichment down with it.
    if mtime.is_none() || mtime == source.mtime {
        return Ok(None);
    }

    let reader = open_reader(&source.path)?;
    // Recorded only once the reopen succeeded, so a bad file is retried.
    source.mtime = mtime;

    Ok(Some(Arc::new(reader)))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, SystemTime};

    use super::*;
    use crate::geoip::enricher::{CacheConfig, DatabasePaths};

    /// A database in MaxMind's City schema, built by scripts/make_fixtures.py.
    const CITY_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/city-test.mmdb");

    /// A database in MaxMind's ASN schema, built by scripts/make_fixtures.py.
    const ASN_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/asn-test.mmdb");

    /// An address the city database holds and the ASN database does not.
    const BOXFORD: &str = "2.125.160.216";

    /// An address both databases hold.
    const LINKOPING: &str = "89.160.20.112";

    /// A fixed point the test timestamps are offset from.
    const BASE: Duration = Duration::from_secs(1_000_000);

    /// Parse a literal the tests are asserting about.
    fn ip(literal: &str) -> IpAddr {
        literal.parse().unwrap()
    }

    /// A timestamp `offset` seconds after the fixed base.
    fn at(offset: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + BASE + Duration::from_secs(offset)
    }

    /// Put `contents` at `path` the way the provisioning half does: write a
    /// sibling file, then rename it over the destination.
    ///
    /// Writing in place would truncate a file that is currently mapped, and a
    /// read of the mapped pages past the new end of file is a fault rather than
    /// an error. The rename replaces the directory entry and leaves any
    /// existing map addressing the old inode.
    fn replace(path: &Path, contents: &str, mtime: SystemTime) {
        let staged = path.with_extension("staged");
        fs::copy(contents, &staged).unwrap();
        File::options()
            .write(true)
            .open(&staged)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&staged, path).unwrap();
    }

    /// Overwrite `path` with bytes that are not a database at all.
    fn corrupt(path: &Path, mtime: SystemTime) {
        let staged = path.with_extension("staged");
        fs::write(&staged, b"not a MaxMind DB").unwrap();
        File::options()
            .write(true)
            .open(&staged)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        fs::rename(&staged, path).unwrap();
    }

    /// The city name an address resolves to, owned so the record can be dropped.
    fn city_of(geoip: &GeoIp, literal: &str) -> Option<String> {
        geoip
            .lookup(ip(literal))
            .and_then(|record| record.city_name.as_deref().map(str::to_owned))
    }

    #[test]
    fn an_unchanged_database_is_not_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.mmdb");
        replace(&path, CITY_DB, at(0));

        let geoip = GeoIp::open(DatabasePaths::city_only(&path), CacheConfig::default()).unwrap();
        assert!(geoip.lookup(ip(BOXFORD)).is_some());
        assert_eq!(geoip.cached_entries(), 1);

        assert!(!geoip.refresh_if_changed().unwrap());
        // Nothing was swapped, so nothing was cleared.
        assert_eq!(geoip.cached_entries(), 1);
    }

    #[test]
    fn a_replaced_database_is_reopened_and_the_cache_is_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.mmdb");
        replace(&path, CITY_DB, at(0));

        let geoip = GeoIp::open(DatabasePaths::city_only(&path), CacheConfig::default()).unwrap();
        assert_eq!(city_of(&geoip, BOXFORD).as_deref(), Some("Boxford"));
        assert_eq!(geoip.cached_entries(), 1);

        // A different database behind the same path.
        replace(&path, ASN_DB, at(60));

        assert!(geoip.refresh_if_changed().unwrap());
        // The swap dropped every answer the old file produced, which is what
        // makes a time limit on the cache unnecessary.
        assert_eq!(geoip.cached_entries(), 0);
        // The ASN database holds no record for this address, so the reader
        // behind the path really did change.
        assert!(geoip.lookup(ip(BOXFORD)).is_none());
    }

    #[test]
    fn a_second_refresh_after_a_swap_reports_nothing_further() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.mmdb");
        replace(&path, CITY_DB, at(0));

        let geoip = GeoIp::open(DatabasePaths::city_only(&path), CacheConfig::default()).unwrap();
        replace(&path, CITY_DB, at(60));

        assert!(geoip.refresh_if_changed().unwrap());
        assert!(!geoip.refresh_if_changed().unwrap());
    }

    #[test]
    fn refreshing_one_database_leaves_the_other_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let city = dir.path().join("city.mmdb");
        let asn = dir.path().join("asn.mmdb");
        replace(&city, CITY_DB, at(0));
        replace(&asn, ASN_DB, at(0));

        let geoip = GeoIp::open(
            DatabasePaths {
                city: Some(&city),
                asn: Some(&asn),
            },
            CacheConfig::default(),
        )
        .unwrap();
        let before = geoip.lookup(ip(LINKOPING)).unwrap();
        assert!(before.city_name.is_some());
        assert!(before.autonomous_system_number.is_some());

        replace(&city, CITY_DB, at(60));
        assert!(geoip.refresh_if_changed().unwrap());

        let after = geoip.lookup(ip(LINKOPING)).unwrap();
        // The ASN reader was carried across the swap rather than reopened, and
        // still answers.
        assert_eq!(
            after.autonomous_system_number,
            before.autonomous_system_number
        );
        assert_eq!(after.city_name, before.city_name);
        // A fresh allocation, because the swap cleared the cache.
        assert!(!Arc::ptr_eq(&before, &after));
    }

    #[test]
    fn a_replacement_that_will_not_open_is_reported_and_the_old_reader_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.mmdb");
        replace(&path, CITY_DB, at(0));

        let geoip = GeoIp::open(DatabasePaths::city_only(&path), CacheConfig::default()).unwrap();
        corrupt(&path, at(60));

        assert!(matches!(
            geoip.refresh_if_changed(),
            Err(GeoIpLookupError::Open { .. })
        ));
        // The mapped reader was never replaced, so enrichment carries on.
        assert_eq!(city_of(&geoip, BOXFORD).as_deref(), Some("Boxford"));
        // The time was not recorded either, so the next call tries again.
        assert!(matches!(
            geoip.refresh_if_changed(),
            Err(GeoIpLookupError::Open { .. })
        ));
    }

    #[test]
    fn a_vanished_file_keeps_the_mapped_reader_answering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.mmdb");
        replace(&path, CITY_DB, at(0));

        let geoip = GeoIp::open(DatabasePaths::city_only(&path), CacheConfig::default()).unwrap();
        fs::remove_file(&path).unwrap();

        assert!(!geoip.refresh_if_changed().unwrap());
        // The lookup path performs no filesystem access, and the map outlives
        // the directory entry it was made through.
        assert_eq!(city_of(&geoip, BOXFORD).as_deref(), Some("Boxford"));
    }

    #[test]
    fn a_lookup_racing_a_refresh_never_files_an_answer_from_the_old_database() {
        // Reversed, a lookup taking its placeholder after the clear has passed
        // that shard reads the old file and files an answer nothing removes.
        const PROBES: u8 = 255;
        const FILL: u32 = 20_000;
        const ROUNDS: u64 = 4;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.mmdb");
        replace(&path, ASN_DB, at(0));

        let geoip = GeoIp::open(DatabasePaths::asn_only(&path), CacheConfig::default()).unwrap();
        // The allocation the ASN database answers and the city database holds
        // no record in, so one file resolves it and the other does not.
        let probes: Vec<IpAddr> = (0..PROBES)
            .map(|host| IpAddr::V4(Ipv4Addr::new(1, 0, 0, host)))
            .collect();

        for round in 1..=ROUNDS {
            // Alternate the file, so both directions of the swap are raced.
            let (source, resolves) = if round % 2 == 1 {
                (CITY_DB, false)
            } else {
                (ASN_DB, true)
            };

            // A full cache makes the clear long enough for a racing lookup to
            // land inside it. 11.0.0.0/8 is routable, so none of it
            // short-circuits ahead of the cache.
            for filler in 0..FILL {
                let _ = geoip.lookup(IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + filler)));
            }

            let stop = Arc::new(AtomicBool::new(false));
            let hammer = {
                let geoip = geoip.clone();
                let probes = probes.clone();
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        for &probe in &probes {
                            let _ = geoip.lookup(probe);
                        }
                    }
                })
            };

            replace(&path, source, at(60 * round));
            assert!(geoip.refresh_if_changed().unwrap());

            stop.store(true, Ordering::Relaxed);
            hammer.join().unwrap();

            for &probe in &probes {
                assert_eq!(
                    geoip.lookup(probe).is_some(),
                    resolves,
                    "{probe} after round {round}"
                );
            }
        }
    }

    #[test]
    fn an_enricher_over_no_database_has_nothing_to_refresh() {
        let geoip = GeoIp::open(DatabasePaths::default(), CacheConfig::default()).unwrap();

        assert!(!geoip.refresh_if_changed().unwrap());
    }
}
