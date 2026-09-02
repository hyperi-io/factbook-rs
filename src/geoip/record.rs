// Project:   factbook
// File:      src/geoip/record.rs
// Purpose:   The enrichment record one address lookup answers with
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! What a GeoIP lookup answers with.
//!
//! The field names are the ones the log-shipping tools already emit, so a
//! pipeline that swaps its enrichment table for this crate keeps its downstream
//! schema. Nothing here is renamed for internal taste.
//!
//! # Two network fields
//!
//! One record merges two databases, and the city database and the ASN database
//! match an address at different prefixes. Reporting a single "the" network
//! would have to discard one of the two real answers, so both are carried:
//! [`network`](GeoIpRecord::network) is the city match and
//! [`asn_network`](GeoIpRecord::asn_network) the ASN match.
//!
//! # Everything else the source carried
//!
//! A database holds more than this record names. GeoIP2-ISP has `isp` and
//! `organization`, Anonymous-IP has its flags, and even a free city build
//! carries geoname ids, confidence scores and names in eight languages. All of
//! it lands in [`extra`](GeoIpRecord::extra), keyed by the path it sits at in
//! the source record, so a paid edition or a new provider delivers its fields
//! without this record being widened for them. They are also what makes a record
//! several kilobytes rather than a few hundred bytes, so a deployment weighing
//! the cache against the fields drops them with
//! [`collect_extra_fields`](super::CacheConfig::collect_extra_fields).

use std::fmt;
use std::slice;
use std::sync::{Arc, LazyLock};

use compact_str::CompactString;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Number of typed fields a record can carry, to size the schema map in one go.
const TYPED_FIELDS: usize = 19;

/// Entries reserved up front from a deserialiser's size hint.
///
/// A hint is what the encoding claims, not what it holds, and a re-read parses
/// whatever a consumer hands back: a claim of sixteen million would otherwise be
/// a sixteen-million-entry allocation before the first element is read. Honest
/// maps larger than this still grow into place, so the clamp costs one or two
/// reallocations on a record no provider publishes.
const HINT_RESERVE: usize = 128;

/// The answer for an address that cannot have a geolocation, built once and
/// shared by every lookup that short-circuits.
static PRIVATE: LazyLock<Arc<GeoIpRecord>> = LazyLock::new(|| Arc::new(GeoIpRecord::private()));

/// One typed value in a [`to_schema_map`](GeoIpRecord::to_schema_map) pair.
///
/// A latitude is a number and a country code is a string, so the map keeps the
/// distinction rather than rendering everything as text for the consumer to
/// parse back.
///
/// The variants cover every value the MaxMind DB format can hold at a leaf, so
/// a field reaches a consumer as what the source wrote rather than as the
/// nearest thing this vocabulary could express.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum FieldValue<'a> {
    /// A text field, borrowed from the record.
    Str(&'a str),
    /// A coordinate, or any other fractional number.
    Float(f64),
    /// A whole number: an ASN, a metro code, an accuracy radius.
    UInt(u64),
    /// A whole number a source wrote as signed.
    Int(i64),
    /// The widest whole number the database format carries.
    UInt128(u128),
    /// A flag.
    Bool(bool),
    /// An opaque value, borrowed from the record.
    Bytes(&'a [u8]),
}

/// One value a source carried, owned by the record holding it.
///
/// The same vocabulary as [`FieldValue`], owned rather than borrowed: a record
/// is cached and outlives the reader it was decoded from, so it cannot point
/// into the database file.
///
/// A whole number arrives as the narrowest of the three integer kinds that
/// holds it, so a value reads the same way whichever width the source wrote it
/// at: [`UInt`](Self::UInt) for anything from zero to [`u64::MAX`],
/// [`Int`](Self::Int) for a negative, and [`UInt128`](Self::UInt128) only above
/// [`u64::MAX`]. JSON has no integer that wide, so that last kind is the one
/// value a JSON round trip widens to a double.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ExtraValue {
    /// A text field.
    Str(CompactString),
    /// A coordinate, or any other fractional number.
    Float(f64),
    /// A whole number.
    UInt(u64),
    /// A whole number a source wrote as signed.
    Int(i64),
    /// The widest whole number the database format carries.
    UInt128(u128),
    /// A flag.
    Bool(bool),
    /// An opaque value.
    Bytes(Vec<u8>),
}

impl ExtraValue {
    /// A signed whole number as the narrowest kind that holds it.
    pub(super) fn from_i64(value: i64) -> Self {
        u64::try_from(value).map_or(Self::Int(value), Self::UInt)
    }

