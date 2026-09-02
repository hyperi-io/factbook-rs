// Project:   factbook
// File:      src/geoip/download/verify/fixtures.rs
// Purpose:   Minimal MaxMind DB files the content checks are exercised against
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Whole MaxMind DB files, assembled byte by byte.
//!
//! No writer exists for the format in this dependency graph, a real provider
//! database is far too large to commit, and the databases MaxMind publishes for
//! testing hold none of the probe addresses. A file built here is the only way
//! to assert that the probe accepts a populated database as well as refusing an
//! empty one -- a probe that only ever refuses would silently stop every
//! deployment updating.
//!
//! Each file is one search-tree node whose two records both point at the single
//! data entry, so every address asked about resolves to that one record.

/// Marker the metadata section opens with.
const MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

/// Zero bytes separating the search tree from the data section.
const SEPARATOR: [u8; 16] = [0; 16];

/// Nodes in the tree, which is the one node every record points out of.
const NODE_COUNT: u32 = 1;

/// Bits per record, which fixes a node at six bytes and a record at three.
const RECORD_SIZE: u16 = 24;

/// Bytes of the separator, which the data pointer is offset past.
const SEPARATOR_LEN: u32 = 16;

/// Type number of a UTF-8 string.
const STRING: u8 = 2;

/// Type number of a 16-bit unsigned integer.
const UINT16: u8 = 5;

/// Type number of a 32-bit unsigned integer.
const UINT32: u8 = 6;

/// Type number of a map, and the base every extended type counts from.
const MAP: u8 = 7;

/// Type number of a 64-bit unsigned integer, which is an extended type.
const UINT64: u8 = 9;

/// Type number of an array, which is an extended type.
const ARRAY: u8 = 11;

/// Size field value that says the real size follows in a trailing byte.
const SIZE_SPILL: u8 = 29;

/// Address family a database declares it was built for.
///
/// A source covering IPv6 declares 6, which sends an IPv4 lookup through the
/// reader's v4-in-v6 mapping instead of straight down the tree.
const IPV4: u16 = 4;

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

/// An ASN database answering every address with one number and one name.
///
/// An empty `organisation` is the `dbip-asn` defect: a valid record carrying a
/// number and a blank name.
pub(crate) fn asn_mmdb(number: u32, organisation: &str) -> Vec<u8> {
    database(&asn_record(number, organisation), "GeoLite2-ASN", IPV4)
}

/// The same database, declared for IPv6 the way a source covering that space
/// publishes it.
#[cfg(feature = "geoip-lookup")]
pub(crate) fn asn_mmdb_v6(number: u32, organisation: &str) -> Vec<u8> {
    database(&asn_record(number, organisation), "GeoLite2-ASN", IPV6)
}

/// A city database answering every address with one country, MaxMind's shape.
pub(crate) fn city_mmdb(iso_code: &str) -> Vec<u8> {
    database(
        &map(&[("country", map(&[("iso_code", string(iso_code))]))]),
        "GeoLite2-City",
        IPV4,
    )
}

/// The same database, declared for IPv6.
#[cfg(feature = "geoip-lookup")]
pub(crate) fn city_mmdb_v6(iso_code: &str) -> Vec<u8> {
    database(
        &map(&[("country", map(&[("iso_code", string(iso_code))]))]),
        "GeoLite2-City",
        IPV6,
    )
}

/// A city database naming the country at the top level, IPinfo Lite's shape.
pub(crate) fn flat_city_mmdb(country_code: &str) -> Vec<u8> {
    database(
        &map(&[
            ("country_code", string(country_code)),
            ("continent_code", string("OC")),
        ]),
        "ipinfo lite",
        IPV4,
    )
}

/// A whole database file around one data record.
fn database(record: &[u8], database_type: &str, ip_version: u16) -> Vec<u8> {
    // The record value a tree slot carries for data at offset zero.
    let pointer = NODE_COUNT + SEPARATOR_LEN;

    let mut file = Vec::new();
    // One node: two records of three bytes, both the same data pointer.
    for _ in 0..2 {
        file.extend_from_slice(&pointer.to_be_bytes()[1..]);
    }
    file.extend_from_slice(&SEPARATOR);
    file.extend_from_slice(record);
    file.extend_from_slice(MARKER);
    file.extend_from_slice(&metadata(database_type, ip_version));
    file
}

/// The nine metadata fields a reader requires before it will open a file.
fn metadata(database_type: &str, ip_version: u16) -> Vec<u8> {
    map(&[
        ("binary_format_major_version", uint16(2)),
        ("binary_format_minor_version", uint16(0)),
        ("build_epoch", uint64(0)),
        ("database_type", string(database_type)),
        ("description", map(&[("en", string("fixture"))])),
        ("ip_version", uint16(ip_version)),
        ("languages", array(&[string("en")])),
        ("node_count", uint32(NODE_COUNT)),
        ("record_size", uint16(RECORD_SIZE)),
    ])
}

/// Control byte for a type whose number fits the top three bits.
///
/// A size of twenty-nine or more spills into a trailing byte, which is what
/// `autonomous_system_organization` needs at thirty characters.
fn header(kind: u8, size: usize) -> Vec<u8> {
    assert!(size < 285, "fixture values are small by construction");
    let spill = usize::from(SIZE_SPILL);
    if size < spill {
        vec![(kind << 5) | u8::try_from(size).unwrap()]
    } else {
        vec![
            (kind << 5) | SIZE_SPILL,
            u8::try_from(size - spill).unwrap(),
        ]
    }
}

/// Control byte plus the trailing byte an extended type is named by.
fn extended(kind: u8, size: usize) -> Vec<u8> {
    assert!(
        size < usize::from(SIZE_SPILL),
        "fixture values are small by construction"
    );
    vec![u8::try_from(size).unwrap(), kind - MAP]
}

/// Big-endian bytes with the leading zeros dropped, which is how the format
/// stores an integer.
fn trimmed(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|&byte| byte != 0).unwrap_or(8);
    bytes[first..].to_vec()
}

/// A UTF-8 string.
fn string(value: &str) -> Vec<u8> {
    let mut out = header(STRING, value.len());
    out.extend_from_slice(value.as_bytes());
    out
}

/// A 16-bit unsigned integer.
fn uint16(value: u16) -> Vec<u8> {
    let bytes = trimmed(u64::from(value));
    let mut out = header(UINT16, bytes.len());
    out.extend_from_slice(&bytes);
    out
}

/// A 32-bit unsigned integer.
fn uint32(value: u32) -> Vec<u8> {
    let bytes = trimmed(u64::from(value));
    let mut out = header(UINT32, bytes.len());
    out.extend_from_slice(&bytes);
    out
}

/// A 64-bit unsigned integer.
fn uint64(value: u64) -> Vec<u8> {
    let bytes = trimmed(value);
    let mut out = extended(UINT64, bytes.len());
    out.extend_from_slice(&bytes);
    out
}

/// An array, whose size field counts elements rather than bytes.
fn array(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = extended(ARRAY, items.len());
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// A map, whose size field counts pairs rather than bytes.
fn map(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = header(MAP, pairs.len());
    for (key, value) in pairs {
        out.extend_from_slice(&string(key));
        out.extend_from_slice(value);
    }
    out
}
