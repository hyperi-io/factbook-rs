// Project:   factbook
// File:      src/geoip/extra.rs
// Purpose:   Keep the record fields the typed shape has no slot for
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Everything in a database record that the typed fields do not name.
//!
//! [`GeoIpRecord`] names what every provider publishes. A database holds more
//! than that: GeoIP2-ISP has `isp` and `organization`, Anonymous-IP has its
//! flags, IPinfo's paid bundles have carrier and abuse contacts, and even a
//! free city build carries geoname ids and names in eight languages. All of it
//! used to be read and thrown away. This module reads the record again with no
//! schema at all and keeps whatever the typed decode did not take.
//!
//! # Flat paths, not a tree
//!
//! A value is keyed by the path it sits at, dotted through nested maps and
//! array indices -- `city.names.de`, `subdivisions.0.iso_code`. That is the
//! same shape [`to_schema_map`](GeoIpRecord::to_schema_map) already hands a
//! consumer, so an unmodelled field maps onto a schema column exactly the way a
//! typed one does. A value tree would need the consumer to walk it first.
//!
//! # A second decode, not a wider one
//!
//! The address is located once; both decodes read the record bytes the one
//! traversal found, so this costs a record decode rather than a tree walk. It is
//! not selected per database, because there is no database that needs it and no
//! database that does not -- every published GeoIP build carries fields outside
//! the typed set, the free ones included.
//!
//! What the fields cost is the cache rather than the walk: they take an entry
//! from a few hundred bytes to several kilobytes, so a deployment that wants the
//! smaller record turns the whole thing off with
//! [`collect_extra_fields`](super::CacheConfig::collect_extra_fields). Off, the
//! call below is not reached, so the second decode is saved along with the map.
//!
//! # Bounded, because a record is not trusted input
//!
//! The data section of a MaxMind DB is a pointer graph, not a tree. The reader
//! follows a pointer wherever it appears and caps only nesting depth, so a
//! record written as nested two-key maps whose branches both point at the level
//! below expands to two leaves per level: forty levels is a four-hundred-byte
//! file and a million million leaves. Whoever supplies the database supplies
//! the digest it is checked against, so a compromised provider or a hostile URL
//! puts that record inside the reader with nothing upstream to catch it.
//!
//! Two bounds hold that down, and one of them has to be counted in bytes.
//! [`MAX_FIELDS`] alone would not bound memory, because every leaf of a crafted
//! record can point at the same long string and each is copied on the way out;
//! [`MAX_BYTES`] is what makes the retained size finite whatever the leaves
//! hold.
//!
//! Both are enforced by stopping the walk, not by truncating a finished list.
//! The distinction is the whole point: a list truncated afterwards has already
//! cost the traversal that built it, so the memory would be bounded and the
//! time would not. A visitor that stops asking for entries stops the reader
//! following pointers, which bounds both at once.
//!
//! Reaching a bound truncates rather than fails, for the same reason an
//! unreadable record does not fail the event carrying it.

use std::fmt::{self, Write as _};
use std::sync::Once;

use compact_str::CompactString;
use maxminddb::LookupResult;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use tracing::{debug, warn};

use super::enricher::asn_number;
use super::record::{ExtraValue, GeoIpRecord};

/// Most fields one record's map keeps.
///
/// Set about three times above the richest record any provider publishes. A
/// free GeoLite2-City record runs to roughly 21 unmodelled fields; a paid
/// GeoIP2 Enterprise one reaches about 180 once names in a dozen languages
/// across eight named entities, confidence scores, several subdivisions and the
/// traits block are counted. Nothing published comes near 512, so no real
/// database is truncated by it.
const MAX_FIELDS: usize = 512;

/// Most bytes one record's map keeps, counting each path and what its value
/// holds on the heap.
///
/// The field cap does not bound this on its own: a leaf is an owned copy, and a
/// crafted record can point all of its leaves at one long string. The richest
/// real record accounts for well under 16 KiB here -- about 4 KiB of paths and
/// a few KiB of names -- so 64 KiB leaves the same order of headroom the field
/// cap does.
const MAX_BYTES: usize = 64 * 1024;

/// Warns once, so a hostile database costs one line rather than one per address.
static TRUNCATED: Once = Once::new();

