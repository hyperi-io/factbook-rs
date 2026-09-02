# factbook

A high-speed, self-maintaining fact enricher for data at scale.

Point it at reference data -- wherever it lives, in whatever shape it ships --
and it fetches, verifies, indexes and serves it, keeping itself current without
anyone tending it. Your hot path gets a lookup measured in nanoseconds against
facts that were refreshed while it was running.

Reference data is rarely what a system is built for, and costs a surprising
amount anyway. Someone writes a downloader. Someone writes a freshness check.
Someone discovers the provider answered 200 with a login page and quietly
overwrote a good database with 35 KB of HTML. Someone else notices the
enrichment table does a full traversal for a key that appears ten thousand times
a minute. Every service grows its own copy of all four.

**What people point it at:**

- **IP geolocation and reputation** -- the batteries-included case, with the
  providers already modelled.
- **Fraud correlation** -- ASN, hosting and proxy side tables joined onto the
  event that needs scoring.
- **Telco BGP** -- prefix-to-origin and operator-name data at carrier
  granularity rather than the vanilla cyber view.
- **Defence telemetry streams** -- a reference table an operator names in their
  own config and fetches over their own link, no provider is implemented here.

That last one is the point of the generic half: factbook does not need to know
what your data means to keep it fresh and fast.

## Using it

```toml
factbook = "0.1"
```

### GeoIP, with no account anywhere

The default. No credentials, no signup, nothing to configure -- factbook
downloads what is missing or stale and opens it behind a cache:

```rust,no_run
use factbook::geoip::{CacheConfig, GeoIp, GeoIpConfig, ensure_databases};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let databases = ensure_databases(&GeoIpConfig::default()).await?;
let geoip = GeoIp::from_databases(&databases, CacheConfig::default())?;

if let Some(record) = geoip.lookup("8.8.8.8".parse()?) {
    println!("{:?} / {:?}", record.country_code, record.city_name);
}
# Ok(())
# }
```

That takes DB-IP Lite for location and sapics `origin-asn` for networks, which
between them answer for more of the address space than the free tier you need an
account for. The measurements are in
[docs/data-sources.md](https://github.com/hyperi-io/factbook-rs/blob/main/docs/data-sources.md).

### GeoIP with a provider account

Name the provider and hand it your credentials. The tier is named too, never
inferred from which credential happens to be set:

```rust,no_run
use factbook::geoip::{
    AutoDownloadConfig, GeoIpConfig, GeoIpProvider, ProviderSelection, ensure_databases,
};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let config = GeoIpConfig {
    provider: ProviderSelection::from(GeoIpProvider::MaxMind),
    auto_download: AutoDownloadConfig {
        maxmind_account_id: Some("123456".into()),
        maxmind_license_key: Some("your-licence-key".into()),
        ..AutoDownloadConfig::default()
    },
    ..GeoIpConfig::default()
};

let databases = ensure_databases(&config).await?;
# Ok(())
# }
```

Credentials are `Secret`: redacted in `Debug` and `Display`, and never formatted
into a URL or a process argument. Supply them from your secrets layer rather
than as literals. `factbook::geoip::validate` checks a selection when config
loads, so a missing credential is reported by name rather than as a rejected
request on the first fetch.

Provisioning and lookup stay separate calls rather than one convenience
constructor, so a host that pre-seeds its databases or mounts them read-only
skips the first line. A host running someone else's lookup engine -- a Vector
process reading the MMDB itself, say -- takes the download half alone and pays
for no reader:

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip-download"] }
```

### A source you name yourself

Anything else. A source is data, not code -- a URL, a file name, an encoding,
where the column names come from, and which key reaches a row. No closures, no
type parameters, nothing that needs a recompile to add:

```yaml
url: https://example.net/networks.csv
checksum_url: https://example.net/networks.csv.sha256
file: networks.csv
format: csv
index:
  column: asn
```

```rust,no_run
use factbook::geoip::AutoDownloadConfig;
use factbook::table::{Index, Table, TableFormat, TableSource};

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
# Ok(())
# }
```

**CSV and JSON**, raw or gzipped, indexed by an address, by a CIDR range, or by
any column the data has. Column names come from the file by default, and from
config for the headerless case that has no other answer:

```yaml
format:
  csv:
    header: false
schema:
  named: [asn, name, country]
```

Unknown keys are an error rather than a silent default, so a misspelt `indx` is
caught when the config loads rather than at the first fetch.

## How it works

Four stages, and every source goes through the same ones: **acquire** over a
link that may be slow or throttled, **verify** before anything becomes live
data, **maintain** on the source's own cadence, and **serve** from a
memory-mapped reader behind a cache.

A refused download never replaces a good database -- the file already on disk
keeps being served. A hit costs tens of nanoseconds against microseconds for
the database read behind it, and private or reserved addresses never reach the
cache at all.

Why that ordering makes the memory map safe, and what the six checks are:
[docs/architecture.md](https://github.com/hyperi-io/factbook-rs/blob/main/docs/architecture.md).

## Documentation

| Read | When |
|---|---|
| [getting-started](https://github.com/hyperi-io/factbook-rs/blob/main/docs/getting-started.md) | Adding factbook to something, or checking a build works |
| [data-sources](https://github.com/hyperi-io/factbook-rs/blob/main/docs/data-sources.md) | Choosing what to fetch, or asking why the default is what it is |
| [configuration](https://github.com/hyperi-io/factbook-rs/blob/main/docs/configuration.md) | Looking up a key, its type or its default |
| [metrics](https://github.com/hyperi-io/factbook-rs/blob/main/docs/metrics.md) | Wiring up alerts, or asking what to watch |
| [architecture](https://github.com/hyperi-io/factbook-rs/blob/main/docs/architecture.md) | Understanding how a refused download cannot damage a running service |

## Licence

Apache-2.0. See `LICENSE`.

factbook ships no reference data. It fetches at runtime from whichever source
you configure, and each source sets its own terms for its own data.
[SOURCES.md](https://github.com/hyperi-io/factbook-rs/blob/main/SOURCES.md)
points at where each publisher states them.

Credentials are held in a `Secret` newtype that never reaches a log line, a
process argument or a formatted URL.
