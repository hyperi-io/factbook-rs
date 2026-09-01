// Project:   factbook
// File:      src/geoip/enricher.rs
// Purpose:   Cached MMDB lookups, the point of the crate
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Look an address up, once.
//!
//! [`GeoIp`] holds the open databases and a cache in front of them. Real address
//! traffic is heavily skewed -- a handful of sources account for most events --
//! so the traversal a reader crate performs per lookup is nearly all repeat
//! work, and the cache is what removes it.
//!
//! # One cache, two calling conventions
//!
//! [`lookup`](GeoIp::lookup) is synchronous and [`lookup_async`](GeoIp::lookup_async)
//! is not, and both read the same cache instance. The synchronous one is not a
//! convenience: an embedded expression interpreter cannot call an async
//! function, so without it a host would need a second cache.
//!
//! # Misses are cached
//!
//! An address the databases do not hold is stored as a negative answer. Scan
//! traffic is mostly addresses no database has ever heard of, and without the
//! negative entry every one of those packets would traverse the database again.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::net::IpAddr;
//! use std::path::Path;
//!
//! use factbook::geoip::{CacheConfig, GeoIp};
//!
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let geoip = GeoIp::open(
//!     Some(Path::new("/var/lib/geoip/GeoLite2-City.mmdb")),
//!     Some(Path::new("/var/lib/geoip/GeoLite2-ASN.mmdb")),
//!     CacheConfig::default(),
//! )?;
//!
//! if let Some(record) = geoip.lookup("89.160.20.112".parse::<IpAddr>()?) {
//!     println!("{:?} in {:?}", record.city_name, record.country_code);
//! }
//! # Ok(())
//! # }
//! ```

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use arc_swap::ArcSwap;
use compact_str::{CompactString, ToCompactString};
use maxminddb::{Mmap, Reader, geoip2};
use quick_cache::sync::{Cache, GuardResult};
use tracing::debug;

#[cfg(feature = "geoip-download")]
use super::download::{Database, DatabaseFormat, Databases};
use super::private::is_private;
use super::record::GeoIpRecord;

/// Addresses held in the cache by default.
///
/// A cached record is on the order of a hundred bytes, so this is a low tens of
/// megabytes -- small next to the database it is standing in front of, and
/// enough to hold the working set of a busy feed.
const DEFAULT_CAPACITY: usize = 100_000;

/// Errors raised while opening or refreshing the databases.
///
/// A lookup itself returns no error: an address the databases cannot answer is
/// reported as `None`, and a malformed record is logged and treated the same
/// way, because one bad record must not fail the event carrying it.
#[derive(Debug, thiserror::Error)]
pub enum GeoIpLookupError {
    /// The provisioned database is CSV, which this engine does not read.
    #[error("{} is a CSV database, a format not supported by the lookup engine", .path.display())]
    UnsupportedFormat {
        /// First file of the CSV database.
        path: PathBuf,
    },

    /// The database was provisioned with no files at all.
    #[error("the {kind} database was provisioned with no files")]
    NoFiles {
        /// Database kind: `city` or `asn`.
        kind: &'static str,
    },

    /// The file could not be opened or is not a MaxMind DB.
    #[error("could not open {}", .path.display())]
    Open {
        /// File that was being opened.
        path: PathBuf,
        /// What the reader said about it.
        #[source]
        source: maxminddb::MaxMindDbError,
    },
}

/// How the lookup cache is sized and aged.
///
/// The knobs stop here on purpose. Shard count, ghost-queue allocation, weighers,
/// eviction listeners and hashers are the cache implementation's business, and
/// exposing them would make its version a part of this crate's contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheConfig {
    /// Addresses held before eviction starts.
    pub capacity: usize,

    /// How long a cached answer stays usable.
    ///
    /// `None`, the default, is the right setting: a record only goes stale when
    /// the database file changes, and
    /// [`refresh_if_changed`](GeoIp::refresh_if_changed) clears the whole cache
    /// when it does. A time limit would evict correct answers early and still
    /// leave a staleness window, where clearing on the swap leaves none.
    pub max_age: Option<Duration>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            max_age: None,
        }
    }
}

/// The open readers, swapped as a set so a refresh is one atomic store.
pub(super) struct Readers {
    /// City database, when one was provisioned.
    pub(super) city: Option<Arc<Reader<Mmap>>>,
    /// ASN database, when one was provisioned.
    pub(super) asn: Option<Arc<Reader<Mmap>>>,
}

/// One database file and the modification time its reader was opened from.
pub(super) struct Source {
    /// File the reader maps.
    pub(super) path: PathBuf,
    /// Modification time seen when the reader was opened.
    pub(super) mtime: Option<SystemTime>,
}

/// Where the readers came from, for the freshness check.
pub(super) struct Sources {
    /// City database file.
    pub(super) city: Option<Source>,
    /// ASN database file.
    pub(super) asn: Option<Source>,
}

/// One cache entry: the answer, and when it was read.
///
/// `record` is `None` for an address the databases do not hold, which is the
/// negative answer a repeat of that address is served from.
#[derive(Clone)]
pub(super) struct Entry {
    /// When the databases were read for this answer.
    loaded: Instant,
    /// The answer, absent when neither database held the address.
    record: Option<Arc<GeoIpRecord>>,
}