/// Read every field of one record and keep the ones no typed field took.
pub(super) fn collect<S: AsRef<[u8]>>(result: &LookupResult<'_, S>, record: &mut GeoIpRecord) {
    let kept = match result.decode::<Leaves>() {
        Ok(Some(leaves)) => leaves.0,
        Ok(None) => return,
        Err(e) => {
            // One unreadable record must not fail the event carrying it.
            debug!(error = %e, "record fields did not decode");
            return;
        }
    };

    if kept.full() {
        truncated(result);
    }

    record.extra.reserve(kept.fields.len());
    for (path, value) in kept.fields {
        if !typed(&path, &value, record) {
            record.extra.push(path, value);
        }
    }
}

/// Say once that a record reached a bound, and which bounds those are.
///
/// The record is named by the network it matched and its offset in the data
/// section, which is what identifies it inside the file; the reader does not
/// offer the database's own name through a lookup result.
///
/// Latched because a record large enough to truncate is a property of the
/// database rather than of the address, so every address matching it would
/// repeat the line -- and a spray of addresses is exactly the traffic that
/// reaches this.
fn truncated<S: AsRef<[u8]>>(result: &LookupResult<'_, S>) {
    TRUNCATED.call_once(|| {
        warn!(
            network = ?result.network().ok(),
            offset = ?result.offset(),
            max_fields = MAX_FIELDS,
            max_bytes = MAX_BYTES,
            "database record carries more fields than the map holds; it was truncated"
        );
    });
}

/// Every leaf of one record, as a dotted path and an owned value.
struct Leaves(Kept);

/// The leaves kept so far, and what keeping them has cost.
struct Kept {
    /// Path and value, in source order.
    fields: Vec<(CompactString, ExtraValue)>,

    /// Bytes those fields retain: each path, and each value's heap payload.
    bytes: usize,

    /// Whether a bound has been reached, which is where the walk stops.
    full: bool,
}

impl Kept {
    /// Nothing kept yet.
    const fn new() -> Self {
        Self {
            fields: Vec::new(),
            bytes: 0,
            full: false,
        }
    }

    /// Whether a bound is reached, which is where the walk stops.
    const fn full(&self) -> bool {
        self.full
    }

    /// Keep one leaf, charged for its path and for what its value holds.
    ///
    /// A leaf that would carry the record past [`MAX_BYTES`] is dropped rather
    /// than kept and counted afterwards, which is what makes the bound the size
    /// it says it is: one value is as large as the record it was read from, so
    /// admitting it first and stopping second would bound nothing.
    fn push(&mut self, path: &str, value: ExtraValue) {
        let cost = path.len() + payload(&value);
        if self.bytes + cost > MAX_BYTES {
            self.full = true;
            return;
        }

        self.bytes += cost;
        self.fields.push((CompactString::from(path), value));
        self.full = self.fields.len() >= MAX_FIELDS;
    }
}

/// Bytes a value holds beyond the pair itself.
fn payload(value: &ExtraValue) -> usize {
    match value {
        ExtraValue::Str(text) => text.len(),
        ExtraValue::Bytes(bytes) => bytes.len(),
        _ => 0,
    }
}

impl<'de> Deserialize<'de> for Leaves {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut kept = Kept::new();
        let mut path = String::new();
        Walk {
            kept: &mut kept,
            path: &mut path,
        }
        .deserialize(deserializer)?;

        Ok(Self(kept))
    }
}

/// One position in a record: where it sits, and where its leaves go.
struct Walk<'a> {
    /// Leaves found so far, and what they have cost.
    kept: &'a mut Kept,

    /// Dotted path of the value about to be read.
    path: &'a mut String,
}

impl Walk<'_> {
    /// Record one leaf at the current path.
    fn leaf(self, value: ExtraValue) {
        self.kept.push(self.path, value);
    }
}

impl<'de> DeserializeSeed<'de> for Walk<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

