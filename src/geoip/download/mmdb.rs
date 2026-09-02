// Project:   factbook
// File:      src/geoip/download/mmdb.rs
// Purpose:   Turn rows into a MaxMind DB, and refuse one that does not read back
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Writing rows out as a MaxMind DB.
//!
//! A table source is held whole in memory, which stops being possible somewhere
//! above the resident ceiling. MaxMind DB is the one on-disk shape this crate
//! already reads, and it is a prefix trie over a data section that stores each
//! distinct record once, so a source with far more rows than distinct records
//! shrinks to the size of the records rather than the size of the rows.
//!
//! [`to_vec`] hands back the bytes, which is what a database that still fits in
//! memory wants. [`to_file`] writes them, which is what one that does not wants.
//! Both refuse a database that does not read back.
//!
//! # Nothing is admitted unread
//!
//! The writer behind this module is a 0.1 release with no published
//! documentation, so a structurally plausible file is not evidence of a correct
//! one. Every conversion is opened with the reader this crate looks up through
//! and questioned about a spread of the addresses that went into it, before the
//! bytes are returned and before a file is renamed into place. This is
//! [`Guard::admit`](super::verify::Guard::admit) applied to bytes this crate
//! wrote itself.
//!
//! # What the format demands, and what the writer does not supply
//!
//! Three of its behaviours are not visible from its signatures and are handled
//! here:
//!
//! - Its default metadata declares binary format version 0, which every
//!   conforming reader refuses. [`describe`] states version 2.
//! - A network inserted after one that contains it replaces that record's whole
//!   subtree, so rows are placed shortest prefix first.
//! - A zero-length path inserts nothing, so a default route is placed as its two
//!   halves.
//!
//! # One tree, both families
//!
//! The database is declared for IPv6 and every IPv4 network is placed under
//! `::/96`, which is where a reader looks for the IPv4 subtree. One tree then
//! answers both families, and a source that mixes them needs no second file.
//!
//! An IPv6 row inside `::/96` therefore shares space with the IPv4 rows, the
//! same way it does in every database MaxMind publishes. That range is the
//! deprecated IPv4-compatible form, which no published prefix list carries.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs;
use std::hash::Hash;
use std::io;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ipnet::IpNet;
use maxminddb::Reader;
use maxminddb_writer::Database;
use maxminddb_writer::metadata::IpVersion;
use maxminddb_writer::paths::IpAddrWithMask;
use serde::Serialize;

/// Name a converted source is written under.
///
/// The lookup engine dispatches on this field, so a converted user table must
/// never carry a name a geo schema is read from: `ipinfo` prefixes select the
/// flat IPinfo record, and the MaxMind schema is the fallback for everything
/// else. Naming the crate and the shape keeps it out of both -- no publisher
/// ships a database called this, and a reader that meets one knows the rows came
/// from a table source rather than from a geo provider.
pub(crate) const DATABASE_TYPE: &str = "factbook-table";

/// What the description field says, in the one language it is written in.
const DESCRIPTION: &str = "table source converted by factbook";

/// Depth the IPv4 subtree begins at in a database declared for IPv6.
const IPV4_DEPTH: u8 = 96;

/// Bits a tree path can hold.
const ADDRESS_BITS: u32 = 128;

/// Zero bytes the format puts between the search tree and the data section.
const SEPARATOR_LEN: usize = 16;

/// Addresses the written database is questioned about before it is admitted.
const PROBE_COUNT: usize = 32;

/// Extension the database is written under while it is being checked.
const STAGE_EXT: &str = "staged";

/// Upper half of the address space, which a default route is split across.
const UPPER_HALF: IpAddr = IpAddr::V6(Ipv6Addr::new(0x8000, 0, 0, 0, 0, 0, 0, 0));

/// Errors raised while writing a MaxMind DB.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WriteError {
    /// There was nothing to build a database from.
    #[error("a database cannot be written from no rows")]
    NoRows,

    /// A record could not be encoded into the data section.
    #[error("row {at} could not be encoded: {detail}")]
    Encode {
        /// Position of the row in the input.
        at: usize,
        /// What the encoder refused it for.
        detail: String,
    },

    /// The assembled database could not be serialised.
    #[error("writing the database failed: {detail}")]
    Write {
        /// What the writer refused it for.
        detail: String,
    },

    /// A filesystem operation failed.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// The database that was written will not open.
    #[error("the database that was written does not open: {detail}")]
    Unreadable {
        /// What the reader refused it for.
        detail: String,
    },

    /// An address reads back as a record other than the one written for it.
    #[error("{address} reads back as record {actual:?}, not the record {expected} written for it")]
    Mismatch {
        /// Address that was asked.
        address: IpAddr,
        /// Record the tree was built to answer it with.
        expected: usize,
        /// Record it answered with, absent when it answered nothing.
        actual: Option<usize>,
    },
}

