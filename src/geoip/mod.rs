// Project:   factbook
// File:      src/geoip/mod.rs
// Purpose:   GeoIP databases: configuration and provisioning
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! GeoIP databases: which ones a service wants, getting them onto disk, and
//! looking addresses up in them.
//!
//! [`GeoIpConfig`] describes the databases and [`ensure_databases`] provisions
//! them. An app checks its configuration with [`validate`] when it loads it,
//! because provisioning itself never fails loudly. [`GeoIp`] reads the
//! provisioned files behind a cache and answers with a [`GeoIpRecord`].
//!
//! # Non-fatal by contract
//!
//! [`ensure_databases`] returns `Ok` with `None` entries when a download fails.
//! A GeoIP database going missing degrades enrichment; it must not stop a
//! service from starting. Failures are logged at `warn` and a stale local file
//! is preferred over no file at all.
//!
//! # Credentials
//!
//! MaxMind and IPinfo credentials are [`Secret`](crate::Secret) and never reach
//! a log line. The IPinfo token is attached as a query parameter by the request
//! builder rather than being formatted into the URL string, and a transport
//! error is stripped of its URL before it is reported, so neither the logged URL
//! nor the error text can carry it.
//!
//! # Example
//!
//! ```rust,no_run
//! use factbook::geoip::{GeoIpConfig, ensure_databases};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let databases = ensure_databases(&GeoIpConfig::default()).await?;
//! if let Some(city) = databases.city {
//!     println!("city database is {:?} in {:?}", city.format, city.files);
//! }
//! # Ok(())
//! # }
//! ```

// Private, with every public item re-exported below: the module layout is not
// part of the API, so it can be rearranged without a major version. The table
// half is arranged the same way.
#[cfg(feature = "geoip-download")]
mod config;
#[cfg(feature = "geoip-download")]
pub(crate) mod download;

#[cfg(feature = "geoip-lookup")]
pub(crate) mod enricher;
#[cfg(feature = "geoip-lookup")]
mod extra;
#[cfg(feature = "geoip-lookup")]
mod private;
#[cfg(feature = "geoip-lookup")]
mod record;
#[cfg(feature = "geoip-lookup")]
mod refresh;

#[cfg(feature = "geoip-download")]
pub use config::{
    AutoDownloadConfig, GeoIpConfig, GeoIpProvider, ProviderChoice, ProviderSelection, ProviderTier,
};
#[cfg(feature = "geoip-download")]
pub use download::{
    Database, DatabaseFormat, Databases, GeoIpDownloadError, SourceTerms, ensure_databases,
    source_terms, validate,
};

#[cfg(feature = "geoip-lookup")]
pub use enricher::{
    CacheConfig, DatabaseBacking, DatabaseBackings, DatabasePaths, GeoIp, GeoIpLookupError,
};
#[cfg(feature = "geoip-lookup")]
pub use private::is_private;
#[cfg(feature = "geoip-lookup")]
pub use record::{ExtraFields, ExtraValue, FieldValue, GeoIpRecord};

/// Default ceiling, in bytes, under which a database is read into memory rather
/// than mapped.
///
/// 128 MiB clears every database measured -- sapics `origin-asn` at 10 MB,
/// IPinfo Lite at 23 MB, GeoLite2-City at 70 MB and DB-IP Lite at 100-120 MB
/// expanded -- leaving the 120 MB paid city builds as the first to cross it as
/// they grow.
pub(crate) const DEFAULT_RESIDENT_MAX_BYTES: u64 = 128 * 1024 * 1024;
