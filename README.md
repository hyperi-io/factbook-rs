# factbook

A high-speed, self-maintaining fact enricher for data at scale.

Point it at reference data -- wherever it lives, in whatever shape it ships --
and it fetches, verifies, indexes and serves it, keeping itself current without
anyone tending it. Your hot path gets a lookup measured in nanoseconds against
facts that were refreshed while it was running.

Reference data is never the interesting part of a system and costs a surprising
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
  own config and fetches over their own link, with no provider modelled here at
  all.

That last one is the point of the generic half: factbook does not need to know
what your data means to keep it fresh and fast.

## Using it

```toml
factbook = "0.1"
```

### GeoIP with MaxMind

The common case. Name the provider, hand it your credentials, and factbook
downloads what is missing or stale and opens it behind a cache:

```rust,no_run
use factbook::geoip::{
    AutoDownloadConfig, CacheConfig, GeoIp, GeoIpConfig, GeoIpProvider, ProviderSelection,
    ensure_databases,
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
let geoip = GeoIp::from_databases(&databases, CacheConfig::default())?;

if let Some(record) = geoip.lookup("203.0.113.42".parse()?) {
    println!("{:?} / {:?}", record.country_code, record.city_name);
}
# Ok(())
# }
```

Credentials are `Secret`: redacted in `Debug` and `Display`, and never formatted
into a URL or a process argument. Supply them from your secrets layer rather
than as literals.

The same thing in config, which is how most deployments say it:

```yaml
geoip:
  provider:
    city:
      provider: max_mind
      tier: paid
    asn: sapics_origin_asn
```

The tier is named, never inferred from which credential happens to be set --
inferring it means a deployment that forgets a token silently drops to a worse
dataset instead of saying so. `factbook::geoip::validate` is the config-load
check, so a missing credential or an unmodelled tier is reported by name rather
than as a 401 on the first transfer.

Every modelled source, and where its publisher states its terms:
[SOURCES.md](SOURCES.md).

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

**CSV and JSON**, raw or gzipped, indexed by an address or by any column the
data has. Column names come from the file by default, and from config for the
headerless case that has no other answer:

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

Four stages, and every source goes through the same ones:

1. **Acquire.** Resolve the source, fetch it over a link that may be slow or
   throttled, resume what was interrupted, unpack what arrives compressed.
2. **Verify.** Check the published digest, the promised length and the contents
   before anything is allowed to become live data.
3. **Maintain.** Refresh on a cadence, notice a file changed underneath a
   running process, and swap it in without blocking a lookup.
4. **Serve.** An mmap-backed reader behind a cache, keyed for how reference
   lookups are actually distributed.

The swap is an atomic rename, which is what makes the memory-mapped reader safe:
writing a refresh in place underneath an mmap is undefined behaviour, not merely
racy.

## What makes it worth using

### The cache is shaped for reference lookups

Reference lookups are heavily frequency-biased -- your own egress, the CDNs in
front of your users, whoever is scanning you this week -- and the long tail is
nearly all single-appearance noise. That skew punishes a plain recency policy: a
burst of one-off keys evicts the entries that earn their keep. On a synthetic
Zipf stream with 30% one-off addresses, a straight LRU gave up 8.8 points of hit
ratio against a policy cache at the same capacity.

