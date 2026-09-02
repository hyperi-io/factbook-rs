// Project:   factbook
// File:      src/geoip/download/verify.rs
// Purpose:   What a staged database must answer before it replaces one
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! What a staged database has to answer before it replaces the copy on disk.
//!
//! A file can be a structurally perfect MaxMind DB, match the digest its
//! provider published and still answer nothing: `dbip-asn` ships a valid
//! database whose operator-name column is blank on every row, so it parses,
//! matches an address, and hands back an empty string. Structure is therefore
//! not evidence of content.
//!
//! Two questions are asked of the staged file that its structure cannot answer:
//! does it resolve a known address to the field it exists to carry, and is it
//! the size of a database rather than of a stub. Both run before the rename, so
//! a refusal leaves the copy already on disk serving.
//!
//! Both are advisory. An operator running a database whose schema this crate
//! models badly turns either off without giving up the format, digest and
//! length checks in front of them.

use std::fs;
use std::path::Path;

use super::{DatabaseFormat, GeoIpDownloadError, Kind};
use crate::geoip::config::AutoDownloadConfig;
use crate::table::{Schema, TableFormat};

#[cfg(feature = "geoip-lookup")]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[cfg(not(feature = "geoip-lookup"))]
use tracing::debug;

/// Percent, the unit the size floor is stated in.
const PERCENT: u64 = 100;

/// What a staged file has to satisfy before it is renamed over the copy on
/// disk.
///
/// Carried by value: it is two settings the operator chose, resolved once per
/// download rather than re-read per check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Guard {
    /// Kind whose field a probe requires, absent when content is not checked.
    probe: Option<Kind>,

    /// Format a staged table must parse as, absent when content is not checked
    /// or the payload is not a table.
    parses: Option<TableFormat>,

    /// Column count the source names, which is the width the load will require.
    /// Absent when the schema takes its names from the file.
    declared: Option<usize>,

    /// Smallest percentage of the copy on disk a replacement may be, zero for
    /// no floor.
    min_size_percent: u8,
}

impl Guard {
    /// Nothing checked, which is the shape the transport and archive cases are
    /// asserted against.
    #[cfg(test)]
    pub(super) const OFF: Self = Self {
        probe: None,
        parses: None,
        declared: None,
        min_size_percent: 0,
    };

    /// The guard the operator's settings ask for on one database kind.
    pub(super) const fn new(kind: Kind, auto: &AutoDownloadConfig) -> Self {
        Self {
            probe: if auto.verify_content {
                Some(kind)
            } else {
                None
            },
            parses: None,
            declared: None,
            min_size_percent: auto.min_size_percent,
        }
    }

    /// The guard the operator's settings ask for on a table.
    ///
    /// A table has no address to probe, so the content question it answers is
    /// whether the bytes hold rows of the format the source states, at the width
    /// its schema names.
    pub(crate) fn for_table(
        format: TableFormat,
        schema: &Schema,
        auto: &AutoDownloadConfig,
    ) -> Self {
        Self {
            probe: None,
            parses: if auto.verify_content {
                Some(format)
            } else {
                None
            },
            declared: match schema {
                Schema::Named(names) => Some(names.len()),
                Schema::Auto => None,
            },
            min_size_percent: auto.min_size_percent,
        }
    }

    /// Whether the staged file may replace what is at `dest`.
    ///
    /// The size comparison runs first because it costs two `stat` calls where
    /// the probe costs a mapped database.
    ///
    /// # Errors
    ///
    /// [`GeoIpDownloadError::Undersized`] when the replacement is a fraction of
    /// the copy it would replace, [`GeoIpDownloadError::Unpopulated`] when it
    /// resolves nothing for the field its kind exists to carry, or
    /// [`GeoIpDownloadError::Unparseable`] when a table holds no rows of the
    /// format it states.
    pub(super) fn admit(
        self,
        staged: &Path,
        dest: &Path,
        format: DatabaseFormat,
    ) -> Result<(), GeoIpDownloadError> {
        self.check_size(staged, dest)?;

        if let Some(kind) = self.probe {
            probe(staged, dest, kind, format)?;
        }

        if let Some(table) = self.parses {
            parses(staged, dest, table, self.declared)?;
        }

        Ok(())
    }

