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
//! use factbook::geoip::{CacheConfig, DatabasePaths, GeoIp};
//!
//! # fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let geoip = GeoIp::open(
//!     DatabasePaths {
//!         city: Some(Path::new("/var/lib/geoip/GeoLite2-City.mmdb")),
//!         asn: Some(Path::new("/var/lib/geoip/GeoLite2-ASN.mmdb")),
//!         ..DatabasePaths::default()
//!     },
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

use super::DEFAULT_RESIDENT_MAX_BYTES;
#[cfg(feature = "geoip-download")]
use super::download::{Database, DatabaseFormat, Databases};
use super::extra;
use super::private::is_private;
use super::record::GeoIpRecord;

/// Addresses held in the cache by default.
///
/// Enough to hold the working set of a busy feed. What an entry costs follows
/// the source and how much of it is kept: a few hundred bytes on a lean ASN
/// table, and two to four kilobytes on a city build whose records carry geoname
/// ids, confidence scores and names in eight languages. At this capacity that is
/// tens of megabytes at the low end and a few hundred at the high one, against a
/// database of 10 to 120 MB.
/// [`collect_extra_fields`](CacheConfig::collect_extra_fields) is what moves an
/// entry between those two ends.
const DEFAULT_CAPACITY: usize = 100_000;

/// Errors raised while opening or refreshing the databases.
///
/// A lookup itself returns no error: an address the databases cannot answer is
/// reported as `None`, and a malformed record is logged and treated the same
/// way, because one bad record must not fail the event carrying it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GeoIpLookupError {
    /// The provisioned database is not a MaxMind DB, which is the only format
    /// this engine reads.
    // The format is a name rather than a `DatabaseFormat`, which lives behind
    // `geoip-download` and would not exist in a lookup-only build.
    #[error("{} is a {format} database, a format not supported by the lookup engine", .path.display())]
    UnsupportedFormat {
        /// First file of the database.
        path: PathBuf,
        /// Format the provisioning half reported for it.
        format: &'static str,
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

/// How the lookup side is set up: the cache, how a database is held, and how
/// much of a record is kept.
///
/// The cache knobs stop here on purpose. Shard count, ghost-queue allocation,
/// weighers, eviction listeners and hashers are the cache implementation's
/// business, and exposing them would make its version a part of this crate's
/// contract.
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

    /// Largest file, in bytes, read into memory rather than mapped.
    ///
    /// At or under this a database is read onto the heap when it is opened, so
    /// no lookup can fault on a page; above it the file is mapped and its pages
    /// arrive at the operating system's discretion. The database occupies its
    /// own size either way -- what changes is whether a lookup can stall.
    /// Zero maps every database.
    pub resident_max_bytes: u64,

    /// Keep the source fields no typed field names, in
    /// [`extra`](GeoIpRecord::extra).
    ///
    /// On by default: a database holds more than the record names -- an ISP
    /// edition's `isp` and `organization`, a city build's geoname ids,
    /// confidence scores and names in eight languages -- and dropping them
    /// enriches from part of the source rather than from all of it.
    ///
    /// Turn it off where the cache costs more than those fields are worth. They
    /// are what takes an entry from a few hundred bytes to two to four
    /// kilobytes, so at [`capacity`](Self::capacity) they are the difference
    /// between tens of megabytes of cache and a few hundred. Off, the fields are
    /// never read rather than read and dropped, so the second record decode is
    /// saved along with the memory. The typed fields resolve the same either
    /// way.
    pub collect_extra_fields: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            max_age: None,
            resident_max_bytes: DEFAULT_RESIDENT_MAX_BYTES,
            collect_extra_fields: true,
        }
    }
}

/// Where an open database's bytes live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DatabaseBacking {
    /// Read onto the heap when the database was opened.
    Resident,

    /// Mapped, with the pages arriving as the operating system supplies them.
    Mapped,
}

impl DatabaseBacking {
    /// Label used in metric series and log fields.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Mapped => "mapped",
        }
    }
}

/// Which backing each open database got.
///
/// A kind is `None` when no database of that kind was provisioned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DatabaseBackings {
    /// City database, when one was provisioned.
    pub city: Option<DatabaseBacking>,

    /// ASN database, when one was provisioned.
    pub asn: Option<DatabaseBacking>,
}

/// One open database, over whichever backing its size selected.
pub(super) enum Backing {
    /// The file, read onto the heap.
    Resident(Reader<Vec<u8>>),