    /// A wide whole number as the narrowest kind that holds it.
    pub(super) fn from_u128(value: u128) -> Self {
        u64::try_from(value).map_or(Self::UInt128(value), Self::UInt)
    }

    /// The value as a borrowed [`FieldValue`], for the schema map.
    #[must_use]
    pub fn as_field(&self) -> FieldValue<'_> {
        match self {
            Self::Str(value) => FieldValue::Str(value),
            Self::Float(value) => FieldValue::Float(*value),
            Self::UInt(value) => FieldValue::UInt(*value),
            Self::Int(value) => FieldValue::Int(*value),
            Self::UInt128(value) => FieldValue::UInt128(*value),
            Self::Bool(value) => FieldValue::Bool(*value),
            Self::Bytes(value) => FieldValue::Bytes(value),
        }
    }

    /// The text this value holds, when it is text.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }
}

/// Reads whichever type the source wrote, since the value is only known once
/// the wire has been read.
impl<'de> Deserialize<'de> for ExtraValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ExtraValueVisitor)
    }
}

/// Builds an [`ExtraValue`] from whatever the deserialiser hands over.
struct ExtraValueVisitor;

impl<'de> Visitor<'de> for ExtraValueVisitor {
    type Value = ExtraValue;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string, number, boolean or byte string")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(ExtraValue::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(ExtraValue::from_i64(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(ExtraValue::UInt(value))
    }

    fn visit_u128<E>(self, value: u128) -> Result<Self::Value, E> {
        Ok(ExtraValue::from_u128(value))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(ExtraValue::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(ExtraValue::Str(CompactString::from(value)))
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E> {
        Ok(ExtraValue::Bytes(value.to_vec()))
    }

    /// JSON has no byte string, so a re-read arrives as a sequence of numbers.
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or_default().min(HINT_RESERVE));
        while let Some(byte) = seq.next_element::<u8>()? {
            bytes.push(byte);
        }
        Ok(ExtraValue::Bytes(bytes))
    }
}

/// Fields a source carried that the record has no typed field for.
///
/// A key is the path the value sits at in the source record, dotted through
/// nested maps and array indices -- `city.names.de`, `subdivisions.0.iso_code`,
/// `isp` -- so it maps onto a schema column or an event field as directly as a
/// typed field's name does. Entries stay in the order the source wrote them.
///
/// Nothing here is renamed, unit-converted or otherwise interpreted: a source
/// field arrives under its own name, carrying the type the source wrote.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtraFields {
    /// Path and value, in source order.
    fields: Vec<(CompactString, ExtraValue)>,
}

impl ExtraFields {
    /// No fields.
    #[must_use]
    pub const fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// The value at a path, absent when the source carried no such field.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&ExtraValue> {
        self.fields
            .iter()
            .find(|(key, _)| key == path)
            .map(|(_, value)| value)
    }

    /// Every path and value, in source order.
    pub fn iter(&self) -> slice::Iter<'_, (CompactString, ExtraValue)> {
        self.fields.iter()
    }

    /// How many fields are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether the source carried nothing outside the typed fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Add one field the source carried.
    pub(super) fn push(&mut self, path: CompactString, value: ExtraValue) {
        self.fields.push((path, value));
    }

    /// Make room for a database's worth of fields in one allocation.
    pub(super) fn reserve(&mut self, additional: usize) {
        self.fields.reserve(additional);
    }
}

impl<'a> IntoIterator for &'a ExtraFields {
    type Item = &'a (CompactString, ExtraValue);
    type IntoIter = slice::Iter<'a, (CompactString, ExtraValue)>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Written as an object, so a path is a key rather than the first half of a
/// two-element array.
impl Serialize for ExtraFields {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (path, value) in &self.fields {
            map.serialize_entry(path.as_str(), value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ExtraFields {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ExtraFieldsVisitor)
    }
}

/// Reads the object [`ExtraFields`] writes.
struct ExtraFieldsVisitor;

impl<'de> Visitor<'de> for ExtraFieldsVisitor {
    type Value = ExtraFields;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a map of source paths to values")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut fields = Vec::with_capacity(map.size_hint().unwrap_or_default().min(HINT_RESERVE));
        while let Some(entry) = map.next_entry::<CompactString, ExtraValue>()? {
            fields.push(entry);
        }
        Ok(ExtraFields { fields })
    }
}

/// Everything one address lookup resolved.
///
/// String fields are [`CompactString`]: a city, country, timezone or
/// subdivision is nearly always under 24 bytes, which is stored inline with no
/// heap allocation per field per record.
///
/// Every field but [`is_private`](Self::is_private) is optional, because a
/// database answers with whatever it holds for the matched network and the free
/// tiers hold less than the paid ones.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GeoIpRecord {
    /// City name, in English.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city_name: Option<CompactString>,