    /// Refuse a replacement that is catastrophically smaller than the copy it
    /// would replace.
    ///
    /// A first download has nothing to compare against and passes, as does one
    /// over a destination whose size cannot be read.
    fn check_size(self, staged: &Path, dest: &Path) -> Result<(), GeoIpDownloadError> {
        if self.min_size_percent == 0 {
            return Ok(());
        }

        let Ok(existing) = fs::metadata(dest).map(|metadata| metadata.len()) else {
            return Ok(());
        };
        if existing == 0 {
            return Ok(());
        }

        let actual = fs::metadata(staged)?.len();
        // Cross-multiplied, so integer division cannot round a small file past
        // the floor. Saturating, so a size no filesystem can hold is admitted
        // rather than wrapping into a refusal.
        if actual.saturating_mul(PERCENT)
            < existing.saturating_mul(u64::from(self.min_size_percent))
        {
            return Err(GeoIpDownloadError::Undersized {
                path: dest.display().to_string(),
                actual,
                existing,
                floor_percent: self.min_size_percent,
            });
        }

        Ok(())
    }
}

/// Addresses a probe is made against.
///
/// Each is an anycast public resolver announced out of its operator's own
/// allocation for years, so every provider's data holds it and no vintage of a
/// database drops it. None is taken from a test fixture: a fixture address is
/// absent from every real provider database, so probing for one would refuse
/// every legitimate download.
/// Both families, because a source covering only IPv6 space would otherwise be
/// refused as unpopulated: the probe passes on the first address that answers.
#[cfg(feature = "geoip-lookup")]
const PROBE_ADDRESSES: [IpAddr; 6] = [
    // Google Public DNS, in Google's own 8.8.8.0/24, AS15169.
    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
    // Cloudflare's resolver, in 1.1.1.0/24, AS13335 -- the allocation whose
    // blank operator name is the defect this check exists for.
    IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
    // Quad9, in 9.9.9.0/24, AS19281.
    IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
    // The same three operators' IPv6 resolvers.
    IpAddr::V6(Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888)),
    IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
    IpAddr::V6(Ipv6Addr::new(0x2620, 0x00fe, 0, 0, 0, 0, 0, 0x00fe)),
];

/// Paths a city database publishes its country identifier under.
///
/// Country rather than city: an anycast address has no meaningful city in any
/// provider's data, and a database that resolves no country resolves nothing.
#[cfg(feature = "geoip-lookup")]
const COUNTRY_PATHS: [&[&str]; 3] = [
    &["country", "iso_code"],
    &["registered_country", "iso_code"],
    // IPinfo Lite names it at the top level rather than under a country map.
    &["country_code"],
];

/// Paths an ASN database publishes its operator name under.
#[cfg(feature = "geoip-lookup")]
const ORGANISATION_PATHS: [&[&str]; 3] = [
    &["autonomous_system_organization"],
    // The paid MaxMind line carries the ASN fields in its ISP database.
    &["isp"],
    &["as_name"],
];

/// Look a known address up in the staged file and require an answer.
///
/// The check passes on the first address that answers, because one provider
/// dropping one allocation is not a broken database while all three coming back
/// empty is.
#[cfg(feature = "geoip-lookup")]
fn probe(
    staged: &Path,
    dest: &Path,
    kind: Kind,
    format: DatabaseFormat,
) -> Result<(), GeoIpDownloadError> {
    // Only the binary format has a reader here, so a text database is admitted
    // on its structure alone.
    if format != DatabaseFormat::Mmdb {
        return Ok(());
    }

    // A file the reader will not open is one no lookup can serve, whatever the
    // metadata marker said about it.
    let reader = crate::geoip::enricher::open_reader(staged).map_err(|_| {
        GeoIpDownloadError::NotADatabase {
            url: dest.display().to_string(),
        }
    })?;

    let answered = PROBE_ADDRESSES
        .iter()
        .any(|&address| answers(&reader, address, kind));

    if answered {
        Ok(())
    } else {
        Err(GeoIpDownloadError::Unpopulated {
            path: dest.display().to_string(),
            field: field_of(kind),
        })
    }
}