/// What one conversion produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Written {
    /// Networks placed in the search tree.
    pub(crate) networks: usize,

    /// Distinct records the data section holds, which is what the dedup left.
    pub(crate) records: usize,

    /// Length of the database, in bytes.
    pub(crate) bytes: u64,
}

/// Convert rows into a MaxMind DB held in memory.
///
/// # Errors
///
/// [`WriteError::NoRows`] for an empty input, [`WriteError::Encode`] for a
/// record the data section cannot hold, and [`WriteError::Unreadable`] or
/// [`WriteError::Mismatch`] when the bytes do not read back as what went in.
pub(crate) fn to_vec<T, R>(rows: R) -> Result<(Vec<u8>, Written), WriteError>
where
    T: Serialize + Hash + Eq,
    R: IntoIterator<Item = (IpNet, T)>,
{
    let built = build(rows)?;

    let bytes = built
        .db
        .write_to(Vec::new())
        .map_err(|e| WriteError::Write {
            detail: e.to_string(),
        })?;

    verify_bytes(&bytes, &built.probes)?;

    let written = Written {
        networks: built.networks,
        records: built.records,
        bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    };
    Ok((bytes, written))
}

/// Convert rows into a MaxMind DB at `dest`.
///
/// Written to a sibling of `dest` and renamed over it once it reads back, so a
/// refusal leaves whatever is already there exactly as it is.
///
/// # Errors
///
/// The errors of [`to_vec`], and [`WriteError::Io`] when the file cannot be
/// written or renamed.
pub(crate) fn to_file<T, R>(rows: R, dest: &Path) -> Result<Written, WriteError>
where
    T: Serialize + Hash + Eq,
    R: IntoIterator<Item = (IpNet, T)>,
{
    let Built {
        db,
        probes,
        networks,
        records,
    } = build(rows)?;

    let staged = staged_path(dest);
    let staging = stage(&db, &staged);
    // The assembled tree is larger than the file it just wrote, and nothing
    // below reads it.
    drop(db);

    let admitted = staging.and_then(|bytes| {
        let reader =
            crate::geoip::enricher::open_reader(&staged).map_err(|e| WriteError::Unreadable {
                detail: e.to_string(),
            })?;
        verify(&reader, &probes)?;
        Ok(bytes)
    });

    let bytes = match admitted {
        Ok(bytes) => bytes,
        Err(e) => {
            let _ = fs::remove_file(&staged);
            return Err(e);
        }
    };

    fs::rename(&staged, dest)?;
    // Best effort: makes the rename itself durable, and not every filesystem
    // allows a directory to be synced.
    if let Some(parent) = dest.parent() {
        let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
    }

    Ok(Written {
        networks,
        records,
        bytes,
    })
}

/// A database assembled from rows, before it has been written anywhere.
#[derive(Debug)]
struct Built {
    /// The database itself.
    db: Database,

    /// Addresses the written file is questioned about.
    probes: Vec<Probe>,

    /// Networks placed in the search tree.
    networks: usize,

    /// Distinct records the data section holds.
    records: usize,
}

/// One address the written database has to answer correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Probe {
    /// Address a lookup is made with, in the family its row was written in.
    address: IpAddr,

    /// The same address in the tree's own space, for the containment test.
    bits: u128,

    /// Offset of the record the tree resolves it to.
    expected: usize,
}