/// The shared state every clone of a [`GeoIp`] points at.
pub(super) struct Inner {
    /// Readers, replaced wholesale by a refresh.
    pub(super) readers: ArcSwap<Readers>,
    /// The answers, keyed by address.
    cache: Cache<IpAddr, Entry>,
    /// Files the readers came from, locked only by a refresh.
    pub(super) sources: Mutex<Sources>,
    /// Age limit for a cached answer, off by default.
    max_age: Option<Duration>,
}

impl Inner {
    /// Install a new reader set and drop every answer the old one produced.
    ///
    /// Clearing on the swap is what removes the need for a time limit on the
    /// cache: an answer can only go stale when the file behind it changes, so
    /// this is exact and leaves no window. The store precedes the clear, so a
    /// lookup that misses during the swap reads the new file rather than
    /// repopulating from the old one.
    pub(super) fn swap_readers(&self, readers: Readers) {
        self.readers.store(Arc::new(readers));
        self.cache.clear();
        telemetry::size(self.cache.len());
    }
}

/// Cached GeoIP lookups over the provisioned databases.
///
/// Cheap to clone: every clone shares one cache and one set of readers, so a
/// service hands a clone to each worker rather than building a cache per task.
#[derive(Clone)]
pub struct GeoIp {
    /// Shared readers, cache and source paths.
    pub(super) inner: Arc<Inner>,
}

impl GeoIp {
    /// Open the databases and put a cache in front of them.
    ///
    /// Either database may be absent: a city-only deployment resolves no ASN
    /// fields, and an ASN-only one resolves no location, rather than failing.
    ///
    /// # Errors
    ///
    /// [`GeoIpLookupError::Open`] naming the file that could not be read as a
    /// MaxMind DB.
    pub fn open(
        city: Option<&Path>,
        asn: Option<&Path>,
        cache: CacheConfig,
    ) -> Result<Self, GeoIpLookupError> {
        let (city_source, city_reader) = city.map(open_source).transpose()?.unzip();
        let (asn_source, asn_reader) = asn.map(open_source).transpose()?.unzip();

        Ok(Self {
            inner: Arc::new(Inner {
                readers: ArcSwap::from_pointee(Readers {
                    city: city_reader,
                    asn: asn_reader,
                }),
                cache: Cache::new(cache.capacity),
                sources: Mutex::new(Sources {
                    city: city_source,
                    asn: asn_source,
                }),
                max_age: cache.max_age,
            }),
        })
    }

    /// Open the databases the provisioning half resolved.
    ///
    /// # Errors
    ///
    /// [`GeoIpLookupError::UnsupportedFormat`] for a CSV database,
    /// [`GeoIpLookupError::NoFiles`] for a database resolved to no files, or
    /// [`GeoIpLookupError::Open`] when a file will not open.
    #[cfg(feature = "geoip-download")]
    #[cfg_attr(docsrs, doc(cfg(feature = "geoip-download")))]
    pub fn from_databases(
        databases: &Databases,
        cache: CacheConfig,
    ) -> Result<Self, GeoIpLookupError> {
        let city = mmdb_path(databases.city.as_ref(), "city")?;
        let asn = mmdb_path(databases.asn.as_ref(), "asn")?;
        Self::open(city, asn, cache)
    }

    /// Look one address up.
    ///
    /// Returns `None` when neither database holds the address. Concurrent
    /// callers that miss on the same cold address perform one database read
    /// between them, not one each.
    #[must_use]
    pub fn lookup(&self, ip: IpAddr) -> Option<Arc<GeoIpRecord>> {
        // Reserved space is answered before the cache is touched: it cannot
        // have a geolocation, so caching the fact would only evict answers that
        // took a database read to produce.
        if is_private(ip) {
            return Some(GeoIpRecord::private_shared());
        }

        let started = telemetry::started();
        // A waiter that a coalesced load hands the value to arrives here as a
        // hit, so the miss count stays equal to the number of database reads.
        let entry = match self.inner.cache.get_value_or_guard(&ip, None) {
            GuardResult::Value(entry) if !self.expired(&entry) => {
                telemetry::hit();
                entry
            }
            // An entry past its age limit is re-read and replaced without
            // coalescing, which is why the age limit is off by default.
            GuardResult::Value(_) => {
                let entry = self.miss(ip);
                self.inner.cache.insert(ip, entry.clone());
                telemetry::size(self.inner.cache.len());
                entry
            }
            GuardResult::Guard(guard) => {
                let entry = self.miss(ip);
                // A failed insert means the placeholder was removed, leaving
                // the answer correct but uncached.
                drop(guard.insert(entry.clone()));
                telemetry::size(self.inner.cache.len());
                entry
            }
            // Unreachable with no timeout, and a direct read is still a correct
            // answer, so it is served rather than asserted about.
            GuardResult::Timeout => self.miss(ip),
        };
        telemetry::elapsed(started);

        entry.record
    }

