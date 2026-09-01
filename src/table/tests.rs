// Project:   factbook
// File:      src/table/tests.rs
// Purpose:   Coverage for arbitrary tabular sources
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Every test here stays off the public internet: a source is a URL the
//! operator chose, and the ones exercised here are served over loopback.

use std::io::Write;
use std::net::Ipv4Addr;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

/// A CSV of two networks, keyed by autonomous system number.
const NETWORKS: &str = "asn,name,country\n13335,CLOUDFLARENET,US\n15169,GOOGLE,US\n";

/// Settings pointed at a scratch directory, with every copy already stale.
fn auto(dir: &Path) -> AutoDownloadConfig {
    AutoDownloadConfig {
        enabled: true,
        data_dir: dir.to_path_buf(),
        max_age_days: 0,
        ..Default::default()
    }
}

/// A source fetching `path` from a local server.
fn source_at(server: &MockServer, name: &str) -> TableSource {
    TableSource::new(
        format!("{}/{name}", server.uri()),
        name,
        TableFormat::Csv { header: true },
        Index::Column("asn".to_string()),
    )
}

/// Serve `body` at `/{name}`.
async fn serve(server: &MockServer, name: &str, body: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/{name}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
        .mount(server)
        .await;
}

/// Read a table from a CSV with a header, indexed by `index`.
fn from_csv(body: &str, index: &Index) -> Result<Table, TableError> {
    Table::from_reader(
        body.as_bytes(),
        TableFormat::Csv { header: true },
        &Schema::Auto,
        index,
    )
}

/// SHA-256 of a body as lowercase hex, the way a publisher writes it.
fn sha256_hex(body: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write as _;

    sha2::Sha256::digest(body)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A gzip stream wrapping `payload`.
fn gzip(payload: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(payload).unwrap();
    encoder.finish().unwrap()
}

// ---------------------------------------------------------------------------
// Indexing and lookup
// ---------------------------------------------------------------------------

#[test]
fn a_named_column_reaches_its_rows() {
    let table = from_csv(NETWORKS, &Index::Column("asn".to_string())).unwrap();

    assert_eq!(table.columns().to_vec(), ["asn", "name", "country"]);
    assert_eq!(table.key_column(), "asn");
    assert_eq!(table.len(), 2);
    assert!(!table.is_empty());

    let row = table.get("15169").unwrap();
    assert_eq!(row.get("name"), Some("GOOGLE"));
    assert_eq!(row.get("country"), Some("US"));
    assert_eq!(row.key(), Some("15169"));
    assert_eq!(row.at(1), Some("GOOGLE"));
    assert_eq!(row.get("no_such_column"), None);
    assert!(table.get("64496").is_none());
}

#[test]
fn a_repeated_key_keeps_every_row() {
    // One prefix per row is the normal shape of an ASN-keyed side table, so
    // filing the second row over the first would drop most of the source.
    let body = "asn,prefix\n13335,1.1.1.0/24\n13335,1.0.0.0/24\n";
    let table = from_csv(body, &Index::Column("asn".to_string())).unwrap();

    let prefixes: Vec<Option<&str>> = table.all("13335").map(|row| row.get("prefix")).collect();
    assert_eq!(prefixes, [Some("1.1.1.0/24"), Some("1.0.0.0/24")]);

    // The first is what a single-answer lookup gets.
    assert_eq!(
        table.get("13335").unwrap().get("prefix"),
        Some("1.1.1.0/24")
    );
    assert_eq!(table.all("64496").count(), 0);
}

#[test]
fn rows_are_kept_in_file_order() {
    let table = from_csv(NETWORKS, &Index::Column("asn".to_string())).unwrap();
    let names: Vec<Option<&str>> = table.rows().map(|row| row.get("name")).collect();

    assert_eq!(names, [Some("CLOUDFLARENET"), Some("GOOGLE")]);
    assert_eq!(table.rows().len(), 2);
}

#[test]
fn an_index_naming_a_column_the_source_lacks_is_refused() {
    let err = from_csv(NETWORKS, &Index::Column("as_number".to_string())).unwrap_err();

    assert!(
        matches!(err, TableError::UnknownColumn { ref column, .. } if column == "as_number"),
        "{err:?}"
    );
    // The message names what the source does have, so the fix is in the error.
    assert!(err.to_string().contains("asn, name, country"), "{err}");
}

#[test]
fn an_address_index_finds_a_conventionally_named_column() {
    let body = "ip,operator\n1.1.1.1,CLOUDFLARENET\n2606:4700:4700::1111,CLOUDFLARENET\n";
    let table = from_csv(body, &Index::Ip).unwrap();

    assert_eq!(table.key_column(), "ip");
    assert_eq!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)))
            .unwrap()
            .get("operator"),
        Some("CLOUDFLARENET")
    );
    assert_eq!(
        table
            .get_by_address("2606:4700:4700::1111".parse().unwrap())
            .unwrap()
            .get("operator"),
        Some("CLOUDFLARENET")
    );
    assert!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
            .is_none()
    );
}