    /// Two-letter continent code, such as `EU`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continent_code: Option<CompactString>,

    /// Continent name, in English.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continent_name: Option<CompactString>,

    /// ISO 3166-1 alpha-2 country code, such as `GB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_code: Option<CompactString>,

    /// Country name, in English.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country_name: Option<CompactString>,

    /// Name of the most specific subdivision, in English.
    ///
    /// Taken from the last entry of the subdivision list, not the first: the
    /// database orders them largest to smallest, so Boxford reports West
    /// Berkshire rather than England.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_name: Option<CompactString>,

    /// ISO 3166-2 code of the same subdivision the name came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region_code: Option<CompactString>,

    /// Postal code, which several countries publish only in part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<CompactString>,

    /// IANA time zone name, such as `Europe/London`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<CompactString>,

    /// Approximate latitude of the matched network, not of a household.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,

    /// Approximate longitude of the matched network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,

    /// Metro code, which MaxMind no longer maintains and newer builds omit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metro_code: Option<u16>,

    /// Radius in kilometres the coordinates are accurate to at 67% confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_radius: Option<u16>,

    /// Autonomous system number the address routes under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomous_system_number: Option<u32>,

    /// Organisation the autonomous system is registered to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomous_system_organization: Option<CompactString>,

    /// Whether the address is private or reserved, and so has no geolocation.
    ///
    /// Always present, because "we did not look this up" is itself the answer.
    pub is_private: bool,

    /// CIDR the city database matched the address at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<CompactString>,

    /// CIDR the ASN database matched the address at, which is a different
    /// prefix from [`network`](Self::network) more often than not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn_network: Option<CompactString>,

    /// Domain the operating network is known by, where a source carries one.
    ///
    /// Distinct from the operator's name: `google.com` against `Google LLC`.
    /// IPinfo publishes it beside the ASN, and MaxMind sells it as its own
    /// edition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_domain: Option<CompactString>,

    /// Every other field the databases carried, under its own source path.
    ///
    /// This is what a paid edition, a richer free build or a provider nobody
    /// has written a field for still delivers: no field above has to exist for
    /// a value to reach a consumer.
    #[serde(default, skip_serializing_if = "ExtraFields::is_empty")]
    pub extra: ExtraFields,
}

impl GeoIpRecord {
    /// The record for an address that cannot have a geolocation answer.
    ///
    /// Every geographic field is absent and [`is_private`](Self::is_private) is
    /// set, which is what distinguishes "reserved address" from "looked up and
    /// found nothing".
    #[must_use]
    pub const fn private() -> Self {
        Self {
            city_name: None,
            continent_code: None,
            continent_name: None,
            country_code: None,
            country_name: None,
            region_name: None,
            region_code: None,
            postal_code: None,
            timezone: None,
            latitude: None,
            longitude: None,
            metro_code: None,
            accuracy_radius: None,
            autonomous_system_number: None,
            autonomous_system_organization: None,
            is_private: true,
            network: None,
            asn_network: None,
            as_domain: None,
            extra: ExtraFields::new(),
        }
    }

    /// The shared [`private`](Self::private) record.
    ///
    /// Private traffic is a large share of most feeds, so the short-circuit
    /// hands back a clone of one allocation rather than building a record per
    /// address.
    #[must_use]
    pub fn private_shared() -> Arc<Self> {
        Arc::clone(&PRIVATE)
    }

    /// The record as flat key and value pairs, absent fields omitted.
    ///
    /// The keys are the field names, and then the source paths of
    /// [`extra`](Self::extra), so the pairs map onto a schema column or an event
    /// field without a translation table in between. Every key and string is
    /// borrowed from the record, so the only allocation is the vector itself.
    #[must_use]
    pub fn to_schema_map(&self) -> Vec<(&str, FieldValue<'_>)> {
        let mut fields = Vec::with_capacity(TYPED_FIELDS + self.extra.len());

        push(&mut fields, "city_name", self.city_name.as_deref());
        push(
            &mut fields,
            "continent_code",
            self.continent_code.as_deref(),
        );
        push(
            &mut fields,
            "continent_name",
            self.continent_name.as_deref(),
        );
        push(&mut fields, "country_code", self.country_code.as_deref());
        push(&mut fields, "country_name", self.country_name.as_deref());
        push(&mut fields, "region_name", self.region_name.as_deref());
        push(&mut fields, "region_code", self.region_code.as_deref());
        push(&mut fields, "postal_code", self.postal_code.as_deref());
        push(&mut fields, "timezone", self.timezone.as_deref());
        push(&mut fields, "latitude", self.latitude);
        push(&mut fields, "longitude", self.longitude);
        push(&mut fields, "metro_code", self.metro_code.map(u32::from));
        push(
            &mut fields,
            "accuracy_radius",
            self.accuracy_radius.map(u32::from),
        );
        push(
            &mut fields,
            "autonomous_system_number",
            self.autonomous_system_number,
        );
        push(
            &mut fields,
            "autonomous_system_organization",
            self.autonomous_system_organization.as_deref(),
        );
        fields.push(("is_private", FieldValue::Bool(self.is_private)));
        push(&mut fields, "network", self.network.as_deref());
        push(&mut fields, "asn_network", self.asn_network.as_deref());
        push(&mut fields, "as_domain", self.as_domain.as_deref());

        for (path, value) in &self.extra {
            fields.push((path.as_str(), value.as_field()));
        }

        fields
    }
}

