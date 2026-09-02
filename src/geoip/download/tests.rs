// Project:   factbook
// File:      src/geoip/download/tests.rs
// Purpose:   Coverage for GeoIP database provisioning
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Every test here stays off the public internet.
//!
//! The provider endpoints are real third-party services; hitting them from a
//! test suite would make it slow, flaky and rude. Resolution is exercised on
//! the paths that decide an outcome before a socket is opened, and the transfer
//! itself is exercised against the same request shapes served over loopback.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::testkit::{gzip, mmdb_body, sha256_hex, tar_gz};
use super::*;
use crate::geoip::config::AutoDownloadConfig;

/// A provider on its free line.
fn free(provider: GeoIpProvider) -> ProviderChoice {
    ProviderChoice::from(provider)
}

/// A provider on its paid line.
fn paid(provider: GeoIpProvider) -> ProviderChoice {
    ProviderChoice {
        provider,
        tier: ProviderTier::Paid,
    }
}

/// Config pointed at a scratch directory, with auto-download on.
fn config_in(dir: &Path, provider: GeoIpProvider) -> GeoIpConfig {
    config_for(dir, free(provider))
}

/// Config for one provider choice, pointed at a scratch directory.
fn config_for(dir: &Path, choice: ProviderChoice) -> GeoIpConfig {
    GeoIpConfig {
        provider: choice.into(),
        auto_download: AutoDownloadConfig {
            enabled: true,
            data_dir: dir.to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Config carrying every provider credential, for the tests that plan a
/// transfer rather than run one.
fn credentialled(dir: &Path, provider: GeoIpProvider) -> GeoIpConfig {
    credentialled_for(dir, free(provider))
}

/// The same, for a choice that names its tier.
fn credentialled_for(dir: &Path, choice: ProviderChoice) -> GeoIpConfig {
    let mut config = config_for(dir, choice);
    config.auto_download.maxmind_account_id = Some("123456".into());
    config.auto_download.maxmind_license_key = Some("secret-key".into());
    config.auto_download.ipinfo_token = Some("token-wxyz".into());
    config
}

/// The client a deployment gets when it injects none.
fn default_client() -> reqwest::Client {
    let defaults = AutoDownloadConfig::default();
    fetch::client(
        None,
        Duration::from_secs(defaults.connect_timeout_secs),
        Duration::from_secs(defaults.read_timeout_secs),
    )
    .unwrap()
}

/// The same URL, served by a local mock instead of the provider.
///
/// The query is kept: MaxMind's archive and its digest share a path and differ
/// only by `suffix`.
fn at_server(server: &MockServer, url: &str) -> String {
    let parsed = reqwest::Url::parse(url).unwrap();
    match parsed.query() {
        Some(query) => format!("{}{}?{query}", server.uri(), parsed.path()),
        None => format!("{}{}", server.uri(), parsed.path()),
    }
}

/// The transfers plan() built, aimed at a local server.
///
/// Only the host moves: the path, the credential, the archive shape and the
/// destination are the ones the provider table chose.
fn planned(kind: Kind, config: &GeoIpConfig, server: &MockServer) -> Vec<Transfer> {
    plan(kind, config)
        .unwrap()
        .into_iter()
        .map(|mut transfer| {
            transfer.url = at_server(server, &transfer.url);
            transfer.fallback_url = transfer
                .fallback_url
                .as_deref()
                .map(|url| at_server(server, url));
            transfer.checksum_url = transfer
                .checksum_url
                .as_deref()
                .map(|url| at_server(server, url));
            transfer
        })
        .collect()
}

/// URL paths the server was asked for, in order.
async fn requested_paths(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_string())
        .collect()
}

/// Method and target of everything the server saw, in order.
///
/// A request the mock is proxying rather than serving arrives as a `CONNECT` to
/// the origin it was aimed at, which is what makes a download attempt visible
/// even though the transfer itself cannot complete.
async fn requested_targets(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| format!("{} {}", request.method, request.url))
        .collect()
}

/// Path component of a URL.
fn path_of(url: &str) -> String {
    reqwest::Url::parse(url).unwrap().path().to_string()
}

/// A transfer of a raw body, which is the shape the IPinfo and sapics providers
/// take.
fn raw_transfer(url: String, dest: PathBuf) -> Transfer {
    Transfer {
        url,
        fallback_url: None,
        checksum_url: None,
        dest,
        archive: Archive::Raw,
        format: DatabaseFormat::Mmdb,
        credential: Credential::None,
    }
}

/// A transfer that is allowed to continue a part file, because a digest is
/// published for it.
fn resumable_transfer(url: String, dest: PathBuf, checksum_url: String) -> Transfer {
    Transfer {
        checksum_url: Some(checksum_url),
        ..raw_transfer(url, dest)
    }
}

/// A transfer of a gzip body with no digest behind it, which resumes on the
/// container's own integrity check alone.
fn gzip_transfer(url: String, dest: PathBuf) -> Transfer {
    Transfer {
        archive: Archive::Gzip,
        ..raw_transfer(url, dest)
    }
}

/// A one-shot server that writes `chunks` with `gap` between them.
///
/// wiremock serves a body in a single write, so a transfer that is slow while
/// still progressing -- the free tiers' normal behaviour -- needs a socket the
/// test drives itself. Returns the URL to fetch.
fn drip_server(chunks: Vec<Vec<u8>>, gap: Duration) -> String {
    let length = chunks.iter().map(Vec::len).sum();
    drip_server_promising(chunks, length, gap)
}

/// The same, declaring a `Content-Length` the chunks do not add up to.
fn drip_server_promising(chunks: Vec<Vec<u8>>, length: usize, gap: Duration) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        // The request head arrives in one write, and nothing here needs to
        // parse it.
        let mut buffer = [0u8; 2048];
        let _ = socket.read(&mut buffer).await;

        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
        // Writes are ignored on failure: the client hangs up first whenever the
        // test is about a transfer that does not finish.
        let _ = socket.write_all(head.as_bytes()).await;

        // The gap sits between chunks rather than before the first, so a test
        // can have bytes land and then have delivery stop.
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(gap).await;
            }
            let _ = socket.write_all(&chunk).await;
            let _ = socket.flush().await;
        }
    });

    format!("http://{address}/db.mmdb")
}

/// A one-shot server that answers only once the test releases it.
///
/// Returns the URL, a receiver that fires when the request head has arrived,
/// and the sender that lets the body go. A transfer aimed here holds its lock
/// from the moment the receiver fires until the sender is used, which is the
/// window a second writer has to be turned away in.
fn held_server(
    body: Vec<u8>,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (arrived, has_arrived) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };

        let mut buffer = [0u8; 2048];
        let _ = socket.read(&mut buffer).await;
        let _ = arrived.send(());
        let _ = released.await;

        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&body).await;
        let _ = socket.flush().await;
    });

    (format!("http://{address}/db.mmdb"), has_arrived, release)
}

/// Names of everything left in a directory that a reader might open.
///
/// Lock files are excluded. They persist by design -- a lock follows the inode,
/// so removing one is how two processes end up writing the same part file --
/// and what these assertions are really asking is whether a partial or refused
/// download was left where something could mistake it for a database.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| Path::new(name).extension() != Some("lock".as_ref()))
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// The provider table
// ---------------------------------------------------------------------------

#[test]
fn every_provider_declares_its_files_and_their_format() {
    let mmdb = |name| {
        Some(DatabaseSpec {
            format: DatabaseFormat::Mmdb,
            names: name,
        })
    };
    let cases = [
        (
            free(GeoIpProvider::DbIp),
            mmdb(&["dbip-city-lite.mmdb"][..]),
            mmdb(&["dbip-asn-lite.mmdb"][..]),
        ),
        (
            free(GeoIpProvider::MaxMind),
            mmdb(&["GeoLite2-City.mmdb"][..]),
            mmdb(&["GeoLite2-ASN.mmdb"][..]),
        ),
        (
            paid(GeoIpProvider::MaxMind),
            mmdb(&["GeoIP2-City.mmdb"][..]),
            // The paid line has no ASN database of its own.
            mmdb(&["GeoIP2-ISP.mmdb"][..]),
        ),
        (
            free(GeoIpProvider::IpInfo),
            mmdb(&["ipinfo-lite.mmdb"][..]),
            None,
        ),
        (
            free(GeoIpProvider::SapicsOriginAsn),
            None,
            mmdb(&["origin-asn.mmdb"][..]),
        ),
        (
            free(GeoIpProvider::SapicsIpToAsn),
            None,
            mmdb(&["iptoasn-asn.mmdb"][..]),
        ),
        (free(GeoIpProvider::Custom), None, None),
    ];

    for (choice, city, asn) in cases {
        assert_eq!(provider_files(choice).unwrap(), (city, asn), "{choice:?}");
    }
}

