// Project:   factbook
// File:      src/table/mod.rs
// Purpose:   Arbitrary tabular sources: fetch, index, look up by key
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! A table fetched from wherever the operator names, keyed by whatever reaches
//! its rows.
//!
//! The geo half of this crate is opinionated on purpose: it knows what a city
//! database is and what an ASN database is, and it can only fetch the providers
//! that publish them. Plenty of useful reference data is neither -- an ASN-keyed
//! side table, a JSON list of relays, an internal CSV of customer prefixes --
//! and the only thing those have in common with a GeoLite2 build is how they
//! are acquired.
//!
//! So acquisition is shared and interpretation is stated. A [`TableSource`] is
//! config: a URL, a file name, an encoding, where the column names come from,
//! and which key reaches a row. Everything the geo download path does applies
//! to it unchanged -- the published digest is verified, a body that turns out
//! to be a login page is refused, a replacement a fraction of the size of the
//! copy on disk is refused, and a refusal leaves that copy exactly where it is.
//!
//! # Two surfaces, not one
//!
//! [`GeoIp`](crate::geoip::GeoIp) answers `lookup(ip)`, which a table keyed by
//! an autonomous system number cannot. [`Table`] is the generic surface: it
//! knows about columns and keys and nothing about geography.
//!
//! # Example
//!
//! ```yaml
//! # Fetch a CSV and reach its rows by autonomous system number.
//! url: https://example.net/networks.csv
//! checksum_url: https://example.net/networks.csv.sha256
//! file: networks.csv
//! format: csv
//! index:
//!   column: asn
//! ```
//!
//! ```rust,no_run
//! use factbook::geoip::AutoDownloadConfig;
//! use factbook::table::{Index, Table, TableFormat, TableSource};
//!
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let source = TableSource::new(
//!     "https://example.net/networks.csv",
//!     "networks.csv",
//!     TableFormat::Csv { header: true },
//!     Index::Column("asn".to_string()),
//! );
//!
//! let table = Table::ensure(&source, &AutoDownloadConfig::default()).await?;
//! if let Some(row) = table.get("13335") {
//!     println!("{:?}", row.get("name"));
//! }
//! # Ok(())
//! # }
//! ```

mod config;
mod parse;

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::net::IpAddr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::slice;
use std::time::Duration;

use ipnet::IpNet;
use prefix_trie::joint::JointPrefixMap;
use tracing::{debug, warn};

use crate::geoip::AutoDownloadConfig;
use crate::geoip::DatabaseFormat;
use crate::geoip::download::fetch::{self, Archive, Credential, Transfer};
use crate::geoip::download::verify::Guard;
use crate::geoip::download::{GeoIpDownloadError, SECS_PER_DAY, is_fresh};
use parse::Parsed;

pub use config::{Index, Schema, SourceArchive, TableFormat, TableSource};
pub(crate) use parse::probe;

/// Column names an [`Index::Ip`] source is likely to have written, lowercase.
const ADDRESS_COLUMNS: [&str; 5] = ["ip", "ip_address", "ipaddress", "address", "addr"];

/// Column names a CIDR-keyed source is likely to have written, lowercase.
const PREFIX_COLUMNS: [&str; 5] = ["prefix", "network", "cidr", "range", "subnet"];

/// Rows sampled when working out which column holds addresses.
const ADDRESS_SAMPLE: usize = 64;

/// No row is filed under this key.
const NO_ROWS: &[usize] = &[];

/// One cell of a row. `None` is a value the source did not supply.
type Cell = Option<Box<str>>;

/// Errors raised while provisioning or reading a table.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TableError {
    /// The file could not be fetched.
    #[error("fetching the table failed: {0}")]
    Fetch(#[from] GeoIpDownloadError),

    /// A filesystem operation failed.
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    /// The source names somewhere the transfer cannot go.
    #[error("{url} is not a URL that can be fetched: {detail}")]
    BadUrl {
        /// URL as the operator wrote it.
        url: String,
        /// What the URL parser refused it for.
        detail: String,
    },

    /// The file name would write outside the data directory.
    #[error("file {file} is not a plain file name")]
    UnsafeFileName {
        /// Name as the operator wrote it.
        file: String,
    },

    /// The file is not the shape its format states.
    #[error("the table is malformed at line {line}: {detail}")]
    Malformed {
        /// Line the record that was refused started on.
        line: usize,
        /// What the reader refused it for.
        detail: String,
    },

    /// The document is not a JSON array of objects.
    #[error("the source is not a JSON array of objects: {detail}")]
    NotAnArrayOfObjects {
        /// What the reader refused it for.
        detail: String,
    },

    /// A CSV with no header row supplies no column names.
    #[error("a CSV with no header row needs schema.named to supply the column names")]
    NamesRequired,

    /// The stated schema names no columns.
    #[error("schema.named names no columns")]
    NoNames,

    /// The file holds no rows.
    #[error("the source holds no rows")]
    Empty,

    /// The index names a column the source does not have.
    #[error("the index names column {column}, which is not one of: {columns}")]
    UnknownColumn {
        /// Column the index asked for.
        column: String,
        /// Columns the source actually has.
        columns: String,
    },

    /// No column holds IP addresses.
    #[error("no column holds IP addresses; name one with index.column instead. columns: {columns}")]
    NoAddressColumn {
        /// Columns the source actually has.
        columns: String,
    },

    /// No column holds CIDR ranges.
    #[error("no column holds CIDR ranges; name one with index.column instead. columns: {columns}")]
    NoPrefixColumn {
        /// Columns the source actually has.
        columns: String,
    },
}

/// How the rows of a table are reached.
#[derive(Debug)]
enum Keys {
    /// Filed under the text of a column.
    Text(HashMap<Box<str>, Vec<usize>>),

    /// Filed under a parsed address.
    Address(HashMap<IpAddr, Vec<usize>>),

    /// Filed under a CIDR range, reached by the longest one containing an
    /// address.
    Prefix(Box<JointPrefixMap<IpNet, Vec<usize>>>),
}

/// A tabular source, in memory and indexed.
///
/// Rows are kept whole and in file order; the index is a second view of them,
/// so a key that appears twice keeps both rows rather than one silently
/// replacing the other.
///
/// ```
/// use factbook::table::{Index, Schema, Table, TableFormat};
///
/// let csv = "asn,name\n13335,CLOUDFLARENET\n15169,GOOGLE\n";
/// let table = Table::from_reader(
///     csv.as_bytes(),
///     TableFormat::Csv { header: true },
///     &Schema::Auto,
///     &Index::Column("asn".to_string()),
/// )?;
///
/// assert_eq!(table.len(), 2);
/// assert_eq!(table.get("13335").unwrap().get("name"), Some("CLOUDFLARENET"));
/// assert!(table.get("64496").is_none());
/// # Ok::<(), factbook::table::TableError>(())
/// ```
#[derive(Debug)]
pub struct Table {
    /// Column names, in the order a row's cells are stored.
    columns: Vec<String>,

    /// Column the index is built on.
    key_column: usize,

    /// Every row read, in file order.
    rows: Vec<Vec<Cell>>,

    /// Where a key leads.
    keys: Keys,
}

impl Table {
    /// Ensure the source is on disk, downloading when the local copy is missing
    /// or stale, then read it.
    ///
    /// Downloading is non-fatal by the same contract as the geo path: a
    /// transfer that fails over a copy already on disk logs a warning and reads
    /// that copy, because a stale table answers most lookups and no table
    /// answers none.
    ///
    /// # Errors
    ///
    /// [`TableError::Fetch`] when the download fails and there is no local copy
    /// to fall back to, or any of the read errors of [`Table::load`].
    pub async fn ensure(
        source: &TableSource,
        auto: &AutoDownloadConfig,
    ) -> Result<Self, TableError> {
        source.validate()?;

        let dest = auto.data_dir.join(&source.file);

        if !auto.enabled {
            debug!(file = %dest.display(), "auto-download is off, reading what is on disk");
        } else if is_fresh(&dest, u64::from(auto.max_age_days) * SECS_PER_DAY) {
            debug!(file = %dest.display(), "table is fresh");
        } else {
            let client = fetch::client(
                source.http_client.as_ref(),
                Duration::from_secs(auto.connect_timeout_secs),
                Duration::from_secs(auto.read_timeout_secs),
            )?;

            let transfer = transfer(source, dest.clone());
            let guard = Guard::for_table(source.format, &source.schema, auto);

            if let Err(e) = transfer.run_guarded(&client, guard).await {
                if !dest.exists() {
                    return Err(e.into());
                }
                warn!(
                    file = %dest.display(),
                    error = %e,
                    "table download failed, reading the copy already on disk"
                );
            }
        }

        Self::load(&dest, source.format, &source.schema, &source.index)
    }

    /// Read a table off disk.
    ///
    /// # Errors
    ///
    /// [`TableError::Io`] when the file cannot be read, and the reader's own
    /// errors -- [`TableError::Malformed`], [`TableError::NotAnArrayOfObjects`],
    /// [`TableError::NamesRequired`], [`TableError::Empty`] -- when its contents
    /// are not the table the source states.
    pub fn load(
        path: &Path,
        format: TableFormat,
        schema: &Schema,
        index: &Index,
    ) -> Result<Self, TableError> {
        Self::from_reader(BufReader::new(fs::File::open(path)?), format, schema, index)
    }

    /// Read a table from bytes already in hand.
    ///
    /// # Errors
    ///
    /// The errors of [`Table::load`], other than the ones that come from
    /// opening a file.
    pub fn from_reader(
        reader: impl BufRead,
        format: TableFormat,
        schema: &Schema,
        index: &Index,
    ) -> Result<Self, TableError> {
        Self::build(parse::read(reader, format, schema)?, index)
    }

    /// Column names, in the order a row's cells are stored.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Column the rows are indexed by.
    ///
    /// Worth reporting because [`Index::Ip`] locates its own column, so this is
    /// how a deployment sees which one it settled on.
    #[must_use]
    pub fn key_column(&self) -> &str {
        &self.columns[self.key_column]
    }

    /// Whether rows are reached by an address or by the text of a column.
    ///
    /// A lookup that does not match answers nothing rather than failing, so a
    /// source whose `index` was edited from a column to a prefix builds, reports
    /// its full [`len`](Self::len), and returns `None` from every
    /// [`get`](Self::get). This is how a caller tells the two apart.
    #[must_use]
    pub const fn keyed_by_address(&self) -> bool {
        matches!(self.keys, Keys::Address(_) | Keys::Prefix(_))
    }

    /// How many rows the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the table holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Every row, in file order.
    #[must_use]
    pub fn rows(&self) -> Rows<'_> {
        Rows {
            table: self,
            positions: Positions::All(0..self.rows.len()),
        }
    }

    /// The first row filed under a text key.
    ///
    /// Answers nothing on a table keyed by address, which
    /// [`keyed_by_address`](Self::keyed_by_address) reports.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Row<'_>> {
        self.positions_of(key).first().map(|&at| self.row(at))
    }

    /// Every row filed under a text key.
    #[must_use]
    pub fn all(&self, key: &str) -> Rows<'_> {
        Rows {
            table: self,
            positions: Positions::Listed(self.positions_of(key).iter()),
        }
    }

    /// The first row filed under an address.
    ///
    /// Answers nothing on a table keyed by a column, which
    /// [`keyed_by_address`](Self::keyed_by_address) reports.
    #[must_use]
    pub fn get_by_address(&self, address: IpAddr) -> Option<Row<'_>> {
        self.positions_at(address).first().map(|&at| self.row(at))
    }

    /// Every row filed under an address.
    #[must_use]
    pub fn all_by_address(&self, address: IpAddr) -> Rows<'_> {
        Rows {
            table: self,
            positions: Positions::Listed(self.positions_at(address).iter()),
        }
    }

    /// Index parsed rows by the key the source states.
    fn build(parsed: Parsed, index: &Index) -> Result<Self, TableError> {
        let Parsed { columns, rows } = parsed;

        let key_column = match index {
            Index::Column(name) => columns
                .iter()
                .position(|column| column == name)
                .ok_or_else(|| TableError::UnknownColumn {
                    column: name.clone(),
                    columns: columns.join(", "),
                })?,
            Index::Ip => keyed_column(&columns, &rows, &ADDRESS_COLUMNS, |text| {
                as_address(text).is_some()
            })
            .ok_or_else(|| TableError::NoAddressColumn {
                columns: columns.join(", "),
            })?,
            Index::Prefix => keyed_column(&columns, &rows, &PREFIX_COLUMNS, |text| {
                as_prefix(text).is_some()
            })
            .ok_or_else(|| TableError::NoPrefixColumn {
                columns: columns.join(", "),
            })?,
        };

        let mut reachable = 0;
        let keys = match index {
            Index::Column(_) => {
                let mut filed: HashMap<Box<str>, Vec<usize>> = HashMap::new();
                for (at, row) in rows.iter().enumerate() {
                    if let Some(key) = cell(row, key_column) {
                        filed.entry(Box::from(key)).or_default().push(at);
                        reachable += 1;
                    }
                }
                Keys::Text(filed)
            }
            Index::Ip => {
                let mut filed: HashMap<IpAddr, Vec<usize>> = HashMap::new();
                for (at, row) in rows.iter().enumerate() {
                    if let Some(address) = cell(row, key_column).and_then(as_address) {
                        filed.entry(address).or_default().push(at);
                        reachable += 1;
                    }
                }
                Keys::Address(filed)
            }
            Index::Prefix => {
                let mut filed: JointPrefixMap<IpNet, Vec<usize>> = JointPrefixMap::default();
                for (at, row) in rows.iter().enumerate() {
                    if let Some(prefix) = cell(row, key_column).and_then(as_prefix) {
                        // A repeated range keeps every row, the same as a
                        // repeated text key does.
                        filed.entry(prefix).or_default().push(at);
                        reachable += 1;
                    }
                }
                Keys::Prefix(Box::new(filed))
            }
        };

        // Rows whose key is missing or unparseable are kept and unreachable,
        // which is worth saying once rather than losing silently.
        if reachable < rows.len() {
            debug!(
                column = columns[key_column].as_str(),
                rows = rows.len(),
                unreachable = rows.len() - reachable,
                "some table rows carry no key"
            );
        }

        Ok(Self {
            columns,
            key_column,
            rows,
            keys,
        })
    }

    /// Where a text key leads.
    fn positions_of(&self, key: &str) -> &[usize] {
        match &self.keys {
            Keys::Text(filed) => filed.get(key).map_or(NO_ROWS, Vec::as_slice),
            Keys::Address(_) | Keys::Prefix(_) => NO_ROWS,
        }
    }

    /// Where an address leads.
    ///
    /// An exact index answers the address itself; a prefix index answers the
    /// most specific range that contains it, so a `/24` wins over the `/8`
    /// around it.
    fn positions_at(&self, address: IpAddr) -> &[usize] {
        match &self.keys {
            Keys::Address(filed) => filed.get(&address).map_or(NO_ROWS, Vec::as_slice),
            Keys::Prefix(filed) => filed
                .get_lpm(&IpNet::from(address))
                .map_or(NO_ROWS, |(_, rows)| rows.as_slice()),
            Keys::Text(_) => NO_ROWS,
        }
    }

    /// The row at a position.
    fn row(&self, at: usize) -> Row<'_> {
        Row {
            table: self,
            cells: &self.rows[at],
        }
    }
}

