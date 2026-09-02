// Project:   factbook
// File:      src/table/tests.rs
// Purpose:   Coverage for arbitrary tabular sources
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Every test here stays off the public internet: a source is a URL the
//! operator chose, and the ones exercised here are served over loopback.

use std::net::Ipv4Addr;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::geoip::download::testkit::{gzip, sha256_hex};

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

/// One column of every row, as owned text.
///
/// A row is handed back rather than borrowed from the table, so a value read out
/// of it has to be taken before the row is dropped.
fn column(rows: Rows<'_>, name: &str) -> Vec<String> {
    rows.map(|row| row.get(name).unwrap_or_default().to_string())
        .collect()
}

/// A source read out of the data directory and never fetched.
fn local(name: &str) -> TableSource {
    TableSource::new(
        format!("https://example.net/{name}"),
        name,
        TableFormat::Csv { header: true },
        Index::Column("asn".to_string()),
    )
}

/// Settings reading `dir` and downloading nothing.
fn offline(dir: &Path) -> AutoDownloadConfig {
    AutoDownloadConfig {
        enabled: false,
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

/// A CSV of `rows` numbered operators, big enough to run past a small ceiling.
fn operators(rows: u32) -> String {
    use std::fmt::Write as _;

    (0..rows).fold(String::from("asn,name\n"), |mut body, at| {
        let asn = 64_500 + at;
        let _ = writeln!(body, "{asn},OPERATOR-{asn}");
        body
    })
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
    assert!(!table.keyed_by_address());

    let row = table.get("15169").unwrap();
    assert_eq!(row.get("name"), Some("GOOGLE"));
    assert_eq!(row.get("country"), Some("US"));
    assert_eq!(row.key(), Some("15169"));
    assert_eq!(row.at(1), Some("GOOGLE"));
    assert_eq!(row.get("no_such_column"), None);
    assert!(table.get("64496").is_none());
}

#[test]
fn a_source_with_nothing_to_do_with_addresses_works_the_same() {
    // Nothing in the table half is about networks. A deployment enriching
    // orders by product code reaches its rows the same way one enriching
    // events by autonomous system number does.
    let catalogue = "sku,description,unit,hazard_class\n\
                     AX-1180,Sodium hydroxide pellets,kg,8\n\
                     BR-4402,Acetone,L,3\n";
    let table = from_csv(catalogue, &Index::Column("sku".to_string())).unwrap();

    assert_eq!(table.key_column(), "sku");
    assert_eq!(table.len(), 2);

    let row = table.get("BR-4402").unwrap();
    assert_eq!(row.get("description"), Some("Acetone"));
    assert_eq!(row.get("hazard_class"), Some("3"));
    assert!(table.get("ZZ-0000").is_none());
}

#[test]
fn a_repeated_key_keeps_every_row() {
    // One prefix per row is the normal shape of an ASN-keyed side table, so
    // filing the second row over the first would drop most of the source.
    let body = "asn,prefix\n13335,1.1.1.0/24\n13335,1.0.0.0/24\n";
    let table = from_csv(body, &Index::Column("asn".to_string())).unwrap();

    let prefixes = column(table.all("13335"), "prefix");
    assert_eq!(prefixes, ["1.1.1.0/24", "1.0.0.0/24"]);

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

    assert_eq!(column(table.rows(), "name"), ["CLOUDFLARENET", "GOOGLE"]);
    assert_eq!(table.rows().count(), 2);
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
fn a_v6_only_column_is_detected_under_an_unconventional_name() {
    // Detection samples the values, so a column holding nothing but IPv6 has to
    // be found the same as a mixed or v4-only one.
    let body = "relay,exit_endpoint,country\n\
                Quintex152,2606:4700:4700::1111,US\n\
                Sunet,2001:6b0:7::2,SE\n";
    let table = from_csv(body, &Index::Ip).unwrap();

    assert_eq!(table.key_column(), "exit_endpoint");
    assert_eq!(
        table
            .get_by_address("2001:6b0:7::2".parse().unwrap())
            .unwrap()
            .get("country"),
        Some("SE")
    );
    // A long form and its compressed form are the same address.
    assert_eq!(
        table
            .get_by_address("2606:4700:4700:0000:0000:0000:0000:1111".parse().unwrap())
            .unwrap()
            .get("relay"),
        Some("Quintex152")
    );
}

#[test]
fn a_prefix_index_answers_with_the_most_specific_range() {
    // A prefix list is layered on purpose -- a broad allocation with more
    // specific announcements inside it -- so the narrow one has to win.
    let body = "prefix,operator\n\
                8.0.0.0/8,LEVEL3\n\
                8.8.8.0/24,GOOGLE\n\
                8.8.0.0/16,GOOGLE-WIDE\n";
    let table = from_csv(body, &Index::Prefix).unwrap();

    assert_eq!(table.key_column(), "prefix");
    // A text lookup on an address-keyed table answers nothing rather than
    // failing, so the kind has to be reportable.
    assert!(table.keyed_by_address());
    assert!(table.get("8.8.8.0/24").is_none());
    assert_eq!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)))
            .unwrap()
            .get("operator"),
        Some("GOOGLE")
    );
    assert_eq!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(8, 8, 9, 1)))
            .unwrap()
            .get("operator"),
        Some("GOOGLE-WIDE")
    );
    assert_eq!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(8, 9, 0, 1)))
            .unwrap()
            .get("operator"),
        Some("LEVEL3")
    );
    // Outside every range, rather than falling back to the broadest.
    assert!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)))
            .is_none()
    );
}

