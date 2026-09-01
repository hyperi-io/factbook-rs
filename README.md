# factbook

IP enrichment end to end: provision the GeoIP databases, keep them fresh, and
look addresses up through a cache built for the traffic real systems actually
see.

Most GeoIP libraries do one half. A reader crate hands you `Reader::open` and
leaves provisioning to you, so every service grows its own downloader, its own
freshness check, and its own idea of where the file lives. Meanwhile the
enrichment tables in the log-shipping tools do the reading but no caching at
all, so a hot address costs a full database traversal every time it appears.

factbook is both halves in one crate:

- **Provision** -- resolve the URL for a provider, check whether the copy on
  disk is still fresh, download and unpack it, and move it into place
  atomically.
- **Refresh** -- notice the file changed underneath a running process and swap
  the reader without blocking a lookup.
- **Look up** -- an mmap-backed reader behind a cache whose admission policy
  matches how IP traffic is distributed.

## Why the cache is the point

IP traffic is heavily frequency-biased. A handful of addresses -- your own
egress, the CDNs in front of your users, whoever is scanning you this week --
account for most of what you see, and the long tail is nearly all
single-appearance noise.

That skew punishes a plain recency policy: a burst of one-off addresses evicts
the entries that earn their keep. On a synthetic Zipf stream with 30% one-off
addresses, a straight LRU gave up 8.8 points of hit ratio against a policy cache
at the same capacity -- a comparison of cache policies rather than a benchmark
of this crate, and synthetic traffic is kinder than the real thing.

