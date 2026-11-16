<!-- Project:   factbook -->
<!-- File:      docs/configuration.md -->
<!-- Purpose:   Every configuration key, its type and its default -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Configuration

Every key a deployment can set. Unknown keys are rejected when config loads, so a misspelt key is reported rather than silently defaulted.

## GeoIP

Serialised as a `geoip` block in the host's config, or built directly as `GeoIpConfig`.

| key | type | default | what it does |
|---|---|---|---|
| `enabled` | bool | `true` | Provision databases at all. The config-side opt-out for a service that calls `ensure_databases` unconditionally |
| `provider` | selection | DB-IP city, sapics `origin-asn` ASN | Where each half comes from, `custom` to download nothing. See [data-sources.md](data-sources.md) |
| `city_db_path` | path | unset | Use this file and provision nothing for the location half |
| `asn_db_path` | path | unset | The same for the network half |
| `auto_download` | block | below | Transfer settings |

Setting a path bypasses the provider for that half. The other half still provisions normally.

### Auto-download

| key | type | default | what it does |
|---|---|---|---|
| `enabled` | bool | `true` | Fetch when the local copy is missing or stale. When false, the files on disk are returned and nothing is fetched |
| `data_dir` | path | `/var/lib/geoip` | Where databases are written |
| `maxmind_account_id` | secret | unset | Required by `max_mind` on either tier |
| `maxmind_license_key` | secret | unset | Required by `max_mind` on either tier |
| `ipinfo_token` | secret | unset | Required by `ip_info` |
| `max_age_days` | u32 | `30` | Ceiling on how stale a copy may be. The two halves read it differently, below |
| `connect_timeout_secs` | u64 | `30` | A dead host should fail fast. Zero is refused by `validate`, and a download with it fails naming the key. Not read when a client is injected |
| `read_timeout_secs` | u64 | `60` | Idle bound, not a whole-request budget, so a slow transfer is not cut off for being slow. Zero is refused like `connect_timeout_secs` |
| `verify_content` | bool | `true` | Check that a downloaded database answers a known address. A table source is loaded before it replaces the copy on disk whatever this says |
| `min_size_percent` | u8 | `50` | Refuse a replacement below this fraction of the copy on disk. Zero disables the floor, and a value above 100 is refused by `validate` |
| `age_from_source` | bool | `true` | Report database age from the publisher's build stamp rather than the local write time. Read by the metric alone, and freshness still counts from the write |
| `resident_max_bytes` | u64 | `134217728` | Read a database this size or smaller onto the heap when it is opened, and map a larger one. Zero maps everything. A table source that outgrows it is refused, below |

A geo provider reads `max_age_days` against its published cadence: the window is that cadence, shortened by this ceiling, then floored at whatever minimum interval between fetches the provider's row states. MaxMind and IPinfo state one day, which is what their download caps allow. DB-IP and sapics state none, so nothing floors the ceiling there.

A table source has neither a cadence nor a minimum, so `max_age_days` is used as it stands and `max_age_days: 0` re-fetches on every `Table::ensure` call.

There is deliberately no total request timeout. A whole-request budget puts a ceiling on the link speed a deployment is allowed to have, which fails a slow but healthy transfer. The idle bound fails a stalled one inside a minute and lets a progressing one finish.

The three credential keys are redacted in debug and display output, and never reach a URL or a process argument.

## A mapped database can stall a lookup on the file, and a resident one cannot

`resident_max_bytes` decides how an opened database is held. At or under it the file is read onto the heap, so no lookup waits for the file's pages partway through a tree traversal. Above it the file is mapped and its pages arrive when the operating system supplies them. A resident copy costs its size in each process, and a mapped one is shared through the page cache, so one copy serves every process on the node.

The default is 128 MiB. A database that grows past it is mapped from then on, and raising the ceiling keeps it resident.

Set it to zero to map everything, which is what a deployment wants when several processes share a host short of memory.

The same key exists on `CacheConfig`, which is what the lookup half acts on. Two fields rather than one because the lookup half compiles without the download half, so it cannot read `auto_download` at all, and `GeoIp::open` takes no download settings -- provisioning and lookup are separate calls by design. A host that configures one copies it across.

### Cache

Held separately from provisioning, because a deployment that pre-seeds its databases still configures a cache. `CacheConfig` is built in code and has no config-file form.

| key | type | default | what it does |
|---|---|---|---|
| `capacity` | usize | `100_000` | Addresses held before eviction starts. A ceiling, not a reservation |
| `max_age` | duration | unset | How long a cached answer stays usable |
| `collect_extra_fields` | bool | `true` | Keep the source fields the record has no typed field for. Off is what makes a cached entry small |
| `resident_max_bytes` | u64 | `134217728` | The ceiling the lookup half acts on, above |

Leave `max_age` unset. An answer changes when the file behind it changes, and the reader swap `refresh_if_changed` makes clears the cache. A time limit evicts correct answers early.