impl<'a> From<&'a str> for FieldValue<'a> {
    fn from(value: &'a str) -> Self {
        Self::Str(value)
    }
}

impl From<f64> for FieldValue<'_> {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<u32> for FieldValue<'_> {
    fn from(value: u32) -> Self {
        Self::UInt(u64::from(value))
    }
}

impl From<u64> for FieldValue<'_> {
    fn from(value: u64) -> Self {
        Self::UInt(value)
    }
}

/// Append a field when the lookup resolved one.
fn push<'a, T: Into<FieldValue<'a>>>(
    fields: &mut Vec<(&'a str, FieldValue<'a>)>,
    key: &'static str,
    value: Option<T>,
) {
    if let Some(value) = value {
        fields.push((key, value.into()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_record_carries_no_answer_and_is_not_private() {
        let record = GeoIpRecord::default();
        assert!(record.city_name.is_none());
        assert!(record.network.is_none());
        assert!(record.asn_network.is_none());
        // Default is "nothing resolved", which is not the same claim as
        // "reserved address", so the flag stays clear.
        assert!(!record.is_private);
    }

    #[test]
    fn the_private_record_is_flagged_and_otherwise_empty() {
        let record = GeoIpRecord::private();
        assert!(record.is_private);
        assert!(record.country_code.is_none());
        assert!(record.autonomous_system_number.is_none());
        assert!(record.latitude.is_none());
    }

    #[test]
    fn the_shared_private_record_is_one_allocation() {
        let first = GeoIpRecord::private_shared();
        let second = GeoIpRecord::private_shared();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.is_private);
    }

    #[test]
    fn the_schema_map_omits_absent_fields_but_always_states_privacy() {
        let record = GeoIpRecord::default();
        let fields = record.to_schema_map();
        assert_eq!(fields, vec![("is_private", FieldValue::Bool(false))]);
    }

    #[test]
    fn the_schema_map_keys_are_the_field_names_and_keep_their_types() {
        let record = GeoIpRecord {
            city_name: Some(CompactString::from("Boxford")),
            country_code: Some(CompactString::from("GB")),
            region_name: Some(CompactString::from("West Berkshire")),
            latitude: Some(51.75),
            accuracy_radius: Some(100),
            autonomous_system_number: Some(15169),
            network: Some(CompactString::from("2.125.160.216/29")),
            asn_network: Some(CompactString::from("2.125.160.0/24")),
            ..GeoIpRecord::default()
        };

        let fields = record.to_schema_map();

        assert!(fields.contains(&("city_name", FieldValue::Str("Boxford"))));
        assert!(fields.contains(&("country_code", FieldValue::Str("GB"))));
        assert!(fields.contains(&("region_name", FieldValue::Str("West Berkshire"))));
        // A radius is a number in the map, not the string "100".
        assert!(fields.contains(&("accuracy_radius", FieldValue::UInt(100))));
        assert!(fields.contains(&("autonomous_system_number", FieldValue::UInt(15169))));
        assert!(fields.contains(&("is_private", FieldValue::Bool(false))));
        // Both networks survive the flattening as separate keys.
        assert!(fields.contains(&("network", FieldValue::Str("2.125.160.216/29"))));
        assert!(fields.contains(&("asn_network", FieldValue::Str("2.125.160.0/24"))));

        let latitude = fields
            .iter()
            .find_map(|(key, value)| match (key, value) {
                (&"latitude", &FieldValue::Float(value)) => Some(value),
                _ => None,
            })
            .expect("latitude is in the map");
        assert!((latitude - 51.75).abs() < f64::EPSILON);
    }

    #[test]
    fn serialisation_drops_the_fields_that_resolved_to_nothing() {
        let record = GeoIpRecord {
            country_code: Some(CompactString::from("SE")),
            ..GeoIpRecord::default()
        };

        let json = serde_json::to_string(&record).unwrap();

        assert!(json.contains(r#""country_code":"SE""#), "{json}");
        assert!(json.contains(r#""is_private":false"#), "{json}");
        // An absent field is absent, not a null a consumer has to filter.
        assert!(!json.contains("city_name"), "{json}");
        assert!(!json.contains("null"), "{json}");
    }

    /// A record carrying two source fields the typed set does not name.
    fn with_extra() -> GeoIpRecord {
        let mut record = GeoIpRecord {
            city_name: Some(CompactString::from("Boxford")),
            ..GeoIpRecord::default()
        };
        record.extra.push(
            CompactString::from("isp"),
            ExtraValue::Str("Telstra".into()),
        );
        record.extra.push(
            CompactString::from("city.geoname_id"),
            ExtraValue::UInt(2_655_045),
        );
        record
    }

    #[test]
    fn the_schema_map_carries_the_source_fields_beside_the_typed_ones() {
        let record = with_extra();
        let fields = record.to_schema_map();

        assert!(fields.contains(&("city_name", FieldValue::Str("Boxford"))));
        assert!(fields.contains(&("isp", FieldValue::Str("Telstra"))));
        assert!(fields.contains(&("city.geoname_id", FieldValue::UInt(2_655_045))));
    }

    #[test]
    fn the_source_fields_are_reached_by_path_and_kept_in_source_order() {
        let record = with_extra();

        assert_eq!(record.extra.len(), 2);
        assert_eq!(
            record.extra.get("isp"),
            Some(&ExtraValue::Str("Telstra".into()))
        );
        assert!(record.extra.get("organization").is_none());

        let paths: Vec<&str> = record.extra.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, vec!["isp", "city.geoname_id"]);
    }

    #[test]
    fn the_source_fields_serialise_as_an_object_and_round_trip() {
        let record = with_extra();

        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains(r#""extra":{"isp":"Telstra""#), "{json}");
        assert!(json.contains(r#""city.geoname_id":2655045"#), "{json}");

        let decoded: GeoIpRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn a_record_with_no_source_fields_writes_no_map() {
        let record = GeoIpRecord::default();

        let json = serde_json::to_string(&record).unwrap();

        assert!(!json.contains("extra"), "{json}");
    }

    #[test]
    fn every_value_kind_the_database_format_holds_round_trips() {
        for value in [
            ExtraValue::Str(CompactString::from("residential")),
            ExtraValue::Float(-1.25),
            ExtraValue::UInt(2_655_045),
            ExtraValue::Int(-42),
            ExtraValue::Bool(true),
            ExtraValue::Bytes(vec![0, 1, 254, 255]),
        ] {
            let json = serde_json::to_string(&value).unwrap();
            let decoded: ExtraValue = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, value, "{json}");
        }
    }

    #[test]
    fn a_whole_number_reads_as_the_narrowest_kind_that_holds_it() {
        // One representation per value, whichever width the source wrote it at.
        assert_eq!(ExtraValue::from_i64(5), ExtraValue::UInt(5));
        assert_eq!(ExtraValue::from_i64(-5), ExtraValue::Int(-5));
        assert_eq!(ExtraValue::from_u128(5), ExtraValue::UInt(5));

        let past_u64 = u128::from(u64::MAX) + 1;
        assert_eq!(
            ExtraValue::from_u128(past_u64),
            ExtraValue::UInt128(past_u64)
        );
    }

    #[test]
    fn a_number_wider_than_json_carries_is_written_out_and_re_read_as_a_double() {
        let value = ExtraValue::UInt128(u128::from(u64::MAX) + 1);

        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, "18446744073709551616");

        // JSON has no integer this wide, so the re-read is a double. Every
        // other kind survives, and a binary format carries this one too.
        let decoded: ExtraValue = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, ExtraValue::Float(_)), "{decoded:?}");
    }

    #[test]
    fn a_record_round_trips_through_serde() {
        let record = GeoIpRecord {
            city_name: Some(CompactString::from("Linkoping")),
            country_code: Some(CompactString::from("SE")),
            latitude: Some(58.4167),
            metro_code: Some(500),
            autonomous_system_organization: Some(CompactString::from("Bredband2 AB")),
            network: Some(CompactString::from("89.160.20.112/28")),
            asn_network: Some(CompactString::from("89.160.20.112/29")),
            ..GeoIpRecord::default()
        };

        let json = serde_json::to_string(&record).unwrap();
        let decoded: GeoIpRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, record);
    }
}
