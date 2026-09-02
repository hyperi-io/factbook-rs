// Project:   factbook
// File:      src/geoip/mmdb_wire.rs
// Purpose:   The MaxMind DB encoding, for the tests that build their own file
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Whole MaxMind DB files, assembled byte by byte.
//!
//! Nothing in this dependency graph writes the format, a real provider database
//! is far too large to commit, and the databases MaxMind publishes for testing
//! hold neither the probe addresses nor a record shaped to defeat a bound. Two
//! suites therefore build their own: the content checks need a populated
//! database beside a blank one, and the bounds need a record that expands far
//! past anything a publisher writes.
//!
//! What they share is the encoding, and that is this module -- the control
//! bytes, the integer and container forms, and the tree-plus-metadata frame a
//! reader requires before it will open a file. What each suite builds out of it
//! stays with the suite that asserts about it.
//!
//! Each file is one search-tree node whose two records both point into the data
//! section, so every address the database is asked about resolves to one record.

/// Marker the metadata section opens with.
const MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

/// Zero bytes separating the search tree from the data section.
const SEPARATOR: [u8; 16] = [0; 16];

/// Nodes in the tree, which is the one node every address lands in.
const NODE_COUNT: u32 = 1;

/// Bits per record, which fixes a node at six bytes and a record at three.
const RECORD_SIZE: u16 = 24;

/// Type number of a UTF-8 string.
const STRING: u8 = 2;

/// Type number of a 16-bit unsigned integer.
const UINT16: u8 = 5;

/// Type number of a 32-bit unsigned integer.
const UINT32: u8 = 6;

/// Type number of a map, and the base every extended type counts from.
pub(super) const MAP: u8 = 7;

/// Type number of a 64-bit unsigned integer, which is an extended type.
const UINT64: u8 = 9;

/// Type number of an array, which is an extended type.
const ARRAY: u8 = 11;

/// Size field that says the real size follows in one trailing byte.
const SPILL_ONE: u8 = 0x1d;

/// Size field that says the real size follows in two trailing bytes.
const SPILL_TWO: u8 = 0x1e;

/// Address family a database declares it was built for.
///
/// A source covering IPv6 declares 6, which sends an IPv4 lookup through the
/// reader's v4-in-v6 mapping instead of straight down the tree.
pub(super) const IPV4: u16 = 4;

/// A whole database file around a data section whose record starts at `root`.
pub(super) fn database(data: &[u8], root: usize, database_type: &str, ip_version: u16) -> Vec<u8> {
    // A tree slot carries the data offset past the node count and the
    // separator, which is how the reader tells a record from a node.
    let slot = NODE_COUNT + u32::try_from(SEPARATOR.len()).unwrap() + u32::try_from(root).unwrap();

    let mut file = Vec::new();
    // One node: two records of three bytes, both the same data pointer.
    for _ in 0..2 {
        file.extend_from_slice(&slot.to_be_bytes()[1..]);
    }
    file.extend_from_slice(&SEPARATOR);
    file.extend_from_slice(data);
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

/// Control byte, plus the trailing bytes a size too large for it spills to.
pub(super) fn header(kind: u8, size: usize) -> Vec<u8> {
    match size {
        0..=28 => vec![(kind << 5) | u8::try_from(size).unwrap()],
        29..=284 => vec![(kind << 5) | SPILL_ONE, u8::try_from(size - 29).unwrap()],
        285..=65_819 => {
            let spilled = u16::try_from(size - 285).unwrap().to_be_bytes();
            vec![(kind << 5) | SPILL_TWO, spilled[0], spilled[1]]
        }
        _ => panic!("test values stay inside the two-byte size form"),
    }
}

/// Control byte plus the trailing byte an extended type is named by.
fn extended(kind: u8, size: usize) -> Vec<u8> {
    assert!(
        size < usize::from(SPILL_ONE),
        "extended values are small by construction"
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
pub(super) fn string(value: &str) -> Vec<u8> {
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
pub(super) fn uint32(value: u32) -> Vec<u8> {
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
pub(super) fn map(pairs: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = header(MAP, pairs.len());
    for (key, value) in pairs {
        out.extend_from_slice(&string(key));
        out.extend_from_slice(value);
    }
    out
}