    /// The file, mapped.
    Mapped(Reader<Mmap>),
}

impl Backing {
    /// Which backing this is, for the accessor and the metric.
    const fn reported(&self) -> DatabaseBacking {
        match self {
            Self::Resident(_) => DatabaseBacking::Resident,
            Self::Mapped(_) => DatabaseBacking::Mapped,
        }
    }
}

/// The open readers, swapped as a set so a refresh is one atomic store.
pub(super) struct Readers {
    /// City database, when one was provisioned.
    pub(super) city: Option<Arc<Backing>>,
    /// ASN database, when one was provisioned.
    pub(super) asn: Option<Arc<Backing>>,
}

impl Readers {
    /// Which backing each open database got.
    fn backings(&self) -> DatabaseBackings {
        DatabaseBackings {
            city: self.city.as_deref().map(Backing::reported),
            asn: self.asn.as_deref().map(Backing::reported),
        }
    }
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
    /// Ceiling under which a reopened database is read into memory.
    pub(super) resident_max_bytes: u64,
    /// Whether a miss keeps the source fields no typed field names.
    collect_extra_fields: bool,
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
        let backings = readers.backings();
        self.readers.store(Arc::new(readers));
        self.cache.clear();
        telemetry::size(self.cache.len());
        backing_telemetry::backings(backings);
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

/// Which file serves each half of a lookup.
///
/// Named rather than two path arguments: both halves are `Option<&Path>`, so
/// positional arguments compile when swapped, and the engine then reads city
/// fields out of an ASN database and answers nothing for every address.
///
/// Construct it with `..DatabasePaths::default()` rather than an exhaustive
/// literal. Providers publish more than these two kinds -- ISP, domain,
/// connection type, anonymous IP -- so a field is added here as those are
/// supported, and functional update takes it without a change.
///
/// A caller-supplied file must only ever be replaced by rename. A file past the
/// resident ceiling is memory-mapped, and writing one in place under a live
/// mapping is undefined behaviour rather than a race.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DatabasePaths<'a> {
    /// City, country and location database.
    pub city: Option<&'a Path>,

    /// Autonomous system database.
    pub asn: Option<&'a Path>,
}

impl<'a> DatabasePaths<'a> {
    /// A city database alone, resolving no ASN fields.
    #[must_use]
    pub const fn city_only(path: &'a Path) -> Self {
        Self {
            city: Some(path),
            asn: None,
        }
    }

