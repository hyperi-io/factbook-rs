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
//! traversal found, so this costs a record decode rather than a tree walk. It
//! runs on every miss because there is no database that needs it and no
//! database that does not -- every published GeoIP build carries fields outside
//! the typed set, the free ones included.

use std::fmt::{self, Write as _};

use compact_str::CompactString;
use maxminddb::LookupResult;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use tracing::debug;

use super::enricher::asn_number;
use super::record::{ExtraValue, GeoIpRecord};

/// Read every field of one record and keep the ones no typed field took.
pub(super) fn collect<S: AsRef<[u8]>>(result: &LookupResult<'_, S>, record: &mut GeoIpRecord) {
    let leaves = match result.decode::<Leaves>() {
        Ok(Some(leaves)) => leaves.0,
        Ok(None) => return,
        Err(e) => {
            // One unreadable record must not fail the event carrying it.
            debug!(error = %e, "record fields did not decode");
            return;
        }
    };

    record.extra.reserve(leaves.len());
    for (path, value) in leaves {
        if !typed(&path, &value, record) {
            record.extra.push(path, value);
        }
    }
}

/// Every leaf of one record, as a dotted path and an owned value.
struct Leaves(Vec<(CompactString, ExtraValue)>);

impl<'de> Deserialize<'de> for Leaves {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut leaves = Vec::new();
        let mut path = String::new();
        Walk {
            leaves: &mut leaves,
            path: &mut path,
        }
        .deserialize(deserializer)?;

        Ok(Self(leaves))
    }
}

/// One position in a record: where it sits, and where its leaves go.
struct Walk<'a> {
    /// Leaves found so far, in source order.
    leaves: &'a mut Vec<(CompactString, ExtraValue)>,

    /// Dotted path of the value about to be read.
    path: &'a mut String,
}

impl Walk<'_> {
    /// Record one leaf at the current path.
    fn leaf(self, value: ExtraValue) {
        self.leaves
            .push((CompactString::from(self.path.as_str()), value));
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

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(key) = map.next_key::<&str>()? {
            let parent = self.path.len();
            if parent != 0 {
                self.path.push('.');
            }
            self.path.push_str(key);

            map.next_value_seed(Walk {
                leaves: &mut *self.leaves,
                path: &mut *self.path,
            })?;
            self.path.truncate(parent);
        }

        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut index = 0usize;
        loop {
            let parent = self.path.len();
            if parent != 0 {
                self.path.push('.');
            }
            // Written straight onto the buffer, so an index costs no allocation.
            let _ = write!(self.path, "{index}");

            let element = seq.next_element_seed(Walk {
                leaves: &mut *self.leaves,
                path: &mut *self.path,
            })?;
            self.path.truncate(parent);

            if element.is_none() {
                return Ok(());
            }
            index += 1;
        }
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
