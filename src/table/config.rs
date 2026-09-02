// Project:   factbook
// File:      src/table/config.rs
// Purpose:   What a user-supplied table source states, as config
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! One source a deployment names in its own configuration.
//!
//! Every field here is something a config file can write down: a URL, a file
//! name, an enum tag, a list of strings. Nothing is a closure or a type
//! parameter, because a source that cannot be expressed in YAML is a source
//! only a recompile can add, which is the failure this whole module exists to
//! avoid.

use serde::{Deserialize, Serialize};

/// Default for [`CsvOptions::header`]: a CSV published for consumption
/// normally names its columns on the first row.
const fn header_default() -> bool {
    true
}

/// How the rows are encoded in the fetched file.
///
/// A third encoding is one variant and one arm of [`crate::table`]'s reader:
/// YAML is deliberately absent because it would pull in a parser no source
/// shipped here needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableFormat {
    /// Comma-separated rows, quoted per RFC 4180.
    Csv {
        /// Whether the first row names the columns.
        ///
        /// False is the headerless case, where the names come from
        /// [`Schema::Named`] instead.
        header: bool,
    },

    /// A JSON array of objects, where the keys of the objects are the columns.
    Json,
}

/// Where the column names come from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Schema {
    /// Derived from the file: the header row of a CSV, the keys of a JSON
    /// object.
    #[default]
    Auto,

    /// Stated by the operator, which is what a headerless CSV requires and
    /// what pins a JSON source against a provider adding a key.
    Named(Vec<String>),
}

/// Key the rows are reachable by.
///
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Index {
    /// An IP address, in whichever column holds addresses.
    Ip,

    /// A CIDR range, in whichever column holds ranges.
    ///
    /// An address is answered by the most specific range containing it, so a
    /// `/24` wins over the `/8` around it. A bare address counts as the range
    /// holding only itself, because publishers mix single hosts into a prefix
    /// list.
    Prefix,

    /// The exact value of a named column.
    Column(String),
}

/// How the fetched bytes are packaged.
///
/// A tar member is not expressible here: the transfer names its member with a
/// `&'static str`, which config cannot supply without leaking one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceArchive {
    /// The body is the file itself.
    #[default]
    Raw,

    /// The body is a gzip stream wrapping the file.
    Gzip,
}

/// One tabular source a deployment fetches and indexes.
///
/// ```yaml
/// # Fetch a CSV and reach its rows by autonomous system number.
/// url: https://example.net/networks.csv
/// # Published beside the file, and verified before the file is admitted.
/// checksum_url: https://example.net/networks.csv.sha256
/// file: networks.csv
/// format: csv
/// index:
///   column: asn
/// ```
///
/// The same source published headerless, with the names supplied instead:
///
/// ```yaml
/// url: https://example.net/networks.csv.gz
/// archive: gzip
/// file: networks.csv
/// format:
///   csv:
///     header: false
/// schema:
///   named: [asn, name, country]
/// index:
///   column: asn
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableSource {
    /// Where the file is fetched from.
    pub url: String,

    /// Digest published beside the file, where the source publishes one.
    #[serde(default)]
    pub checksum_url: Option<String>,

    /// How the fetched bytes are packaged.
    #[serde(default)]
    pub archive: SourceArchive,

    /// Name the file is written under, inside the configured data directory.
    ///
    /// A plain file name, not a path: a source must not be able to write
    /// outside the directory the operator gave it.
    pub file: String,

    /// How the rows are encoded.
    pub format: TableFormat,

    /// Where the column names come from.
    #[serde(default)]
    pub schema: Schema,

    /// Key the rows are reachable by.
    pub index: Index,

    /// HTTP client the transfer rides on.
    ///
    /// `None` builds a default rustls client carrying the download timeouts.
    /// Not a config-file field: it is a live handle, so serde skips it and
    /// [`with_http_client`](Self::with_http_client) sets it.
    #[serde(skip)]
    pub http_client: Option<reqwest::Client>,
}

impl TableSource {
    /// A source fetched from `url` into `file`, read as `format` and reached
    /// through `index`.
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        file: impl Into<String>,
        format: TableFormat,
        index: Index,
    ) -> Self {
        Self {
            url: url.into(),
            checksum_url: None,
            archive: SourceArchive::Raw,
            file: file.into(),
            format,
            schema: Schema::Auto,
            index,
            http_client: None,
        }
    }

    /// Run the transfer through a client the caller has already configured.
    #[must_use]
    pub fn with_http_client(mut self, client: reqwest::Client) -> Self {
        self.http_client = Some(client);
        self
    }
}

/// Equality is over the configuration, not the transport.
///
/// A consumer compares an old source against a new one to decide whether a
/// reload has to re-fetch, so the comparison covers every field an operator can
/// set. The injected client is excluded because it is a handle rather than a
/// setting, and `reqwest::Client` has no equality to defer to.
impl PartialEq for TableSource {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
            && self.checksum_url == other.checksum_url
            && self.archive == other.archive
            && self.file == other.file
            && self.format == other.format
            && self.schema == other.schema
            && self.index == other.index
    }
}

impl Eq for TableSource {}

/// Bare form of [`TableFormat`], which is what an operator writes when the
/// defaults suit.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BareFormat {
    Csv,
    Json,
}

/// Options a CSV takes.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CsvOptions {
    #[serde(default = "header_default")]
    header: bool,
}