/// The probe with no reader compiled in, which admits the file unexamined.
///
/// The lookup engine is the only MaxMind DB reader in this crate, so a build
/// that provisions files for someone else's reader has nothing to ask the file
/// with.
#[cfg(not(feature = "geoip-lookup"))]
// The signature matches the feature-gated sibling because one call site invokes
// both, so the result cannot be dropped here.
#[allow(clippy::unnecessary_wraps)]
fn probe(
    staged: &Path,
    _dest: &Path,
    _kind: Kind,
    _format: DatabaseFormat,
) -> Result<(), GeoIpDownloadError> {
    debug!(
        staged = %staged.display(),
        "no lookup engine is compiled in, so the staged database is not probed"
    );
    Ok(())
}

/// Read the head of the staged file and require rows of the stated format.
///
/// Bounded rather than exhaustive: this runs before the rename, where the
/// question is whether the bytes are a table at all, and a fault deeper in the
/// file is reported by the load that follows.
fn parses(
    staged: &Path,
    dest: &Path,
    format: TableFormat,
    declared: Option<usize>,
) -> Result<(), GeoIpDownloadError> {
    crate::table::probe(staged, format, declared).map_err(|e| GeoIpDownloadError::Unparseable {
        path: dest.display().to_string(),
        detail: e.to_string(),
    })
}

/// Whether the database answers `address` with the field its kind exists to
/// carry.
///
/// A refused lookup, an undecodable record and a field present but blank are
/// the same answer: this address resolved nothing.
#[cfg(feature = "geoip-lookup")]
fn answers(reader: &maxminddb::Reader<maxminddb::Mmap>, address: IpAddr, kind: Kind) -> bool {
    let Ok(result) = reader.lookup(address) else {
        return false;
    };

    paths_of(kind).iter().any(|path| {
        let elements: Vec<maxminddb::PathElement<'_>> = path
            .iter()
            .copied()
            .map(maxminddb::PathElement::Key)
            .collect();
        matches!(
            result.decode_path::<String>(&elements),
            Ok(Some(value)) if !value.trim().is_empty()
        )
    })
}

/// Where a database of this kind publishes the field that makes it useful.
#[cfg(feature = "geoip-lookup")]
const fn paths_of(kind: Kind) -> &'static [&'static [&'static str]] {
    match kind {
        Kind::City => &COUNTRY_PATHS,
        Kind::Asn => &ORGANISATION_PATHS,
    }
}

/// What a database of this kind exists to answer with, for the refusal message.
#[cfg(feature = "geoip-lookup")]
const fn field_of(kind: Kind) -> &'static str {
    match kind {
        Kind::City => "country",
        Kind::Asn => "organisation name",
    }
}

#[cfg(test)]
pub(super) mod fixtures;

#[cfg(test)]
mod tests {
    use super::*;

    /// Settings with both checks at their defaults.
    fn defaults() -> AutoDownloadConfig {
        AutoDownloadConfig::default()
    }

    /// A file of `bytes` length at `path`.
    fn sized(path: &Path, bytes: usize) {
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn the_default_guard_checks_both_halves() {
        let guard = Guard::new(Kind::Asn, &defaults());

        assert_eq!(guard.probe, Some(Kind::Asn));
        assert_eq!(guard.min_size_percent, 50);
        assert_ne!(guard, Guard::OFF);
    }

    #[test]
    fn each_check_is_switched_off_on_its_own() {
        // An operator running a database this crate models badly turns one off
        // and keeps the other.
        let no_content = AutoDownloadConfig {
            verify_content: false,
            ..defaults()
        };
        let no_floor = AutoDownloadConfig {
            min_size_percent: 0,
            ..defaults()
        };

        assert_eq!(Guard::new(Kind::City, &no_content).probe, None);
        assert_eq!(Guard::new(Kind::City, &no_content).min_size_percent, 50);
        assert_eq!(Guard::new(Kind::City, &no_floor).probe, Some(Kind::City));
        assert_eq!(Guard::new(Kind::City, &no_floor).min_size_percent, 0);
    }

    #[test]
    fn a_first_download_has_nothing_to_compare_against() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("db.mmdb.staged");
        let dest = dir.path().join("db.mmdb");
        sized(&staged, 16);

        let guard = Guard::new(Kind::Asn, &defaults());
        guard.check_size(&staged, &dest).unwrap();
    }