    /// Look one address up from an async context.
    ///
    /// The same cache as [`lookup`](Self::lookup): a service that reads
    /// addresses on a runtime and evaluates expressions synchronously shares one
    /// set of answers between the two.
    pub async fn lookup_async(&self, ip: IpAddr) -> Option<Arc<GeoIpRecord>> {
        if is_private(ip) {
            return Some(GeoIpRecord::private_shared());
        }

        let started = telemetry::started();
        let entry = match self.inner.cache.get_value_or_guard_async(&ip).await {
            Ok(entry) if !self.expired(&entry) => {
                telemetry::hit();
                entry
            }
            Ok(_) => {
                let entry = self.miss(ip);
                self.inner.cache.insert(ip, entry.clone());
                telemetry::size(self.inner.cache.len());
                entry
            }
            Err(guard) => {
                let entry = self.miss(ip);
                drop(guard.insert(entry.clone()));
                telemetry::size(self.inner.cache.len());
                entry
            }
        };
        telemetry::elapsed(started);

        entry.record
    }

    /// Look a batch of addresses up, answers in the order they were given.
    ///
    /// A repeat costs a cache hit rather than a second database read, because
    /// [`Self::lookup`] caches the first one. Deduplicating the batch first was
    /// measurably slower both warm and cold: it duplicates what the cache
    /// already does and pays an allocation for it.
    #[must_use]
    pub fn lookup_many(&self, ips: &[IpAddr]) -> Vec<Option<Arc<GeoIpRecord>>> {
        ips.iter().map(|&ip| self.lookup(ip)).collect()
    }

    /// Answers currently held in the cache.
    #[must_use]
    pub fn cached_entries(&self) -> usize {
        self.inner.cache.len()
    }

    /// Drop every cached answer.
    ///
    /// A refresh does this on its own, so this is for a consumer with its own
    /// reason to start again.
    pub fn clear_cache(&self) {
        self.inner.cache.clear();
        telemetry::size(self.inner.cache.len());
    }

    /// Read one address out of the databases and stamp it for the age check.
    ///
    /// The reader set is taken by `load_full` rather than by a guard: this runs
    /// on the miss path, where one reference count is nothing against a database
    /// traversal, and a plain `Arc` is safe to hold across an await.
    fn miss(&self, ip: IpAddr) -> Entry {
        telemetry::miss();
        let readers = self.inner.readers.load_full();

        Entry {
            // Stamped even when no age limit is set: an unconditional clock read
            // on the miss path is far below the cost of the read it accompanies.
            loaded: Instant::now(),
            record: read(&readers, ip).map(Arc::new),
        }
    }

    /// Whether a cached answer has outlived the configured age limit.
    fn expired(&self, entry: &Entry) -> bool {
        self.inner
            .max_age
            .is_some_and(|max_age| entry.loaded.elapsed() > max_age)
    }
}

impl fmt::Debug for GeoIp {
    /// Reports the cache, not the databases: a reader renders as its metadata,
    /// which is several hundred bytes of language names nobody is debugging.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let readers = self.inner.readers.load();
        f.debug_struct("GeoIp")
            .field("city", &readers.city.is_some())
            .field("asn", &readers.asn.is_some())
            .field("cached_entries", &self.inner.cache.len())
            .finish_non_exhaustive()
    }
}

/// Read one address out of both databases.
///
/// Returns `None` when neither held it, which the cache stores as the negative
/// answer. Both are read even when the first answers: they are different
/// databases and the ASN one routinely holds an address the city one does not.
fn read(readers: &Readers, ip: IpAddr) -> Option<GeoIpRecord> {
    let mut record = GeoIpRecord::default();
    let mut found = false;

    if let Some(reader) = readers.city.as_deref() {
        found |= read_city(reader, ip, &mut record);
    }
    if let Some(reader) = readers.asn.as_deref() {
        found |= read_asn(reader, ip, &mut record);
    }

    found.then_some(record)
}

/// IPinfo Lite's record, which is flat where MaxMind's nests and carries the
/// network alongside the location.
#[derive(serde::Deserialize)]
struct IpInfoRecord<'a> {
    #[serde(borrow, default)]
    country_code: Option<&'a str>,
    #[serde(borrow, default)]
    country: Option<&'a str>,
    #[serde(borrow, default)]
    continent_code: Option<&'a str>,
    #[serde(borrow, default)]
    asn: Option<&'a str>,
    #[serde(borrow, default)]
    as_name: Option<&'a str>,
}

/// Whether a database is written in IPinfo's schema rather than MaxMind's.
///
/// Every database names its own schema in its metadata, so this holds for a
/// file that was pre-seeded or mounted rather than downloaded.
fn is_ipinfo(reader: &Reader<Mmap>) -> bool {
    reader.metadata().database_type.starts_with("ipinfo")
}

/// IPinfo writes `AS15169` where every other source writes `15169`.
fn asn_number(text: &str) -> Option<u32> {
    text.strip_prefix("AS").unwrap_or(text).parse().ok()
}

/// Fill the fields an IPinfo database carries, location and network together.
fn read_ipinfo(reader: &Reader<Mmap>, ip: IpAddr, record: &mut GeoIpRecord) -> bool {
    let Some(result) = lookup_in(reader, ip, "ipinfo") else {
        return false;
    };
    let found = match result.decode::<IpInfoRecord<'_>>() {
        Ok(Some(found)) => found,
        Ok(None) => return false,
        Err(e) => {
            debug!(%ip, error = %e, "IPinfo record did not decode");
            return false;
        }
    };

    record.country_code = found.country_code.map(CompactString::from);
    record.country_name = found.country.map(CompactString::from);
    record.continent_code = found.continent_code.map(CompactString::from);
    record.autonomous_system_number = found.asn.and_then(asn_number);
    record.autonomous_system_organization = found.as_name.map(CompactString::from);

    let network = network_of(&result);
    record.network.clone_from(&network);
    record.asn_network = network;

    true
}

