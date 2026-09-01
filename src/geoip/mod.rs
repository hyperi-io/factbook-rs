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
//! [`config`] describes the databases; [`download`] provisions the files. The
//! entry point is [`ensure_databases`], and its contract -- non-fatal, stale
//! beats absent -- is documented on the [`download`] module. An app checks its
//! configuration with [`validate`] when it loads it, because provisioning
//! itself never fails loudly.
//!
//! [`enricher`] reads the provisioned files behind a cache, [`record`] is what
//! a lookup returns, and [`refresh`] swaps a reader when the file underneath it
//! changes.

#[cfg(feature = "geoip-download")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-download")))]
pub mod config;
#[cfg(feature = "geoip-download")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-download")))]
pub mod download;

#[cfg(feature = "geoip-lookup")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-lookup")))]
pub mod enricher;
#[cfg(feature = "geoip-lookup")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-lookup")))]
pub mod private;
#[cfg(feature = "geoip-lookup")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-lookup")))]
pub mod record;
/// Reader refresh, as an inherent `impl` on [`enricher::GeoIp`].
#[cfg(feature = "geoip-lookup")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-lookup")))]
pub mod refresh;

#[cfg(feature = "geoip-download")]
pub use config::{
    AutoDownloadConfig, GeoIpConfig, GeoIpProvider, ProviderChoice, ProviderSelection, ProviderTier,
};
#[cfg(feature = "geoip-download")]
pub use download::{
    Database, DatabaseFormat, Databases, GeoIpDownloadError, ensure_databases, validate,
};

#[cfg(feature = "geoip-lookup")]
pub use enricher::{CacheConfig, GeoIp, GeoIpLookupError};
#[cfg(feature = "geoip-lookup")]
pub use private::is_private;
#[cfg(feature = "geoip-lookup")]
pub use record::{FieldValue, GeoIpRecord};