#[test]
fn planned_destinations_match_the_declared_files() {
    // The freshness check reads the declared names while the download writes
    // whatever plan() chose. A mismatch would re-download every startup.
    let dir = PathBuf::from("/var/lib/geoip");
    for choice in [
        free(GeoIpProvider::DbIp),
        free(GeoIpProvider::MaxMind),
        paid(GeoIpProvider::MaxMind),
        free(GeoIpProvider::IpInfo),
        free(GeoIpProvider::SapicsIpToAsn),
        free(GeoIpProvider::SapicsOriginAsn),
    ] {
        let config = credentialled_for(&dir, choice);

        for kind in [Kind::City, Kind::Asn] {
            match (plan(kind, &config), kind.spec(choice).unwrap()) {
                (Ok(transfers), Some(spec)) => {
                    let destinations: Vec<PathBuf> = transfers
                        .iter()
                        .map(|transfer| transfer.dest.clone())
                        .collect();
                    let declared: Vec<PathBuf> =
                        spec.names.iter().map(|name| dir.join(name)).collect();
                    assert_eq!(destinations, declared, "{choice:?} {kind:?}");
                }
                (Err(GeoIpDownloadError::NoDatabases { .. }), None) => {}
                (result, expected) => {
                    panic!("{choice:?} {kind:?}: {result:?} does not match {expected:?}")
                }
            }
        }
    }
}

#[test]
fn each_provider_resolves_its_documented_urls() {
    let dir = PathBuf::from("/var/lib/geoip");
    let urls = |kind, provider| -> Vec<String> {
        plan(kind, &credentialled(&dir, provider))
            .unwrap()
            .into_iter()
            .map(|transfer| transfer.url)
            .collect()
    };

    assert_eq!(
        urls(Kind::City, GeoIpProvider::MaxMind),
        ["https://download.maxmind.com/geoip/databases/GeoLite2-City/download?suffix=tar.gz"]
    );
    assert_eq!(
        urls(Kind::Asn, GeoIpProvider::MaxMind),
        ["https://download.maxmind.com/geoip/databases/GeoLite2-ASN/download?suffix=tar.gz"]
    );
    assert_eq!(
        urls(Kind::City, GeoIpProvider::IpInfo),
        ["https://ipinfo.io/data/ipinfo_lite.mmdb"]
    );
    // sapics serves release assets rather than repository paths, on an undated
    // URL, with a digest published beside each one.
    assert_eq!(
        urls(Kind::Asn, GeoIpProvider::SapicsOriginAsn),
        ["https://github.com/sapics/ip-location-db/releases/download/latest/origin-asn.mmdb"]
    );
    assert_eq!(
        urls(Kind::Asn, GeoIpProvider::SapicsIpToAsn),
        ["https://github.com/sapics/ip-location-db/releases/download/latest/iptoasn-asn.mmdb"]
    );

    let checksums = |provider| -> Vec<Option<String>> {
        plan(Kind::Asn, &credentialled(&dir, provider))
            .unwrap()
            .into_iter()
            .map(|transfer| transfer.checksum_url)
            .collect()
    };
    assert_eq!(
        checksums(GeoIpProvider::SapicsOriginAsn),
        [Some(
            "https://github.com/sapics/ip-location-db/releases/download/checksum/origin-asn.mmdb.sha256"
                .to_string()
        )]
    );
}

#[test]
fn the_maxmind_tier_selects_the_edition_on_one_endpoint() {
    // Both lines download from the one endpoint under the one credential, so
    // the tier shows up only as the edition id and the file it writes.
    let dir = PathBuf::from("/var/lib/geoip");
    let planned = |kind, choice| -> Transfer {
        plan(kind, &credentialled_for(&dir, choice))
            .unwrap()
            .remove(0)
    };

    let free_city = planned(Kind::City, free(GeoIpProvider::MaxMind));
    let paid_city = planned(Kind::City, paid(GeoIpProvider::MaxMind));
    let paid_asn = planned(Kind::Asn, paid(GeoIpProvider::MaxMind));

    assert_eq!(
        free_city.url,
        "https://download.maxmind.com/geoip/databases/GeoLite2-City/download?suffix=tar.gz"
    );
    assert_eq!(
        paid_city.url,
        "https://download.maxmind.com/geoip/databases/GeoIP2-City/download?suffix=tar.gz"
    );
    assert_eq!(paid_city.dest, dir.join("GeoIP2-City.mmdb"));
    assert_eq!(
        paid_city.archive,
        Archive::TarGz {
            member: "GeoIP2-City.mmdb"
        }
    );
    // The paid line carries ASN data in its ISP database.
    assert_eq!(
        paid_asn.url,
        "https://download.maxmind.com/geoip/databases/GeoIP2-ISP/download?suffix=tar.gz"
    );
    assert_eq!(paid_asn.dest, dir.join("GeoIP2-ISP.mmdb"));
}

#[test]
fn a_paid_maxmind_selection_still_needs_the_credential() {
    // The tier is named in the config, so the failure is the missing licence
    // key rather than a 401 on the first transfer.
    let dir = PathBuf::from("/var/lib/geoip");
    let config = config_for(&dir, paid(GeoIpProvider::MaxMind));

    let err = validate(&config).unwrap_err();

    assert!(
        matches!(
            err,
            GeoIpDownloadError::MissingCredential {
                provider: "MaxMind",
                field: "auto_download.maxmind_account_id",
            }
        ),
        "{err:?}"
    );
}

#[test]
fn an_unmodelled_paid_tier_is_refused_rather_than_guessed_at() {
    let dir = PathBuf::from("/var/lib/geoip");

    for provider in [
        GeoIpProvider::DbIp,
        GeoIpProvider::IpInfo,
        GeoIpProvider::SapicsIpToAsn,
        GeoIpProvider::SapicsOriginAsn,
    ] {
        let config = credentialled_for(&dir, paid(provider));
        let err = validate(&config).unwrap_err();

        let message = err.to_string();
        assert!(
            matches!(
                err,
                GeoIpDownloadError::UnsupportedTier { tier: "paid", .. }
            ),
            "{provider:?}: {err:?}"
        );
        assert!(message.contains("paid"), "{message}");
    }
}

#[test]
fn a_config_that_can_be_acted_on_validates() {
    let dir = PathBuf::from("/var/lib/geoip");

    // A provider that publishes only one kind is a legitimate config.
    validate(&config_in(&dir, GeoIpProvider::SapicsIpToAsn)).unwrap();
    validate(&credentialled_for(&dir, paid(GeoIpProvider::MaxMind))).unwrap();
    validate(&config_in(&dir, GeoIpProvider::DbIp)).unwrap();

    // Nothing is fetched, so nothing is required.
    let mut off = config_for(&dir, paid(GeoIpProvider::IpInfo));
    off.auto_download.enabled = false;
    validate(&off).unwrap();

    // An explicit path is the operator's own file.
    let mut explicit = config_for(&dir, paid(GeoIpProvider::IpInfo));
    explicit.city_db_path = Some("/data/city.mmdb".into());
    explicit.asn_db_path = Some("/data/asn.mmdb".into());
    validate(&explicit).unwrap();
}

#[tokio::test]
async fn an_unmodelled_tier_resolves_to_nothing_rather_than_a_wrong_url() {
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled_for(dir.path(), paid(GeoIpProvider::DbIp));

    let databases = ensure_databases(&config).await.unwrap();

    assert_eq!(databases, Databases::default());
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

#[test]
fn the_dbip_url_is_this_month_with_last_month_behind_it() {
    /// The `YYYY-MM` a DB-IP URL points at.
    fn month_in(url: &str) -> String {
        url.rsplit_once("-lite-")
            .unwrap()
            .1
            .trim_end_matches(".mmdb.gz")
            .to_string()
    }

    /// Count of months from `earlier` to `later`, both `YYYY-MM`.
    fn months_between(earlier: &str, later: &str) -> i32 {
        let ordinal = |month: &str| {
            let (year, month) = month.split_once('-').unwrap();
            year.parse::<i32>().unwrap() * 12 + month.parse::<i32>().unwrap()
        };
        ordinal(later) - ordinal(earlier)
    }

    let config = config_in(Path::new("/var/lib/geoip"), GeoIpProvider::DbIp);

    for (kind, slug) in [(Kind::City, "city"), (Kind::Asn, "asn")] {
        let transfers = plan(kind, &config).unwrap();
        assert_eq!(transfers.len(), 1);
        let fallback = transfers[0].fallback_url.clone().unwrap();

        let current = month_in(&transfers[0].url);
        let previous = month_in(&fallback);
        assert_eq!(
            transfers[0].url,
            format!("https://download.db-ip.com/free/dbip-{slug}-lite-{current}.mmdb.gz")
        );
        assert_eq!(
            fallback,
            format!("https://download.db-ip.com/free/dbip-{slug}-lite-{previous}.mmdb.gz")
        );

        assert_eq!(current, chrono::Utc::now().format("%Y-%m").to_string());
        assert_eq!(
            months_between(&previous, &current),
            1,
            "{previous} {current}"
        );
    }
}

// ---------------------------------------------------------------------------
// is_fresh
// ---------------------------------------------------------------------------

#[test]
fn is_fresh_rejects_a_missing_file() {
    let missing = Path::new("/nonexistent/path/file.mmdb");
    assert!(!is_fresh(missing, 0));
    assert!(!is_fresh(missing, 86_400));
    assert!(!is_fresh(missing, u64::MAX));
}

#[test]
fn is_fresh_accepts_a_new_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(is_fresh(file.path(), 86_400));
    assert!(is_fresh(file.path(), u64::MAX / 2));
}