#[test]
fn a_prefix_index_covers_ipv6_and_bare_addresses() {
    let body = "cidr,label\n\
                2606:4700::/32,cloudflare\n\
                2606:4700:4700::/48,resolver\n\
                1.1.1.1,single-host\n";
    let table = from_csv(body, &Index::Prefix).unwrap();

    assert_eq!(
        table
            .get_by_address("2606:4700:4700::1111".parse().unwrap())
            .unwrap()
            .get("label"),
        Some("resolver")
    );
    assert_eq!(
        table
            .get_by_address("2606:4700:1::1".parse().unwrap())
            .unwrap()
            .get("label"),
        Some("cloudflare")
    );
    // A bare address is the range holding only itself.
    assert_eq!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)))
            .unwrap()
            .get("label"),
        Some("single-host")
    );
    assert!(
        table
            .get_by_address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 2)))
            .is_none()
    );
}

#[test]
fn a_source_with_no_ranges_is_refused_rather_than_guessed_at() {
    let err = from_csv(NETWORKS, &Index::Prefix).unwrap_err();

    assert!(matches!(err, TableError::NoPrefixColumn { .. }), "{err:?}");
    assert!(err.to_string().contains("index.column"), "{err}");
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
    assert!(table.get("").is_none());
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

    // With no copy to fall back to, the refusal reaches the caller, which is
    // the only place its variant and its detail are visible.
    let bare = tempfile::tempdir().unwrap();
    let err = Table::ensure(&source_at(&server, "networks.csv"), &auto(bare.path()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            TableError::Fetch(GeoIpDownloadError::Unparseable { detail, .. })
                if detail.contains("holds no rows")
        ),
        "{err:?}"
    );

    // A record of the wrong width is refused by the line it started on.
    let ragged = MockServer::start().await;
    serve(&ragged, "networks.csv", b"asn,name\n1,one\n2,two,three\n").await;
    let empty = tempfile::tempdir().unwrap();
    let err = Table::ensure(&source_at(&ragged, "networks.csv"), &auto(empty.path()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            &err,
            TableError::Fetch(GeoIpDownloadError::Unparseable { detail, .. })
                if detail.contains("line 3")
        ),
        "{err:?}"
    );
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

#[tokio::test]
async fn a_json_source_is_fetched_indexed_and_queried() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let body = br#"[{"asn": 13335, "name": "CLOUDFLARENET"}, {"asn": 15169, "name": "GOOGLE"}]"#;
    serve(&server, "networks.json", body).await;

    let source = TableSource::new(
        format!("{}/networks.json", server.uri()),
        "networks.json",
        TableFormat::Json,
        Index::Column("asn".to_string()),
    );

    let table = Table::ensure(&source, &auto(dir.path())).await.unwrap();

    assert_eq!(table.columns().to_vec(), ["asn", "name"]);
    assert_eq!(table.get("15169").unwrap().get("name"), Some("GOOGLE"));
    // The file stays on disk for the next start, the same as a CSV source.
    assert_eq!(
        fs::read(dir.path().join("networks.json")).unwrap(),
        body.to_vec()
    );
}