/// Answers every value kind the database format can hold at a leaf, so a field
/// is kept as what the source wrote rather than refused for its type.
impl<'de> Visitor<'de> for Walk<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value a database record can hold")
    }

    fn visit_bool<E>(self, value: bool) -> Result<(), E> {
        self.leaf(ExtraValue::Bool(value));
        Ok(())
    }

    fn visit_i64<E>(self, value: i64) -> Result<(), E> {
        self.leaf(ExtraValue::from_i64(value));
        Ok(())
    }

    fn visit_u64<E>(self, value: u64) -> Result<(), E> {
        self.leaf(ExtraValue::UInt(value));
        Ok(())
    }

    fn visit_u128<E>(self, value: u128) -> Result<(), E> {
        self.leaf(ExtraValue::from_u128(value));
        Ok(())
    }

    fn visit_f64<E>(self, value: f64) -> Result<(), E> {
        self.leaf(ExtraValue::Float(value));
        Ok(())
    }

    fn visit_str<E>(self, value: &str) -> Result<(), E> {
        self.leaf(ExtraValue::Str(CompactString::from(value)));
        Ok(())
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<(), E> {
        self.leaf(ExtraValue::Bytes(value.to_vec()));
        Ok(())
    }

    /// Stops asking once a bound is reached, which is what bounds the decode
    /// as well as the record: an entry never requested is never followed.
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while !self.kept.full() {
            let Some(key) = map.next_key::<&str>()? else {
                break;
            };

            let parent = self.path.len();
            if parent != 0 {
                self.path.push('.');
            }
            self.path.push_str(key);

            map.next_value_seed(Walk {
                kept: &mut *self.kept,
                path: &mut *self.path,
            })?;
            self.path.truncate(parent);
        }

        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0usize;
        while !self.kept.full() {
            let parent = self.path.len();
            if parent != 0 {
                self.path.push('.');
            }
            // Written straight onto the buffer, so an index costs no allocation.
            let _ = write!(self.path, "{index}");

            let element = seq.next_element_seed(Walk {
                kept: &mut *self.kept,
                path: &mut *self.path,
            })?;
            self.path.truncate(parent);

            if element.is_none() {
                break;
            }
            index += 1;
        }

        Ok(())
    }
}

/// Whether the typed decode already took this field.
///
/// The test is on the value rather than on the path alone, so a path the typed
/// decode reads but did not keep stays in the map: a subdivision a more
/// specific one superseded, or a value whose wire type the typed shape refused.
fn typed(path: &str, value: &ExtraValue, record: &GeoIpRecord) -> bool {
    match path {
        "city.names.en" => is_str(value, record.city_name.as_deref()),
        "continent.code" | "continent_code" => is_str(value, record.continent_code.as_deref()),
        "continent.names.en" | "continent" => is_str(value, record.continent_name.as_deref()),
        "country.iso_code" | "country_code" => is_str(value, record.country_code.as_deref()),
        "country.names.en" | "country" => is_str(value, record.country_name.as_deref()),
        "postal.code" => is_str(value, record.postal_code.as_deref()),
        "location.time_zone" => is_str(value, record.timezone.as_deref()),
        "location.latitude" => is_float(value, record.latitude),
        "location.longitude" => is_float(value, record.longitude),
        "location.metro_code" => is_uint(value, record.metro_code.map(u64::from)),
        "location.accuracy_radius" => is_uint(value, record.accuracy_radius.map(u64::from)),
        "autonomous_system_number" => {
            is_uint(value, record.autonomous_system_number.map(u64::from))
        }
        "autonomous_system_organization" | "as_name" => {
            is_str(value, record.autonomous_system_organization.as_deref())
        }
        "as_domain" => is_str(value, record.as_domain.as_deref()),
        // IPinfo writes `AS15169` where the record holds 15169, so the match is
        // against what this path resolved to rather than against what it holds.
        "asn" => {
            record.autonomous_system_number.is_some()
                && value.as_str().and_then(asn_number) == record.autonomous_system_number
        }
        _ => match subdivision(path) {
            Some("names.en") => is_str(value, record.region_name.as_deref()),
            Some("iso_code") => is_str(value, record.region_code.as_deref()),
            _ => false,
        },
    }
}

/// The field of a subdivision entry, when the path names one.
fn subdivision(path: &str) -> Option<&str> {
    let (index, field) = path.strip_prefix("subdivisions.")?.split_once('.')?;
    index
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then_some(field)
}

/// Whether the record already holds this text.
fn is_str(value: &ExtraValue, held: Option<&str>) -> bool {
    matches!((value.as_str(), held), (Some(value), Some(held)) if value == held)
}