impl TableSource {
    /// Check the source can be acted on, before anything is downloaded.
    ///
    /// This is the config-load check: a URL nothing can fetch, a file name that
    /// would write outside the data directory, or a headerless CSV that names
    /// no columns is reported here rather than part way through a transfer.
    ///
    /// # Errors
    ///
    /// [`TableError::BadUrl`], [`TableError::UnsafeFileName`],
    /// [`TableError::NamesRequired`] or [`TableError::NoNames`], naming the
    /// field to fix.
    pub fn validate(&self) -> Result<(), TableError> {
        reqwest::Url::parse(&self.url).map_err(|e| TableError::BadUrl {
            url: self.url.clone(),
            detail: e.to_string(),
        })?;

        // A source states a file name, never a path: it is written into a
        // directory the operator chose, and it may not climb out of it.
        if Path::new(&self.file).file_name() != Some(self.file.as_ref()) {
            return Err(TableError::UnsafeFileName {
                file: self.file.clone(),
            });
        }

        match (&self.format, &self.schema) {
            (TableFormat::Csv { header: false }, Schema::Auto) => Err(TableError::NamesRequired),
            (_, Schema::Named(named)) if named.is_empty() => Err(TableError::NoNames),
            _ => Ok(()),
        }
    }
}

/// One row of a table, borrowed from it.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    /// Table the row belongs to, which is what names its cells.
    table: &'a Table,

    /// Cells of this row, as wide as the table's columns.
    cells: &'a [Cell],
}