#[test]
fn is_fresh_rejects_everything_at_zero_max_age() {
    let file = tempfile::NamedTempFile::new().unwrap();
    assert!(!is_fresh(file.path(), 0));
}

#[test]
fn is_fresh_handles_a_directory_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    assert!(is_fresh(dir.path(), 86_400));
}

// ---------------------------------------------------------------------------
// Explicit paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn custom_provider_returns_the_explicit_paths() {
    let config = GeoIpConfig {
        provider: GeoIpProvider::Custom.into(),
        city_db_path: Some("/data/city.mmdb".into()),
        asn_db_path: Some("/data/asn.mmdb".into()),
        ..Default::default()
    };

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(
        databases.city.unwrap().files,
        [PathBuf::from("/data/city.mmdb")]
    );
    assert_eq!(
        databases.asn.unwrap().files,
        [PathBuf::from("/data/asn.mmdb")]
    );
}

#[tokio::test]
async fn explicit_paths_override_the_provider() {
    let config = GeoIpConfig {
        provider: GeoIpProvider::DbIp.into(),
        city_db_path: Some("/custom/city.mmdb".into()),
        asn_db_path: Some("/custom/asn.mmdb".into()),
        ..Default::default()
    };

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(
        databases.city.unwrap().files,
        [PathBuf::from("/custom/city.mmdb")]
    );
    assert_eq!(
        databases.asn.unwrap().files,
        [PathBuf::from("/custom/asn.mmdb")]
    );
}

#[tokio::test]
async fn custom_provider_without_paths_returns_nothing() {
    let config = GeoIpConfig {
        provider: GeoIpProvider::Custom.into(),
        ..Default::default()
    };

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases, Databases::default());
}

#[tokio::test]
async fn an_explicit_path_bypasses_its_own_kind_only() {
    // Each kind resolves on its own, so an explicit ASN path leaves the city
    // database to the provider it was configured with.
    let dir = tempfile::tempdir().unwrap();
    let city = dir.path().join("dbip-city-lite.mmdb");
    fs::write(&city, b"fake city").unwrap();

    let mut config = config_in(dir.path(), GeoIpProvider::DbIp);
    config.asn_db_path = Some("/opt/asn.mmdb".into());
    config.auto_download.enabled = false;

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases.city.unwrap().files, [city]);
    assert_eq!(
        databases.asn.unwrap().files,
        [PathBuf::from("/opt/asn.mmdb")]
    );
}

// ---------------------------------------------------------------------------
// enabled / auto_download gates
// ---------------------------------------------------------------------------

#[tokio::test]
async fn disabled_config_resolves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("dbip-city-lite.mmdb"), b"fake mmdb").unwrap();

    let mut config = config_in(dir.path(), GeoIpProvider::DbIp);
    config.enabled = false;

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases, Databases::default());
}

#[tokio::test]
async fn auto_download_off_returns_only_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    let city = dir.path().join("dbip-city-lite.mmdb");
    fs::write(&city, b"fake mmdb").unwrap();
    // The ASN file is deliberately absent.

    let mut config = config_in(dir.path(), GeoIpProvider::DbIp);
    config.auto_download.enabled = false;

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases.city.unwrap().files, [city]);
    assert!(databases.asn.is_none());
}

#[tokio::test]
async fn auto_download_off_over_an_empty_directory_returns_nothing() {
    let mut config = config_in(Path::new("/nonexistent"), GeoIpProvider::DbIp);
    config.auto_download.enabled = false;

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases, Databases::default());
}

// ---------------------------------------------------------------------------
// Per-kind providers and the declared format
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_kind_resolves_through_its_own_provider() {
    // City from one provider, ASN from another, which is the point of the
    // per-kind selection: no single provider publishes the best of both.
    let dir = tempfile::tempdir().unwrap();
    let city = dir.path().join("ipinfo-lite.mmdb");
    let asn = dir.path().join("origin-asn.mmdb");
    for file in [&city, &asn] {
        fs::write(file, b"fake").unwrap();
    }

    let mut config = config_in(dir.path(), GeoIpProvider::DbIp);
    config.provider = ProviderSelection {
        city: free(GeoIpProvider::IpInfo),
        asn: free(GeoIpProvider::SapicsOriginAsn),
    };
    config.auto_download.enabled = false;

    let databases = ensure_databases(&config).await.unwrap();
    let city_database = databases.city.unwrap();
    let asn_database = databases.asn.unwrap();

    assert_eq!(city_database.files, [city]);
    assert_eq!(city_database.format, DatabaseFormat::Mmdb);
    assert_eq!(asn_database.files, [asn]);
    assert_eq!(asn_database.format, DatabaseFormat::Mmdb);
}

#[test]
fn a_database_is_its_whole_file_set() {
    // Every provider shipped today publishes one file, and the resolve path is
    // written for a set, so the multi-file case is covered here rather than
    // through a provider that does not exist yet.
    let spec = DatabaseSpec {
        format: DatabaseFormat::Csv,
        names: &["ranges-ipv4.csv", "ranges-ipv6.csv"],
    };
    let database = spec.at(Path::new("/var/lib/geoip"));

    assert_eq!(database.format, DatabaseFormat::Csv);
    assert_eq!(
        database.files,
        [
            PathBuf::from("/var/lib/geoip/ranges-ipv4.csv"),
            PathBuf::from("/var/lib/geoip/ranges-ipv6.csv"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Missing credentials
// ---------------------------------------------------------------------------

#[tokio::test]
async fn maxmind_without_credentials_degrades_to_no_paths() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::MaxMind);

    // Both downloads fail at the credential check, before any socket is opened.
    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases, Databases::default());
}

#[tokio::test]
async fn ipinfo_without_a_token_degrades_to_no_paths() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::IpInfo);

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases, Databases::default());
}

#[tokio::test]
async fn a_stale_file_survives_a_failed_download() {
    // max_age_days = 0 makes every file stale, and the missing MaxMind
    // credentials make the download fail without a network call. The stale file
    // must still come back: degraded enrichment beats none.
    let dir = tempfile::tempdir().unwrap();
    let city = dir.path().join("GeoLite2-City.mmdb");
    fs::write(&city, b"stale city").unwrap();

    let mut config = config_in(dir.path(), GeoIpProvider::MaxMind);
    config.auto_download.max_age_days = 0;

    let databases = ensure_databases(&config).await.unwrap();
    assert_eq!(databases.city.unwrap().files, [city]);
    assert!(databases.asn.is_none(), "no stale ASN file exists");
}

#[test]
fn missing_credentials_name_the_config_field() {
    let dir = PathBuf::from("/var/lib/geoip");

    let err = plan(Kind::City, &config_in(&dir, GeoIpProvider::MaxMind)).unwrap_err();
    assert!(matches!(
        err,
        GeoIpDownloadError::MissingCredential {
            provider: "MaxMind",
            field: "auto_download.maxmind_account_id",
        }
    ));

    let mut config = config_in(&dir, GeoIpProvider::MaxMind);
    config.auto_download.maxmind_account_id = Some("account".into());
    let err = plan(Kind::Asn, &config).unwrap_err();
    assert!(matches!(
        err,
        GeoIpDownloadError::MissingCredential {
            field: "auto_download.maxmind_license_key",
            ..
        }
    ));

    let err = plan(Kind::City, &config_in(&dir, GeoIpProvider::IpInfo)).unwrap_err();
    assert!(matches!(
        err,
        GeoIpDownloadError::MissingCredential {
            provider: "IpInfo",
            field: "auto_download.ipinfo_token",
        }
    ));
}

// ---------------------------------------------------------------------------
// Providers that publish only one kind
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_sapics_providers_publish_no_city_database() {
    for provider in [GeoIpProvider::SapicsIpToAsn, GeoIpProvider::SapicsOriginAsn] {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_in(dir.path(), provider);
        // Off, so the ASN half does not reach for the network.
        config.auto_download.enabled = false;

        let databases = ensure_databases(&config).await.unwrap();
        assert!(databases.city.is_none(), "{provider:?}");
    }
}

