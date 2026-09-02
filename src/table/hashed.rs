// Project:   factbook
// File:      src/table/hashed.rs
// Purpose:   Store a text-keyed table as an MMDB by hashing each key to a network
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Storing a text-keyed table in the one on-disk format this crate has.
//!
//! MMDB files a record under an IP address or a CIDR range and under nothing
//! else. A table keyed by a column of text -- an autonomous system number, a
//! country code, a customer id -- has no address to be filed under, so the
//! writer in [`mmdb`] cannot take it as it stands. This module gives it one:
//! the key is hashed, and the digest becomes a synthetic `/128` that the writer
//! places like any other network.
//!
//! # This is a fudge, and is written down as one
//!
//! A country code is not an IPv6 address, and a file written this way is a
//! prefix trie over addresses that never existed. Derek took the decision to do
//! it anyway, and asked for it to be recorded as what it is: a fudge, and one
//! that is fast, reliable and works.
//!
//! The argument for it is that every alternative costs more than it does. A
//! second on-disk format gives up the reason one format was chosen. Columnar
//! storage was measured against MMDB's six-to-seven byte nodes and its
//! deduplicated data section, and lost. Keeping these tables in memory is the
//! case the conversion exists to escape. Hashing a key into an address costs
//! one digest of a short string, and it reuses the writer, the reader, the
//! staged-write-then-verify path and the guards that are all already built.
//!
//! # The failure mode is a collision, and it is handled
//!
//! Two keys can hash to one network. A network therefore holds a bucket of
//! every key placed on it, each with its own rows, and a read compares the key
//! it was asked for against the key it found before it returns anything. A
//! collision costs one string comparison, and no lookup can answer with a row
//! belonging to a different key.
//!
//! A key is placed by 95 bits of digest, so the chance that any two keys in one
//! table share a network is about `n^2 / 2^96`: around one in `10^17` for a
//! million keys, and one in `10^13` for a hundred million. The bucket exists
//! because that number is small rather than zero.
//!
//! # Why SHA-256
//!
//! The mapping has to come out the same in the process that writes a database
//! and in the process that reads it, on another machine and another release.
//! That rules out `DefaultHasher`, whose `RandomState` is seeded per process,
//! and it rules out anything the standard library is free to change under us.
//! `sha2` is already in the graph verifying the digests providers publish, so
//! the choice costs no dependency, and truncating a cryptographic digest puts
//! the collision rate at the birthday bound with nothing left to argue about
//! how well the bits mix. It is not chosen for its cryptography: a key mapping
//! has no adversary. One digest of a short key is well under the cost of the
//! tree walk it feeds.
//!
//! # Layout
//!
//! Everything sits in `2001:db8::/32`, the RFC 3849 documentation prefix, which
//! is not routed and which no real lookup reaches.
//!
//! | address | holds |
//! |---|---|
//! | `2001:db8::` | the header: the key column's name, and one row of column names |
//! | `2001:db8:8000::/33` + 95 digest bits | a bucket: every key placed there, each with its rows |
//!
//! The header sits at the one address in the prefix with the marker bit clear,
//! so no key can be placed on top of it.
//!
//! Every record in the file is a bucket, so one shape decodes the whole of it.
//! A cell the source did not supply is stored as the empty string, which is
//! what the CSV and JSON readers already turn an empty field into.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;

use ipnet::IpNet;
use maxminddb::Reader;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::geoip::download::mmdb::{self, Written};

use super::{Cell, Keys, Table};

/// Address the header record sits at, which is the base of the prefix.
const HEADER: Ipv6Addr = Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0);

/// Prefix every synthetic network sits in.
const SYNTHETIC: u128 = HEADER.to_bits();

/// Digest bits a key is placed by.
const KEY_BITS: u32 = 95;

/// Bit that separates a hashed key from the header.
const KEY_MARK: u128 = 1 << KEY_BITS;

/// Bits of the address a digest fills.
const KEY_MASK: u128 = KEY_MARK - 1;

/// Where a key is placed, which the writer and the reader have to agree on.
type Locate = fn(&str) -> IpNet;

/// One row, as its cells in column order.
type StoredRow<'a> = Vec<&'a str>;

/// Rows filed under one key, with the key kept so a read can check it.
type Filed<'a> = (&'a str, Vec<StoredRow<'a>>);

/// Everything one synthetic network holds.
type Bucket<'a> = Vec<Filed<'a>>;

/// A bucket read into owned strings, for the header, which outlives the lookup.
type OwnedBucket = Vec<(String, Vec<Vec<String>>)>;