    #[test]
    fn a_replacement_under_the_floor_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("db.mmdb.staged");
        let dest = dir.path().join("db.mmdb");
        sized(&dest, 10_000);
        sized(&staged, 4_999);

        let err = Guard::new(Kind::Asn, &defaults())
            .check_size(&staged, &dest)
            .unwrap_err();
        let message = err.to_string();

        assert!(
            matches!(
                err,
                GeoIpDownloadError::Undersized {
                    actual: 4_999,
                    existing: 10_000,
                    floor_percent: 50,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(message.contains("4999"), "{message}");
        assert!(message.contains("10000"), "{message}");
    }

    #[test]
    fn a_replacement_on_the_floor_is_admitted() {
        // Exactly half is inside the floor, so month-to-month variation right
        // at the boundary is not a refusal.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("db.mmdb.staged");
        let dest = dir.path().join("db.mmdb");
        sized(&dest, 10_000);
        sized(&staged, 5_000);

        Guard::new(Kind::Asn, &defaults())
            .check_size(&staged, &dest)
            .unwrap();
    }

    #[test]
    fn ordinary_month_to_month_shrinkage_is_not_a_refusal() {
        // A build that comes back a fifth smaller is a normal provider release,
        // and refusing it would stop a deployment updating for no fault.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("db.mmdb.staged");
        let dest = dir.path().join("db.mmdb");
        sized(&dest, 10_000);
        sized(&staged, 8_000);

        Guard::new(Kind::City, &defaults())
            .check_size(&staged, &dest)
            .unwrap();
    }

    #[test]
    fn a_zero_floor_admits_a_stub() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("db.mmdb.staged");
        let dest = dir.path().join("db.mmdb");
        sized(&dest, 10_000);
        sized(&staged, 1);

        let off = AutoDownloadConfig {
            min_size_percent: 0,
            ..defaults()
        };
        Guard::new(Kind::Asn, &off)
            .check_size(&staged, &dest)
            .unwrap();
    }

    #[test]
    fn an_empty_destination_is_not_a_baseline() {
        // A zero-length file left by an earlier failure would otherwise make
        // every replacement pass or fail on a division by nothing.
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("db.mmdb.staged");
        let dest = dir.path().join("db.mmdb");
        fs::write(&dest, b"").unwrap();
        sized(&staged, 1);

        Guard::new(Kind::Asn, &defaults())
            .check_size(&staged, &dest)
            .unwrap();
    }

    #[cfg(feature = "geoip-lookup")]
    mod probes {
        use super::super::fixtures;
        use super::*;

        /// Stage a fixture and ask the guard whether it may land.
        fn admit(body: &[u8], kind: Kind) -> Result<(), GeoIpDownloadError> {
            let dir = tempfile::tempdir().unwrap();
            let staged = dir.path().join("db.mmdb.staged");
            let dest = dir.path().join("db.mmdb");
            fs::write(&staged, body).unwrap();

            Guard::new(kind, &defaults()).admit(&staged, &dest, DatabaseFormat::Mmdb)
        }

        #[test]
        fn an_asn_database_carrying_operator_names_is_admitted() {
            admit(&fixtures::asn_mmdb(13_335, "CLOUDFLARENET"), Kind::Asn).unwrap();
        }

        #[test]
        fn an_asn_database_with_a_blank_operator_name_is_refused() {
            // The dbip-asn defect exactly: a valid database, a matched address,
            // a number, and an empty string where the name belongs.
            let err = admit(&fixtures::asn_mmdb(13_335, ""), Kind::Asn).unwrap_err();
            let message = err.to_string();

            assert!(
                matches!(
                    err,
                    GeoIpDownloadError::Unpopulated {
                        field: "organisation name",
                        ..
                    }
                ),
                "{err:?}"
            );
            assert!(message.contains("organisation name"), "{message}");
        }

        #[test]
        fn a_whitespace_operator_name_counts_as_blank() {
            let err = admit(&fixtures::asn_mmdb(13_335, "   "), Kind::Asn).unwrap_err();
            assert!(
                matches!(err, GeoIpDownloadError::Unpopulated { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn a_city_database_resolving_a_country_is_admitted() {
            admit(&fixtures::city_mmdb("US"), Kind::City).unwrap();
        }

        #[test]
        fn a_city_database_resolving_no_country_is_refused() {
            let err = admit(&fixtures::city_mmdb(""), Kind::City).unwrap_err();

            assert!(
                matches!(
                    err,
                    GeoIpDownloadError::Unpopulated {
                        field: "country",
                        ..
                    }
                ),
                "{err:?}"
            );
        }

        #[test]
        fn a_database_published_for_ipv6_is_probed_the_same_way() {
            // A source covering IPv6 declares ip_version 6, which sends every
            // lookup through the reader's v4-in-v6 mapping rather than straight
            // down the tree.
            admit(&fixtures::asn_mmdb_v6(13_335, "CLOUDFLARENET"), Kind::Asn).unwrap();
            admit(&fixtures::city_mmdb_v6("US"), Kind::City).unwrap();

            let err = admit(&fixtures::asn_mmdb_v6(13_335, ""), Kind::Asn).unwrap_err();
            assert!(
                matches!(err, GeoIpDownloadError::Unpopulated { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn an_ipv6_address_answers_from_a_database_published_for_it() {
            // Every probe address is IPv4, so this is the only place the
            // content check is asked an IPv6 question at all.
            let dir = tempfile::tempdir().unwrap();
            let staged = dir.path().join("db.mmdb.staged");
            fs::write(&staged, fixtures::asn_mmdb_v6(13_335, "CLOUDFLARENET")).unwrap();

            let reader = crate::geoip::enricher::open_reader(&staged).unwrap();
            let resolver: IpAddr = "2606:4700:4700::1111".parse().unwrap();

            assert!(answers(&reader, resolver, Kind::Asn));
            // The kind decides the field, so an ASN database answers no country
            // over IPv6 either.
            assert!(!answers(&reader, resolver, Kind::City));
        }

        #[test]
        fn a_top_level_country_code_is_read_too() {
            // IPinfo Lite names the field at the top level, and refusing its
            // database over a schema difference would stop a deployment
            // updating for no fault.
            admit(&fixtures::flat_city_mmdb("AU"), Kind::City).unwrap();
        }

        #[test]
        fn an_asn_database_offered_as_a_city_one_is_refused() {
            // The kind decides the field, so a database of the wrong shape
            // answers nothing rather than answering the wrong question.
            let err = admit(&fixtures::asn_mmdb(13_335, "CLOUDFLARENET"), Kind::City).unwrap_err();
            assert!(
                matches!(err, GeoIpDownloadError::Unpopulated { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn a_database_the_reader_will_not_open_is_refused() {
            // The metadata marker is present, so nothing ahead of this check
            // rejected it, and yet no lookup could ever be served from it.
            let mut body = b"not a database".to_vec();
            body.extend_from_slice(b"\xab\xcd\xefMaxMind.com");
            body.extend_from_slice(b"nor is this metadata");

            let err = admit(&body, Kind::Asn).unwrap_err();
            assert!(
                matches!(err, GeoIpDownloadError::NotADatabase { .. }),
                "{err:?}"
            );
        }

        #[test]
        fn content_verification_off_admits_a_blank_database() {
            let dir = tempfile::tempdir().unwrap();
            let staged = dir.path().join("db.mmdb.staged");
            let dest = dir.path().join("db.mmdb");
            fs::write(&staged, fixtures::asn_mmdb(13_335, "")).unwrap();

            let off = AutoDownloadConfig {
                verify_content: false,
                ..defaults()
            };
            Guard::new(Kind::Asn, &off)
                .admit(&staged, &dest, DatabaseFormat::Mmdb)
                .unwrap();
        }

        #[test]
        fn a_text_database_is_admitted_on_its_structure_alone() {
            // There is no reader for the text format here, so the probe has
            // nothing to ask it with.
            let dir = tempfile::tempdir().unwrap();
            let staged = dir.path().join("ranges.csv.staged");
            let dest = dir.path().join("ranges.csv");
            fs::write(&staged, b"1.1.1.0,1.1.1.255,13335,CLOUDFLARENET\n").unwrap();

            Guard::new(Kind::Asn, &defaults())
                .admit(&staged, &dest, DatabaseFormat::Csv)
                .unwrap();
        }
    }
}