#[tokio::test]
async fn ipinfo_lite_publishes_no_asn_database() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_in(dir.path(), GeoIpProvider::IpInfo);
    config.auto_download.enabled = false;

    let databases = ensure_databases(&config).await.unwrap();
    assert!(databases.asn.is_none());
}

// ---------------------------------------------------------------------------
// Transfers, served over loopback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn maxmind_basic_auth_reaches_the_request() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::MaxMind);

    let archive = tar_gz("GeoLite2-City.mmdb", &mmdb_body(b"city database"));
    let digest = sha256_hex(&archive);

    // The digest is of the ARCHIVE, which is what the part file holds when the
    // check runs.
    Mock::given(method("GET"))
        .and(query_param("suffix", "tar.gz.sha256"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(format!("{digest}  GeoLite2-City_20260901.tar.gz\n")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("suffix", "tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(archive))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let dest = transfer.run(&default_client()).await.unwrap();

    assert_eq!(dest, dir.path().join("GeoLite2-City.mmdb"));
    assert_eq!(fs::read(&dest).unwrap(), mmdb_body(b"city database"));

    // The archive and the digest, and MaxMind gates both on the same account.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        // base64 of "123456:secret-key" -- both halves of the credential are on
        // the wire, and neither is anywhere in the URL.
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Basic MTIzNDU2OnNlY3JldC1rZXk="
        );
        let url = request.url.as_str();
        assert!(!url.contains("123456"), "{url}");
        assert!(!url.contains("secret-key"), "{url}");
    }
}

#[cfg(feature = "geoip-lookup")]
#[test]
fn a_database_carries_when_it_was_built_not_when_it_was_fetched() {
    // The age metric reads the publisher's stamp because the two diverge by
    // however long the copy sat published before anyone fetched it.
    // The fixture is stamped with a fixed build epoch months behind whenever the
    // file itself was written, so the two cannot collapse onto one another.
    let path = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/city-test.mmdb"
    ));

    let built = crate::geoip::enricher::open_reader(path)
        .unwrap()
        .metadata()
        .build_time()
        .unwrap();
    let written = fs::metadata(path).unwrap().modified().unwrap();

    let gap = written.duration_since(built).unwrap();
    assert!(
        gap > Duration::from_secs(30 * SECS_PER_DAY),
        "built and fetched are {gap:?} apart"
    );
}

#[test]
fn a_refused_download_is_counted_apart_from_one_that_never_arrived() {
    // Bytes that arrived and were rejected mean the provider published
    // something bad; bytes that never arrived mean the network or the
    // credential. Alerting on the two together hides both.
    let refused = [
        GeoIpDownloadError::NotADatabase {
            url: "https://example.invalid/db".into(),
        },
        GeoIpDownloadError::Undersized {
            path: "/tmp/db.mmdb".into(),
            actual: 1,
            existing: 1_000_000,
            floor_percent: 50,
        },
        GeoIpDownloadError::Truncated {
            url: "https://example.invalid/db".into(),
            expected: 100,
            actual: 10,
        },
    ];
    for error in &refused {
        assert_eq!(error.outcome(), "refused", "{error:?}");
    }

    assert_eq!(
        GeoIpDownloadError::Busy {
            path: "/tmp/db.mmdb".into()
        }
        .outcome(),
        "busy"
    );
    assert_eq!(
        GeoIpDownloadError::UnexpectedStatus {
            url: "https://example.invalid/db".into(),
            status: 500,
        }
        .outcome(),
        "failed"
    );
}

#[test]
fn a_408_is_the_one_4xx_worth_coming_back_from() {
    // A request timeout is the server inviting a retry; the rest of the 4xx
    // range is its answer about this request and will not change on its own.
    let at = |status| GeoIpDownloadError::UnexpectedStatus {
        url: "https://example.invalid/db".into(),
        status,
    };

    assert!(!at(408).is_permanent(), "408 is worth another attempt");
    for status in [400, 401, 403, 404, 407, 409, 410, 451] {
        assert!(at(status).is_permanent(), "{status}");
    }
    for status in [500, 502, 503, 504] {
        assert!(!at(status).is_permanent(), "{status}");
    }
}

#[test]
fn a_body_that_expanded_past_the_ceiling_is_worth_fetching_again() {
    // Nothing about the configuration produced it, so the next build may well
    // be the size it should be.
    let err = GeoIpDownloadError::TooLarge {
        url: "https://example.invalid/db".into(),
        limit: 4 * 1024 * 1024 * 1024,
    };

    assert!(!err.is_permanent(), "{err:?}");
    assert!(err.to_string().contains("4294967296"), "{err}");
}

#[tokio::test]
async fn a_second_writer_is_turned_away_rather_than_interleaved() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::SapicsOriginAsn);

    // Stand in for the other process by holding the lock it would hold. A
    // separate open file description, which is what flock excludes on.
    let lock_path = dir.path().join("origin-asn.mmdb.lock");
    let held = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    held.lock().unwrap();

    let transfer = planned(Kind::Asn, &config, &server).remove(0);
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Busy { .. }), "{err:?}");
    // Turned away before the request, so the other process keeps its quota.
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_transfer_in_flight_turns_a_second_transfer_of_the_same_database_away() {
    // The same exclusion, driven by two real transfers rather than by a lock
    // the test holds: the first is held mid-body while the second runs.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let body = mmdb_body(b"the database the first writer is fetching");
    let (url, arrived, release) = held_server(body.clone());

    let first = tokio::spawn({
        let dest = dest.clone();
        async move { raw_transfer(url, dest).run(&default_client()).await }
    });

    // The request is on the wire, so the first transfer is holding the lock.
    arrived.await.unwrap();

    // Port 1 on loopback refuses immediately, so a second transfer that got
    // past the lock would fail as a transport error rather than as Busy.
    let err = raw_transfer("http://127.0.0.1:1/db.mmdb".to_string(), dest.clone())
        .run(&default_client())
        .await
        .unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Busy { .. }), "{err:?}");
    assert_eq!(err.outcome(), "busy");

    release.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), dest);
    assert_eq!(fs::read(&dest).unwrap(), body);
}

#[tokio::test]
async fn a_destination_that_cannot_be_renamed_over_leaves_no_staged_file() {
    // A directory where the database belongs fails the final rename, and the
    // staged file has to go with it rather than sit there as a second copy.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    fs::create_dir(&dest).unwrap();

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(mmdb_body(b"a real database")))
        .mount(&server)
        .await;

    let err = raw_transfer(format!("{}/db.mmdb", server.uri()), dest.clone())
        .run(&default_client())
        .await
        .unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Io(_)), "{err:?}");
    assert!(dest.is_dir(), "the destination was left as it was found");
    assert_eq!(entries(dir.path()), ["db.mmdb"]);
}

#[tokio::test]
async fn a_data_directory_that_cannot_be_created_costs_no_request() {
    // A file where the data directory belongs is the config fault that reaches
    // here, and it is reported before the lock is taken or a socket is opened.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("geoip");
    fs::write(&blocker, b"not a directory").unwrap();

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(mmdb_body(b"a real database")))
        .mount(&server)
        .await;

    let err = raw_transfer(format!("{}/db.mmdb", server.uri()), blocker.join("db.mmdb"))
        .run(&default_client())
        .await
        .unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Io(_)), "{err:?}");
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(entries(dir.path()), ["geoip"]);
}

#[tokio::test]
async fn the_ipinfo_token_rides_as_a_query_parameter() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);

    // The URL the module logs is the planned one, which the token never enters.
    let planned_url = plan(Kind::City, &config).unwrap().remove(0).url;
    assert!(!planned_url.contains("token-wxyz"), "{planned_url}");

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(mmdb_body(b"ipinfo lite")))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let dest = transfer.run(&default_client()).await.unwrap();
    assert_eq!(fs::read(&dest).unwrap(), mmdb_body(b"ipinfo lite"));

    let requests = server.received_requests().await.unwrap();
    let url = &requests[0].url;
    assert_eq!(url.path(), "/data/ipinfo_lite.mmdb");
    assert_eq!(url.query(), Some("token=token-wxyz"));
}

#[tokio::test]
async fn a_403_is_reported_rather_than_written_out() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);

    // A rejecting provider answers with a page, which must never reach disk as
    // though it were a database.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("<html>forbidden</html>"))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let endpoint = transfer.url.clone();
    let dest = transfer.dest.clone();
    let err = transfer.run(&default_client()).await.unwrap_err();

    // 403 means the credential was accepted and this database refused, so the
    // operator is pointed at the entitlement rather than at their key.
    assert!(
        matches!(&err, GeoIpDownloadError::NotEntitled { url } if *url == endpoint),
        "{err:?}"
    );
    assert!(!dest.exists());
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