/// Assemble a database from rows, interning the records as they arrive.
///
/// One pass places the networks and settles what each probe must read back. The
/// two agree because both take the last network covering an address, which is
/// what the tree answers with once the rows are in shortest-prefix-first order.
fn build<T, R>(rows: R) -> Result<Built, WriteError>
where
    T: Serialize + Hash + Eq,
    R: IntoIterator<Item = (IpNet, T)>,
{
    let mut db = Database::default();
    describe(&mut db);

    // Inference names the writer's record handle, which its own crate declares
    // in a module it does not export.
    let mut interned = HashMap::new();
    let mut placements = Vec::new();

    for (at, (network, record)) in rows.into_iter().enumerate() {
        let data = match interned.entry(record) {
            Entry::Occupied(seen) => *seen.get(),
            Entry::Vacant(unseen) => {
                let data = db
                    .insert_value(unseen.key())
                    .map_err(|e| WriteError::Encode {
                        at,
                        detail: e.to_string(),
                    })?;
                *unseen.insert(data)
            }
        };
        placements.push((network, data));
    }

    if placements.is_empty() {
        return Err(WriteError::NoRows);
    }

    // A network inserted after one that contains it replaces that record's whole
    // subtree, so the shortest prefix has to go in first.
    placements.sort_by_key(|&(network, _)| depth(network));

    let count = PROBE_COUNT.min(placements.len());
    let mut chosen: Vec<usize> = (0..count).map(|i| i * placements.len() / count).collect();
    // The deepest network is the one nothing after it can shadow.
    chosen.push(placements.len() - 1);
    chosen.dedup();

    let mut probes: Vec<Probe> = chosen
        .into_iter()
        .map(|at| {
            let (network, data) = placements[at];
            Probe {
                address: network.network(),
                bits: tree_bits(network),
                expected: data.data_section_offset(0) - SEPARATOR_LEN,
            }
        })
        .collect();

    for &(network, data) in &placements {
        let path = path(network);
        if path.mask == 0 {
            // A zero-length path inserts nothing, so a default route goes in as
            // its two halves.
            db.insert_node(IpAddrWithMask::new(path.addr, 1), data);
            db.insert_node(IpAddrWithMask::new(UPPER_HALF, 1), data);
        } else {
            db.insert_node(path, data);
        }

        let offset = data.data_section_offset(0) - SEPARATOR_LEN;
        for probe in &mut probes {
            if covers(network, probe.bits) {
                probe.expected = offset;
            }
        }
    }

    Ok(Built {
        networks: placements.len(),
        records: interned.len(),
        db,
        probes,
    })
}

/// Declare what the file is, which is what a reader dispatches on.
///
/// Version 2 is stated because the writer defaults it to 0, which every
/// conforming reader refuses. The build stamp is what the age gauge reports for
/// a converted source, so it is the conversion rather than the epoch.
fn describe(db: &mut Database) {
    db.metadata.binary_format_major_version = 2;
    db.metadata.binary_format_minor_version = 0;
    db.metadata.ip_version = IpVersion::V6;
    db.metadata.database_type = DATABASE_TYPE.to_string();
    db.metadata.languages = vec!["en".to_string()];
    db.metadata.description = HashMap::from([("en".to_string(), DESCRIPTION.to_string())]);
    db.metadata.build_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
}

/// Write the database to `staged`, returning its length.
fn stage(db: &Database, staged: &Path) -> Result<u64, WriteError> {
    let mut out = io::BufWriter::new(fs::File::create(staged)?);
    db.write_to(&mut out).map_err(|e| WriteError::Write {
        detail: e.to_string(),
    })?;

    let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
    // A rename is atomic against another process but says nothing about the
    // bytes reaching disk.
    file.sync_all()?;
    Ok(file.metadata()?.len())
}

/// Open the bytes and ask them every probe.
fn verify_bytes(bytes: &[u8], probes: &[Probe]) -> Result<(), WriteError> {
    let reader = Reader::from_source(bytes).map_err(|e| WriteError::Unreadable {
        detail: e.to_string(),
    })?;
    verify(&reader, probes)
}

/// Ask a written database every probe, refusing it on the first wrong answer.
fn verify<S: AsRef<[u8]>>(reader: &Reader<S>, probes: &[Probe]) -> Result<(), WriteError> {
    for probe in probes {
        let actual = reader
            .lookup(probe.address)
            .map_err(|e| WriteError::Unreadable {
                detail: e.to_string(),
            })?
            .offset();

        if actual != Some(probe.expected) {
            return Err(WriteError::Mismatch {
                address: probe.address,
                expected: probe.expected,
                actual,
            });
        }
    }
    Ok(())
}

/// The name a database is written under while it is being checked.
fn staged_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".");
    name.push(STAGE_EXT);
    PathBuf::from(name)
}

/// How deep in the tree a network sits.
fn depth(network: IpNet) -> u8 {
    match network {
        IpNet::V4(v4) => IPV4_DEPTH + v4.prefix_len(),
        IpNet::V6(v6) => v6.prefix_len(),
    }
}