factbook uses [quick_cache](https://crates.io/crates/quick_cache), whose
Clock-PRO/S3-FIFO design keeps a ghost queue of recently-evicted keys -- so an
entry dropped under a scan burst is promoted straight back on its next
appearance rather than starting from nothing.

Three details follow from the same reasoning:

- **A hit hands back an `Arc<GeoIpRecord>`, not a copy.** A record is mostly
  optional strings, so returning an owned one means up to ten allocations to
  serve something already in memory.
- **The key is an `IpAddr`, not the text of one.** 17 bytes, `Copy`, no
  allocation and no string hashing per lookup -- and `::1` and
  `0:0:0:0:0:0:0:1` stop being two entries for one address.
- **Private and reserved ranges never reach the cache.** They cannot have an
  answer, so they short-circuit to a shared constant instead of spending an
  entry.

## Usage

```rust,no_run
use factbook::geoip::{CacheConfig, GeoIp, GeoIpConfig, ensure_databases};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// Downloads whatever is missing or stale, then opens the readers behind a cache.
let databases = ensure_databases(&GeoIpConfig::default()).await?;
let geoip = GeoIp::from_databases(&databases, CacheConfig::default())?;

if let Some(record) = geoip.lookup("203.0.113.42".parse()?) {
    println!("{:?} / {:?}", record.country_code, record.city_name);
}
# Ok(())
# }
```

Provisioning and lookup stay separate calls rather than one convenience
constructor: a host that pre-seeds its databases, or mounts them read-only,
skips the first line entirely.

### Provisioning only

A host that runs someone else's lookup engine -- a Vector process reading the
MMDB itself, say -- wants the download half and nothing more:

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip-download"] }
```

```rust,no_run
use factbook::geoip::{GeoIpConfig, ensure_databases};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let databases = ensure_databases(&GeoIpConfig::default()).await?;
// `databases.city` and `databases.asn` each carry the files that landed and
// the format they arrived in.
# Ok(())
# }
```

## Features

| feature | what it adds |
|---|---|
| `geoip` *(default)* | `geoip-download` + `geoip-lookup` |
| `geoip-download` | provisioning: resolve, freshness-check, download, unpack, refresh |
| `geoip-lookup` | mmap readers, the cache, `GeoIpRecord` |
| `metrics` | emit hit/miss/refresh through the `metrics` facade |
| `vrl` | map a record into `vrl::value::ObjectMap` for a host embedding VRL |

`vrl` is off by default on purpose. The `vrl` crate's build script runs two
LALRPOP grammar builds whatever features you select, so enabling it puts a
grammar build in front of every consumer -- including the ones with no VRL in
them.

## Built for a slow link

A database is tens of megabytes and the free tiers are rate limited, so the
transfer assumes it is slow rather than quick:

- **No total timeout.** A whole-request budget caps the link speed a deployment
  is allowed to have. The bound is an idle one -- `read_timeout_secs`, 60 by
  default -- so a transfer that is progressing keeps going and one that has
  stalled still fails inside a minute. `connect_timeout_secs` stays short at 30,
  because a dead host should fail fast.
- **Interrupted transfers resume.** The `.part` file's length becomes a `Range`
  request. A server that honours it answers 206 and the file continues; a server
  that ignores it answers 200 and the file starts again, because appending a
  whole body to a prefix is exactly the corruption this crate exists to avoid.
- **Progress is logged** every 30 seconds while a transfer runs -- bytes so far,
  the expected total, and the observed rate -- so a slow download is legible
  rather than indistinguishable from a hang.
- **Downloads are sequential.** Two transfers over one throttled link are not
  faster and may trip a rate limiter.

## A bad download never replaces a good database

Providers answer 200 with a login page, an error page, or a body that stops
early, often enough that a status code is not evidence of anything. Every
download is checked before it is allowed to become the database:

- **The published digest, where there is one.** The sapics release assets ship a
  `.sha256` beside each file, and it is verified before anything is renamed.
- **The contents, always.** An MMDB has to carry the metadata marker the format
  requires; a text payload must not open with `<!DOCTYPE`, `<html` or `<?xml`.
- **The length the response promised**, which is the cheapest truncation check
  available.

A file that fails any of those is deleted and logged at error level, and the
copy already on disk stays exactly where it is and keeps being served. That
matters more than it sounds: a bad file at the destination would also have a
fresh mtime, so the freshness check would never replace it.

Failures are classified rather than lumped together. A 401 or 403 names the
config field to fix and is reported as permanent -- retrying it burns the
provider's quota and hides the cause. A 429 carries the provider's own
`Retry-After` and is never retried inside the transfer, because a source that
bans you is worse than one you never added.

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

## Database providers and their licences

Choosing a provider takes on that provider's obligation, and they differ. The
obligation falls on whoever deploys and queries the data, because factbook
downloads at runtime and distributes nothing itself.

**Ask the code, not this table.** `factbook::geoip::source_terms(selection)`
returns the licence, the attribution text, whether a logo is required, the
publish cadence and any fetch ceiling for whatever a config selects, so a
deployment can state its own obligations rather than relying on someone having
read the docs.

| provider | tier | databases | licence |
|---|---|---|---|
| `db_ip` | free (DB-IP Lite) | city, ASN | CC BY 4.0 -- **attribution required** |
| `max_mind` | free (GeoLite2) | city, ASN | MaxMind EULA -- account and licence key |
| `max_mind` | paid (GeoIP2) | city, ISP | MaxMind EULA -- account and licence key |
| `ip_info` | free (IPinfo Lite) | city | CC BY-SA 4.0 -- token required |
| `sapics_origin_asn` | free | ASN | PDDL -- public domain, no attribution |
| `sapics_ip_to_asn` | free | ASN | PDDL -- public domain, no attribution |

Every provider above publishes MMDB. sapics moved its distribution to GitHub
release assets, which is why its files are single combined databases rather than
the split CSV pairs its repository tree still holds.

sapics republishes several upstreams and **the licence is per dataset, not per
provider**: its `dbip-*` sets carry DB-IP's attribution requirement, while
`origin-asn` and `iptoasn-asn` are public domain. Reading "sapics" as one
licence is the mistake to avoid, and it is why the two datasets are separate
providers here rather than one with a switch.

`sapics_origin_asn` is the better default of the two -- the same public-domain
terms, wider coverage, and operator names rather than registry handles.

### Free and paid tiers

The tier is named in the config, never inferred from which credential happens to
be set:

```yaml
geoip:
  provider:
    city:
      provider: max_mind
      tier: paid
    asn: sapics_origin_asn
```

`factbook::geoip::validate` is the config-load check. A paid tier with no
modelled endpoint, or a provider whose credential is missing, is reported there
by name, rather than as a 401 on the first transfer. The paid lines of DB-IP and
IPinfo are not modelled yet: their endpoints are not verified, so selecting one
is refused rather than guessed at.

## Licence

Apache-2.0. See `LICENSE`.

The databases are **not** distributed with this crate and are not covered by its
licence -- factbook fetches them at runtime from whichever provider you
configure, under that provider's terms.