#[tokio::test]
async fn an_anonymous_403_is_reported_as_a_status() {
    // No credential was sent, so there is no config field to name.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("<html>forbidden</html>"))
        .mount(&server)
        .await;

    let url = format!("{}/db.mmdb", server.uri());
    let err = raw_transfer(url.clone(), dest.clone())
        .run(&default_client())
        .await
        .unwrap_err();

    assert!(
        matches!(&err, GeoIpDownloadError::UnexpectedStatus { url: at, status: 403 } if *at == url),
        "{err:?}"
    );
    assert!(err.is_permanent(), "{err:?}");
    assert!(!dest.exists());
}

#[tokio::test]
async fn a_failed_transfer_leaves_no_part_file() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::DbIp);

    // The body arrives in full and only then fails to decompress, so the
    // failure lands after the transfer file has been written.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not a gzip stream".to_vec()))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let dest = transfer.dest.clone();
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Io(_)), "{err:?}");
    assert!(!dest.exists());
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

#[tokio::test]
async fn an_unpublished_current_month_falls_back_to_the_previous_one() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::DbIp);

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let current = path_of(&transfer.url);
    let previous = path_of(transfer.fallback_url.as_deref().unwrap());

    Mock::given(method("GET"))
        .and(path(current.clone()))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(previous.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(gzip(&mmdb_body(b"previous month city"))),
        )
        .mount(&server)
        .await;

    let dest = transfer.run(&default_client()).await.unwrap();

    assert_eq!(fs::read(&dest).unwrap(), mmdb_body(b"previous month city"));
    assert_eq!(requested_paths(&server).await, [current, previous]);
}

#[tokio::test]
async fn a_published_current_month_never_asks_for_the_previous_one() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::DbIp);

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let current = path_of(&transfer.url);

    Mock::given(method("GET"))
        .and(path(current.clone()))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(gzip(&mmdb_body(b"this month city"))),
        )
        .mount(&server)
        .await;

    let dest = transfer.run(&default_client()).await.unwrap();

    assert_eq!(fs::read(&dest).unwrap(), mmdb_body(b"this month city"));
    assert_eq!(requested_paths(&server).await, [current]);
}

#[tokio::test]
async fn a_403_does_not_fall_back_to_the_previous_month() {
    // Only a missing file is worth asking twice for; a rejection is the
    // provider's answer and has to surface.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::DbIp);

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let current = path_of(&transfer.url);
    let previous = path_of(transfer.fallback_url.as_deref().unwrap());

    Mock::given(method("GET"))
        .and(path(current.clone()))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(previous))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(gzip(&mmdb_body(b"previous month city"))),
        )
        .mount(&server)
        .await;

    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(
        matches!(
            err,
            GeoIpDownloadError::UnexpectedStatus { status: 403, .. }
        ),
        "{err:?}"
    );
    assert_eq!(requested_paths(&server).await, [current]);
}

#[tokio::test]
async fn a_published_digest_is_checked_against_the_download() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::SapicsOriginAsn);

    let body = mmdb_body(b"origin asn database");
    // sha256sum output, which is what the provider publishes beside the file.
    let digest = sha256_hex(&body);

    Mock::given(method("GET"))
        .and(path(
            "/sapics/ip-location-db/releases/download/latest/origin-asn.mmdb",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/sapics/ip-location-db/releases/download/checksum/origin-asn.mmdb.sha256",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{digest}  origin-asn.mmdb\n")),
        )
        .mount(&server)
        .await;

    let transfer = planned(Kind::Asn, &config, &server).remove(0);
    let written = transfer.run(&default_client()).await.unwrap();

    assert_eq!(written, dir.path().join("origin-asn.mmdb"));
    assert_eq!(fs::read(&written).unwrap(), body);
}

