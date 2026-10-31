<!-- Project:   factbook -->
<!-- File:      docs/crate.md -->
<!-- Purpose:   The crates.io landing page -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# factbook

A high-speed, self-maintaining fact enricher for data at scale. Point it at reference data and it fetches, verifies, indexes and serves it. You call it on your own schedule, and it replaces only what is stale while lookups carry on.

IP geolocation is the batteries-included case, and it works with no account anywhere.

## What it gives you

- GeoIP with no signup: DB-IP Lite for location and sapics `origin-asn` for networks by default, with MaxMind and IPinfo when you hold an account.
- Downloads checked before they go live: format, size, the provider's digest where there is one, and a content probe you can turn off. A refused file leaves the copy on disk serving.
- Lookups behind a cache, where a hit costs tens of nanoseconds and private or reserved addresses never reach it.
- Refresh with no pause: `refresh_if_changed` swaps readers lock-free. factbook starts no timer, so call it and `ensure_databases` from your own.
- A GeoIP database read onto the heap when it fits under the ceiling you set, and memory-mapped when it does not.
- Your own reference tables: CSV or JSON, raw or gzipped, indexed by address, CIDR range or any column. A table is held in memory, and one past the ceiling is refused.
- Credentials held as `Secret`, redacted in `Debug`, `Display` and the URLs factbook logs. IPinfo takes its token as a query parameter.

## Example

```console
cargo add factbook
cargo add tokio --features macros,rt-multi-thread
```

```rust,no_run
use factbook::geoip::{CacheConfig, GeoIp, GeoIpConfig, ensure_databases};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Downloads whatever is missing or stale, then opens it behind a cache.
    let databases = ensure_databases(&GeoIpConfig::default()).await?;
    let geoip = GeoIp::from_databases(&databases, CacheConfig::default())?;

    if let Some(record) = geoip.lookup("8.8.8.8".parse()?) {
        println!("{:?} / {:?}", record.country_code, record.city_name);
    }
    Ok(())
}
```

## Features

| Feature | Adds |
|---|---|
| `geoip` | `geoip-download` plus `geoip-lookup` (default) |
| `geoip-download` | Resolve, freshness-check, download and verify, plus reference tables |
| `geoip-lookup` | The resident or mapped readers and the cache |
| `metrics` | Download outcomes and database age through the `metrics` facade (default) |
| `metrics-lookup` | Cache hits, misses, size and lookup time (implies `metrics`) |

A host running someone else's lookup engine takes `default-features = false, features = ["geoip-download"]` and pays for no reader.

factbook ships no reference data. It fetches at runtime from the source you configure, and each publisher sets its own terms.

API docs are at <https://docs.rs/factbook>, the source and the data source guide at <https://github.com/hyperi-io/factbook-rs>. Licensed Apache-2.0: <https://github.com/hyperi-io/factbook-rs/blob/main/LICENSE>.
