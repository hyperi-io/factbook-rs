<!-- Project:   factbook -->
<!-- File:      docs/getting-started.md -->
<!-- Purpose:   From an empty project to a verified lookup -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Getting started

By the end of this you will have downloaded two databases and resolved an address, with no provider account.

## Add the crate

```toml
[dependencies]
factbook = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The default features cover both halves: fetching databases and answering from them.

## Resolve an address

`ensure_databases` downloads whatever is missing or stale and hands back the files. `GeoIp::from_databases` maps them behind a cache. They are separate calls so a host that already has its databases can skip the first.

```rust
use factbook::geoip::{CacheConfig, GeoIp, GeoIpConfig, ensure_databases};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = GeoIpConfig::default();
    let databases = ensure_databases(&config).await?;
    let geoip = GeoIp::from_databases(&databases, CacheConfig::default())?;

    if let Some(record) = geoip.lookup("8.8.8.8".parse()?) {
        println!("{:?}", record);
    }
    Ok(())
}
```

The defaults fetch DB-IP Lite for location and sapics `origin-asn` for networks, neither of which needs a signup. Files land in `/var/lib/geoip`; set `auto_download.data_dir` if that directory is not writable.

Expect a record naming the country and the operating network:

```
GeoIpRecord { city_name: Some("Mountain View"), continent_code: Some("NA"), country_code: Some("US"), country_name: Some("United States"), region_name: Some("California"), region_code: None, postal_code: None, timezone: None, latitude: Some(37.422), longitude: Some(-122.085), metro_code: None, accuracy_radius: None, autonomous_system_number: Some(15169), autonomous_system_organization: Some("Google LLC"), is_private: false, network: Some("8.8.8.0/24"), asn_network: Some("8.8.8.0/24") }
```

Every field is present in the output whether or not the source filled it, and which ones come back populated depends on the source rather than on factbook. The first run moves both databases and takes as long as the link allows -- the location half is the larger by an order of magnitude. Progress is logged every 30 seconds through the `tracing` facade, so install a subscriber if you want to watch it.

## Keep it fresh without a background thread

factbook starts no timer. Call `ensure_databases` again on whatever schedule the host already has, then tell the reader to pick up anything that moved:

```rust
if geoip.refresh_if_changed()? {
    println!("reopened");
}
```

The check is one `stat` per database and the reader swap itself is lock-free, so a lookup never waits on it. It is never done during a lookup either: a hit costs tens of nanoseconds and a `stat` is a system call.

## Fetch something no provider models

Any CSV or JSON reachable over HTTP is a source. Name the URL, the file, the encoding and the key that reaches a row:

```rust
use factbook::geoip::AutoDownloadConfig;
use factbook::table::{Index, Table, TableFormat, TableSource};

async fn networks() -> Result<(), Box<dyn std::error::Error>> {
let source = TableSource::new(
    "https://example.net/networks.csv",
    "networks.csv",
    TableFormat::Csv { header: true },
    Index::Column("asn".to_string()),
);

let table = Table::ensure(&source, &AutoDownloadConfig::default()).await?;
if let Some(row) = table.get("13335") {
    println!("{:?}", row.get("name"));
}
Ok(())
}
```

The same verification applies as to a provider database: a published digest is checked where one exists, a body that turns out to be a login page is refused, and a refusal leaves the previous copy in place.

Nothing about this half is specific to networks. The key is whatever column reaches a row, so a table keyed by product code works exactly the same way:

```rust
use factbook::table::{Index, Table, TableFormat, TableSource};

fn catalogue() -> TableSource {
    TableSource::new(
        "https://example.net/catalogue.csv",
        "catalogue.csv",
        TableFormat::Csv { header: true },
        Index::Column("sku".to_string()),
    )
}
```

```
sku,description,unit,hazard_class
AX-1180,Sodium hydroxide pellets,kg,8
BR-4402,Acetone,L,3
```

`table.get("BR-4402")` then reaches that row, and `row.get("hazard_class")` reads a cell from it.

## Take only the half you need

A host running someone else's lookup engine wants the files kept fresh and nothing else:

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip-download"] }
```

A host whose databases arrive by some other route wants the reader and no HTTP client:

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip-lookup"] }
```

## Verify a build

From a clone of the repository:

```sh
cargo test --all-features
```

Three targets run -- the unit tests, the live provider tests and the doctests -- and every one reports `test result: ok` with nothing failed.

The doctests compile the README's examples, so a stale example there fails the build. The examples in this directory are not compiled; treat them as illustrative.

The live provider tests are skipped unless asked for, because they spend a provider's daily quota. To run them:

```sh
FACTBOOK_LIVE=1 cargo test --test live_provider -- --nocapture
```

Without credentials that exercises DB-IP and both sapics sources. Setting `MAXMIND_ACCOUNT_ID`, `MAXMIND_LICENSE_KEY` or `IP_INFO_API_TOKEN` adds the provider each belongs to.

## Where to go next

- [data-sources.md](data-sources.md) -- which source to pick, and what each answers.
- [configuration.md](configuration.md) -- every key and its default.
- [architecture.md](architecture.md) -- why a refused download cannot damage a running service.