#[tokio::test]
async fn a_download_that_misses_its_digest_is_rejected() {
    // A truncated or substituted body passes every status check, so the digest
    // the provider publishes is what decides.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::SapicsOriginAsn);

    let expected = sha256_hex(&mmdb_body(b"what the provider published"));

    Mock::given(method("GET"))
        .and(path(
            "/sapics/ip-location-db/releases/download/latest/origin-asn.mmdb",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(mmdb_body(b"something else")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/sapics/ip-location-db/releases/download/checksum/origin-asn.mmdb.sha256",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{expected}  origin-asn.mmdb\n")),
        )
        .mount(&server)
        .await;

    let transfer = planned(Kind::Asn, &config, &server).remove(0);
    let dest = transfer.dest.clone();
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(
        matches!(err, GeoIpDownloadError::ChecksumMismatch { .. }),
        "{err:?}"
    );
    assert!(!dest.exists());
    // Bytes known to be wrong are not kept for a resume.
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

#[tokio::test]
async fn a_rejected_download_leaves_the_previous_database_in_place() {
    // The service keeps serving the last known-good copy. A bad body that
    // reached the destination would also look fresh, so nothing would ever
    // re-download it.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);
    let good = mmdb_body(b"yesterday's database");
    let dest = dir.path().join("ipinfo-lite.mmdb");

    for body in [
        // A login page answered with 200.
        b"<!DOCTYPE html><title>Log in to your account</title>".to_vec(),
        // A body that is not a database, truncated before its metadata.
        mmdb_body(b"a real database")[..12].to_vec(),
    ] {
        fs::write(&dest, &good).unwrap();
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
            .mount(&server)
            .await;

        let transfer = planned(Kind::City, &config, &server).remove(0);
        let err = transfer.run(&default_client()).await.unwrap_err();

        assert!(
            matches!(err, GeoIpDownloadError::NotADatabase { .. }),
            "{err:?}"
        );
        assert_eq!(fs::read(&dest).unwrap(), good, "{body:?}");
        assert_eq!(entries(dir.path()), ["ipinfo-lite.mmdb"], "{body:?}");
    }
}

#[tokio::test]
async fn a_checksum_mismatch_leaves_the_previous_database_in_place() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = config_in(dir.path(), GeoIpProvider::SapicsOriginAsn);

    let good = mmdb_body(b"yesterday's asn database");
    let dest = dir.path().join("origin-asn.mmdb");
    fs::write(&dest, &good).unwrap();

    let expected = sha256_hex(&mmdb_body(b"what the provider published"));
    Mock::given(method("GET"))
        .and(path(
            "/sapics/ip-location-db/releases/download/latest/origin-asn.mmdb",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(mmdb_body(b"something else")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(
            "/sapics/ip-location-db/releases/download/checksum/origin-asn.mmdb.sha256",
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(format!("{expected}  origin-asn.mmdb\n")),
        )
        .mount(&server)
        .await;

    let transfer = planned(Kind::Asn, &config, &server).remove(0);
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(
        matches!(err, GeoIpDownloadError::ChecksumMismatch { .. }),
        "{err:?}"
    );
    assert_eq!(fs::read(&dest).unwrap(), good);
    assert_eq!(entries(dir.path()), ["origin-asn.mmdb"]);
}

#[tokio::test]
async fn a_digest_that_is_not_a_sha256_leaves_the_previous_database_in_place() {
    // A release asset redirects to a CDN that answers 200 with a page, and a
    // prefixed or empty digest is the same fault: nothing to check against.
    for body in [
        "<!DOCTYPE html><html><title>Not Found</title></html>".to_string(),
        format!("sha256:{}", sha256_hex(&mmdb_body(b"what was published"))),
        String::new(),
    ] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("db.mmdb");
        let good = mmdb_body(b"yesterday's database");
        fs::write(&dest, &good).unwrap();

        Mock::given(method("GET"))
            .and(path("/db.mmdb"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(mmdb_body(b"today's database")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/db.mmdb.sha256"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.clone()))
            .mount(&server)
            .await;

        let err = resumable_transfer(
            format!("{}/db.mmdb", server.uri()),
            dest.clone(),
            format!("{}/db.mmdb.sha256", server.uri()),
        )
        .run(&default_client())
        .await
        .unwrap_err();

        assert!(
            matches!(&err, GeoIpDownloadError::MalformedChecksum { url } if url.ends_with("/db.mmdb.sha256")),
            "{body:?}: {err:?}"
        );
        // Bytes arrived and were rejected, which is the provider's fault rather
        // than the network's.
        assert_eq!(err.outcome(), "refused", "{body:?}");
        // The requirement: the copy on disk is still there, byte for byte.
        assert_eq!(fs::read(&dest).unwrap(), good, "{body:?}");
    }
}

// ---------------------------------------------------------------------------
// What the format cannot state: content and volume
// ---------------------------------------------------------------------------

/// The guard an operator gets by default, for one database kind.
fn default_guard(kind: Kind) -> Guard {
    Guard::new(kind, &AutoDownloadConfig::default())
}

/// Serve `body` from a one-mock server and return the transfer that fetches it.
async fn served(server: &MockServer, dest: PathBuf, body: Vec<u8>) -> Transfer {
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
    raw_transfer(format!("{}/db.mmdb", server.uri()), dest)
}

// The content probe is a no-op without a reader compiled in, so the refusal
// this asserts only exists in a build that has one.
#[cfg(feature = "geoip-lookup")]
#[tokio::test]
async fn a_database_that_answers_nothing_leaves_the_previous_one_in_place() {
    // The dbip-asn defect: a valid MaxMind DB whose operator-name column is
    // blank on every row. It parses, it matches the address, and it answers
    // nothing, so no check on the bytes alone refuses it.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");

    let good = verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET");
    fs::write(&dest, &good).unwrap();

    let transfer = served(
        &server,
        dest.clone(),
        verify::fixtures::asn_mmdb(13_335, ""),
    )
    .await;
    let err = transfer
        .run_guarded(&default_client(), default_guard(Kind::Asn))
        .await
        .unwrap_err();

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
    // The requirement: the database already on disk is still there, byte for
    // byte, and is what the service keeps serving.
    assert_eq!(fs::read(&dest).unwrap(), good);
    assert_eq!(entries(dir.path()), ["db.mmdb"]);
    // Worth another attempt: the provider may publish a populated build next.
    assert!(!err.is_permanent(), "{err:?}");
}

#[tokio::test]
async fn a_populated_database_replaces_the_previous_one() {
    // The mirror of the refusal, and the test that matters most: a check that
    // refused everything would stop a deployment updating and say nothing.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");

    fs::write(&dest, verify::fixtures::asn_mmdb(13_335, "AN OLDER NAME")).unwrap();
    let published = verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET");

    let transfer = served(&server, dest.clone(), published.clone()).await;
    let written = transfer
        .run_guarded(&default_client(), default_guard(Kind::Asn))
        .await
        .unwrap();

    assert_eq!(written, dest);
    assert_eq!(fs::read(&dest).unwrap(), published);
}

#[cfg(feature = "geoip-lookup")]
#[tokio::test]
async fn a_city_database_that_resolves_no_country_leaves_the_previous_one() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");

    let good = verify::fixtures::city_mmdb("US");
    fs::write(&dest, &good).unwrap();

    let transfer = served(&server, dest.clone(), verify::fixtures::city_mmdb("")).await;
    let err = transfer
        .run_guarded(&default_client(), default_guard(Kind::City))
        .await
        .unwrap_err();

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
    assert_eq!(fs::read(&dest).unwrap(), good);
    assert_eq!(entries(dir.path()), ["db.mmdb"]);
}

#[tokio::test]
async fn a_stub_replacement_leaves_the_previous_one_in_place() {
    // A parseable database a fraction of the size of the copy it would replace:
    // every byte of it is valid, and it is not the database the deployment had.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");

    let mut good = verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET");
    good.resize(10_000, 0);
    fs::write(&dest, &good).unwrap();

    let stub = verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET");
    let transfer = served(&server, dest.clone(), stub).await;
    let err = transfer
        .run_guarded(&default_client(), default_guard(Kind::Asn))
        .await
        .unwrap_err();
    let message = err.to_string();

    assert!(
        matches!(
            err,
            GeoIpDownloadError::Undersized {
                existing: 10_000,
                floor_percent: 50,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(message.contains("10000"), "{message}");
    assert_eq!(fs::read(&dest).unwrap(), good);
    assert_eq!(entries(dir.path()), ["db.mmdb"]);
}

#[tokio::test]
async fn a_first_download_has_no_copy_to_be_measured_against() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let published = verify::fixtures::asn_mmdb(15_169, "GOOGLE");

    let transfer = served(&server, dest.clone(), published.clone()).await;
    let written = transfer
        .run_guarded(&default_client(), default_guard(Kind::Asn))
        .await
        .unwrap();

    assert_eq!(written, dest);
    assert_eq!(fs::read(&dest).unwrap(), published);
}

#[tokio::test]
async fn an_operator_can_turn_the_content_check_off() {
    // A deployment on a database this crate models badly has to be able to keep
    // updating, and turning this off gives up neither the format check nor the
    // digest.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    fs::write(&dest, verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET")).unwrap();

    let blank = verify::fixtures::asn_mmdb(13_335, "");
    let auto = AutoDownloadConfig {
        verify_content: false,
        ..Default::default()
    };

    let transfer = served(&server, dest.clone(), blank.clone()).await;
    let written = transfer
        .run_guarded(&default_client(), Guard::new(Kind::Asn, &auto))
        .await
        .unwrap();

    assert_eq!(written, dest);
    assert_eq!(fs::read(&dest).unwrap(), blank);
}

#[tokio::test]
async fn an_operator_can_turn_the_size_floor_off() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");

    let mut previous = verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET");
    previous.resize(10_000, 0);
    fs::write(&dest, &previous).unwrap();

    let small = verify::fixtures::asn_mmdb(13_335, "CLOUDFLARENET");
    let auto = AutoDownloadConfig {
        min_size_percent: 0,
        ..Default::default()
    };

    let transfer = served(&server, dest.clone(), small.clone()).await;
    let written = transfer
        .run_guarded(&default_client(), Guard::new(Kind::Asn, &auto))
        .await
        .unwrap();

    assert_eq!(written, dest);
    assert_eq!(fs::read(&dest).unwrap(), small);
}

#[tokio::test]
async fn a_login_page_answered_with_200_is_not_written_out() {
    // Three provider endpoints answer 200 with HTML rather than a database, so
    // the bytes are checked for what the format requires.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);

    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><title>Log in to your account</title></html>"),
        )
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let dest = transfer.dest.clone();
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(
        matches!(err, GeoIpDownloadError::NotADatabase { .. }),
        "{err:?}"
    );
    assert!(!dest.exists());
    assert!(entries(dir.path()).is_empty(), "{:?}", entries(dir.path()));
}

#[tokio::test]
async fn a_rejected_credential_names_the_config_fields() {
    // 401 is the provider's answer about the credential itself, so the report
    // is the field an operator has to fix rather than a bare status.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::MaxMind);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let err = transfer.run(&default_client()).await.unwrap_err();
    let message = err.to_string();

    assert!(
        matches!(err, GeoIpDownloadError::CredentialRejected { .. }),
        "{err:?}"
    );
    assert!(err.is_permanent(), "{err:?}");
    assert!(
        message.contains("auto_download.maxmind_account_id"),
        "{message}"
    );
    assert!(
        message.contains("auto_download.maxmind_license_key"),
        "{message}"
    );
    // The credential itself is never in the report.
    assert!(!message.contains("secret-key"), "{message}");
}

#[tokio::test]
async fn an_unentitled_edition_is_not_reported_as_a_bad_credential() {
    // MaxMind answers 401 for a licence key it will not accept and 403 for an
    // account that has no claim on the edition asked for. Reporting both as a
    // credential fault sends an operator to check a key that is already right.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::MaxMind);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let err = transfer.run(&default_client()).await.unwrap_err();
    let message = err.to_string();

    assert!(
        matches!(err, GeoIpDownloadError::NotEntitled { .. }),
        "{err:?}"
    );
    // Retrying spends the provider's quota and cannot change the answer.
    assert!(err.is_permanent(), "{err:?}");
    assert_eq!(err.outcome(), "unentitled");
    assert!(message.contains("entitlement"), "{message}");
    assert!(!message.contains("maxmind_license_key"), "{message}");
    assert!(!message.contains("secret-key"), "{message}");
}

#[tokio::test]
async fn a_rate_limit_carries_the_delay_the_provider_asked_for() {
    // A provider bans a client that ignores this, so the delay travels with the
    // error and no retry happens inside the transfer.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(
        matches!(
            err,
            GeoIpDownloadError::RateLimited {
                retry_after_secs: Some(120),
                ..
            }
        ),
        "{err:?}"
    );
    // Worth coming back to, unlike a rejected credential.
    assert!(!err.is_permanent(), "{err:?}");
    assert_eq!(requested_paths(&server).await.len(), 1, "no retry in place");
}

#[tokio::test]
async fn a_transient_failure_is_not_reported_as_permanent() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let transfer = planned(Kind::City, &config, &server).remove(0);
    let err = transfer.run(&default_client()).await.unwrap_err();

    assert!(
        matches!(
            err,
            GeoIpDownloadError::UnexpectedStatus { status: 503, .. }
        ),
        "{err:?}"
    );
    assert!(!err.is_permanent(), "{err:?}");
}

#[tokio::test]
async fn a_body_shorter_than_its_content_length_is_rejected() {
    // A transfer that ends early otherwise looks like a complete small file.
    // The HTTP layer refuses an incomplete body of its own accord, and the
    // length check behind it is the guard for a server that closes cleanly.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");
    let good = mmdb_body(b"yesterday's database");
    fs::write(&dest, &good).unwrap();

    // The server promises the whole body and hangs up one chunk short.
    let body = mmdb_body(b"a database that stops early");
    let mut chunks: Vec<Vec<u8>> = body.chunks(16).map(<[u8]>::to_vec).collect();
    let missing = chunks.pop().unwrap();
    let delivered = body.len() - missing.len();
    let url = drip_server_promising(chunks, body.len(), Duration::from_millis(10));

    let err = raw_transfer(url, dest.clone())
        .run(&default_client())
        .await
        .unwrap_err();

    assert!(
        matches!(
            err,
            GeoIpDownloadError::Http(_) | GeoIpDownloadError::Truncated { .. }
        ),
        "{err:?}"
    );
    assert!(!err.is_permanent(), "{err:?}");
    // The database already on disk is untouched, and what did arrive is a
    // prefix the next run resumes from.
    assert_eq!(fs::read(&dest).unwrap(), good);
    assert_eq!(fs::read(&part).unwrap().len(), delivered);
}

#[tokio::test]
async fn a_slow_but_progressing_transfer_completes() {
    // The body arrives steadily and takes longer than the idle timeout, which a
    // whole-request timeout of the same size would have killed.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let body = mmdb_body(b"a body that arrives in pieces");
    let chunks: Vec<Vec<u8>> = body.chunks(8).map(<[u8]>::to_vec).collect();
    let url = drip_server(chunks, Duration::from_millis(200));

    let client = fetch::client(None, Duration::from_secs(5), Duration::from_secs(1)).unwrap();
    let started = Instant::now();
    let written = raw_transfer(url, dest).run(&client).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(fs::read(&written).unwrap(), body);
    assert!(elapsed > Duration::from_secs(1), "{elapsed:?}");
}

#[tokio::test]
async fn a_stalled_transfer_fails_and_keeps_what_arrived() {
    // Bytes land, delivery stops, and the idle timeout ends it. What arrived
    // stays on disk as the prefix the next run resumes from.
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");
    let url = drip_server(
        vec![b"first half".to_vec(), b"never arrives".to_vec()],
        Duration::from_secs(3),
    );

    let client = fetch::client(None, Duration::from_secs(5), Duration::from_millis(300)).unwrap();
    let err = raw_transfer(url, dest.clone())
        .run(&client)
        .await
        .unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Http(_)), "{err:?}");
    assert!(!dest.exists(), "the destination is only written on success");
    assert_eq!(fs::read(&part).unwrap(), b"first half");
}