#[test]
fn an_address_index_falls_back_to_the_column_that_holds_addresses() {
    // Every publisher names the field differently, so the column is located by
    // what it holds when its name says nothing.
    let body = r#"[{"fingerprint": "AAAA", "or_address": "8.8.8.8"}]"#;
    let table = Table::from_reader(
        body.as_bytes(),
        TableFormat::Json,
        &Schema::Auto,
        &Index::Ip,
    )
    .unwrap();

    assert_eq!(table.key_column(), "or_address");
    assert_eq!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
            .unwrap()
            .get("fingerprint"),
        Some("AAAA")
    );
}

#[test]
fn a_source_with_no_addresses_is_refused_rather_than_guessed_at() {
    let err = from_csv(NETWORKS, &Index::Ip).unwrap_err();

    assert!(matches!(err, TableError::NoAddressColumn { .. }), "{err:?}");
    // The refusal names the escape hatch.
    assert!(err.to_string().contains("index.column"), "{err}");
}

#[test]
fn a_row_with_no_key_is_kept_and_unreachable() {
    // Dropping the row would lose data the source published; indexing an empty
    // key would file unrelated rows together.
    let body = "asn,name\n,ORPHAN\n13335,CLOUDFLARENET\n";
    let table = from_csv(body, &Index::Column("asn".to_string())).unwrap();

    assert_eq!(table.len(), 2);
    assert_eq!(table.rows().next().unwrap().get("name"), Some("ORPHAN"));
    assert_eq!(table.get("").map(|row| row.get("name")), None);
}

#[test]
fn a_row_prints_its_own_cells_rather_than_the_table() {
    let table = from_csv(NETWORKS, &Index::Column("asn".to_string())).unwrap();
    let rendered = format!("{:?}", table.get("13335").unwrap());

    assert!(rendered.contains("\"asn\": Some(\"13335\")"), "{rendered}");
    assert!(
        rendered.contains("\"name\": Some(\"CLOUDFLARENET\")"),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// Config validation
// ---------------------------------------------------------------------------

#[test]
fn a_source_that_cannot_be_fetched_is_reported_before_anything_is() {
    let mut source = TableSource::new(
        "not a url",
        "networks.csv",
        TableFormat::Csv { header: true },
        Index::Ip,
    );
    assert!(matches!(
        source.validate().unwrap_err(),
        TableError::BadUrl { .. }
    ));

    source.url = "https://example.net/networks.csv".to_string();
    source.validate().unwrap();
}

#[test]
fn a_file_name_may_not_climb_out_of_the_data_directory() {
    let mut source = source_name("../../etc/cron.d/payload");
    assert!(matches!(
        source.validate().unwrap_err(),
        TableError::UnsafeFileName { .. }
    ));

    source = source_name("/etc/passwd");
    assert!(matches!(
        source.validate().unwrap_err(),
        TableError::UnsafeFileName { .. }
    ));

    source_name("networks.csv").validate().unwrap();
}

/// A valid source writing to `file`.
fn source_name(file: &str) -> TableSource {
    TableSource::new(
        "https://example.net/networks.csv",
        file,
        TableFormat::Csv { header: true },
        Index::Ip,
    )
}

#[test]
fn a_headerless_csv_without_names_is_a_config_fault() {
    let mut source = source_name("networks.csv");
    source.format = TableFormat::Csv { header: false };

    assert!(matches!(
        source.validate().unwrap_err(),
        TableError::NamesRequired
    ));

    source.schema = Schema::Named(vec![]);
    assert!(matches!(
        source.validate().unwrap_err(),
        TableError::NoNames
    ));

    source.schema = Schema::Named(vec!["asn".to_string()]);
    source.validate().unwrap();
}

#[test]
fn an_mmdb_payload_is_not_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("db.mmdb");
    fs::write(&file, b"whatever").unwrap();

    let err = Table::from_payload(&file, &Payload::Mmdb).unwrap_err();
    assert!(matches!(err, TableError::NotATable), "{err:?}");
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_source_is_fetched_indexed_and_queried() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    serve(&server, "networks.csv", NETWORKS.as_bytes()).await;

    let table = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap();

    assert_eq!(
        table.get("13335").unwrap().get("name"),
        Some("CLOUDFLARENET")
    );
    // The file stays on disk for the next start, which is the point of
    // provisioning it rather than holding it in memory.
    assert_eq!(
        fs::read_to_string(dir.path().join("networks.csv")).unwrap(),
        NETWORKS
    );
}

#[tokio::test]
async fn a_gzip_source_is_unpacked() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    serve(&server, "networks.csv.gz", &gzip(NETWORKS.as_bytes())).await;

    let mut source = source_at(&server, "networks.csv.gz");
    source.archive = SourceArchive::Gzip;
    source.file = "networks.csv".to_string();
    source.url = format!("{}/networks.csv.gz", server.uri());

    let table = Table::ensure(&source, &auto(dir.path())).await.unwrap();
    assert_eq!(table.len(), 2);
}