/// Whether the record already holds this whole number.
fn is_uint(value: &ExtraValue, held: Option<u64>) -> bool {
    matches!((value, held), (ExtraValue::UInt(value), Some(held)) if *value == held)
}

/// Compared bit for bit, both sides being one wire double decoded twice.
fn is_float(value: &ExtraValue, held: Option<f64>) -> bool {
    matches!((value, held), (ExtraValue::Float(value), Some(held)) if value.to_bits() == held.to_bits())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record with the typed fields a collision rule is checked against.
    fn located() -> GeoIpRecord {
        GeoIpRecord {
            city_name: Some(CompactString::from("Boxford")),
            region_name: Some(CompactString::from("West Berkshire")),
            latitude: Some(51.75),
            accuracy_radius: Some(100),
            autonomous_system_number: Some(15169),
            ..GeoIpRecord::default()
        }
    }

    #[test]
    fn a_path_the_typed_decode_took_is_not_repeated() {
        let record = located();

        assert!(typed(
            "city.names.en",
            &ExtraValue::Str(CompactString::from("Boxford")),
            &record
        ));
        assert!(typed(
            "location.accuracy_radius",
            &ExtraValue::UInt(100),
            &record
        ));
        assert!(typed(
            "location.latitude",
            &ExtraValue::Float(51.75),
            &record
        ));
    }

    #[test]
    fn a_path_no_typed_field_names_is_kept() {
        let record = located();

        assert!(!typed(
            "city.names.de",
            &ExtraValue::Str(CompactString::from("Boxford")),
            &record
        ));
        assert!(!typed(
            "city.geoname_id",
            &ExtraValue::UInt(2_655_045),
            &record
        ));
    }

    #[test]
    fn a_superseded_subdivision_is_kept_and_the_specific_one_is_not() {
        let record = located();

        // England lost the region field to West Berkshire, so it is only in the
        // map that it survives at all.
        assert!(!typed(
            "subdivisions.0.names.en",
            &ExtraValue::Str(CompactString::from("England")),
            &record
        ));
        assert!(typed(
            "subdivisions.1.names.en",
            &ExtraValue::Str(CompactString::from("West Berkshire")),
            &record
        ));
    }

    #[test]
    fn a_value_the_typed_decode_could_not_read_is_kept() {
        // A radius the record has no value for was not taken, whatever the path
        // says, so the map is what stops it being lost.
        let record = GeoIpRecord::default();

        assert!(!typed(
            "location.accuracy_radius",
            &ExtraValue::UInt(100),
            &record
        ));
    }

    #[test]
    fn the_as_prefixed_asn_matches_the_number_it_resolved_to() {
        let record = located();

        assert!(typed(
            "asn",
            &ExtraValue::Str(CompactString::from("AS15169")),
            &record
        ));
        assert!(!typed(
            "asn",
            &ExtraValue::Str(CompactString::from("AS13335")),
            &record
        ));
    }

    #[test]
    fn a_subdivision_path_is_read_only_when_it_carries_an_index() {
        assert_eq!(subdivision("subdivisions.0.iso_code"), Some("iso_code"));
        assert_eq!(subdivision("subdivisions.12.names.en"), Some("names.en"));
        assert_eq!(subdivision("subdivisions.x.iso_code"), None);
        assert_eq!(subdivision("subdivisions.0"), None);
        assert_eq!(subdivision("city.names.en"), None);
    }
}

/// Records built to defeat the bounds, in whole MaxMind DB files.
///
/// A pointer graph is not something any writer will emit and not something to
/// commit as a fixture, so the hostile record is built here from the format's
/// own encoding -- [`mmdb_wire`](crate::geoip::mmdb_wire) -- and thrown away
/// with the temporary file it is written to.
#[cfg(test)]
mod crafted {
    use crate::geoip::mmdb_wire::{IPV4, MAP, database, header, string};

    /// Database type every crafted file declares, so the typed decode in front
    /// of the walk reads it as the schema the bound is measured on.
    const DATABASE_TYPE: &str = "GeoLite2-City";

    /// Type number of a pointer into the data section.
    const POINTER: u8 = 1;

    /// Largest offset the two-byte pointer form reaches.
    const POINTER_LIMIT: usize = 2048;