/// Errors raised while converting a table or reading one back.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HashedError {
    /// The table is reached by an address, which the writer already takes.
    #[error("the table is keyed by an address, which needs no hashed key")]
    NotKeyedByText,

    /// The conversion could not be written.
    #[error("writing the converted table failed: {0}")]
    Write(#[from] mmdb::WriteError),

    /// The database will not open, or a record will not decode.
    #[error("the converted table does not read back: {detail}")]
    Unreadable {
        /// What the reader refused it for.
        detail: String,
    },

    /// The file is a MaxMind DB, but not one this module wrote.
    #[error("the database is a {found}, not a converted table")]
    NotATable {
        /// Type the file declares itself to be.
        found: String,
    },

    /// The header record is missing, or is not the shape it is written in.
    #[error("the database carries no table header")]
    NoHeader,
}

/// A reader error as the refusal it caused.
///
/// The error itself is not carried: it belongs to a 0.x crate, and every caller
/// here wants its text rather than its variant.
impl From<maxminddb::MaxMindDbError> for HashedError {
    fn from(e: maxminddb::MaxMindDbError) -> Self {
        Self::Unreadable {
            detail: e.to_string(),
        }
    }
}

/// What one conversion produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Converted {
    /// What the writer put in the file.
    pub(crate) written: Written,

    /// Distinct text keys placed.
    pub(crate) keys: usize,

    /// Keys that landed on a network an earlier key already held.
    pub(crate) collisions: usize,
}

/// Convert a text-keyed table into a MaxMind DB held in memory.
///
/// # Errors
///
/// [`HashedError::NotKeyedByText`] for a table reached by an address, and
/// [`HashedError::Write`] when the writer refuses what it was handed.
pub(crate) fn to_vec(table: &Table) -> Result<(Vec<u8>, Converted), HashedError> {
    let Placement {
        networks,
        keys,
        collisions,
    } = place(table, network)?;

    let (bytes, written) = mmdb::to_vec(networks)?;

    Ok((
        bytes,
        Converted {
            written,
            keys,
            collisions,
        },
    ))
}

/// Convert a text-keyed table into a MaxMind DB at `dest`.
///
/// # Errors
///
/// The errors of [`to_vec`], and the writer's own IO errors.
pub(crate) fn to_file(table: &Table, dest: &Path) -> Result<Converted, HashedError> {
    let Placement {
        networks,
        keys,
        collisions,
    } = place(table, network)?;

    let written = mmdb::to_file(networks, dest)?;

    Ok(Converted {
        written,
        keys,
        collisions,
    })
}

/// A converted table, opened for reading.
///
/// The header is read once, at open, so a lookup is one hash and one tree walk.
pub(crate) struct HashedTable<S: AsRef<[u8]>> {
    /// The database itself.
    reader: Reader<S>,

    /// Column names, in the order a row's cells are stored.
    columns: Vec<String>,

    /// Name of the column the rows are keyed by.
    key_column: String,

    /// Where a key is placed, which has to match what wrote the file.
    network: Locate,
}

impl<S: AsRef<[u8]>> HashedTable<S> {
    /// Open a converted table.
    ///
    /// # Errors
    ///
    /// [`HashedError::Unreadable`] when the bytes are not a database this
    /// crate's reader opens, [`HashedError::NotATable`] when they are some
    /// other MaxMind DB, and [`HashedError::NoHeader`] when the header record
    /// is missing or malformed.
    pub(crate) fn from_source(source: S) -> Result<Self, HashedError> {
        Self::with(source, network)
    }

    /// Column names, in the order a row's cells are stored.
    pub(crate) fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Column the rows are indexed by.
    pub(crate) fn key_column(&self) -> &str {
        &self.key_column
    }

    /// Every row filed under a text key, in the order they were placed.
    ///
    /// A key nothing was filed under answers no rows, and so does a key that
    /// shares its network with another key but was never itself placed.
    ///
    /// # Errors
    ///
    /// [`HashedError::Unreadable`] when the record the key resolves to will not
    /// decode.
    pub(crate) fn rows(&self, key: &str) -> Result<Vec<Vec<Cell>>, HashedError> {
        let at = (self.network)(key);

        let bucket: Option<Bucket<'_>> = self.reader.lookup(at.addr())?.decode()?;

        // The stored key is what settles which rows belong to the caller's key,
        // because a network can hold more than one key.
        let rows = bucket
            .into_iter()
            .flatten()
            .find(|&(found, _)| found == key)
            .map(|(_, rows)| rows.iter().map(|row| cells(row)).collect())
            .unwrap_or_default();

        Ok(rows)
    }