    /// An ASN database alone, resolving no location fields.
    #[must_use]
    pub const fn asn_only(path: &'a Path) -> Self {
        Self {
            city: None,
            asn: Some(path),
        }
    }
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
    pub fn open(paths: DatabasePaths<'_>, cache: CacheConfig) -> Result<Self, GeoIpLookupError> {
        let open = |path| open_source(path, cache.resident_max_bytes);
        let (city_source, city_reader) = paths.city.map(open).transpose()?.unzip();
        let (asn_source, asn_reader) = paths.asn.map(open).transpose()?.unzip();

        let readers = Readers {
            city: city_reader,
            asn: asn_reader,
        };
        backing_telemetry::backings(readers.backings());

        Ok(Self {
            inner: Arc::new(Inner {
                readers: ArcSwap::from_pointee(readers),
                cache: Cache::new(cache.capacity),
                sources: Mutex::new(Sources {
                    city: city_source,
                    asn: asn_source,
                }),
                max_age: cache.max_age,
                resident_max_bytes: cache.resident_max_bytes,
                collect_extra_fields: cache.collect_extra_fields,
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
        let paths = DatabasePaths {
            city: mmdb_path(databases.city.as_ref(), "city")?,
            asn: mmdb_path(databases.asn.as_ref(), "asn")?,
        };
        Self::open(paths, cache)
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

    /// Which backing each open database got.
    ///
    /// Chosen per file from its size, at open and again whenever a refresh
    /// reopens it, so this is reported rather than configured.
    #[must_use]
    pub fn backings(&self) -> DatabaseBackings {
        self.inner.readers.load().backings()
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
            record: read(&readers, ip, self.inner.collect_extra_fields).map(Arc::new),
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
fn read(readers: &Readers, ip: IpAddr, collect_extra: bool) -> Option<GeoIpRecord> {
    let mut record = GeoIpRecord::default();
    let mut found = false;

    // The backing is matched once per database rather than per tree node, so
    // the traversal itself stays monomorphic.
    if let Some(backing) = readers.city.as_deref() {
        found |= match backing {
            Backing::Resident(reader) => read_city(reader, ip, &mut record, collect_extra),
            Backing::Mapped(reader) => read_city(reader, ip, &mut record, collect_extra),
        };
    }
    if let Some(backing) = readers.asn.as_deref() {
        found |= match backing {
            Backing::Resident(reader) => read_asn(reader, ip, &mut record, collect_extra),
            Backing::Mapped(reader) => read_asn(reader, ip, &mut record, collect_extra),
        };
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
    continent: Option<&'a str>,
    #[serde(borrow, default)]
    asn: Option<&'a str>,
    #[serde(borrow, default)]
    as_name: Option<&'a str>,
    #[serde(borrow, default)]
    as_domain: Option<&'a str>,
}

/// Whether a database is written in IPinfo's schema rather than MaxMind's.
///
/// Every database names its own schema in its metadata, so this holds for a
/// file that was pre-seeded or mounted rather than downloaded.
fn is_ipinfo<S: AsRef<[u8]>>(reader: &Reader<S>) -> bool {
    reader.metadata().database_type.starts_with("ipinfo")
}

/// IPinfo writes `AS15169` where every other source writes `15169`.
pub(super) fn asn_number(text: &str) -> Option<u32> {
    text.strip_prefix("AS").unwrap_or(text).parse().ok()
}

/// Fill the fields an IPinfo database carries, location and network together.
fn read_ipinfo<S: AsRef<[u8]>>(
    reader: &Reader<S>,
    ip: IpAddr,
    record: &mut GeoIpRecord,
    collect_extra: bool,
) -> bool {
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
    record.continent_name = found.continent.map(CompactString::from);
    record.autonomous_system_number = found.asn.and_then(asn_number);
    record.autonomous_system_organization = found.as_name.map(CompactString::from);
    record.as_domain = found.as_domain.map(CompactString::from);

    let network = network_of(&result);
    record.network.clone_from(&network);
    record.asn_network = network;

    // Collected last, so the typed fields it is filtered against are set.
    if collect_extra {
        extra::collect(&result, record);
    }

    true
}

/// Fill the location fields, reporting whether the database held the address.
fn read_city<S: AsRef<[u8]>>(
    reader: &Reader<S>,
    ip: IpAddr,
    record: &mut GeoIpRecord,
    collect_extra: bool,
) -> bool {
    if is_ipinfo(reader) {
        return read_ipinfo(reader, ip, record, collect_extra);
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
    record.continent_name = city.continent.names.english.map(CompactString::from);
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

    if collect_extra {
        extra::collect(&result, record);
    }

    true
}

/// Fill the ASN fields, reporting whether the database held the address.
fn read_asn<S: AsRef<[u8]>>(
    reader: &Reader<S>,
    ip: IpAddr,
    record: &mut GeoIpRecord,
    collect_extra: bool,
) -> bool {
    if is_ipinfo(reader) {
        return read_ipinfo(reader, ip, record, collect_extra);
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

    if collect_extra {
        extra::collect(&result, record);
    }

    true
}

/// Traverse one database, reporting a refusal rather than propagating it.
///
/// An IPv6 address offered to an IPv4-only database is the case that reaches
/// here, and it is a property of the deployed database rather than of the event.
fn lookup_in<'a, S: AsRef<[u8]>>(
    reader: &'a Reader<S>,
    ip: IpAddr,
    kind: &'static str,
) -> Option<maxminddb::LookupResult<'a, S>> {
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
fn network_of<S: AsRef<[u8]>>(result: &maxminddb::LookupResult<'_, S>) -> Option<CompactString> {
    result
        .network()
        .ok()
        .map(|network| network.to_compact_string())
}

/// Open one database file and note the modification time it was opened at.
fn open_source(
    path: &Path,
    resident_max_bytes: u64,
) -> Result<(Source, Arc<Backing>), GeoIpLookupError> {
    // The time is read first: a replacement landing between the two calls then
    // costs one redundant reopen, where the other order would record the new
    // time against the old reader and never notice the change.
    let mtime = file_mtime(path);
    let backing = open_backing(path, resident_max_bytes)?;

    Ok((
        Source {
            path: path.to_path_buf(),
            mtime,
        },
        Arc::new(backing),
    ))
}

/// Open one database, resident at or under `resident_max_bytes` and mapped
/// above it.
///
/// A file whose length will not read is mapped, since an unknown size is not
/// one to read onto the heap.
///
/// # Errors
///
/// [`GeoIpLookupError::Open`] naming the file that could not be read as a
/// MaxMind DB.
pub(super) fn open_backing(
    path: &Path,
    resident_max_bytes: u64,
) -> Result<Backing, GeoIpLookupError> {
    if file_len(path).is_none_or(|len| len > resident_max_bytes) {
        return open_reader(path).map(Backing::Mapped);
    }

    Reader::open_readfile(path)
        .map(Backing::Resident)
        .map_err(|source| GeoIpLookupError::Open {
            path: path.to_path_buf(),
            source,
        })
}

/// Map one database file into the address space.
///
/// The pages are shared through the page cache, so a database past the resident
/// ceiling costs its size once per node rather than once per process.
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

/// Length of `path` in bytes, or `None` when it cannot be read.
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).map(|metadata| metadata.len()).ok()
}

/// The single MMDB file of a provisioned database.
///
/// # Errors
///
/// [`GeoIpLookupError::UnsupportedFormat`] for anything that is not a MaxMind
/// DB, which is deliberately out of scope: this engine reads that binary format
/// and nothing else. Tested against the format rather than against CSV alone,
/// so a format added later is refused by name instead of reaching the reader.
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
    if database.format != DatabaseFormat::Mmdb {
        return Err(GeoIpLookupError::UnsupportedFormat {
            path: path.clone(),
            format: database.format.name(),
        });
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

/// The backing each database got, through the facade.
///
/// On the same footing as the download and age metrics rather than the lookup
/// ones: it is emitted once per database per open or refresh, so it costs
/// nothing a lookup can feel.
#[cfg(feature = "metrics")]
mod backing_telemetry {
    use super::{DatabaseBacking, DatabaseBackings};

    /// Value of the `type` label on every series.
    const TYPE: &str = "geoip";

    /// Report the backing of each open database.
    pub(super) fn backings(chosen: DatabaseBackings) {
        if let Some(city) = chosen.city {
            report("city", city);
        }
        if let Some(asn) = chosen.asn {
            report("asn", asn);
        }
    }

    /// Set every backing's series, so a refresh that flips one leaves no series
    /// still claiming the backing it replaced.
    fn report(kind: &'static str, chosen: DatabaseBacking) {
        for candidate in [DatabaseBacking::Resident, DatabaseBacking::Mapped] {
            metrics::gauge!(
                "enrichment_database_backing",
                "type" => TYPE,
                "kind" => kind,
                "backing" => candidate.label(),
            )
            .set(if candidate == chosen { 1.0 } else { 0.0 });
        }
    }
}

/// Backing emission, compiled out.
#[cfg(not(feature = "metrics"))]
mod backing_telemetry {
    use super::DatabaseBackings;

    /// Report the backing of each open database.
    pub(super) const fn backings(_chosen: DatabaseBackings) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::geoip::{ExtraFields, ExtraValue};

    /// A database in MaxMind's City schema, built by scripts/make_fixtures.py.
    const CITY_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/city-test.mmdb");

    /// A database in MaxMind's ASN schema, built by scripts/make_fixtures.py.
    const ASN_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/asn-test.mmdb");

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
            DatabasePaths {
                city: Some(Path::new(CITY_DB)),
                asn: Some(Path::new(ASN_DB)),
            },
            CacheConfig::default(),
        )
        .unwrap()
    }

    /// A database in IPinfo Lite's schema, built by scripts/make_fixtures.py.
    const IPINFO_DB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/IPinfo-Lite-Test.mmdb"
    );

    /// The City schema as a current build writes it, with the geoname ids,
    /// confidence scores, languages and traits no typed field names.
    const CITY_RICH_DB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/city-rich-test.mmdb"
    );

    /// A database in MaxMind's paid ISP schema, built by scripts/make_fixtures.py.
    const ISP_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/isp-test.mmdb");

    /// Parse a literal the tests are asserting about.
    fn ip(literal: &str) -> IpAddr {
        literal.parse().unwrap()
    }

    /// An enricher over the IPinfo database alone.
    fn ipinfo() -> GeoIp {
        GeoIp::open(
            DatabasePaths::city_only(Path::new(IPINFO_DB)),
            CacheConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn an_ipv6_address_resolves_from_the_city_database() {
        let record = both().lookup(ip("2001:218::1")).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("JP"));
        assert_eq!(record.continent_code.as_deref(), Some("AS"));
        assert_eq!(record.continent_name.as_deref(), Some("Asia"));
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
        // The operator's domain, which is not its name.
        assert_eq!(record.as_domain.as_deref(), Some("google.com"));
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
        let geoip = GeoIp::open(
            DatabasePaths::asn_only(Path::new(IPINFO_DB)),
            CacheConfig::default(),
        )
        .unwrap();
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
        assert_eq!(record.continent_name.as_deref(), Some("Europe"));
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_async_misses_on_one_address_share_a_single_read() {
        const TASKS: usize = 16;

        let geoip = both();
        let gate = Arc::new(tokio::sync::Barrier::new(TASKS));

        let tasks: Vec<_> = (0..TASKS)
            .map(|_| {
                let geoip = geoip.clone();
                let gate = Arc::clone(&gate);
                tokio::spawn(async move {
                    gate.wait().await;
                    geoip.lookup_async(ip(BOXFORD)).await.unwrap()
                })
            })
            .collect();

        let mut records = Vec::with_capacity(TASKS);
        for task in tasks {
            records.push(task.await.unwrap());
        }

        // Uncoalesced misses would each have built their own record, of which
        // only one could win the cache, so a shared allocation across every
        // task is the observable that separates the two.
        for record in &records {
            assert!(Arc::ptr_eq(&records[0], record));
        }
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
            DatabasePaths::city_only(Path::new(CITY_DB)),
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
            DatabasePaths::city_only(Path::new(CITY_DB)),
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
        let city_only = GeoIp::open(
            DatabasePaths::city_only(Path::new(CITY_DB)),
            CacheConfig::default(),
        )
        .unwrap();
        let record = city_only.lookup(ip(LINKOPING)).unwrap();

        assert_eq!(record.city_name.as_deref(), Some(LINKOPING_NAME));
        assert!(record.autonomous_system_number.is_none());
        assert!(record.asn_network.is_none());
    }

    #[test]
    fn an_enricher_over_no_database_answers_nothing_and_does_not_fail() {
        let empty = GeoIp::open(DatabasePaths::default(), CacheConfig::default()).unwrap();

        assert!(empty.lookup(ip(BOXFORD)).is_none());
        // Reserved space is still answered: that check never consults a database.
        assert!(empty.lookup(ip("10.0.0.1")).unwrap().is_private);
    }

    #[test]
    fn opening_something_that_is_not_a_database_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-a-database.mmdb");
        std::fs::write(&path, b"not a MaxMind DB").unwrap();

        let err = GeoIp::open(DatabasePaths::city_only(&path), CacheConfig::default()).unwrap_err();

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
        // The fixture's file name is also its `database_type`, so this catches a
        // Debug that rendered either the path or the reader's metadata.
        assert!(!rendered.contains("city-test"), "{rendered}");
    }

    /// Length of a fixture on disk, for a ceiling set relative to it.
    fn size_of(path: &str) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }

    /// An enricher over both test databases, under the given ceiling.
    fn both_under(resident_max_bytes: u64) -> GeoIp {
        GeoIp::open(
            DatabasePaths {
                city: Some(Path::new(CITY_DB)),
                asn: Some(Path::new(ASN_DB)),
            },
            CacheConfig {
                resident_max_bytes,
                ..CacheConfig::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn a_database_under_the_ceiling_is_held_in_memory() {
        // The committed fixtures are kilobytes, so the default ceiling takes
        // both of them.
        assert_eq!(
            both().backings(),
            DatabaseBackings {
                city: Some(DatabaseBacking::Resident),
                asn: Some(DatabaseBacking::Resident),
            }
        );
    }

    #[test]
    fn a_database_over_the_ceiling_is_mapped() {
        // The ceiling lands on the ASN file exactly, so the larger city file is
        // the one over it.
        let geoip = both_under(size_of(ASN_DB));

        assert_eq!(
            geoip.backings(),
            DatabaseBackings {
                city: Some(DatabaseBacking::Mapped),
                asn: Some(DatabaseBacking::Resident),
            }
        );
    }

    #[test]
    fn the_ceiling_admits_a_file_of_exactly_its_size() {
        let exact = size_of(CITY_DB);

        assert_eq!(
            both_under(exact).backings().city,
            Some(DatabaseBacking::Resident)
        );
        assert_eq!(
            both_under(exact - 1).backings().city,
            Some(DatabaseBacking::Mapped)
        );
    }

    #[test]
    fn a_zero_ceiling_maps_every_database() {
        assert_eq!(
            both_under(0).backings(),
            DatabaseBackings {
                city: Some(DatabaseBacking::Mapped),
                asn: Some(DatabaseBacking::Mapped),
            }
        );
    }

    #[test]
    fn the_two_backings_answer_the_same_record() {
        let resident = both();
        let mapped = both_under(0);

        for literal in [BOXFORD, LINKOPING, "1.0.0.1", "2001:218::1", ABSENT] {
            assert_eq!(
                resident.lookup(ip(literal)).map(|record| (*record).clone()),
                mapped.lookup(ip(literal)).map(|record| (*record).clone()),
                "{literal}"
            );
        }
    }

    #[test]
    fn an_enricher_over_no_database_reports_no_backing() {
        let empty = GeoIp::open(DatabasePaths::default(), CacheConfig::default()).unwrap();

        assert_eq!(empty.backings(), DatabaseBackings::default());
    }

    /// An enricher over the rich city database alone.
    fn city_rich() -> GeoIp {
        GeoIp::open(
            DatabasePaths::city_only(Path::new(CITY_RICH_DB)),
            CacheConfig::default(),
        )
        .unwrap()
    }

    /// The value at a source path, for a test asserting one field.
    fn extra(record: &GeoIpRecord, path: &str) -> ExtraValue {
        record
            .extra
            .get(path)
            .unwrap_or_else(|| panic!("{path} is in the map: {:?}", record.extra))
            .clone()
    }

    /// A text value at a source path.
    fn extra_str(record: &GeoIpRecord, path: &str) -> CompactString {
        match extra(record, path) {
            ExtraValue::Str(text) => text,
            other => panic!("{path} is text, not {other:?}"),
        }
    }

    #[test]
    fn a_field_no_typed_field_names_reaches_the_map() {
        let record = city_rich().lookup(ip(BOXFORD)).unwrap();

        // A paid or current build writes all of this, and none of it has a
        // typed field to land in.
        assert_eq!(extra_str(&record, "city.names.de"), "Boxford");
        assert_eq!(
            extra(&record, "city.geoname_id"),
            ExtraValue::UInt(2_655_045)
        );
        assert_eq!(extra(&record, "city.confidence"), ExtraValue::UInt(25));
        assert_eq!(
            extra(&record, "country.is_in_european_union"),
            ExtraValue::Bool(false)
        );
        assert_eq!(
            extra(&record, "location.average_income"),
            ExtraValue::UInt(32_323)
        );
        assert_eq!(extra(&record, "traits.is_anycast"), ExtraValue::Bool(true));
        assert_eq!(extra_str(&record, "traits.user_type"), "residential");
        assert_eq!(extra_str(&record, "represented_country.type"), "military");
        assert_eq!(extra_str(&record, "registered_country.iso_code"), "GB");
    }

    #[test]
    fn a_nested_name_keeps_its_own_characters() {
        let record = city_rich().lookup(ip(BOXFORD)).unwrap();

        // Escaped to keep the source ASCII: katakana for Boxford.
        assert_eq!(
            extra_str(&record, "city.names.ja"),
            "\u{30dc}\u{30c3}\u{30af}\u{30b9}\u{30d5}\u{30a9}\u{30fc}\u{30c9}"
        );
    }

    #[test]
    fn the_typed_fields_still_resolve_beside_the_map() {
        let record = city_rich().lookup(ip(BOXFORD)).unwrap();

        assert_eq!(record.city_name.as_deref(), Some("Boxford"));
        assert_eq!(record.continent_name.as_deref(), Some("Europe"));
        assert_eq!(record.country_code.as_deref(), Some("GB"));
        assert_eq!(record.region_name.as_deref(), Some("West Berkshire"));
        assert_eq!(record.region_code.as_deref(), Some("WBK"));
        assert_eq!(record.postal_code.as_deref(), Some("OX1"));
        assert_eq!(record.timezone.as_deref(), Some("Europe/London"));
        assert_eq!(record.accuracy_radius, Some(100));
        assert_eq!(record.network.as_deref(), Some("2.125.160.216/29"));
    }

    #[test]
    fn a_field_a_typed_field_took_is_not_in_the_map_as_well() {
        let record = city_rich().lookup(ip(BOXFORD)).unwrap();

        for path in [
            "city.names.en",
            "continent.code",
            "continent.names.en",
            "country.iso_code",
            "country.names.en",
            "postal.code",
            "location.time_zone",
            "location.latitude",
            "location.longitude",
            "location.accuracy_radius",
        ] {
            assert!(record.extra.get(path).is_none(), "{path}");
        }
    }

    #[test]
    fn an_array_is_carried_under_its_indices() {
        let record = city_rich().lookup(ip(BOXFORD)).unwrap();

        // England lost the region field to West Berkshire, so the map is the
        // only place it survives at all.
        assert_eq!(extra_str(&record, "subdivisions.0.names.en"), "England");
        assert_eq!(extra_str(&record, "subdivisions.0.iso_code"), "ENG");
        assert_eq!(
            extra(&record, "subdivisions.0.geoname_id"),
            ExtraValue::UInt(6_269_131)
        );
        // The subdivision the typed fields did take is not repeated, but the
        // rest of its entry still is.
        assert!(record.extra.get("subdivisions.1.names.en").is_none());
        assert!(record.extra.get("subdivisions.1.iso_code").is_none());
        assert_eq!(
            extra(&record, "subdivisions.1.confidence"),
            ExtraValue::UInt(40)
        );
    }

    #[test]
    fn the_isp_edition_delivers_the_fields_it_is_bought_for() {
        let geoip = GeoIp::open(
            DatabasePaths::asn_only(Path::new(ISP_DB)),
            CacheConfig::default(),
        )
        .unwrap();
        let record = geoip.lookup(ip("1.0.0.1")).unwrap();

        assert_eq!(record.autonomous_system_number, Some(15169));
        assert_eq!(
            record.autonomous_system_organization.as_deref(),
            Some("Google Inc.")
        );
        // The distinguishing fields of the paid edition, which the record has
        // never had a typed slot for.
        assert_eq!(extra_str(&record, "isp"), "Telstra Mobile");
        assert_eq!(extra_str(&record, "organization"), "Telstra Mobile Data");
        assert_eq!(extra_str(&record, "mobile_country_code"), "505");
        assert_eq!(extra_str(&record, "mobile_network_code"), "01");
        assert!(record.extra.get("autonomous_system_number").is_none());
        assert!(record.extra.get("autonomous_system_organization").is_none());
    }

    #[test]
    fn the_lean_fixtures_were_discarding_fields_too() {
        let record = both().lookup(ip(BOXFORD)).unwrap();

        // England has never had a typed field to reach, on a database shape the
        // suite has been asserting about since the beginning.
        assert_eq!(extra_str(&record, "subdivisions.0.names.en"), "England");
        assert_eq!(extra_str(&record, "subdivisions.0.iso_code"), "ENG");
    }

    #[test]
    fn a_source_the_record_fully_names_carries_no_map() {
        let record = ipinfo().lookup(ip("8.8.8.8")).unwrap();

        // Every field IPinfo Lite writes has a typed field, so there is nothing
        // left over and nothing repeated.
        assert_eq!(record.continent_name.as_deref(), Some("North America"));
        assert!(record.extra.is_empty(), "{:?}", record.extra);
    }

    #[test]
    fn a_reserved_address_carries_no_source_fields() {
        let record = city_rich().lookup(ip("10.0.0.1")).unwrap();

        assert!(record.is_private);
        assert!(record.extra.is_empty());
    }

    #[test]
    fn both_databases_contribute_to_one_map() {
        let geoip = GeoIp::open(
            DatabasePaths {
                city: Some(Path::new(CITY_RICH_DB)),
                asn: Some(Path::new(ISP_DB)),
            },
            CacheConfig::default(),
        )
        .unwrap();
        let record = geoip.lookup(ip(LINKOPING)).unwrap();

        // The city half holds nothing for this address, so the answer is the
        // ISP half's alone.
        assert_eq!(extra_str(&record, "isp"), "Bredband2 AB");
        assert_eq!(extra_str(&record, "organization"), "Bredband2 Customer");

        let boxford = geoip.lookup(ip(BOXFORD)).unwrap();
        assert_eq!(extra_str(&boxford, "traits.user_type"), "residential");
    }

    /// The lookup settings with the source fields dropped.
    fn without_extras() -> CacheConfig {
        CacheConfig {
            collect_extra_fields: false,
            ..CacheConfig::default()
        }
    }

    /// An enricher over the given databases, keeping no source fields.
    fn lean(paths: DatabasePaths<'_>) -> GeoIp {
        GeoIp::open(paths, without_extras()).unwrap()
    }

    #[test]
    fn the_source_fields_are_kept_unless_the_setting_says_otherwise() {
        // The stated requirement is enrichment from everything the source
        // supplies, so an unconfigured deployment gets all of it.
        assert!(CacheConfig::default().collect_extra_fields);
    }

    #[test]
    fn the_source_fields_are_dropped_when_collection_is_off() {
        let rich = city_rich().lookup(ip(BOXFORD)).unwrap();
        let lean = lean(DatabasePaths::city_only(Path::new(CITY_RICH_DB)))
            .lookup(ip(BOXFORD))
            .unwrap();

        // The same database, the same address, and the map is the whole
        // difference between the two answers.
        assert!(!rich.extra.is_empty());
        assert!(lean.extra.is_empty(), "{:?}", lean.extra);

        let mut typed_only = (*rich).clone();
        typed_only.extra = ExtraFields::new();
        assert_eq!(typed_only, *lean);
    }

    #[test]
    fn the_typed_fields_resolve_with_collection_off() {
        let record = lean(DatabasePaths::city_only(Path::new(CITY_RICH_DB)))
            .lookup(ip(BOXFORD))
            .unwrap();

        assert_eq!(record.city_name.as_deref(), Some("Boxford"));
        assert_eq!(record.continent_name.as_deref(), Some("Europe"));
        assert_eq!(record.country_code.as_deref(), Some("GB"));
        // The last subdivision still wins, which is a decision the typed decode
        // makes over an array the map is no longer holding.
        assert_eq!(record.region_name.as_deref(), Some("West Berkshire"));
        assert_eq!(record.region_code.as_deref(), Some("WBK"));
        assert_eq!(record.postal_code.as_deref(), Some("OX1"));
        assert_eq!(record.timezone.as_deref(), Some("Europe/London"));
        assert_eq!(record.accuracy_radius, Some(100));
        assert_eq!(record.network.as_deref(), Some("2.125.160.216/29"));
        assert!(record.extra.is_empty());
    }

    #[test]
    fn the_asn_half_drops_its_fields_and_keeps_its_typed_ones() {
        let geoip = lean(DatabasePaths::asn_only(Path::new(ISP_DB)));
        let record = geoip.lookup(ip("1.0.0.1")).unwrap();

        // The paid edition's own fields are what the setting gives up.
        assert!(record.extra.is_empty(), "{:?}", record.extra);
        assert_eq!(record.autonomous_system_number, Some(15169));
        assert_eq!(
            record.autonomous_system_organization.as_deref(),
            Some("Google Inc.")
        );
        assert_eq!(record.asn_network.as_deref(), Some("1.0.0.0/24"));
    }

    #[test]
    fn an_ipinfo_database_answers_with_collection_off() {
        let geoip = lean(DatabasePaths::city_only(Path::new(IPINFO_DB)));
        let record = geoip.lookup(ip("8.8.8.8")).unwrap();

        assert_eq!(record.country_code.as_deref(), Some("US"));
        assert_eq!(record.autonomous_system_number, Some(15169));
        assert_eq!(record.as_domain.as_deref(), Some("google.com"));
        assert!(record.extra.is_empty());
    }

    #[test]
    fn neither_database_contributes_a_field_when_collection_is_off() {
        let geoip = lean(DatabasePaths {
            city: Some(Path::new(CITY_RICH_DB)),
            asn: Some(Path::new(ISP_DB)),
        });

        // Both halves are read for both addresses, so this covers the city path
        // and the ASN path answering into one record.
        let linkoping = geoip.lookup(ip(LINKOPING)).unwrap();
        assert_eq!(
            linkoping.autonomous_system_organization.as_deref(),
            Some("Bredband2 AB")
        );
        assert!(linkoping.extra.is_empty(), "{:?}", linkoping.extra);

        let boxford = geoip.lookup(ip(BOXFORD)).unwrap();
        assert_eq!(boxford.city_name.as_deref(), Some("Boxford"));
        assert!(boxford.extra.is_empty(), "{:?}", boxford.extra);
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