#[tokio::test]
async fn a_checksum_mismatch_leaves_the_previous_file_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("networks.csv");
    fs::write(&file, NETWORKS).unwrap();

    let server = MockServer::start().await;
    serve(
        &server,
        "networks.csv",
        b"asn,name,country\n1,one,AU\n2,two,NZ\n3,three,US\n",
    )
    .await;
    serve(
        &server,
        "networks.csv.sha256",
        format!("{}  networks.csv\n", sha256_hex(b"what was published")).as_bytes(),
    )
    .await;

    let mut source = source_at(&server, "networks.csv");
    source.checksum_url = Some(format!("{}/networks.csv.sha256", server.uri()));

    let table = Table::ensure(&source, &auto(dir.path())).await.unwrap();

    assert_eq!(table.len(), 2);
    assert_eq!(fs::read_to_string(&file).unwrap(), NETWORKS);

    // With no copy behind it the refusal reaches the caller by name.
    let bare = tempfile::tempdir().unwrap();
    let err = Table::ensure(&source, &auto(bare.path()))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            TableError::Fetch(GeoIpDownloadError::ChecksumMismatch { .. })
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_table_another_process_is_fetching_is_reported_rather_than_waited_on() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    serve(&server, "networks.csv", NETWORKS.as_bytes()).await;

    // Stand in for the other process by holding the lock it would hold. A
    // separate open file description, which is what flock excludes on.
    let held = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.path().join("networks.csv.lock"))
        .unwrap();
    held.lock().unwrap();

    let err = Table::ensure(&source_at(&server, "networks.csv"), &auto(dir.path()))
        .await
        .unwrap_err();

    assert!(
        matches!(err, TableError::Fetch(GeoIpDownloadError::Busy { .. })),
        "{err:?}"
    );
    // Turned away before the request, so the other process keeps its quota.
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_injected_client_is_what_the_transfer_rides_on() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    serve(&server, "networks.csv", NETWORKS.as_bytes()).await;

    // The user agent is the part of the request only the caller's client can
    // have set: the default one names the crate and its version.
    let injected = reqwest::Client::builder()
        .user_agent("a client the operator configured")
        .build()
        .unwrap();
    let source = source_at(&server, "networks.csv").with_http_client(injected);

    let table = Table::ensure(&source, &auto(dir.path())).await.unwrap();

    assert_eq!(table.len(), 2);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("user-agent").unwrap(),
        "a client the operator configured"
    );
}

#[tokio::test]
async fn a_schema_that_names_its_columns_refuses_a_file_of_another_width() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    serve(&server, "networks.csv", NETWORKS.as_bytes()).await;

    // The body is internally consistent at three columns, so nothing but the
    // width the schema names has anything to refuse it for.
    let mut source = source_at(&server, "networks.csv");
    source.schema = Schema::Named(vec!["asn".to_string(), "name".to_string()]);

    let err = Table::ensure(&source, &auto(dir.path())).await.unwrap_err();

    assert!(
        matches!(
            &err,
            TableError::Fetch(GeoIpDownloadError::Unparseable { detail, .. })
                if detail.contains("3 fields against the expected 2")
        ),
        "{err:?}"
    );
    assert!(!dir.path().join("networks.csv").exists());

    // The same body under a schema taken from the file is admitted, so it is
    // the declared width doing the refusing.
    source.schema = Schema::Auto;
    let table = Table::ensure(&source, &auto(dir.path())).await.unwrap();
    assert_eq!(table.len(), 2);
}

// ---------------------------------------------------------------------------
// Held in memory, or refused
// ---------------------------------------------------------------------------

#[test]
fn a_read_stops_at_the_ceiling_rather_than_reading_to_the_end() {
    // A quoting fault the reader refuses the instant it meets it, past the row
    // the ceiling is reached on. Reporting it would mean the whole file had
    // been read, which is the memory the ceiling exists not to spend.
    let mut body = operators(200);
    body.push_str("64999,\"OPERATOR\" LTD\n");

    let stopped = parse::read(
        body.as_bytes(),
        TableFormat::Csv { header: true },
        &Schema::Auto,
        512,
    )
    .unwrap_err();
    assert!(
        matches!(stopped, TableError::OverResidentCeiling { ceiling: 512 }),
        "{stopped:?}"
    );

    // Unbounded, the same body reaches that row, so it is the ceiling doing the
    // stopping rather than anything about the file.
    let read_out = parse::read(
        body.as_bytes(),
        TableFormat::Csv { header: true },
        &Schema::Auto,
        u64::MAX,
    )
    .unwrap_err();
    assert!(
        matches!(read_out, TableError::Malformed { .. }),
        "{read_out:?}"
    );
}

#[tokio::test]
async fn a_table_that_fits_is_held_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("networks.csv"), NETWORKS).unwrap();

    let table = Table::ensure(&local("networks.csv"), &offline(dir.path()))
        .await
        .unwrap();

    assert_eq!(table.backing(), TableBacking::Resident);
    assert_eq!(
        table.get("13335").unwrap().get("name"),
        Some("CLOUDFLARENET")
    );
}

#[tokio::test]
async fn a_table_that_will_not_fit_is_refused_at_load() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("networks.csv"), operators(200)).unwrap();

    let settings = AutoDownloadConfig {
        resident_max_bytes: 512,
        ..offline(dir.path())
    };
    let err = Table::ensure(&local("networks.csv"), &settings)
        .await
        .unwrap_err();

    assert!(
        matches!(&err, TableError::OverResidentCeiling { ceiling } if *ceiling == 512),
        "{err:?}"
    );
    // The refusal names the setting that decided it.
    assert!(err.to_string().contains("resident_max_bytes"), "{err}");

    // The same file loads with room for it, so it is the ceiling doing the
    // refusing rather than anything about the source.
    let table = Table::ensure(&local("networks.csv"), &offline(dir.path()))
        .await
        .unwrap();
    assert_eq!(table.len(), 200);
}
