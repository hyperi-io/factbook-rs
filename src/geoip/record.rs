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
//! # No ISP or organisation
//!
//! Those fields live in GeoIP2-ISP, a commercial database none of the providers
//! in [`GeoIpProvider`](super::config::GeoIpProvider) publish, so no lookup this
//! crate can perform would populate them.

use std::sync::{Arc, LazyLock};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

/// Number of fields a record can carry, to size the schema map in one go.
const FIELD_COUNT: usize = 17;

/// The answer for an address that cannot have a geolocation, built once and
/// shared by every lookup that short-circuits.
static PRIVATE: LazyLock<Arc<GeoIpRecord>> = LazyLock::new(|| Arc::new(GeoIpRecord::private()));

/// One typed value in a [`to_schema_map`](GeoIpRecord::to_schema_map) pair.
///
/// A latitude is a number and a country code is a string, so the map keeps the
/// distinction rather than rendering everything as text for the consumer to
/// parse back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FieldValue<'a> {
    /// A text field, borrowed from the record.
    Str(&'a str),
    /// A coordinate.
    Float(f64),
    /// A whole number: an ASN, a metro code, an accuracy radius.
    UInt(u32),
    /// A flag.
    Bool(bool),
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
    /// The keys are the field names, so the pairs map onto a schema column or an
    /// event field without a translation table in between. Strings are borrowed
    /// from the record, so the only allocation is the vector itself.
    #[must_use]
    pub fn to_schema_map(&self) -> Vec<(&'static str, FieldValue<'_>)> {
        let mut fields = Vec::with_capacity(FIELD_COUNT);

        push_str(&mut fields, "city_name", self.city_name.as_deref());
        push_str(
            &mut fields,
            "continent_code",
            self.continent_code.as_deref(),
        );
        push_str(&mut fields, "country_code", self.country_code.as_deref());
        push_str(&mut fields, "country_name", self.country_name.as_deref());
        push_str(&mut fields, "region_name", self.region_name.as_deref());
        push_str(&mut fields, "region_code", self.region_code.as_deref());
        push_str(&mut fields, "postal_code", self.postal_code.as_deref());
        push_str(&mut fields, "timezone", self.timezone.as_deref());
        push_float(&mut fields, "latitude", self.latitude);
        push_float(&mut fields, "longitude", self.longitude);
        push_uint(&mut fields, "metro_code", self.metro_code.map(u32::from));
        push_uint(
            &mut fields,
            "accuracy_radius",
            self.accuracy_radius.map(u32::from),
        );
        push_uint(
            &mut fields,
            "autonomous_system_number",
            self.autonomous_system_number,
        );
        push_str(
            &mut fields,
            "autonomous_system_organization",
            self.autonomous_system_organization.as_deref(),
        );
        fields.push(("is_private", FieldValue::Bool(self.is_private)));
        push_str(&mut fields, "network", self.network.as_deref());
        push_str(&mut fields, "asn_network", self.asn_network.as_deref());

        fields
    }
}

/// Append a text field when the lookup resolved one.
fn push_str<'a>(
    fields: &mut Vec<(&'static str, FieldValue<'a>)>,
    key: &'static str,
    value: Option<&'a str>,
) {
    if let Some(value) = value {
        fields.push((key, FieldValue::Str(value)));
    }
}

/// Append a coordinate when the lookup resolved one.
fn push_float(
    fields: &mut Vec<(&'static str, FieldValue<'_>)>,
    key: &'static str,
    value: Option<f64>,
) {
    if let Some(value) = value {
        fields.push((key, FieldValue::Float(value)));
    }
}

/// Append a whole-number field when the lookup resolved one.
fn push_uint(
    fields: &mut Vec<(&'static str, FieldValue<'_>)>,
    key: &'static str,
    value: Option<u32>,
) {
    if let Some(value) = value {
        fields.push((key, FieldValue::UInt(value)));
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
