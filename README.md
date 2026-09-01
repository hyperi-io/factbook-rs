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
the entries that earn their keep. Measured on a Zipf stream with a 30% scan
burst, a straight LRU gave up 8.8 points of hit ratio against a policy cache at
the same capacity.

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

```rust
use factbook::geoip::{GeoIp, GeoIpConfig};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
// Downloads what is missing or stale, opens the readers, builds the cache.
let geoip = GeoIp::from_config(&GeoIpConfig::default()).await?;

if let Some(record) = geoip.lookup("203.0.113.42".parse()?) {
    println!("{:?} / {:?}", record.country_code, record.city_name);
}
# Ok(())
# }
```

### Provisioning only

A host that runs someone else's lookup engine -- a Vector process reading the
MMDB itself, say -- wants the download half and nothing more:

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip-download"] }
```

```rust
use factbook::geoip::{ensure_databases, GeoIpConfig};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let paths = ensure_databases(&GeoIpConfig::default()).await?;
// `paths.city` and `paths.asn` are now on disk and fresh.
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

```rust
use factbook::geoip::GeoIpConfig;

# fn example(configured: reqwest::Client) {
let config = GeoIpConfig::default().with_http_client(configured);
# }
```

An injected client is used exactly as it stands, timeouts included.

## Database providers and their licences

The provider you choose carries an obligation, and it is not the same one in
every case. This matters if you redistribute a product that ships a default.

| provider | tier | databases | format | licence |
|---|---|---|---|---|
| `db_ip` | free (DB-IP Lite) | city, ASN | MMDB | CC BY 4.0 -- **attribution required** |
| `max_mind` | free (GeoLite2) | city, ASN | MMDB | MaxMind EULA -- account and licence key required |
| `max_mind` | paid (GeoIP2) | city, ASN through GeoIP2-ISP | MMDB | MaxMind EULA -- account and licence key required |
| `ip_info` | free (IPinfo Lite) | city | MMDB | IPinfo terms -- token required |
| `sapics_db_ip_asn` | free | ASN | CSV | CC BY 4.0 -- **attribution required** |
| `sapics_origin_asn` | free | ASN | CSV | PDDL -- public domain, no attribution |

sapics republishes several upstreams and **the licence is per dataset, not per
provider** -- its `dbip-*` sets carry DB-IP's attribution requirement, and only
the `iptoasn`, `origin-asn` and `*-country` sets are attribution-free. Reading
"sapics" as one licence is the mistake to avoid.

Note the format column: the sapics datasets are published as CSV, not MMDB, and
as two files -- IPv4 and IPv6 -- so choosing one is also choosing a format and a
file count. Every provisioned database reports its own `format`, so a consumer
picks its reader from what it was handed.

### Free and paid tiers

The tier is named in the config, never inferred from which credential happens to
be set:

```yaml
geoip:
  provider:
    city:
      provider: max_mind
      tier: paid
    asn: sapics_db_ip_asn
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
