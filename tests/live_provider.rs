// Project:   factbook
// File:      tests/live_provider.rs
// Purpose:   Provision from the real provider endpoints, credentials permitting
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Every modelled provider against its live endpoint rather than a mock.
//!
//! Off unless asked for: these spend a daily quota and move tens of megabytes.
//!
//! ```sh
//! FACTBOOK_LIVE=1 MAXMIND_ACCOUNT_ID=... MAXMIND_LICENSE_KEY=... \
//!     IP_INFO_API_TOKEN=... cargo test --test live_provider -- --nocapture
//! ```

#![cfg(all(feature = "geoip-download", feature = "geoip-lookup"))]

use std::net::IpAddr;
use std::sync::Arc;

use factbook::geoip::{
    AutoDownloadConfig, CacheConfig, Databases, GeoIp, GeoIpConfig, GeoIpProvider, GeoIpRecord,
    ProviderSelection, ensure_databases, source_terms,
};

/// Google Public DNS, in Google's own allocation, held by every source.
const PROBE: &str = "8.8.8.8";

/// The autonomous system that allocation belongs to.
const PROBE_ASN: u32 = 15169;

/// Whether the caller asked for live runs at all.
fn live() -> bool {
    std::env::var("FACTBOOK_LIVE").ok().as_deref() == Some("1")
}

/// A named credential, when it was supplied and is not blank.
fn credential(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    (!value.is_empty()).then_some(value)
}

/// Fetch one selection into a temporary directory.
async fn provision(
    selection: ProviderSelection,
    auto: AutoDownloadConfig,
) -> (tempfile::TempDir, Databases) {
    let data_dir = tempfile::tempdir().expect("temp dir");

    let config = GeoIpConfig {
        provider: selection,
        auto_download: AutoDownloadConfig {
            data_dir: data_dir.path().to_path_buf(),
            ..auto
        },
        ..GeoIpConfig::default()
    };

    for terms in source_terms(selection) {
        eprintln!(
            "{} ({}) -- terms: {}",
            terms.name, terms.kind, terms.terms_url
        );
    }

    let databases = ensure_databases(&config).await.expect("provision");
    (data_dir, databases)
}

/// Refuse a file too small to be a database.
fn assert_real_databases(databases: &Databases) {
    let files = databases
        .city
        .iter()
        .chain(databases.asn.iter())
        .flat_map(|database| &database.files);

    let mut seen = 0;
    for file in files {
        let size = std::fs::metadata(file).expect("a downloaded file").len();
        eprintln!("  {} -- {size} bytes", file.display());
        assert!(size > 1_000_000, "{} is {size} bytes", file.display());
        seen += 1;
    }
    assert!(seen > 0, "nothing was downloaded");
}

/// Read the probe address and report what the source said about it.
fn record_for(databases: &Databases) -> Arc<GeoIpRecord> {
    let geoip = GeoIp::from_databases(databases, CacheConfig::default()).expect("readers");
    let probe: IpAddr = PROBE.parse().expect("a valid address");
    let record = geoip.lookup(probe).expect("a record for the probe address");
    eprintln!("  {PROBE} -> {record:?}");
    record
}

/// The operator fields an ASN-carrying source must fill.
fn assert_names_the_operator(record: &GeoIpRecord) {
    assert_eq!(
        record.autonomous_system_number,
        Some(PROBE_ASN),
        "wrong or missing ASN: {record:?}"
    );
    assert!(
        record
            .autonomous_system_organization
            .as_ref()
            .is_some_and(|name| !name.trim().is_empty()),
        "no operator name: {record:?}"
    );
}

#[tokio::test]
async fn db_ip_lite_provisions_and_answers() {
    if !live() {
        eprintln!("skipped: set FACTBOOK_LIVE=1");
        return;
    }

    let (_dir, databases) = provision(
        ProviderSelection::from(GeoIpProvider::DbIp),
        AutoDownloadConfig::default(),
    )
    .await;

    assert_real_databases(&databases);
    let record = record_for(&databases);
    assert_eq!(record.country_code.as_deref(), Some("US"), "{record:?}");

    // DB-IP's free ASN database leaves its operator-name column blank on every
    // row, so the content check refuses it and this provider serves the city
    // half alone.
    assert!(databases.asn.is_none(), "{databases:?}");
    assert!(record.autonomous_system_number.is_none(), "{record:?}");
}

#[tokio::test]
async fn maxmind_free_provisions_and_answers() {
    let (Some(account), Some(key)) = (
        credential("MAXMIND_ACCOUNT_ID"),
        credential("MAXMIND_LICENSE_KEY"),
    ) else {
        eprintln!("skipped: no MaxMind credential");
        return;
    };
    if !live() {
        eprintln!("skipped: set FACTBOOK_LIVE=1");
        return;
    }

    let (_dir, databases) = provision(
        ProviderSelection::from(GeoIpProvider::MaxMind),
        AutoDownloadConfig {
            maxmind_account_id: Some(account.into()),
            maxmind_license_key: Some(key.into()),
            ..AutoDownloadConfig::default()
        },
    )
    .await;

    assert_real_databases(&databases);
    let record = record_for(&databases);
    assert_eq!(record.country_code.as_deref(), Some("US"), "{record:?}");
    assert_names_the_operator(&record);
}

#[tokio::test]
async fn ipinfo_lite_provisions_and_answers() {
    let Some(token) = credential("IP_INFO_API_TOKEN") else {
        eprintln!("skipped: no IPinfo token");
        return;
    };
    if !live() {
        eprintln!("skipped: set FACTBOOK_LIVE=1");
        return;
    }

    // IPinfo publishes no separate ASN database; its one file carries both.
    let (_dir, databases) = provision(
        ProviderSelection {
            city: GeoIpProvider::IpInfo.into(),
            ..ProviderSelection::default()
        },
        AutoDownloadConfig {
            ipinfo_token: Some(token.into()),
            ..AutoDownloadConfig::default()
        },
    )
    .await;

    let only_city = Databases {
        city: databases.city.clone(),
        asn: None,
    };
    assert_real_databases(&only_city);

    let record = record_for(&only_city);
    assert_eq!(record.country_code.as_deref(), Some("US"), "{record:?}");
    assert_names_the_operator(&record);
}

#[tokio::test]
async fn sapics_origin_asn_provisions_and_answers() {
    if !live() {
        eprintln!("skipped: set FACTBOOK_LIVE=1");
        return;
    }

    // ASN-only, so the city half is left unselected and no country resolves.
    let (_dir, databases) = provision(
        ProviderSelection {
            asn: GeoIpProvider::SapicsOriginAsn.into(),
            ..ProviderSelection::default()
        },
        AutoDownloadConfig::default(),
    )
    .await;

    let only_asn = Databases {
        city: None,
        asn: databases.asn.clone(),
    };
    assert_real_databases(&only_asn);
    assert_names_the_operator(&record_for(&only_asn));
}

#[tokio::test]
async fn sapics_iptoasn_provisions_and_answers() {
    if !live() {
        eprintln!("skipped: set FACTBOOK_LIVE=1");
        return;
    }

    let (_dir, databases) = provision(
        ProviderSelection {
            asn: GeoIpProvider::SapicsIpToAsn.into(),
            ..ProviderSelection::default()
        },
        AutoDownloadConfig::default(),
    )
    .await;

    let only_asn = Databases {
        city: None,
        asn: databases.asn.clone(),
    };
    assert_real_databases(&only_asn);
    assert_names_the_operator(&record_for(&only_asn));
}