/// Fill the location fields, reporting whether the database held the address.
fn read_city(reader: &Reader<Mmap>, ip: IpAddr, record: &mut GeoIpRecord) -> bool {
    if is_ipinfo(reader) {
        return read_ipinfo(reader, ip, record);
    }

    let Some(result) = lookup_in(reader, ip, "city") else {
        return false;
    };
    let city = match result.decode::<geoip2::City>() {
        Ok(Some(city)) => city,
        Ok(None) => return false,
        Err(e) => {
            // One unreadable record must not fail the event carrying it, and at
            // warn level a malformed network would flood on every address in it.
            debug!(%ip, error = %e, "city record did not decode");
            return false;
        }
    };

    record.city_name = city.city.names.english.map(CompactString::from);
    record.continent_code = city.continent.code.map(CompactString::from);
    record.country_code = city.country.iso_code.map(CompactString::from);
    record.country_name = city.country.names.english.map(CompactString::from);

    // The last subdivision is the most specific one -- England, then West
    // Berkshire -- and the specific one is the region an event belongs to.
    if let Some(region) = city.subdivisions.last() {
        record.region_name = region.names.english.map(CompactString::from);
        record.region_code = region.iso_code.map(CompactString::from);
    }

    record.postal_code = city.postal.code.map(CompactString::from);
    record.timezone = city.location.time_zone.map(CompactString::from);
    record.latitude = city.location.latitude;
    record.longitude = city.location.longitude;
    record.metro_code = city.location.metro_code;
    record.accuracy_radius = city.location.accuracy_radius;
    record.network = network_of(&result);

    true
}

/// Fill the ASN fields, reporting whether the database held the address.
fn read_asn(reader: &Reader<Mmap>, ip: IpAddr, record: &mut GeoIpRecord) -> bool {
    if is_ipinfo(reader) {
        return read_ipinfo(reader, ip, record);
    }

    let Some(result) = lookup_in(reader, ip, "asn") else {
        return false;
    };
    let asn = match result.decode::<geoip2::Asn>() {
        Ok(Some(asn)) => asn,
        Ok(None) => return false,
        Err(e) => {
            debug!(%ip, error = %e, "ASN record did not decode");
            return false;
        }
    };

    record.autonomous_system_number = asn.autonomous_system_number;
    record.autonomous_system_organization =
        asn.autonomous_system_organization.map(CompactString::from);
    record.asn_network = network_of(&result);

    true
}

/// Traverse one database, reporting a refusal rather than propagating it.
///
/// An IPv6 address offered to an IPv4-only database is the case that reaches
/// here, and it is a property of the deployed database rather than of the event.
fn lookup_in<'a>(
    reader: &'a Reader<Mmap>,
    ip: IpAddr,
    kind: &'static str,
) -> Option<maxminddb::LookupResult<'a, Mmap>> {
    match reader.lookup(ip) {
        Ok(result) => Some(result),
        Err(e) => {
            debug!(%ip, kind, error = %e, "GeoIP database refused the address");
            None
        }
    }
}

/// The CIDR a lookup matched at.
///
/// The reader hands this back as a type from a crate this one does not depend
/// on, so it is rendered rather than stored, which is also the shape a schema
/// column and an event field both want.
fn network_of(result: &maxminddb::LookupResult<'_, Mmap>) -> Option<CompactString> {
    result
        .network()
        .ok()
        .map(|network| network.to_compact_string())
}

/// Open one database file and note the modification time it was opened at.
fn open_source(path: &Path) -> Result<(Source, Arc<Reader<Mmap>>), GeoIpLookupError> {
    // The time is read first: a replacement landing between the two calls then
    // costs one redundant reopen, where the other order would record the new
    // time against the old reader and never notice the change.
    let mtime = file_mtime(path);
    let reader = open_reader(path)?;

    Ok((
        Source {
            path: path.to_path_buf(),
            mtime,
        },
        Arc::new(reader),
    ))
}

/// Map one database file into the address space.
///
/// A city database runs to around 70 MB, and reading it onto the heap costs
/// that per process. The map costs it once per node, shared through the page
/// cache, which is the defect this crate was extracted to fix.
#[expect(
    unsafe_code,
    reason = "the reader offers no safe mmap constructor, and the provisioning \
              half never writes a database in place, which is what makes the \
              mapping sound"
)]
pub(super) fn open_reader(path: &Path) -> Result<Reader<Mmap>, GeoIpLookupError> {
    // SAFETY: a memory map is unsound if the mapped file is modified underneath
    // it -- a truncation turns a read of the mapped pages into a fault. The
    // provisioning half never writes a database in place: it streams to a
    // sibling temp file and renames that over the destination, so a replacement
    // swaps the directory entry and leaves this map addressing the old, now
    // unlinked, inode that nothing holds a writeable handle to.
    unsafe { Reader::open_mmap(path) }.map_err(|source| GeoIpLookupError::Open {
        path: path.to_path_buf(),
        source,
    })
}

/// Modification time of `path`, or `None` when it cannot be read.
pub(super) fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