#[tokio::test]
async fn a_published_digest_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    serve(&server, "networks.csv", NETWORKS.as_bytes()).await;
    serve(
        &server,
        "networks.csv.sha256",
        format!("{}  networks.csv\n", sha256_hex(NETWORKS.as_bytes())).as_bytes(),
    )
    .await;

    let mut source = source_at(&server, "networks.csv");
    source.checksum_url = Some(format!("{}/networks.csv.sha256", server.uri()));

    let table = Table::ensure(&source, &auto(dir.path())).await.unwrap();
    assert_eq!(table.len(), 2);
}

#[tokio::test]
async fn a_fresh_copy_is_not_fetched_again() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    fs::write(dir.path().join("networks.csv"), NETWORKS).unwrap();

    let settings = AutoDownloadConfig {
        max_age_days: 30,
        ..auto(dir.path())
    };
    let table = Table::ensure(&source_at(&server, "networks.csv"), &settings)
        .await
        .unwrap();

    assert_eq!(table.len(), 2);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a fresh copy must cost no request"
    );
}

#[tokio::test]
async fn a_download_that_fails_over_a_local_copy_reads_that_copy() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    fs::write(dir.path().join("networks.csv"), NETWORKS).unwrap();

    // Nothing is mounted, so the transfer gets a 404.
    let table = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap();

    assert_eq!(table.len(), 2);
}

#[tokio::test]
async fn a_download_that_fails_with_no_local_copy_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;

    let err = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap_err();

    assert!(matches!(err, TableError::Fetch(_)), "{err:?}");
}

#[tokio::test]
async fn a_body_that_is_not_a_table_leaves_the_previous_file_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("networks.csv");
    fs::write(&file, NETWORKS).unwrap();

    // A provider answering 200 with a login page is the case that would
    // otherwise wedge that page in place of the last good copy.
    let server = MockServer::start().await;
    serve(
        &server,
        "networks.csv",
        b"<!DOCTYPE html><html><title>Log in</title></html>",
    )
    .await;

    let table = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap();

    assert_eq!(table.len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), NETWORKS);
}

#[tokio::test]
async fn a_replacement_that_holds_no_rows_leaves_the_previous_file_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("networks.csv");
    fs::write(&file, NETWORKS).unwrap();

    // Text, not markup, and over the size floor -- and still not a table.
    let server = MockServer::start().await;
    serve(
        &server,
        "networks.csv",
        b"asn,name,country,region,network\n",
    )
    .await;

    let table = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap();

    assert_eq!(table.len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), NETWORKS);
}

#[tokio::test]
async fn a_replacement_under_the_size_floor_leaves_the_previous_file_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("networks.csv");
    let full: String = (0..500).fold(String::from("asn,name\n"), |mut body, row| {
        use std::fmt::Write as _;
        let _ = writeln!(body, "{row},operator-{row}");
        body
    });
    fs::write(&file, &full).unwrap();

    let server = MockServer::start().await;
    serve(&server, "networks.csv", b"asn,name\n1,one\n").await;

    let table = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap();

    assert_eq!(table.len(), 500);
    assert_eq!(fs::read_to_string(&file).unwrap(), full);
}

#[tokio::test]
async fn auto_download_off_reads_whatever_is_already_there() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    fs::write(dir.path().join("networks.csv"), NETWORKS).unwrap();

    let settings = AutoDownloadConfig {
        enabled: false,
        ..auto(dir.path())
    };
    let table = Table::ensure(&source_at(&server, "networks.csv"), &settings)
        .await
        .unwrap();

    assert_eq!(table.len(), 2);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_config_fault_costs_no_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let mut source = source_at(&server, "networks.csv");
    source.format = TableFormat::Csv { header: false };

    let err = Table::ensure(&source, &auto(dir.path())).await.unwrap_err();

    assert!(matches!(err, TableError::NamesRequired), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
}