    /// Open a converted table placed by `network`.
    fn with(source: S, network: Locate) -> Result<Self, HashedError> {
        let reader = Reader::from_source(source)?;

        // A geo database decoded through this shape would answer with whatever
        // its records happen to look like, so the type is checked first.
        let declared = &reader.metadata().database_type;
        if declared != mmdb::DATABASE_TYPE {
            return Err(HashedError::NotATable {
                found: declared.clone(),
            });
        }

        let header: OwnedBucket = reader
            .lookup(IpAddr::V6(HEADER))?
            .decode()?
            .ok_or(HashedError::NoHeader)?;

        let Some((key_column, rows)) = header.into_iter().next() else {
            return Err(HashedError::NoHeader);
        };
        let Some(columns) = rows.into_iter().next() else {
            return Err(HashedError::NoHeader);
        };

        Ok(Self {
            reader,
            columns,
            key_column,
            network,
        })
    }
}

impl<S: AsRef<[u8]>> fmt::Debug for HashedTable<S> {
    /// Prints what the header stated rather than the database behind it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashedTable")
            .field("columns", &self.columns)
            .field("key_column", &self.key_column)
            .finish_non_exhaustive()
    }
}

/// A table's rows, placed on the networks they will be written to.
struct Placement<'a> {
    /// Every network, with everything filed on it.
    networks: Vec<(IpNet, Bucket<'a>)>,

    /// Distinct text keys placed.
    keys: usize,

    /// Keys that landed on a network an earlier key already held.
    collisions: usize,
}

/// Group a table's rows by key and place each group on a synthetic network.
///
/// Keys are placed in sorted order, so one table converts to one set of bytes
/// and a collision chain comes out in a settled order.
fn place(table: &Table, network: Locate) -> Result<Placement<'_>, HashedError> {
    let Keys::Text(filed) = &table.keys else {
        return Err(HashedError::NotKeyedByText);
    };

    let mut keys: Vec<(&str, &Vec<usize>)> = filed
        .iter()
        .map(|(key, positions)| (key.as_ref(), positions))
        .collect();
    keys.sort_unstable_by_key(|&(key, _)| key);

    let mut buckets: BTreeMap<IpNet, Bucket<'_>> = BTreeMap::new();
    let mut collisions = 0;

    for (key, positions) in &keys {
        let rows = positions
            .iter()
            .map(|&at| stored_row(&table.rows[at]))
            .collect();

        let at = network(key);
        let bucket = buckets.entry(at).or_default();
        if let Some((held, _)) = bucket.first() {
            collisions += 1;
            warn!(
                key,
                held = *held,
                network = %at,
                "two table keys hash to one synthetic network"
            );
        }
        bucket.push((key, rows));
    }

    // The header is what makes the file readable on its own: nothing else in it
    // names the columns the cells are stored in.
    let columns: StoredRow<'_> = table.columns.iter().map(String::as_str).collect();
    let header = vec![(table.key_column(), vec![columns])];

    let mut networks = vec![(IpNet::from(IpAddr::V6(HEADER)), header)];
    networks.extend(buckets);

    Ok(Placement {
        networks,
        keys: keys.len(),
        collisions,
    })
}

/// The network a text key is placed on.
fn network(key: &str) -> IpNet {
    let digest = Sha256::digest(key.as_bytes());

    let mut leading = [0u8; 16];
    leading.copy_from_slice(&digest[..16]);
    let bits = u128::from_be_bytes(leading) & KEY_MASK;

    IpNet::from(IpAddr::V6(Ipv6Addr::from_bits(SYNTHETIC | KEY_MARK | bits)))
}

/// One row as its cells, in column order.
fn stored_row(row: &[Cell]) -> StoredRow<'_> {
    row.iter()
        .map(|cell| cell.as_deref().unwrap_or_default())
        .collect()
}