/// The single MMDB file of a provisioned database.
///
/// # Errors
///
/// [`GeoIpLookupError::UnsupportedFormat`] for CSV, which is deliberately out of
/// scope: this engine reads the MaxMind DB binary format and nothing else.
#[cfg(feature = "geoip-download")]
fn mmdb_path<'a>(
    database: Option<&'a Database>,
    kind: &'static str,
) -> Result<Option<&'a Path>, GeoIpLookupError> {
    let Some(database) = database else {
        return Ok(None);
    };
    let Some(path) = database.files.first() else {
        return Err(GeoIpLookupError::NoFiles { kind });
    };
    if database.format == DatabaseFormat::Csv {
        return Err(GeoIpLookupError::UnsupportedFormat { path: path.clone() });
    }

    Ok(Some(path.as_path()))
}

/// Metric emission through the facade.
///
/// The names are bare and each carries a `type` label, so one recorder can host
/// this enricher beside others without the series colliding.
#[cfg(feature = "metrics-lookup")]
mod telemetry {
    use std::time::Instant;

    /// Value of the `type` label on every series.
    const TYPE: &str = "geoip";

    /// Start of one lookup.
    #[derive(Clone, Copy)]
    pub(super) struct Started(Instant);

    /// Begin timing a lookup.
    pub(super) fn started() -> Started {
        Started(Instant::now())
    }

    /// Report how long a lookup took.
    pub(super) fn elapsed(started: Started) {
        metrics::histogram!("enrichment_duration_seconds", "type" => TYPE)
            .record(started.0.elapsed().as_secs_f64());
    }

    /// An address the cache already held.
    pub(super) fn hit() {
        metrics::counter!("enrichment_cache_hits_total", "type" => TYPE).increment(1);
    }

    /// An address that cost a database read.
    pub(super) fn miss() {
        metrics::counter!("enrichment_cache_misses_total", "type" => TYPE).increment(1);
    }

    /// How full the cache is.
    ///
    /// Emitted from the miss path and from a refresh, never from a hit: the
    /// count sums a read lock per shard, which is nothing beside a database
    /// traversal and everything beside a cache hit.
    // A cache large enough to lose f64 precision would hold 2^53 addresses, and
    // the cast is lossless on a 32-bit target, so this is allowed rather than
    // expected.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn size(entries: usize) {
        metrics::gauge!("enrichment_cache_size", "type" => TYPE).set(entries as f64);
    }
}

/// Metric emission, compiled out.
///
/// The shapes match the emitting module exactly, so the call sites are identical
/// in both builds and no clock is read when nothing is listening.
#[cfg(not(feature = "metrics-lookup"))]
mod telemetry {
    /// Stand-in for the timer.
    #[derive(Clone, Copy)]
    pub(super) struct Started;

    /// Begin timing a lookup.
    pub(super) const fn started() -> Started {
        Started
    }

    /// Report how long a lookup took.
    pub(super) const fn elapsed(_started: Started) {}

    /// An address the cache already held.
    pub(super) const fn hit() {}

    /// An address that cost a database read.
    pub(super) const fn miss() {}

