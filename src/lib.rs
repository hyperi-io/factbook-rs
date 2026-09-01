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
//! Acquisition turns out not to be a geo concern at all, so [`table`] opens the
//! same fetch-verify-replace path to any CSV or JSON a deployment names in its
//! own config, indexed by an address or by any column it has.
//!
//! # Features
//!
//! | feature | adds |
//! |---|---|
//! | `geoip` *(default)* | `geoip-download` + `geoip-lookup` |
//! | `geoip-download` | resolve, freshness-check, download, unpack, refresh, and [`table`] |
//! | `geoip-lookup` | mmap readers, the cache, [`GeoIpRecord`] |
//! | `metrics` | emit through the `metrics` facade |
//! | `vrl` | map a record into `vrl::value::ObjectMap` |
//!
//! # Databases are not bundled
//!
//! Nothing here ships a database. Files are fetched at runtime from the
//! provider you configure, under that provider's licence -- which differs by
//! provider, and in most cases requires attribution.
//! [`geoip::source_terms`] reports what a given selection commits a deployer
//! to, and is the authority the README's table defers to.

#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(unsafe_code)]
// The README's examples are compiled by `cargo test`, so an API change that
// outdates them fails the build rather than misleading a reader.
#![cfg_attr(
    all(doctest, feature = "geoip-download", feature = "geoip-lookup"),
    doc = include_str!("../README.md")
)]

#[cfg(any(feature = "geoip-download", feature = "geoip-lookup"))]
#[cfg_attr(
    docsrs,
    doc(cfg(any(feature = "geoip-download", feature = "geoip-lookup")))
)]
pub mod geoip;

// Acquisition, not geography: a table rides the download stack and needs no
// MMDB reader, so it is gated with the half of the crate that fetches.
#[cfg(feature = "geoip-download")]
#[cfg_attr(docsrs, doc(cfg(feature = "geoip-download")))]
pub mod table;

pub mod secret;

pub use secret::Secret;

/// Crate version, for the user-agent a provider sees.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
