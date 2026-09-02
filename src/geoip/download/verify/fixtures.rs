// Project:   factbook
// File:      src/geoip/download/verify/fixtures.rs
// Purpose:   Minimal MaxMind DB files the content checks are exercised against
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! The databases the content checks are asked about.
//!
//! A real provider database is far too large to commit and the ones MaxMind
//! publishes for testing hold none of the probe addresses, so a file built here
//! is the only way to assert that the probe accepts a populated database as well
//! as refusing an empty one -- a probe that only ever refuses would silently
//! stop every deployment updating.
//!
//! The encoding is [`mmdb_wire`](crate::geoip::mmdb_wire); this is the records
//! put through it.

use crate::geoip::mmdb_wire::{IPV4, database, map, string, uint32};

/// The other family, for a source published over IPv6 space.
#[cfg(feature = "geoip-lookup")]
const IPV6: u16 = 6;

/// The record an ASN database answers with.
fn asn_record(number: u32, organisation: &str) -> Vec<u8> {
    map(&[
        ("autonomous_system_number", uint32(number)),
        ("autonomous_system_organization", string(organisation)),
    ])
}

/// The record a city database answers with, MaxMind's nested shape.
#[cfg(feature = "geoip-lookup")]
fn city_record(iso_code: &str) -> Vec<u8> {
    map(&[("country", map(&[("iso_code", string(iso_code))]))])
}

/// An ASN database answering every address with one number and one name.
///
/// An empty `organisation` is the `dbip-asn` defect: a valid record carrying a
/// number and a blank name.
pub(crate) fn asn_mmdb(number: u32, organisation: &str) -> Vec<u8> {
    database(&asn_record(number, organisation), 0, "GeoLite2-ASN", IPV4)
}

/// The same database, declared for IPv6 the way a source covering that space
/// publishes it.
#[cfg(feature = "geoip-lookup")]
pub(crate) fn asn_mmdb_v6(number: u32, organisation: &str) -> Vec<u8> {
    database(&asn_record(number, organisation), 0, "GeoLite2-ASN", IPV6)
}

/// A city database answering every address with one country, MaxMind's shape.
///
/// Only the probe reads it, and the probe needs a reader compiled in.
#[cfg(feature = "geoip-lookup")]
pub(crate) fn city_mmdb(iso_code: &str) -> Vec<u8> {
    database(&city_record(iso_code), 0, "GeoLite2-City", IPV4)
}

/// The same database, declared for IPv6.
#[cfg(feature = "geoip-lookup")]
pub(crate) fn city_mmdb_v6(iso_code: &str) -> Vec<u8> {
    database(&city_record(iso_code), 0, "GeoLite2-City", IPV6)
}

/// A city database naming the country at the top level, IPinfo Lite's shape.
///
/// Only the probe reads it, and the probe needs a reader compiled in.
#[cfg(feature = "geoip-lookup")]
pub(crate) fn flat_city_mmdb(country_code: &str) -> Vec<u8> {
    database(
        &map(&[
            ("country_code", string(country_code)),
            ("continent_code", string("OC")),
        ]),
        0,
        "ipinfo lite",
        IPV4,
    )
}