impl<'a> Row<'a> {
    /// The value under a column name.
    #[must_use]
    pub fn get(&self, column: &str) -> Option<&'a str> {
        let at = self.table.columns.iter().position(|name| name == column)?;
        self.at(at)
    }

    /// The value at a column position.
    #[must_use]
    pub fn at(&self, column: usize) -> Option<&'a str> {
        cell(self.cells, column)
    }

    /// The value this row is indexed by.
    #[must_use]
    pub fn key(&self) -> Option<&'a str> {
        self.at(self.table.key_column)
    }

    /// Column names, in the order the cells are stored.
    #[must_use]
    pub fn columns(&self) -> &'a [String] {
        &self.table.columns
    }
}

impl fmt::Debug for Row<'_> {
    /// Prints the row's own cells rather than the table behind it, which every
    /// row would otherwise carry a copy of.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut map = f.debug_map();
        for (column, value) in self.table.columns.iter().zip(self.cells) {
            map.entry(column, &value.as_deref());
        }
        map.finish()
    }
}

/// Which rows a [`Rows`] walks.
#[derive(Debug)]
enum Positions<'a> {
    /// The rows filed under one key.
    Listed(slice::Iter<'a, usize>),

    /// Every row, in file order.
    All(Range<usize>),
}

/// Rows of a table, in file order.
#[derive(Debug)]
pub struct Rows<'a> {
    /// Table the rows belong to.
    table: &'a Table,

    /// Positions still to walk.
    positions: Positions<'a>,
}