    /// A pointer to a data-section offset, in the two-byte form.
    fn pointer(target: usize) -> Vec<u8> {
        assert!(target < POINTER_LIMIT, "crafted records stay under 2 KiB");
        vec![
            (POINTER << 5) | u8::try_from(target >> 8).unwrap(),
            u8::try_from(target & 0xff).unwrap(),
        ]
    }

    /// A database whose one record expands to `2 ^ levels` copies of `leaf`.
    ///
    /// Each level is a two-key map whose branches are both pointers to the level
    /// below, so the record doubles per level while the file grows by nine
    /// bytes. `leaf` is stored once and reached down every path, which is what
    /// makes a small file able to retain a large record. Every address the
    /// database is asked about resolves to it.
    pub(super) fn pointer_bomb(levels: u32, leaf: &str) -> Vec<u8> {
        let mut data = string(leaf);
        let mut below = 0;

        for _ in 0..levels {
            let here = data.len();
            let branch = pointer(below);
            data.extend_from_slice(&header(MAP, 2));
            data.extend_from_slice(&string("a"));
            data.extend_from_slice(&branch);
            data.extend_from_slice(&string("b"));
            data.extend_from_slice(&branch);
            below = here;
        }

        database(&data, below, DATABASE_TYPE, IPV4)
    }

    /// A database whose one record is a map of `fields` distinct leaves.
    ///
    /// The honest shape of a rich record, for the bound's edges: no pointer, no
    /// sharing, and every leaf under its own key.
    pub(super) fn wide_record(fields: usize, value: &str) -> Vec<u8> {
        let mut data = header(MAP, fields);
        for index in 0..fields {
            data.extend_from_slice(&string(&format!("f{index}")));
            data.extend_from_slice(&string(value));
        }

        database(&data, 0, DATABASE_TYPE, IPV4)
    }
}

/// What the bounds hold, against records built to defeat them.
#[cfg(test)]
mod bounds {
    use std::io::Write as _;
    use std::net::IpAddr;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use tempfile::NamedTempFile;

    use super::crafted;
    use super::{ExtraValue, MAX_BYTES, MAX_FIELDS};
    use crate::geoip::{CacheConfig, DatabasePaths, GeoIp, GeoIpRecord};
    use crate::maxminddb::Reader;