#[tokio::test]
async fn an_interrupted_transfer_resumes_from_the_part_file() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");

    let body = mmdb_body(b"the whole database body");
    let (prefix, remainder) = body.split_at(8);
    fs::write(&part, prefix).unwrap();

    // A server honouring the range sends what is missing, and says so with 206.
    Mock::given(method("GET"))
        .and(path("/db.mmdb"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "content-range",
                    format!("bytes 8-{}/{}", body.len() - 1, body.len()).as_str(),
                )
                .set_body_bytes(remainder.to_vec()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/db.mmdb.sha256"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sha256_hex(&body)))
        .mount(&server)
        .await;

    let written = resumable_transfer(
        format!("{}/db.mmdb", server.uri()),
        dest,
        format!("{}/db.mmdb.sha256", server.uri()),
    )
    .run(&default_client())
    .await
    .unwrap();

    assert_eq!(fs::read(&written).unwrap(), body);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers.get("range").unwrap(), "bytes=8-");
}

#[tokio::test]
async fn a_server_that_ignores_the_range_restarts_the_file() {
    // A 200 is the whole body, so appending it to the prefix would build a file
    // that is longer than the database and corrupt from byte one.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");

    let body = mmdb_body(b"the whole database body");
    fs::write(&part, &body[..8]).unwrap();

    Mock::given(method("GET"))
        .and(path("/db.mmdb"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/db.mmdb.sha256"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sha256_hex(&body)))
        .mount(&server)
        .await;

    let written = resumable_transfer(
        format!("{}/db.mmdb", server.uri()),
        dest,
        format!("{}/db.mmdb.sha256", server.uri()),
    )
    .run(&default_client())
    .await
    .unwrap();

    assert_eq!(fs::read(&written).unwrap(), body);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].headers.get("range").unwrap(), "bytes=8-");
}

#[tokio::test]
async fn a_raw_body_with_no_digest_is_never_continued() {
    // Nothing binds the range to the bytes it resumed from, so a prefix from one
    // build can be spliced onto the tail of the next. With no digest and no
    // container to fail on the seam, the splice would reach the destination.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");

    let body = mmdb_body(b"the whole database body");
    fs::write(&part, b"a prefix of yesterday's build").unwrap();

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let written = raw_transfer(format!("{}/db.mmdb", server.uri()), dest)
        .run(&default_client())
        .await
        .unwrap();

    assert_eq!(fs::read(&written).unwrap(), body);
    let requests = server.received_requests().await.unwrap();
    assert!(
        requests[0].headers.get("range").is_none(),
        "the request must not ask for a range it cannot check"
    );
}

#[tokio::test]
async fn a_gzip_resume_spliced_from_another_build_fails_the_decode() {
    // A compressed transfer is allowed to continue with no digest behind it,
    // because the container's own integrity check is what catches a prefix of
    // one build spliced onto the tail of the next.
    const AT: usize = 24;

    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");

    let good = mmdb_body(b"yesterday's database");
    fs::write(&dest, &good).unwrap();

    let yesterday = gzip(&mmdb_body(&b"yesterday's build ".repeat(64)));
    let today = gzip(&mmdb_body(&b"today's build ".repeat(64)));
    // Past the ten-byte gzip header, so the splice lands inside the deflate
    // stream rather than on two identical headers.
    fs::write(&part, &yesterday[..AT]).unwrap();

    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header(
                    "content-range",
                    format!("bytes {AT}-{}/{}", today.len() - 1, today.len()).as_str(),
                )
                .set_body_bytes(today[AT..].to_vec()),
        )
        .mount(&server)
        .await;

    let err = gzip_transfer(format!("{}/db.mmdb", server.uri()), dest.clone())
        .run(&default_client())
        .await
        .unwrap_err();

    assert!(matches!(err, GeoIpDownloadError::Io(_)), "{err:?}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers.get("range").unwrap(),
        format!("bytes={AT}-").as_str()
    );
    // The destination is untouched, and the spliced prefix is gone rather than
    // left to poison the next attempt.
    assert_eq!(fs::read(&dest).unwrap(), good);
    assert_eq!(entries(dir.path()), ["db.mmdb"]);
}

#[tokio::test]
async fn a_part_file_past_the_resume_window_is_not_continued() {
    // A provider that dates its URLs publishes a different file each month, so
    // an old prefix belongs to a body this request is not fetching.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("db.mmdb");
    let part = dir.path().join("db.mmdb.part");

    let body = mmdb_body(b"this month's database body");
    fs::write(&part, b"last month's prefix").unwrap();
    fs::File::options()
        .write(true)
        .open(&part)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60))
        .unwrap();

    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;

    let written = raw_transfer(format!("{}/db.mmdb", server.uri()), dest)
        .run(&default_client())
        .await
        .unwrap();

    assert_eq!(fs::read(&written).unwrap(), body);
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("range").is_none());
}

#[tokio::test]
async fn a_transport_error_never_carries_the_token() {
    // Port 1 on loopback refuses the connection immediately, so the error path
    // is exercised without leaving the machine.
    let dir = tempfile::tempdir().unwrap();
    let config = credentialled(dir.path(), GeoIpProvider::IpInfo);

    let mut transfer = plan(Kind::City, &config).unwrap().remove(0);
    transfer.url = "http://127.0.0.1:1/data/ipinfo_lite.mmdb".to_string();

    let err = transfer.run(&default_client()).await.unwrap_err();
    let rendered = format!("{err} {err:?}");

    assert!(matches!(err, GeoIpDownloadError::Http(_)), "{err:?}");
    assert!(!rendered.contains("token-wxyz"), "{rendered}");
}