impl<'a> Iterator for Rows<'a> {
    type Item = Row<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let at = match &mut self.positions {
            Positions::Listed(listed) => *listed.next()?,
            Positions::All(all) => all.next()?,
        };
        Some(self.table.row(at))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.positions {
            Positions::Listed(listed) => listed.size_hint(),
            Positions::All(all) => all.size_hint(),
        }
    }
}

impl ExactSizeIterator for Rows<'_> {}

/// The transfer that fetches one table source.
fn transfer(source: &TableSource, dest: PathBuf) -> Transfer {
    Transfer {
        url: source.url.clone(),
        // A user-supplied source states one URL: there is no publication month
        // to fall back behind.
        fallback_url: None,
        checksum_url: source.checksum_url.clone(),
        dest,
        archive: match source.archive {
            SourceArchive::Raw => Archive::Raw,
            SourceArchive::Gzip => Archive::Gzip,
        },
        format: match source.format {
            TableFormat::Csv { .. } => DatabaseFormat::Csv,
            TableFormat::Json => DatabaseFormat::Json,
        },
        credential: Credential::None,
    }
}

/// The value at a position, absent when the source did not supply one.
fn cell(cells: &[Cell], at: usize) -> Option<&str> {
    cells.get(at).and_then(Option::as_deref)
}