    /// How full the cache is.
    pub(super) const fn size(_entries: usize) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    /// The city database MaxMind publishes for testing, under Apache-2.0.
    const CITY_DB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/GeoLite2-City-Test.mmdb"
    );

    /// The ASN database MaxMind publishes for testing, under Apache-2.0.
    const ASN_DB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/GeoLite2-ASN-Test.mmdb"
    );

    /// Boxford, the one entry in the test database with two subdivisions.
    const BOXFORD: &str = "2.125.160.216";

    /// An address both databases hold, at different prefixes.
    const LINKOPING: &str = "89.160.20.112";

    /// City name stored for that address, escaped to keep the source ASCII.
    const LINKOPING_NAME: &str = "Link\u{f6}ping";

    /// An address neither test database holds.
    const ABSENT: &str = "8.8.8.8";

    /// An enricher over both test databases.
    fn both() -> GeoIp {
        GeoIp::open(
            Some(Path::new(CITY_DB)),
            Some(Path::new(ASN_DB)),
            CacheConfig::default(),
        )
        .unwrap()
    }

    /// A database in IPinfo Lite's schema, built to match the real one.
    const IPINFO_DB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/IPinfo-Lite-Test.mmdb"
    );

    /// Parse a literal the tests are asserting about.
    fn ip(literal: &str) -> IpAddr {
        literal.parse().unwrap()
    }

    /// An enricher over the IPinfo database alone.
    fn ipinfo() -> GeoIp {
        GeoIp::open(Some(Path::new(IPINFO_DB)), None, CacheConfig::default()).unwrap()
    }

    #[test]
    fn an_ipv6_address_resolves_from_the_city_database() {
        let record = both().lookup(ip("2001:218::1")).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("JP"));
        assert_eq!(record.continent_code.as_deref(), Some("AS"));
        assert_eq!(record.network.as_deref(), Some("2001:218::/32"));
        assert!(!record.is_private);
    }

    #[test]
    fn an_ipv6_address_resolves_from_the_asn_database() {
        let record = both().lookup(ip("2001:2000::1")).unwrap();

        assert_eq!(record.autonomous_system_number, Some(1299));
        assert_eq!(
            record.autonomous_system_organization.as_deref(),
            Some("TeliaSonera International Carrier")
        );
        assert_eq!(record.asn_network.as_deref(), Some("2001:2000::/20"));
    }

    #[test]
    fn an_ipv6_address_only_one_database_holds_still_answers() {
        // The two databases cover different v6 space, so a record filled from
        // one half must not be discarded because the other had nothing.
        let city_only = both().lookup(ip("2001:218::1")).unwrap();
        assert!(city_only.country_code.is_some());
        assert!(city_only.autonomous_system_number.is_none());

        let asn_only = both().lookup(ip("2001:2000::1")).unwrap();
        assert!(asn_only.country_code.is_none());
        assert!(asn_only.autonomous_system_number.is_some());
    }

    #[test]
    fn a_v6_literal_and_its_long_form_are_one_cache_entry() {
        let geoip = both();
        let short = geoip.lookup(ip("2001:218::1")).unwrap();
        let long = geoip
            .lookup(ip("2001:0218:0000:0000:0000:0000:0000:0001"))
            .unwrap();

        assert!(Arc::ptr_eq(&short, &long));
    }

    #[test]
    fn an_ipinfo_database_resolves_location_and_network_together() {
        let record = ipinfo().lookup(ip("8.8.8.8")).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("US"));
        assert_eq!(record.country_name.as_deref(), Some("United States"));
        assert_eq!(record.continent_code.as_deref(), Some("NA"));
        // The source writes `AS15169`; every other source writes a number.
        assert_eq!(record.autonomous_system_number, Some(15169));
        assert_eq!(
            record.autonomous_system_organization.as_deref(),
            Some("Google LLC")
        );
        assert_eq!(record.network.as_deref(), Some("8.8.8.0/24"));
        assert_eq!(record.asn_network.as_deref(), Some("8.8.8.0/24"));
    }

    #[test]
    fn an_ipinfo_database_answers_ipv6() {
        let record = ipinfo().lookup(ip("2606:4700:4700::1111")).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("US"));
        assert_eq!(record.autonomous_system_number, Some(13335));
    }

    #[test]
    fn an_ipinfo_record_without_network_fields_still_resolves() {
        let record = ipinfo().lookup(ip("45.45.45.45")).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("AU"));
        assert!(record.autonomous_system_number.is_none());
        assert!(record.autonomous_system_organization.is_none());
    }

    #[test]
    fn an_address_the_ipinfo_database_omits_reads_as_absent() {
        assert!(ipinfo().lookup(ip("9.9.9.9")).is_none());
    }

    #[test]
    fn an_ipinfo_database_serves_the_asn_slot_too() {
        let geoip = GeoIp::open(None, Some(Path::new(IPINFO_DB)), CacheConfig::default()).unwrap();
        let record = geoip.lookup(ip("1.1.1.1")).unwrap();

        assert_eq!(record.autonomous_system_number, Some(13335));
        assert_eq!(record.country_code.as_deref(), Some("AU"));
    }

    #[test]
    fn an_as_prefixed_number_parses_and_a_malformed_one_does_not() {
        assert_eq!(asn_number("AS15169"), Some(15169));
        assert_eq!(asn_number("15169"), Some(15169));
        assert_eq!(asn_number("AS"), None);
        assert_eq!(asn_number(""), None);
        assert_eq!(asn_number("ASN15169"), None);
    }

    #[test]
    fn a_known_city_resolves_every_location_field() {
        let record = both().lookup(ip(BOXFORD)).unwrap();

        assert_eq!(record.city_name.as_deref(), Some("Boxford"));
        assert_eq!(record.continent_code.as_deref(), Some("EU"));
        assert_eq!(record.country_code.as_deref(), Some("GB"));
        assert_eq!(record.country_name.as_deref(), Some("United Kingdom"));
        assert_eq!(record.postal_code.as_deref(), Some("OX1"));
        assert_eq!(record.timezone.as_deref(), Some("Europe/London"));
        assert_eq!(record.accuracy_radius, Some(100));
        assert_eq!(record.network.as_deref(), Some("2.125.160.216/29"));
        assert!(!record.is_private);
        // Newer builds omit the metro code, and an absent field stays absent
        // rather than being filled with a zero.
        assert!(record.metro_code.is_none());

        let latitude = record.latitude.unwrap();
        let longitude = record.longitude.unwrap();
        assert!((latitude - 51.75).abs() < 1e-6, "{latitude}");
        assert!((longitude + 1.25).abs() < 1e-6, "{longitude}");
    }

    #[test]
    fn the_region_is_the_last_subdivision_not_the_first() {
        // Boxford is listed under England and then West Berkshire, so taking the
        // first entry would report a country-sized area as the region.
        let record = both().lookup(ip(BOXFORD)).unwrap();

        assert_eq!(record.region_name.as_deref(), Some("West Berkshire"));
        assert_eq!(record.region_code.as_deref(), Some("WBK"));
        assert_ne!(record.region_name.as_deref(), Some("England"));
        assert_ne!(record.region_code.as_deref(), Some("ENG"));
    }

    #[test]
    fn a_known_asn_resolves_its_number_and_organisation() {
        let record = both().lookup(ip("1.0.0.1")).unwrap();

        assert_eq!(record.autonomous_system_number, Some(15169));
        assert_eq!(
            record.autonomous_system_organization.as_deref(),
            Some("Google Inc.")
        );
        assert_eq!(record.asn_network.as_deref(), Some("1.0.0.0/24"));
        // The city database does not hold this address, so the merge carries the
        // ASN half alone rather than refusing the answer.
        assert!(record.city_name.is_none());
        assert!(record.network.is_none());
    }

    #[test]
    fn the_two_databases_report_their_own_prefixes() {
        let record = both().lookup(ip(LINKOPING)).unwrap();

        assert_eq!(record.city_name.as_deref(), Some(LINKOPING_NAME));
        assert_eq!(record.country_code.as_deref(), Some("SE"));
        assert_eq!(
            record.autonomous_system_organization.as_deref(),
            Some("Bredband2 AB")
        );
        // Both matched, at different prefixes, which is why one "the network"
        // field could not have carried both. The ASN allocation is the broader
        // of the two, as an ASN block is against a city block.
        assert_eq!(record.network.as_deref(), Some("89.160.20.112/28"));
        assert_eq!(record.asn_network.as_deref(), Some("89.160.0.0/17"));
        assert_ne!(record.network, record.asn_network);
    }

    #[test]
    fn a_reserved_address_answers_without_touching_the_cache() {
        let geoip = both();

        for reserved in ["10.0.0.1", "127.0.0.1", "::1", "100.64.0.1", "240.0.0.1"] {
            let record = geoip.lookup(ip(reserved)).unwrap();
            assert!(record.is_private, "{reserved}");
            assert!(record.country_code.is_none(), "{reserved}");
        }

        // The short-circuit is in front of the cache, so none of that traffic
        // evicted an answer that cost a database read.
        assert_eq!(geoip.cached_entries(), 0);
    }

    #[test]
    fn every_reserved_answer_is_the_same_allocation() {
        let geoip = both();
        let first = geoip.lookup(ip("10.0.0.1")).unwrap();
        let second = geoip.lookup(ip("192.168.1.1")).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn an_address_no_database_holds_is_cached_as_a_miss() {
        let geoip = both();

        assert!(geoip.lookup(ip(ABSENT)).is_none());
        // The negative answer occupies the cache, which is what stops scan
        // traffic traversing both databases on every packet.
        assert_eq!(geoip.cached_entries(), 1);

        assert!(geoip.lookup(ip(ABSENT)).is_none());
        // Still one entry, so the second call was served from the negative one
        // rather than reading and storing again.
        assert_eq!(geoip.cached_entries(), 1);
    }

    #[test]
    fn a_repeat_lookup_returns_the_first_answer_rather_than_a_copy() {
        let geoip = both();
        let first = geoip.lookup(ip(BOXFORD)).unwrap();
        let second = geoip.lookup(ip(BOXFORD)).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(geoip.cached_entries(), 1);
    }

    #[test]
    fn concurrent_misses_on_one_address_share_a_single_read() {
        const THREADS: usize = 16;

        let geoip = both();
        let barrier = Arc::new(Barrier::new(THREADS));

        let records: Vec<Arc<GeoIpRecord>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    let geoip = geoip.clone();
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        geoip.lookup(ip(BOXFORD)).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        // Uncoalesced misses would each have built their own record, of which
        // only one could win the cache, so a shared allocation across every
        // thread is the observable that separates the two.
        for record in &records {
            assert!(Arc::ptr_eq(&records[0], record));
        }
        assert_eq!(geoip.cached_entries(), 1);
    }

    #[test]
    fn a_batch_resolves_each_repeat_once_and_keeps_its_order() {
        let geoip = both();
        let batch = [
            ip(BOXFORD),
            ip(LINKOPING),
            ip(BOXFORD),
            ip("10.0.0.1"),
            ip(BOXFORD),
            ip(ABSENT),
        ];

        let answers = geoip.lookup_many(&batch);

        assert_eq!(answers.len(), batch.len());
        assert_eq!(
            answers[0].as_ref().unwrap().city_name.as_deref(),
            Some("Boxford")
        );
        assert_eq!(
            answers[1].as_ref().unwrap().city_name.as_deref(),
            Some(LINKOPING_NAME)
        );
        assert!(answers[3].as_ref().unwrap().is_private);
        assert!(answers[5].is_none());

        // The three Boxford entries are one answer handed out three times.
        assert!(Arc::ptr_eq(
            answers[0].as_ref().unwrap(),
            answers[2].as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            answers[0].as_ref().unwrap(),
            answers[4].as_ref().unwrap()
        ));
        // Two located addresses and one negative answer, the reserved address
        // never having reached the cache.
        assert_eq!(geoip.cached_entries(), 3);
    }

    #[tokio::test]
    async fn the_async_entry_point_reads_the_same_cache() {
        let geoip = both();
        let synchronous = geoip.lookup(ip(BOXFORD)).unwrap();
        let asynchronous = geoip.lookup_async(ip(BOXFORD)).await.unwrap();

        // One cache instance serves both, which is what lets an async service
        // and a synchronous expression interpreter share their answers.
        assert!(Arc::ptr_eq(&synchronous, &asynchronous));
        assert_eq!(geoip.cached_entries(), 1);
    }

    #[tokio::test]
    async fn the_async_entry_point_short_circuits_reserved_space_too() {
        let geoip = both();
        let record = geoip.lookup_async(ip("192.168.0.1")).await.unwrap();

        assert!(record.is_private);
        assert_eq!(geoip.cached_entries(), 0);
    }

    #[test]
    fn an_answer_inside_the_age_limit_is_served_from_the_cache() {
        let geoip = GeoIp::open(
            Some(Path::new(CITY_DB)),
            None,
            CacheConfig {
                max_age: Some(Duration::from_secs(3600)),
                ..CacheConfig::default()
            },
        )
        .unwrap();

        let first = geoip.lookup(ip(BOXFORD)).unwrap();
        let second = geoip.lookup(ip(BOXFORD)).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn an_age_limit_re_reads_the_answer_it_expired() {
        let geoip = GeoIp::open(
            Some(Path::new(CITY_DB)),
            None,
            CacheConfig {
                max_age: Some(Duration::ZERO),
                ..CacheConfig::default()
            },
        )
        .unwrap();

        let first = geoip.lookup(ip(BOXFORD)).unwrap();
        // A zero limit expires an entry as soon as the clock has moved at all.
        thread::sleep(Duration::from_millis(1));
        let second = geoip.lookup(ip(BOXFORD)).unwrap();

        // The second call read the database again and holds its own allocation.
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(first.city_name, second.city_name);
        assert_eq!(geoip.cached_entries(), 1);
    }

    #[test]
    fn a_missing_database_leaves_the_other_half_answering() {
        let city_only =
            GeoIp::open(Some(Path::new(CITY_DB)), None, CacheConfig::default()).unwrap();
        let record = city_only.lookup(ip(LINKOPING)).unwrap();

        assert_eq!(record.city_name.as_deref(), Some(LINKOPING_NAME));
        assert!(record.autonomous_system_number.is_none());
        assert!(record.asn_network.is_none());
    }

    #[test]
    fn an_enricher_over_no_database_answers_nothing_and_does_not_fail() {
        let empty = GeoIp::open(None, None, CacheConfig::default()).unwrap();

        assert!(empty.lookup(ip(BOXFORD)).is_none());
        // Reserved space is still answered: that check never consults a database.
        assert!(empty.lookup(ip("10.0.0.1")).unwrap().is_private);
    }

    #[test]
    fn opening_something_that_is_not_a_database_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-database.mmdb");
        std::fs::write(&path, b"not a MaxMind DB").unwrap();

        let err = GeoIp::open(Some(&path), None, CacheConfig::default()).unwrap_err();

        assert!(matches!(err, GeoIpLookupError::Open { .. }));
        assert!(err.to_string().contains("not-a-database.mmdb"));
    }

    #[test]
    fn clearing_the_cache_leaves_the_readers_answering() {
        let geoip = both();
        assert!(geoip.lookup(ip(BOXFORD)).is_some());
        assert_eq!(geoip.cached_entries(), 1);

        geoip.clear_cache();

        assert_eq!(geoip.cached_entries(), 0);
        assert!(geoip.lookup(ip(BOXFORD)).is_some());
    }

    #[test]
    fn a_clone_shares_the_cache_rather_than_building_its_own() {
        let geoip = both();
        let worker = geoip.clone();

        let first = geoip.lookup(ip(BOXFORD)).unwrap();
        let second = worker.lookup(ip(BOXFORD)).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(geoip.cached_entries(), 1);
        assert_eq!(worker.cached_entries(), 1);
    }

    #[test]
    fn debug_reports_the_cache_without_dumping_the_databases() {
        let geoip = both();
        let rendered = format!("{geoip:?}");

        assert!(rendered.contains("cached_entries"), "{rendered}");
        assert!(!rendered.contains("GeoLite2"), "{rendered}");
    }

    #[cfg(feature = "geoip-download")]
    #[test]
    fn a_csv_database_is_refused_by_name() {
        let databases = Databases {
            city: Some(Database {
                format: DatabaseFormat::Csv,
                files: vec![
                    PathBuf::from("/var/lib/geoip/city-ipv4.csv"),
                    PathBuf::from("/var/lib/geoip/city-ipv6.csv"),
                ],
            }),
            asn: None,
        };

        let err = GeoIp::from_databases(&databases, CacheConfig::default()).unwrap_err();

        assert!(matches!(err, GeoIpLookupError::UnsupportedFormat { .. }));
        let message = err.to_string();
        assert!(
            message.contains("/var/lib/geoip/city-ipv4.csv"),
            "{message}"
        );
        assert!(message.contains("CSV"), "{message}");
    }

    #[cfg(feature = "geoip-download")]
    #[test]
    fn a_provisioned_mmdb_database_opens_through_the_download_types() {
        let databases = Databases {
            city: Some(Database {
                format: DatabaseFormat::Mmdb,
                files: vec![PathBuf::from(CITY_DB)],
            }),
            asn: Some(Database {
                format: DatabaseFormat::Mmdb,
                files: vec![PathBuf::from(ASN_DB)],
            }),
        };

        let geoip = GeoIp::from_databases(&databases, CacheConfig::default()).unwrap();
        let record = geoip.lookup(ip(LINKOPING)).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("SE"));
        assert_eq!(record.autonomous_system_number, Some(29518));
    }

    #[cfg(feature = "geoip-download")]
    #[test]
    fn a_database_resolved_to_no_files_is_refused_by_kind() {
        let databases = Databases {
            city: None,
            asn: Some(Database {
                format: DatabaseFormat::Mmdb,
                files: Vec::new(),
            }),
        };

        let err = GeoIp::from_databases(&databases, CacheConfig::default()).unwrap_err();

        assert!(matches!(err, GeoIpLookupError::NoFiles { kind: "asn" }));
    }
}
