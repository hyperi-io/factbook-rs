// Project:   factbook
// File:      src/lib.rs
// Purpose:   Crate root
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! IP enrichment end to end: provision the GeoIP databases, keep them fresh,
//! and look addresses up through a cache sized for skewed traffic.
//!
//! Reader crates leave provisioning to the caller, so every service grows its
//! own downloader and its own idea of where the file lives. The enrichment
//! tables in the log-shipping tools do the reading but hold no cache, so a hot
//! address costs a full database traversal every time it appears. This crate is
//! both halves.
//!
//! # Features
//!
//! | feature | adds |
//! |---|---|
//! | `geoip` *(default)* | `geoip-download` + `geoip-lookup` |
//! | `geoip-download` | resolve, freshness-check, download, unpack, refresh |
//! | `geoip-lookup` | mmap readers, the cache, [`GeoIpRecord`] |
//! | `metrics` | emit through the `metrics` facade |
//! | `vrl` | map a record into `vrl::value::ObjectMap` |
//!
//! # Databases are not bundled
//!
//! Nothing here ships a database. Files are fetched at runtime from the
//! provider you configure, under that provider's licence -- which differs by
//! provider, and in two cases requires attribution. See the provider table in
//! the README before shipping a default to someone else.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(unsafe_code)]

#[cfg(any(feature = "geoip-download", feature = "geoip-lookup"))]
#[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "geoip-download", feature = "geoip-lookup")))
)]
pub mod geoip;

pub mod secret;

pub use secret::Secret;

/// Crate version, for the user-agent a provider sees.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