/// A cell as an address, when it is one.
fn as_address(text: &str) -> Option<IpAddr> {
    text.parse().ok()
}

/// A cell as a CIDR range, when it is one.
///
/// A bare address is accepted as the range holding only itself, because
/// publishers mix single hosts into a prefix list.
fn as_prefix(text: &str) -> Option<IpNet> {
    text.parse()
        .ok()
        .or_else(|| as_address(text).map(IpNet::from))
}

/// Which column a source is keyed by.
///
/// A conventional name is preferred, and a source that uses none is sampled
/// instead, because the field is named differently by every publisher. The
/// sampling policy is shared by both key kinds and changes for both at once.
fn keyed_column(
    columns: &[String],
    rows: &[Vec<Cell>],
    names: &[&str],
    parses: fn(&str) -> bool,
) -> Option<usize> {
    let holds = |at: usize| {
        let mut seen = 0;

        for row in rows.iter().take(ADDRESS_SAMPLE) {
            let Some(text) = cell(row, at) else {
                continue;
            };
            if !parses(text) {
                return false;
            }
            seen += 1;
        }

        seen > 0
    };

    let conventional = columns
        .iter()
        .position(|column| names.contains(&column.to_ascii_lowercase().as_str()));

    if let Some(at) = conventional
        && holds(at)
    {
        return Some(at);
    }

    (0..columns.len()).find(|&at| holds(at))
}

#[cfg(test)]
mod tests;