/// Map form of [`TableFormat`], which only CSV has anything to say in.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum FullFormat {
    Csv(CsvOptions),
}

/// Serialised shape of [`TableFormat`]: a bare tag, or a tag with options.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum FormatWire {
    Bare(BareFormat),
    Full(FullFormat),
}

impl Serialize for TableFormat {
    /// Emits the bare form for a CSV with a header, which is what an operator
    /// writes when they have not asked for anything unusual.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match *self {
            Self::Json => FormatWire::Bare(BareFormat::Json),
            Self::Csv { header: true } => FormatWire::Bare(BareFormat::Csv),
            Self::Csv { header } => FormatWire::Full(FullFormat::Csv(CsvOptions { header })),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TableFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match FormatWire::deserialize(deserializer)? {
            FormatWire::Bare(BareFormat::Json) => Self::Json,
            FormatWire::Bare(BareFormat::Csv) => Self::Csv {
                header: header_default(),
            },
            FormatWire::Full(FullFormat::Csv(CsvOptions { header })) => Self::Csv { header },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The source the module docs describe, as a value.
    fn networks() -> TableSource {
        TableSource::new(
            "https://example.net/networks.csv",
            "networks.csv",
            TableFormat::Csv { header: true },
            Index::Column("asn".to_string()),
        )
    }

    #[test]
    fn a_source_needs_four_things_and_defaults_the_rest() {
        let source = networks();

        assert_eq!(source.archive, SourceArchive::Raw);
        assert_eq!(source.schema, Schema::Auto);
        assert!(source.checksum_url.is_none());
        assert!(source.http_client.is_none());
    }

    #[test]
    fn the_common_case_writes_four_keys() {
        // The whole point of the feature is that a source is config rather than
        // code, so the config for an ordinary CSV has to be short.
        let json = r#"{
            "url": "https://example.net/networks.csv",
            "file": "networks.csv",
            "format": "csv",
            "index": {"column": "asn"}
        }"#;
        let source: TableSource = serde_json::from_str(json).unwrap();

        assert_eq!(source, networks());
        assert_eq!(source.format, TableFormat::Csv { header: true });
    }

    #[test]
    fn the_headerless_case_states_its_names() {
        let json = r#"{
            "url": "https://example.net/networks.csv",
            "file": "networks.csv",
            "format": {"csv": {"header": false}},
            "schema": {"named": ["asn", "name", "country"]},
            "index": {"column": "asn"}
        }"#;
        let source: TableSource = serde_json::from_str(json).unwrap();

        assert_eq!(source.format, TableFormat::Csv { header: false });
        assert_eq!(
            source.schema,
            Schema::Named(vec![
                "asn".to_string(),
                "name".to_string(),
                "country".to_string(),
            ])
        );
    }

    #[test]
    fn a_format_round_trips_through_the_form_an_operator_writes() {
        for (format, wire) in [
            (TableFormat::Csv { header: true }, "\"csv\""),
            (TableFormat::Json, "\"json\""),
            (
                TableFormat::Csv { header: false },
                r#"{"csv":{"header":false}}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&format).unwrap(), wire);
            assert_eq!(serde_json::from_str::<TableFormat>(wire).unwrap(), format);
        }
    }

    #[test]
    fn an_index_and_a_schema_round_trip() {
        for (index, wire) in [
            (Index::Ip, "\"ip\""),
            (Index::Column("asn".to_string()), r#"{"column":"asn"}"#),
        ] {
            assert_eq!(serde_json::to_string(&index).unwrap(), wire);
            assert_eq!(serde_json::from_str::<Index>(wire).unwrap(), index);
        }

        assert_eq!(serde_json::to_string(&Schema::Auto).unwrap(), "\"auto\"");
        assert_eq!(
            serde_json::from_str::<Schema>(r#"{"named":["a"]}"#).unwrap(),
            Schema::Named(vec!["a".to_string()])
        );
    }

    #[test]
    fn a_misspelt_key_is_rejected() {
        // A typo that deserialised into a default would fetch the wrong thing
        // quietly, which is the failure mode config has to be loud about.
        let json = r#"{
            "url": "https://example.net/networks.csv",
            "file": "networks.csv",
            "format": "csv",
            "indx": {"column": "asn"}
        }"#;
        assert!(serde_json::from_str::<TableSource>(json).is_err());

        let json = r#"{
            "url": "https://example.net/networks.csv",
            "file": "networks.csv",
            "format": {"csv": {"headr": false}},
            "index": "ip"
        }"#;
        assert!(serde_json::from_str::<TableSource>(json).is_err());
    }

    #[test]
    fn a_whole_source_round_trips() {
        let mut source = networks();
        source.checksum_url = Some("https://example.net/networks.csv.sha256".to_string());
        source.archive = SourceArchive::Gzip;
        source.schema = Schema::Named(vec!["asn".to_string()]);

        let dumped = serde_json::to_string(&source).unwrap();
        assert_eq!(
            serde_json::from_str::<TableSource>(&dumped).unwrap(),
            source
        );
    }

    #[test]
    fn an_injected_client_is_not_a_config_change() {
        // The client is a transport handle, so swapping it must not read as a
        // config edit that a consumer would re-fetch on.
        let injected = networks().with_http_client(reqwest::Client::new());

        assert_eq!(networks(), injected);
        assert!(injected.http_client.is_some());
    }
}