`collect_extra_fields` decides what an entry costs. A database holds more than the record names -- an ISP edition's `isp` and `organization`, a city build's geoname ids, confidence scores and names in eight languages -- and on, all of it is kept under the path it sits at in the source. That takes an entry from a few hundred bytes on a lean ASN table to two to four kilobytes on a rich city build, so at the default capacity it is the difference between tens of megabytes of cache and a few hundred.

Off, those fields are never read rather than read and dropped, so it saves the decode as well as the memory. The typed fields resolve the same either way, which is what makes this a trade of fields for cache and nothing else. It stays on by default because enriching from everything the source supplies is the point of the crate, and a default that silently dropped fields would undercut it.

## Table sources

A table source is any file factbook has no provider row for, described in config.

| key | type | default | what it does |
|---|---|---|---|
| `url` | string | required | Where to fetch it |
| `file` | string | required | Name it is written under, inside `data_dir` |
| `format` | `csv` or `json` | required | How rows are encoded |
| `index` | `ip`, `prefix` or `{column: name}` | required | The key that reaches a row |
| `checksum_url` | string | unset | A digest published beside the file, verified before the file is admitted |
| `archive` | `raw` or `gzip` | `raw` | How the fetched bytes are packaged |
| `schema` | `auto` or `{named: [...]}` | `auto` | Where column names come from |

The common case is four keys:

```yaml
url: https://example.net/networks.csv
file: networks.csv
format: csv
index:
  column: asn
```

A CSV published without a header has no other way to name its columns:

```yaml
url: https://example.net/networks.csv.gz
archive: gzip
file: networks.csv
format:
  csv:
    header: false
schema:
  named: [asn, name, country]
index:
  column: asn
```

### Reaching a row by CIDR range

`index: prefix` keys the rows by range and answers an address with the most specific range containing it, so a `/24` wins over the `/8` around it. A bare address counts as a range of one address, because publishers mix single hosts into a prefix list.

```yaml
url: https://example.net/announcements.csv
file: announcements.csv
format: csv
index: prefix
```

The column is found the same way `ip` finds one -- a conventional name (`prefix`, `network`, `cidr`, `range`, `subnet`) is preferred, and a source using none is sampled instead. Name it with `index: {column: ...}` when neither works.

### A table that will not fit is refused

A geo database is in the format the reader reads, so `resident_max_bytes` decides no more than how it is opened. A table source is a CSV or a JSON document, and holding one costs several times the file it came from -- so the same key decides something bigger for a table: whether it can be held at all.

If it loads into memory it is used. If it does not, `Table::ensure` returns `OverResidentCeiling` and the table does not load. Raise the ceiling, or point the source at less data.

A download is held to the same ceiling before it replaces anything. One that would not load is refused and the copy on disk keeps serving. With no copy on disk the refusal reaches the caller as `Fetch(Unparseable)`, carrying the same message.

Whether a source fits is not knowable before it is read, so the ceiling is hit rather than predicted: the read stops the moment it crosses `resident_max_bytes` and releases what it had taken. [architecture.md](architecture.md#a-database-that-will-not-fit-is-mapped-and-a-table-that-will-not-fit-is-refused) has the detail.

There is no fallback. Writing an oversized source out as a MaxMind DB and serving it from there is not built, tracked in [issue #2](https://github.com/hyperi-io/factbook-rs/issues/2).

### How the formats differ on a bad row

A CSV row of the wrong width is refused, with the line number and both counts. A CSV states its width once, so a wrong row means an unquoted separator that would shift every later column without complaint.

Ragged JSON widens the table instead of failing it. Objects with differing keys are ordinary in published feeds, and a key an object lacks is an absent cell rather than an error.

## Bringing your own HTTP client

Both `GeoIpConfig` and `TableSource` accept a configured `reqwest::Client`, which is how a deployment behind a proxy or trusting a private certificate authority reaches a provider. An injected client is used exactly as it stands, so `connect_timeout_secs` and `read_timeout_secs` stop applying and the client's timeouts govern. Every other setting above still applies.

A client is a handle rather than a setting, so it is excluded from equality: a consumer comparing an old config against a new one to decide whether to reload compares what an operator sets.

### Authenticate a table source through its client

`TableSource` has no credential field. A token, a signed header or a client certificate reaches the request through the client the source is handed:

```rust
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

let mut headers = HeaderMap::new();
let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))?;
auth.set_sensitive(true);
headers.insert(AUTHORIZATION, auth);

let source = TableSource::new(url, "networks.csv", format, index)
    .with_http_client(reqwest::Client::builder().default_headers(headers).build()?);
```

That client fetches the digest at `checksum_url` as well as the file, so a source with its checksum endpoint behind the same wall needs no further wiring. `http_client` is skipped by serde and never comes from a config file, which splits the two concerns the way they are usually owned: the operator's config names the source, and the calling code attaches the credential from wherever it keeps secrets.

The geo providers are the exception. Their credentials have dedicated keys -- `maxmind_account_id`, `maxmind_license_key`, `ipinfo_token` -- because factbook models those endpoints and knows how each expects to be asked.