/// A network's address in the tree's own space.
fn tree_bits(network: IpNet) -> u128 {
    match network.network() {
        IpAddr::V4(v4) => u128::from(u32::from(v4)),
        IpAddr::V6(v6) => u128::from(v6),
    }
}

/// The path a network occupies, which the tree walks bit by bit.
fn path(network: IpNet) -> IpAddrWithMask {
    IpAddrWithMask::new(
        IpAddr::V6(Ipv6Addr::from(tree_bits(network))),
        depth(network),
    )
}

/// Whether a network covers an address, both in the tree's own space.
fn covers(network: IpNet, address: u128) -> bool {
    let depth = depth(network);
    depth == 0 || (tree_bits(network) ^ address) >> (ADDRESS_BITS - u32::from(depth)) == 0
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::net::Ipv4Addr;

    use super::*;

    /// A row of an ASN-keyed table, which is the shape a conversion meets.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
    struct Operator {
        asn: u32,
        name: String,
        country: String,
    }

    impl Operator {
        /// One operator, named after its number.
        fn new(asn: u32) -> Self {
            Self {
                asn,
                name: format!("OPERATOR-{asn}"),
                country: "AU".to_string(),
            }
        }
    }

    /// A network, from the text an operator would have written.
    fn net(text: &str) -> IpNet {
        text.parse().unwrap()
    }

    /// An address, from the text an operator would have written.
    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// The record a written database answers `address` with.
    fn lookup(bytes: &[u8], address: IpAddr) -> Option<Operator> {
        Reader::from_source(bytes)
            .unwrap()
            .lookup(address)
            .unwrap()
            .decode()
            .unwrap()
    }

    /// Two IPv4 networks and two IPv6 ones, each with its own operator.
    fn mixed() -> Vec<(IpNet, Operator)> {
        vec![
            (net("1.1.1.0/24"), Operator::new(13_335)),
            (net("8.8.8.0/24"), Operator::new(15_169)),
            (net("2606:4700::/32"), Operator::new(13_335)),
            (net("2001:4860::/32"), Operator::new(15_169)),
        ]
    }

    #[test]
    fn rows_read_back_as_they_went_in() {
        let (bytes, written) = to_vec(mixed()).unwrap();

        assert_eq!(written.networks, 4);
        // Two operators over four networks, so the data section holds two.
        assert_eq!(written.records, 2);
        assert_eq!(
            lookup(&bytes, ip("1.1.1.1")),
            Some(Operator::new(13_335)),
            "an IPv4 row must answer an IPv4 lookup"
        );
        assert_eq!(lookup(&bytes, ip("8.8.8.8")), Some(Operator::new(15_169)));
        assert_eq!(
            lookup(&bytes, ip("2606:4700::1111")),
            Some(Operator::new(13_335)),
            "an IPv6 row must answer an IPv6 lookup"
        );
        assert_eq!(
            lookup(&bytes, ip("2001:4860::8888")),
            Some(Operator::new(15_169))
        );
    }

    #[test]
    fn an_address_outside_every_network_answers_nothing() {
        let (bytes, _) = to_vec(mixed()).unwrap();

        assert_eq!(lookup(&bytes, ip("9.9.9.9")), None);
        assert_eq!(lookup(&bytes, ip("2620:fe::fe")), None);
    }

    #[test]
    fn the_file_states_what_it_is_and_which_families_it_holds() {
        // The reader dispatches on the type name, so a converted table that
        // named itself after a geo product would be decoded as one.
        let (bytes, _) = to_vec(mixed()).unwrap();
        let reader = Reader::from_source(bytes.as_slice()).unwrap();
        let metadata = reader.metadata();

        assert_eq!(metadata.database_type, DATABASE_TYPE);
        assert!(
            !metadata.database_type.starts_with("ipinfo"),
            "{metadata:?}"
        );
        assert_eq!(metadata.binary_format_major_version, 2);
        assert_eq!(metadata.ip_version, 6);
        assert!(metadata.build_epoch > 1_700_000_000, "{metadata:?}");
    }

    #[test]
    fn identical_records_share_one_data_entry() {
        // Every network carries the same operator, so a data section that grew
        // with the rows would be the whole advantage lost.
        let one = std::iter::repeat_with(|| Operator::new(64_496))
            .take(256)
            .enumerate()
            .map(|(at, operator)| {
                let at = u8::try_from(at).unwrap();
                (net(&format!("10.{at}.0.0/16")), operator)
            })
            .collect::<Vec<_>>();

        let (bytes, written) = to_vec(one).unwrap();

        assert_eq!(written.networks, 256);
        assert_eq!(written.records, 1);
        assert_eq!(
            lookup(&bytes, ip("10.7.0.1")),
            Some(Operator::new(64_496)),
            "every network must still answer"
        );
        assert_eq!(
            lookup(&bytes, ip("10.200.0.1")),
            Some(Operator::new(64_496))
        );
    }

    /// The rows of a table, and the CSV they would have been fetched as.
    fn table(networks: u32, operators: u32) -> (Vec<(IpNet, Operator)>, usize) {
        let mut csv = String::from("network,asn,name,country\n");
        let rows = (0..networks)
            .map(|at| {
                let network = net(&format!(
                    "{}.{}.{}.0/24",
                    100 + at / 65_536,
                    (at / 256) % 256,
                    at % 256
                ));
                let operator = Operator::new(at % operators);
                writeln!(
                    csv,
                    "{network},{},{},{}",
                    operator.asn, operator.name, operator.country
                )
                .unwrap();
                (network, operator)
            })
            .collect();
        (rows, csv.len())
    }

    #[test]
    fn the_dedup_ratio_on_a_table_shaped_input() {
        // Two published shapes: an origin-ASN table runs about five prefixes to
        // an operator, and a country table runs hundreds of prefixes to a code.
        for operators in [10_000_u32, 50_u32] {
            let (rows, csv) = table(50_000, operators);
            let (bytes, written) = to_vec(rows).unwrap();

            let ratio = |a: usize, b: usize| {
                f64::from(u32::try_from(a).unwrap()) / f64::from(u32::try_from(b).unwrap())
            };
            println!(
                "{} networks over {} records: dedup {:.1}:1, database {} bytes against {} bytes of CSV ({:.2}x)",
                written.networks,
                written.records,
                ratio(written.networks, written.records),
                written.bytes,
                csv,
                ratio(csv, usize::try_from(written.bytes).unwrap()),
            );

            assert_eq!(written.networks, 50_000);
            assert_eq!(written.records, operators as usize);
            assert_eq!(
                lookup(&bytes, ip("100.0.0.1")),
                Some(Operator::new(0)),
                "the first network must still answer"
            );
        }
    }

    #[test]
    fn a_longer_prefix_wins_over_the_one_around_it() {
        // Handed to the writer specific-first, which is the order that loses the
        // longer prefix if the rows are not sorted before they are placed.
        let rows = vec![
            (net("10.1.2.0/24"), Operator::new(2)),
            (net("10.0.0.0/8"), Operator::new(1)),
            (net("2001:db8:1::/48"), Operator::new(4)),
            (net("2001:db8::/32"), Operator::new(3)),
        ];

        let (bytes, written) = to_vec(rows).unwrap();

        assert_eq!(written.records, 4);
        assert_eq!(lookup(&bytes, ip("10.1.2.3")), Some(Operator::new(2)));
        assert_eq!(lookup(&bytes, ip("10.9.9.9")), Some(Operator::new(1)));
        assert_eq!(
            lookup(&bytes, ip("2001:db8:1::1")),
            Some(Operator::new(4)),
            "the more specific IPv6 network must survive the broader one"
        );
        assert_eq!(lookup(&bytes, ip("2001:db8:9::1")), Some(Operator::new(3)));
    }

    #[test]
    fn a_default_route_is_placed_rather_than_dropped() {
        // A zero-length path inserts nothing, so an unsplit default route would
        // leave the catch-all row silently missing.
        let rows = vec![
            (net("0.0.0.0/0"), Operator::new(1)),
            (net("::/0"), Operator::new(2)),
            (net("1.1.1.0/24"), Operator::new(3)),
        ];

        let (bytes, _) = to_vec(rows).unwrap();

        assert_eq!(lookup(&bytes, ip("1.1.1.1")), Some(Operator::new(3)));
        assert_eq!(lookup(&bytes, ip("203.0.113.1")), Some(Operator::new(1)));
        assert_eq!(lookup(&bytes, ip("2001:db8::1")), Some(Operator::new(2)));
        assert_eq!(
            lookup(&bytes, ip("8000::1")),
            Some(Operator::new(2)),
            "the upper half of the space is the half a single insert would lose"
        );
    }

    #[test]
    fn a_repeated_network_keeps_the_last_row() {
        let rows = vec![
            (net("1.1.1.0/24"), Operator::new(1)),
            (net("1.1.1.0/24"), Operator::new(2)),
        ];

        let (bytes, written) = to_vec(rows).unwrap();

        assert_eq!(written.networks, 2);
        assert_eq!(written.records, 2);
        assert_eq!(lookup(&bytes, ip("1.1.1.1")), Some(Operator::new(2)));
    }

    #[test]
    fn no_rows_is_not_a_database() {
        let empty: Vec<(IpNet, Operator)> = Vec::new();

        let err = to_vec(empty).unwrap_err();

        assert!(matches!(err, WriteError::NoRows), "{err:?}");
    }

    #[test]
    fn a_truncated_database_is_refused() {
        // Half a file is still a plausible prefix of one, and the check is the
        // only thing between it and a lookup engine.
        let built = build(mixed()).unwrap();
        let mut bytes = built.db.write_to(Vec::new()).unwrap();
        bytes.truncate(bytes.len() / 2);

        let err = verify_bytes(&bytes, &built.probes).unwrap_err();

        assert!(matches!(err, WriteError::Unreadable { .. }), "{err:?}");
    }

    #[test]
    fn a_database_that_answers_nothing_is_refused() {
        // The metadata is untouched, so it opens, reports its record size and
        // resolves every address to nothing.
        let built = build(mixed()).unwrap();
        let mut bytes = built.db.write_to(Vec::new()).unwrap();
        let reader = Reader::from_source(bytes.as_slice()).unwrap();
        let tree =
            reader.metadata().node_count as usize * reader.metadata().record_size as usize / 8;
        drop(reader);
        bytes[..tree].fill(0);

        let err = verify_bytes(&bytes, &built.probes).unwrap_err();

        assert!(
            matches!(err, WriteError::Mismatch { actual: None, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_file_is_written_and_nothing_is_left_staged() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("networks.mmdb");

        let written = to_file(mixed(), &dest).unwrap();

        assert_eq!(written.networks, 4);
        assert_eq!(written.records, 2);
        assert_eq!(written.bytes, fs::metadata(&dest).unwrap().len());
        assert!(
            !staged_path(&dest).exists(),
            "staged file must be renamed away"
        );

        let bytes = fs::read(&dest).unwrap();
        assert_eq!(lookup(&bytes, ip("1.1.1.1")), Some(Operator::new(13_335)));
        assert_eq!(
            lookup(&bytes, ip("2001:4860::8888")),
            Some(Operator::new(15_169))
        );
    }

    #[test]
    fn a_refused_conversion_leaves_the_copy_on_disk_alone() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("networks.mmdb");
        fs::write(&dest, b"the copy already there").unwrap();

        let empty: Vec<(IpNet, Operator)> = Vec::new();
        let err = to_file(empty, &dest).unwrap_err();

        assert!(matches!(err, WriteError::NoRows), "{err:?}");
        assert_eq!(fs::read(&dest).unwrap(), b"the copy already there");
        assert!(!staged_path(&dest).exists());
    }

    #[test]
    fn a_network_covers_the_addresses_inside_it_and_no_others() {
        assert!(covers(net("10.0.0.0/8"), tree_bits(net("10.1.2.3/32"))));
        assert!(!covers(net("10.0.0.0/8"), tree_bits(net("11.1.2.3/32"))));
        assert!(covers(net("::/0"), tree_bits(net("2001:db8::/32"))));
        assert!(covers(
            net("2001:db8::/32"),
            u128::from(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1))
        ));
        assert!(!covers(
            net("2001:db8::/32"),
            u128::from(Ipv6Addr::new(0x2001, 0x0db9, 0, 0, 0, 0, 0, 1))
        ));
        // An IPv4 network sits under ::/96, so no IPv6 network outside it can
        // reach the same bits.
        assert!(covers(
            net("0.0.0.0/0"),
            u128::from(u32::from(Ipv4Addr::new(203, 0, 113, 1)))
        ));
        assert!(!covers(net("0.0.0.0/0"), tree_bits(net("2001:db8::/32"))));
    }

    #[test]
    fn a_network_sits_where_the_reader_looks_for_its_family() {
        assert_eq!(depth(net("0.0.0.0/0")), IPV4_DEPTH);
        assert_eq!(depth(net("10.0.0.0/8")), IPV4_DEPTH + 8);
        assert_eq!(depth(net("::/0")), 0);
        assert_eq!(depth(net("2001:db8::/32")), 32);

        // Host bits are dropped, so two rows inside one network share a path.
        assert_eq!(path(net("10.1.2.3/8")), path(net("10.9.9.9/8")));
    }
}