#[tokio::test]
async fn a_fresh_file_is_reused_without_a_transfer() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let city = dir.path().join("dbip-city-lite.mmdb");
    let asn = dir.path().join("dbip-asn-lite.mmdb");
    fs::write(&city, b"fake city").unwrap();
    fs::write(&asn, b"fake asn").unwrap();

    // Every request this client could make is pinned to the mock server, so any
    // attempt to download would be recorded there.
    let proxied = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(server.uri()).unwrap())
        .build()
        .unwrap();
    let config = config_in(dir.path(), GeoIpProvider::DbIp).with_http_client(proxied);

    let databases = ensure_databases(&config).await.unwrap();

    assert_eq!(requested_targets(&server).await, Vec::<String>::new());
    assert_eq!(fs::read(&city).unwrap(), b"fake city");
    assert_eq!(databases.city.unwrap().files, [city]);
    assert_eq!(databases.asn.unwrap().files, [asn]);
}

#[tokio::test]
async fn a_stale_file_is_re_fetched() {
    // The mirror of the freshness test: the same config with max_age_days = 0
    // does reach for the network, which is what makes the skip above a result
    // rather than an accident.
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let city = dir.path().join("dbip-city-lite.mmdb");
    fs::write(&city, b"stale city").unwrap();

    let proxied = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(server.uri()).unwrap())
        .build()
        .unwrap();
    let mut config = config_in(dir.path(), GeoIpProvider::DbIp).with_http_client(proxied);
    config.auto_download.max_age_days = 0;

    let databases = ensure_databases(&config).await.unwrap();

    // The transfer cannot succeed through a mock proxy, so the stale file is
    // what comes back -- the attempt itself is the point. Both kinds try, and
    // both are aimed at the provider the config names.
    assert_eq!(databases.city.unwrap().files, [city]);
    assert_eq!(
        requested_targets(&server).await,
        [
            "CONNECT download.db-ip.com:443",
            "CONNECT download.db-ip.com:443"
        ]
    );
}

// ---------------------------------------------------------------------------
// Result types + errors
// ---------------------------------------------------------------------------

#[test]
fn databases_default_is_empty() {
    let databases = Databases::default();
    assert!(databases.city.is_none());
    assert!(databases.asn.is_none());
}

#[test]
fn databases_hold_either_half() {
    let databases = Databases {
        city: Some(Database {
            format: DatabaseFormat::Mmdb,
            files: vec![PathBuf::from("/tmp/city.mmdb")],
        }),
        asn: None,
    };
    assert_eq!(
        databases.city.unwrap().files,
        [PathBuf::from("/tmp/city.mmdb")]
    );
    assert!(databases.asn.is_none());
}

#[test]
fn error_messages_name_the_problem() {
    let err = GeoIpDownloadError::MissingCredential {
        provider: "MaxMind",
        field: "auto_download.maxmind_account_id",
    };
    let msg = err.to_string();
    assert!(msg.contains("MaxMind"), "{msg}");
    assert!(msg.contains("auto_download.maxmind_account_id"), "{msg}");

    let err = GeoIpDownloadError::NoDatabases {
        provider: "IpInfo".to_string(),
        kind: "asn",
    };
    assert!(err.to_string().contains("IpInfo"));

    let err = GeoIpDownloadError::ArchiveMemberMissing {
        member: "GeoLite2-City.mmdb",
    };
    assert!(err.to_string().contains("GeoLite2-City.mmdb"));

    let err = GeoIpDownloadError::UnexpectedStatus {
        url: "https://example.invalid/db".to_string(),
        status: 403,
    };
    assert!(err.to_string().contains("403"));
}

// ---------------------------------------------------------------------------
// What the table states beyond the URL
// ---------------------------------------------------------------------------

#[test]
fn every_built_in_selection_is_expressible_as_a_row() {
    // The table is the whole provider model, so a selection it cannot state is
    // a gap in the table rather than a case handled somewhere else.
    for (choice, kind) in [
        (free(GeoIpProvider::DbIp), Kind::City),
        (free(GeoIpProvider::DbIp), Kind::Asn),
        (free(GeoIpProvider::MaxMind), Kind::City),
        (free(GeoIpProvider::MaxMind), Kind::Asn),
        (paid(GeoIpProvider::MaxMind), Kind::City),
        (paid(GeoIpProvider::MaxMind), Kind::Asn),
        (free(GeoIpProvider::IpInfo), Kind::City),
        (free(GeoIpProvider::SapicsOriginAsn), Kind::Asn),
        (free(GeoIpProvider::SapicsIpToAsn), Kind::Asn),
    ] {
        let source = SourceSpec::select(choice, kind)
            .unwrap_or_else(|e| panic!("{choice:?} {kind:?}: {e}"))
            .unwrap_or_else(|| panic!("{choice:?} {kind:?} has no row"));

        assert_eq!(source.database().format, DatabaseFormat::Mmdb);
        assert_eq!(source.database().names.len(), 1, "{choice:?} {kind:?}");
    }

    // The one provider that fetches nothing states that by having no row, at
    // either tier: its files are the operator's own.
    for tier in [ProviderTier::Free, ProviderTier::Paid] {
        let choice = ProviderChoice {
            provider: GeoIpProvider::Custom,
            tier,
        };
        for kind in [Kind::City, Kind::Asn] {
            assert!(
                SourceSpec::select(choice, kind).unwrap().is_none(),
                "{choice:?} {kind:?}"
            );
        }
    }
}

#[test]
fn a_selection_reports_where_its_terms_are_published() {
    // Readable at runtime, so a deployment can find the terms that apply to
    // what it fetched rather than someone remembering to go looking.
    let dbip = source_terms(ProviderSelection::from(GeoIpProvider::DbIp));
    assert_eq!(dbip.len(), 2);
    for terms in &dbip {
        assert!(terms.terms_url.starts_with("https://"), "{terms:?}");
    }

    let sapics = source_terms(ProviderSelection::from(GeoIpProvider::SapicsOriginAsn));
    assert_eq!(sapics.len(), 1);
    assert_eq!(sapics[0].kind, "asn");
    assert!(sapics[0].terms_url.starts_with("https://"));

    // A provider that publishes one kind reports one entry.
    let ipinfo = source_terms(ProviderSelection::from(GeoIpProvider::IpInfo));
    assert_eq!(ipinfo.len(), 1);
    assert_eq!(ipinfo[0].kind, "city");
    assert!(ipinfo[0].min_interval.is_some());

    // Nothing is fetched for an operator-supplied file, so there is nothing to
    // point at.
    assert!(source_terms(ProviderSelection::from(GeoIpProvider::Custom)).is_empty());
}

#[test]
fn the_freshness_default_is_the_providers_own_cadence() {
    let month = Duration::from_secs(30 * 86_400);
    let defaults = AutoDownloadConfig::default();
    let geolite = SourceSpec::select(free(GeoIpProvider::MaxMind), Kind::City)
        .unwrap()
        .unwrap();
    let dbip = SourceSpec::select(free(GeoIpProvider::DbIp), Kind::City)
        .unwrap()
        .unwrap();

    // One global thirty days sits exactly on the GeoLite2 update duty, so a
    // twice-weekly source is re-fetched with days of margin rather than none.
    assert_eq!(defaults.max_age_days, 30);
    assert!(geolite.staleness_window(&defaults) < month);

    // A monthly source keeps the monthly window: the cadence is the default,
    // not a blanket reduction.
    assert_eq!(dbip.staleness_window(&defaults), month);

    // The operator's ceiling still tightens it.
    let tightened = AutoDownloadConfig {
        max_age_days: 2,
        ..Default::default()
    };
    assert_eq!(
        dbip.staleness_window(&tightened),
        Duration::from_secs(2 * 86_400)
    );
}

#[test]
fn a_fetch_ceiling_cannot_be_configured_away() {
    // A provider that caps how often it may be fetched has that cap enforced
    // by the freshness window, rather than by someone remembering it.
    let aggressive = AutoDownloadConfig {
        max_age_days: 0,
        ..Default::default()
    };

    let ipinfo = SourceSpec::select(free(GeoIpProvider::IpInfo), Kind::City)
        .unwrap()
        .unwrap();
    assert_eq!(
        ipinfo.staleness_window(&aggressive),
        Duration::from_secs(86_400)
    );

    // A source whose terms state no ceiling honours the setting as given.
    let dbip = SourceSpec::select(free(GeoIpProvider::DbIp), Kind::City)
        .unwrap()
        .unwrap();
    assert_eq!(dbip.staleness_window(&aggressive), Duration::ZERO);
}

#[test]
fn a_plan_error_never_carries_a_credential() {
    // Error values are logged verbatim by the non-fatal path, so they must not
    // become the leak the Secret wrapper is there to prevent.
    let mut config = config_in(Path::new("/var/lib/geoip"), GeoIpProvider::MaxMind);
    config.auto_download.maxmind_account_id = Some("account-1234".into());

    let err = plan(Kind::City, &config).unwrap_err();
    let rendered = format!("{err} {err:?}");
    assert!(!rendered.contains("account-1234"), "{rendered}");
}