    /// The City schema as a current build writes it, the richest real record the
    /// suite holds.
    const CITY_RICH_DB: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/city-rich-test.mmdb"
    );

    /// The address that database holds the most fields for.
    const BOXFORD: &str = "2.125.160.216";

    /// Long enough that a walk which stopped counting fields alone would still
    /// retain megabytes.
    const LONG_LEAF_BYTES: usize = 200;

    /// Nesting that expands to 2^45 leaves -- a walk that follows every pointer
    /// does not finish this side of a year.
    const DEEP: u32 = 45;

    /// Generous by three orders of magnitude against the sub-millisecond walk,
    /// and unreachable for one that is not bounded, so it fails on the defect
    /// rather than on a slow machine.
    const BUDGET: Duration = Duration::from_secs(2);

    /// Parse a literal the tests are asserting about.
    fn ip(literal: &str) -> IpAddr {
        literal.parse().unwrap()
    }

    /// Write a crafted database out, keeping the file alive for the reader.
    fn written(bytes: &[u8]) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(bytes).unwrap();
        file.flush().unwrap();
        file
    }

    /// Collect one crafted record, timing the walk on its own.
    ///
    /// Driven straight at the reader rather than through a lookup, because the
    /// typed decode in front of it walks the same pointer graph and would put
    /// its own cost in the way of the bound being measured.
    fn collected(bytes: &[u8]) -> (GeoIpRecord, Duration) {
        let file = written(bytes);
        let reader = Reader::open_readfile(file.path()).unwrap();
        let result = reader.lookup(ip("1.2.3.4")).unwrap();

        let mut record = GeoIpRecord::default();
        let started = Instant::now();
        super::collect(&result, &mut record);

        (record, started.elapsed())
    }

    /// Bytes the map retains: each path, and each value's heap payload.
    fn retained(record: &GeoIpRecord) -> usize {
        record
            .extra
            .iter()
            .map(|(path, value)| path.len() + super::payload(value))
            .sum()
    }

    #[test]
    fn a_pointer_graph_cannot_grow_the_map_or_stall_the_walk() {
        let bytes = crafted::pointer_bomb(DEEP, "x");
        // The whole hostile database is smaller than this source file.
        assert!(bytes.len() < 1024, "{}", bytes.len());

        let (record, elapsed) = collected(&bytes);

        // Unbounded this is 2^45 fields; the walk stops asking at the cap.
        assert_eq!(record.extra.len(), MAX_FIELDS);
        // Bounded in time as well as in size, which is what stopping the visitor
        // rather than truncating a finished list buys.
        assert!(elapsed < BUDGET, "{elapsed:?}");
    }

    #[test]
    fn leaves_sharing_one_long_string_are_bounded_in_bytes_not_just_in_count() {
        // Every leaf points at the same string, so the file stays tiny while
        // each field kept copies it. A count cap alone would let 512 of them
        // through; the byte cap is what stops short of that.
        let leaf = "v".repeat(LONG_LEAF_BYTES);
        let bytes = crafted::pointer_bomb(DEEP, &leaf);
        assert!(bytes.len() < 1024, "{}", bytes.len());

        let (record, elapsed) = collected(&bytes);

        assert!(retained(&record) <= MAX_BYTES, "{}", retained(&record));
        assert!(record.extra.len() < MAX_FIELDS, "{}", record.extra.len());
        assert!(elapsed < BUDGET, "{elapsed:?}");
    }

    #[test]
    fn a_crafted_record_cannot_stall_a_lookup() {
        // Shallower than the walk's own test: the typed decode runs first and
        // follows the same pointers, so this depth is what that half tolerates.
        // The bound under test is the field count, which is exact either way.
        let bytes = crafted::pointer_bomb(20, "x");
        let file = written(&bytes);

        let geoip = GeoIp::open(
            DatabasePaths::city_only(file.path()),
            CacheConfig::default(),
        )
        .unwrap();

        let started = Instant::now();
        let record = geoip.lookup(ip("1.2.3.4")).unwrap();
        let elapsed = started.elapsed();

        // A million and a half fields before the bound, and the answer is still
        // an answer rather than an error.
        assert_eq!(record.extra.len(), MAX_FIELDS);
        assert!(elapsed < BUDGET, "{elapsed:?}");
    }

    #[test]
    fn an_honest_record_over_the_cap_is_truncated_rather_than_refused() {
        let bytes = crafted::wide_record(MAX_FIELDS + 100, "v");

        let (record, _) = collected(&bytes);

        // Truncated, not failed: the fields that fit are still delivered.
        assert_eq!(record.extra.len(), MAX_FIELDS);
        assert_eq!(record.extra.get("f0").unwrap().as_str(), Some("v"));
    }

    #[test]
    fn an_honest_record_under_the_cap_keeps_every_field() {
        let fields = MAX_FIELDS - 1;
        let bytes = crafted::wide_record(fields, "v");

        let (record, _) = collected(&bytes);

        assert_eq!(record.extra.len(), fields);
    }

    #[test]
    fn the_richest_published_record_is_nowhere_near_the_bounds() {
        let geoip = GeoIp::open(
            DatabasePaths::city_only(Path::new(CITY_RICH_DB)),
            CacheConfig::default(),
        )
        .unwrap();
        let record = geoip.lookup(ip(BOXFORD)).unwrap();

        // The bound exists to be unreachable for a real database. A current
        // city build with geoname ids, confidence scores, traits and names in
        // several languages sits an order of magnitude below both caps.
        assert!(
            record.extra.len() * 8 < MAX_FIELDS,
            "{}",
            record.extra.len()
        );
        assert!(retained(&record) * 8 < MAX_BYTES, "{}", retained(&record));

        // Nothing the earlier tests assert about this database went missing.
        assert_eq!(
            record
                .extra
                .get("city.names.de")
                .and_then(ExtraValue::as_str),
            Some("Boxford")
        );
        assert_eq!(
            record
                .extra
                .get("traits.user_type")
                .and_then(ExtraValue::as_str),
            Some("residential")
        );
        assert!(record.extra.get("city.geoname_id").is_some());
        assert!(record.extra.get("subdivisions.1.confidence").is_some());
    }
}
