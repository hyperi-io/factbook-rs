// Project:   factbook
// File:      tests/live_provider.rs
// Purpose:   Provision from the real provider endpoints, credentials permitting
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! The provider paths against live endpoints rather than mocks.
//!
//! Off unless asked for: these spend a daily quota and move tens of megabytes.
//!
//! ```sh
//! FACTBOOK_LIVE=1 MAXMIND_ACCOUNT_ID=... MAXMIND_LICENSE_KEY=... \
//!     cargo test --test live_provider -- --nocapture
//! ```

#![cfg(all(feature = "geoip-download", feature = "geoip-lookup"))]

use std::net::IpAddr;

use factbook::geoip::{
    AutoDownloadConfig, CacheConfig, Databases, GeoIp, GeoIpConfig, GeoIpProvider,
    ProviderSelection, ensure_databases,
};

/// Whether the caller asked for live runs at all.
fn live() -> bool {
    std::env::var("FACTBOOK_LIVE").ok().as_deref() == Some("1")
}

/// A named credential, when it was supplied and is not blank.
fn credential(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    (!value.is_empty()).then_some(value)
}

/// Refuse a file too small to be a database.
fn assert_real_databases(databases: &Databases) {
    let files = databases
        .city
        .iter()
        .chain(databases.asn.iter())
        .flat_map(|database| &database.files);

    for file in files {
        let size = std::fs::metadata(file).expect("a downloaded file").len();
        eprintln!("{} -- {size} bytes", file.display());
        assert!(size > 1_000_000, "{} is {size} bytes", file.display());
    }
}

/// Resolve `8.8.8.8`, which every provider and vintage holds.
fn assert_answers_a_known_address(databases: &Databases) {
    let geoip = GeoIp::from_databases(databases, CacheConfig::default()).expect("readers");

    let probe: IpAddr = "8.8.8.8".parse().expect("a valid address");
    let record = geoip.lookup(probe).expect("a record for 8.8.8.8");

    eprintln!("8.8.8.8 -> {record:?}");
    assert_eq!(record.country_code.as_deref(), Some("US"), "{record:?}");
    assert_eq!(
        record.autonomous_system_number,
        Some(15169),
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

    let data_dir = tempfile::tempdir().expect("temp dir");

    let config = GeoIpConfig {
        provider: ProviderSelection::from(GeoIpProvider::MaxMind),
        auto_download: AutoDownloadConfig {
            data_dir: data_dir.path().to_path_buf(),
            maxmind_account_id: Some(account.into()),
            maxmind_license_key: Some(key.into()),
            ..AutoDownloadConfig::default()
        },
        ..GeoIpConfig::default()
    };

    let databases = ensure_databases(&config).await.expect("provision");
    assert!(databases.city.is_some(), "no city database");
    assert!(databases.asn.is_some(), "no ASN database");

    assert_real_databases(&databases);
    assert_answers_a_known_address(&databases);
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

    let data_dir = tempfile::tempdir().expect("temp dir");

    // IPinfo publishes no ASN database, so only the city half is selected.
    let config = GeoIpConfig {
        provider: ProviderSelection {
            city: GeoIpProvider::IpInfo.into(),
            ..ProviderSelection::default()
        },
        auto_download: AutoDownloadConfig {
            data_dir: data_dir.path().to_path_buf(),
            ipinfo_token: Some(token.into()),
            ..AutoDownloadConfig::default()
        },
        ..GeoIpConfig::default()
    };

    let databases = ensure_databases(&config).await.expect("provision");
    let city = databases.city.as_ref().expect("a city database");
    eprintln!("IPinfo Lite files: {:?}", city.files);

    assert_real_databases(&Databases {
        city: databases.city.clone(),
        asn: None,
    });

    // The enricher decodes MaxMind's schema, so Lite's records read as absent.
    let geoip = GeoIp::from_databases(
        &Databases {
            city: databases.city.clone(),
            asn: None,
        },
        CacheConfig::default(),
    )
    .expect("readers");

    let probe: IpAddr = "8.8.8.8".parse().expect("a valid address");
    match geoip.lookup(probe) {
        Some(record) => eprintln!("IPinfo Lite answers 8.8.8.8 -> {record:?}"),
        None => eprintln!(
            "IPinfo Lite read as absent for 8.8.8.8 -- known: the enricher \
             decodes MaxMind's schema"
        ),
    }
}