factbook uses [quick_cache](https://crates.io/crates/quick_cache), whose
Clock-PRO/S3-FIFO design keeps a ghost queue of recently-evicted keys, so an
entry dropped under a scan burst is promoted straight back on its next
appearance rather than starting from nothing.

| path | cost | against |
|---|---|---|
| cache hit | 27-37 ns | -- |
| private or reserved address | 17-19 ns | about **2x** cheaper than a hit |
| cold read, key present | 2.4-2.7 µs | cache worth roughly **65-100x** |
| owned record instead of an `Arc` | 69-90 ns | about **2.4x** the cost of a hit |

Ranges are the spread across two runs of the default feature set on one loaded
machine, not a leaderboard entry. Take the ratios, treat the absolutes as
indicative, and measure your own traffic.

**Measure with the features you ship.** Enabling `metrics` puts two
`Instant::now()` calls on the hit path and costs roughly 3-4x there, which is
enough to invert the ratios above. It is off by default for that reason.

Two details follow from the same reasoning:

- **A hit hands back an `Arc`, not a copy.** Returning an owned record costs
  about two and a half times a hit, to reproduce something already in memory.
- **The key is an `IpAddr`, not the text of one.** 17 bytes, `Copy`, no
  allocation and no string hashing per lookup -- and `::1` and
  `0:0:0:0:0:0:0:1` stop being two entries for one address.

**`lookup_many` is not a fast path for warm data.** It deduplicates a batch
through a map before reading, which pays only when the reads behind it are
expensive: on a cold cache it comes out ahead, and on a warm one it measured
1.4-1.7x *slower* than just calling `lookup` in a loop. `lookup` already
caches, so the loop is the right default and the batch call is for the cold
case.

### It keeps itself current, safely

Refresh runs on the source's own cadence, honours any fetch ceiling the provider
sets, and swaps the reader underneath a running process without blocking a
lookup. Nothing schedules it, nothing restarts for it.

The transfer assumes the link is slow rather than quick, because reference data
is tens of megabytes and free tiers are rate limited:

- **No total timeout.** A whole-request budget caps the link speed a deployment
  is allowed to have. The bound is an idle one -- 60 seconds by default -- so a
  transfer that is progressing keeps going and a stalled one still fails inside
  a minute. Connect stays short at 30, because a dead host should fail fast.
- **Interrupted transfers resume.** The `.part` file's length becomes a `Range`
  request. A server that ignores it answers 200 and the file starts again,
  because appending a whole body to a prefix is exactly the corruption this
  crate exists to avoid.
- **Progress is logged** every 30 seconds, so a slow download is legible rather
  than indistinguishable from a hang.
- **Downloads are sequential.** Two transfers over one throttled link are not
  faster and may trip a rate limiter.

And a bad download never replaces good data. Providers answer 200 with a login
page, an error page, or a body that stops early, often enough that a status code
is not evidence of anything, so every download is checked before it is allowed
to become live:

- **The published digest**, wherever the source ships one beside the file.
- **The contents, always.** An MMDB carries the metadata marker the format
  requires and answers a known-answer probe. A text payload must not open with
  `<!DOCTYPE`, `<html` or `<?xml`.
- **The volume.** A replacement a fraction of the size of the copy on disk is
  refused rather than trusted.
- **The promised length**, the cheapest truncation check available.

A file failing any of those is deleted and logged at error level, and the copy
already on disk keeps being served. That matters more than it sounds: a bad file
at the destination would also have a fresh mtime, so the freshness check would
never replace it.

Failures are classified rather than lumped together. A 401 or 403 names the
config field to fix and is reported as permanent -- retrying burns the
provider's quota and hides the cause. A 429 carries the provider's own
`Retry-After` and is never retried inside the transfer, because a source that
bans you is worse than one you never added.

## Features

| feature | what it adds |
|---|---|
| `geoip` *(default)* | `geoip-download` + `geoip-lookup` |
| `geoip-download` | acquisition: resolve, freshness-check, download, verify, unpack, refresh -- and the generic `table` module |
| `geoip-lookup` | mmap readers, the cache, `GeoIpRecord` |
| `metrics` | emit hit/miss/refresh through the `metrics` facade |
| `vrl` | map a record into `vrl::value::ObjectMap` for a host embedding VRL |

`vrl` is off by default and exists for consumers that actually embed VRL. Turn
it on if you are one; leave it off otherwise.

## Bring your own HTTP client

The default client is plain rustls. A deployment behind a proxy, or one that
trusts a private CA, passes its own configured `reqwest::Client` instead:

```rust,no_run
use factbook::geoip::GeoIpConfig;

# fn example(configured: reqwest::Client) {
let config = GeoIpConfig::default().with_http_client(configured);
# }
```

An injected client is used exactly as it stands, timeouts included.

## Licence

Apache-2.0. See `LICENSE`.

factbook ships no reference data. It fetches at runtime from whichever source
you configure, and each source sets its own terms for its own data.

Credentials are held in a `Secret` newtype that never reaches a log line, a
process argument or a formatted URL.