/// One stored row back as cells, where an empty cell is a value the source did
/// not supply.
fn cells(row: &[&str]) -> Vec<Cell> {
    row.iter()
        .map(|cell| (!cell.is_empty()).then(|| Box::from(*cell)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::table::{Index, Schema, TableFormat};

    /// What the format puts between the data section and the metadata.
    const METADATA_MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

    /// An ASN-keyed CSV, with one number carrying two rows.
    const NETWORKS: &str = "asn,name,country\n\
        13335,CLOUDFLARENET,US\n\
        15169,GOOGLE,US\n\
        13335,CLOUDFLARE-AU,AU\n\
        64496,,AU\n";

    /// The table that CSV parses to.
    fn networks() -> Table {
        Table::from_reader(
            NETWORKS.as_bytes(),
            TableFormat::Csv { header: true },
            &Schema::Auto,
            &Index::Column("asn".to_string()),
        )
        .unwrap()
    }

    /// Every row a table files under a key, as cells.
    fn in_memory(table: &Table, key: &str) -> Vec<Vec<Cell>> {
        table.all(key).map(|row| row.cells.to_vec()).collect()
    }

    /// A mapping that puts every key on one network.
    fn collide(_key: &str) -> IpNet {
        IpNet::from(IpAddr::V6(Ipv6Addr::from_bits(SYNTHETIC | KEY_MARK)))
    }

    /// A converted table, opened over the bytes of the conversion.
    fn round_trip(table: &Table) -> HashedTable<Vec<u8>> {
        let (bytes, _) = to_vec(table).unwrap();
        HashedTable::from_source(bytes).unwrap()
    }

    #[test]
    fn a_text_keyed_table_reads_back_the_way_it_went_in() {
        let table = networks();
        let converted = round_trip(&table);

        assert_eq!(converted.columns(), ["asn", "name", "country"]);
        assert_eq!(converted.key_column(), "asn");

        for key in ["13335", "15169", "64496"] {
            assert_eq!(
                converted.rows(key).unwrap(),
                in_memory(&table, key),
                "{key} must read back as the rows the table files under it"
            );
        }
    }

    #[test]
    fn a_key_with_two_rows_keeps_both_of_them() {
        // `Table::all` is part of the surface, so a conversion that kept one row
        // per key would answer half of it.
        let converted = round_trip(&networks());

        let rows = converted.rows("13335").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1].as_deref(), Some("CLOUDFLARENET"));
        assert_eq!(rows[1][1].as_deref(), Some("CLOUDFLARE-AU"));
    }

    #[test]
    fn a_cell_the_source_did_not_supply_reads_back_as_absent() {
        let converted = round_trip(&networks());

        let rows = converted.rows("64496").unwrap();
        assert_eq!(rows[0][1], None);
        assert_eq!(rows[0][2].as_deref(), Some("AU"));
    }

    #[test]
    fn a_key_that_was_never_placed_answers_nothing() {
        let converted = round_trip(&networks());

        assert!(converted.rows("64497").unwrap().is_empty());
        assert!(converted.rows("").unwrap().is_empty());
    }

    #[test]
    fn what_was_converted_is_reported() {
        let (_, converted) = to_vec(&networks()).unwrap();

        assert_eq!(converted.keys, 3);
        assert_eq!(converted.collisions, 0);
        // Three keys and the header.
        assert_eq!(converted.written.networks, 4);
    }

    #[test]
    fn a_table_keyed_by_an_address_is_not_converted_here() {
        let table = Table::from_reader(
            "ip,country\n1.1.1.1,AU\n".as_bytes(),
            TableFormat::Csv { header: true },
            &Schema::Auto,
            &Index::Ip,
        )
        .unwrap();

        let err = to_vec(&table).unwrap_err();

        assert!(matches!(err, HashedError::NotKeyedByText), "{err:?}");
    }

    #[test]
    fn every_key_on_one_network_answers_with_only_its_own_rows() {
        // Forced rather than found: 95 bits of digest will not collide inside a
        // test, and a collision that is never exercised is a collision that is
        // hoped away.
        let table = networks();
        let placed = place(&table, collide).unwrap();

        assert_eq!(placed.collisions, 2, "three keys on one network");
        // The header, and the one network the three keys share.
        assert_eq!(placed.networks.len(), 2);

        let (bytes, _) = mmdb::to_vec(placed.networks).unwrap();
        let converted = HashedTable::with(bytes, collide).unwrap();

        for key in ["13335", "15169", "64496"] {
            let rows = converted.rows(key).unwrap();
            assert_eq!(
                rows,
                in_memory(&table, key),
                "{key} shares a network with the other keys and must not read as one of them"
            );
            for row in &rows {
                assert_eq!(
                    row[0].as_deref(),
                    Some(key),
                    "a row answered for {key} must carry {key}"
                );
            }
        }
    }

    #[test]
    fn a_key_absent_from_a_bucket_it_collides_with_answers_nothing() {
        // Without the stored-key check this is the lookup that returns another
        // key's rows.
        let table = networks();
        let placed = place(&table, collide).unwrap();
        let (bytes, _) = mmdb::to_vec(placed.networks).unwrap();

        let converted = HashedTable::with(bytes, collide).unwrap();

        assert!(
            !converted.rows("13335").unwrap().is_empty(),
            "the network the missing key lands on has to be holding rows"
        );
        assert!(
            converted.rows("64497").unwrap().is_empty(),
            "a key nothing was filed under must answer nothing, not the bucket it landed in"
        );
    }

    #[test]
    fn a_key_is_placed_where_the_layout_says_and_never_on_the_header() {
        for key in ["", "13335", "AU", "a key with spaces", "\u{1f600}"] {
            let at = network(key);
            let IpAddr::V6(address) = at.addr() else {
                panic!("{key} was not placed on an IPv6 address");
            };

            assert_eq!(at.prefix_len(), 128, "{key}");
            assert_eq!(
                address.to_bits() >> 96,
                SYNTHETIC >> 96,
                "{key} must sit in the documentation prefix"
            );
            assert_ne!(address, HEADER, "{key} must not land on the header");
            assert_eq!(
                address.to_bits() & KEY_MARK,
                KEY_MARK,
                "{key} must carry the marker that keeps it off the header"
            );
        }
    }

    #[test]
    fn a_key_maps_to_the_same_network_in_every_process_and_on_every_machine() {
        // Pinned, because a database written by one run has to be readable by
        // the next: a hash that varies per process or per release would make
        // every file on disk unreadable without anything failing loudly.
        assert_eq!(
            network("13335"),
            "2001:db8:ac88:23e6:d862:9745:fe87:474e/128"
                .parse::<IpNet>()
                .unwrap()
        );
        assert_eq!(
            network(""),
            "2001:db8:98fc:1c14:9afb:f4c8:996f:b924/128"
                .parse::<IpNet>()
                .unwrap()
        );
    }

    #[test]
    fn a_conversion_is_the_same_bytes_every_time() {
        // A table whose file changes without its source changing re-uploads and
        // re-syncs itself forever.
        let table = networks();

        let (first, _) = to_vec(&table).unwrap();
        let (second, _) = to_vec(&table).unwrap();

        // Everything up to the metadata marker: the build stamp is the one
        // field that moves between two runs, and it lives behind it.
        let marker = find(&first, METADATA_MARKER);
        assert_eq!(marker, find(&second, METADATA_MARKER));
        assert_eq!(first[..marker], second[..marker]);
    }

    #[test]
    fn a_file_is_written_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("networks.mmdb");
        let table = networks();

        let converted = to_file(&table, &dest).unwrap();

        assert_eq!(converted.keys, 3);
        assert_eq!(
            converted.written.bytes,
            std::fs::metadata(&dest).unwrap().len()
        );

        let opened = HashedTable::from_source(std::fs::read(&dest).unwrap()).unwrap();
        assert_eq!(opened.columns(), ["asn", "name", "country"]);
        assert_eq!(opened.rows("15169").unwrap(), in_memory(&table, "15169"));
    }

    #[test]
    fn a_database_that_is_not_a_converted_table_is_refused() {
        // A geo database decoded through the bucket shape would answer with
        // whatever its records happened to look like.
        let rows = vec![(
            "1.1.1.0/24".parse::<IpNet>().unwrap(),
            vec![("13335", vec![vec!["CLOUDFLARENET"]])],
        )];
        let (mut bytes, _) = mmdb::to_vec(rows).unwrap();

        // The type name is the only thing that separates the two, so it is what
        // the test edits. The same length, because the metadata states it once
        // and every offset behind it would otherwise move.
        let renamed = "GeoIP2-Country";
        assert_eq!(renamed.len(), mmdb::DATABASE_TYPE.len());
        let at = find(&bytes, mmdb::DATABASE_TYPE.as_bytes());
        bytes[at..at + renamed.len()].copy_from_slice(renamed.as_bytes());

        let err = HashedTable::from_source(bytes).unwrap_err();

        assert!(
            matches!(&err, HashedError::NotATable { found } if found == renamed),
            "{err:?}"
        );
    }

    #[test]
    fn a_database_with_no_header_is_refused() {
        // Every other record decodes, so nothing but the missing header says
        // the file is not a converted table.
        let table = networks();
        let placed = place(&table, network).unwrap();
        let without_header: Vec<_> = placed.networks.into_iter().skip(1).collect();

        let (bytes, _) = mmdb::to_vec(without_header).unwrap();
        let err = HashedTable::from_source(bytes).unwrap_err();

        assert!(matches!(err, HashedError::NoHeader), "{err:?}");
    }

    /// Where `needle` starts in `haystack`.
    fn find(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("the database has to carry what was looked for")
    }
}
