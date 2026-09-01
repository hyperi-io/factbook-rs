<!-- Project:   factbook -->
<!-- File:      docs/configuration.md -->
<!-- Purpose:   Every configuration key, its type and its default -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Configuration

Every key a deployment can set. Unknown keys are rejected when config loads, so a misspelt key is reported rather than silently defaulted.

## GeoIP

Serialised as the `geoip` block, or built directly as `GeoIpConfig`.

| key | type | default | what it does |
|---|---|---|---|
| `enabled` | bool | `true` | Provision databases at all. The config-side opt-out for a service that calls `ensure_databases` unconditionally |
| `provider` | selection | DB-IP city, sapics `origin-asn` ASN | Where each half comes from, `custom` to download nothing. See [data-sources.md](data-sources.md) |
| `city_db_path` | path | unset | Use this file and provision nothing for the location half |
| `asn_db_path` | path | unset | The same for the network half |
| `auto_download` | block | below | Transfer settings |

Setting a path bypasses the provider for that half only. The other half still provisions normally.

### Auto-download

| key | type | default | what it does |
|---|---|---|---|
| `enabled` | bool | `true` | Fetch when the local copy is missing or stale. When false, only files already on disk are returned |
| `data_dir` | path | `/var/lib/geoip` | Where databases are written |
| `maxmind_account_id` | secret | unset | Required by `max_mind` on either tier |
| `maxmind_license_key` | secret | unset | Required by `max_mind` on either tier |
| `ipinfo_token` | secret | unset | Required by `ip_info` |
| `max_age_days` | u32 | `30` | Ceiling on how stale a copy may be. It can shorten a provider's own cadence but never lengthen it, and never shortens past the shortest interval that provider allows between fetches |
| `connect_timeout_secs` | u64 | `30` | A dead host should fail fast |
| `read_timeout_secs` | u64 | `60` | Idle bound, not a whole-request budget, so a slow transfer is not cut off for being slow |
| `verify_content` | bool | `true` | Check that a download is the kind of thing it claims to be |
| `min_size_percent` | u8 | `50` | Refuse a replacement below this fraction of the copy on disk. Zero disables the floor |

There is deliberately no total request timeout. A whole-request budget puts a ceiling on the link speed a deployment is allowed to have, which fails a slow but healthy transfer. The idle bound fails a stalled one inside a minute and lets a progressing one finish.

The three credential keys are redacted in debug and display output, and never reach a URL or a process argument.

### Cache

Held separately from provisioning, because a deployment that pre-seeds its databases still configures a cache.

| key | type | default | what it does |
|---|---|---|---|
| `capacity` | usize | `100_000` | Addresses held before eviction starts. A ceiling, not a reservation |
| `max_age` | duration | unset | How long a cached answer stays usable |

Leave `max_age` unset. An answer only goes stale when the file behind it changes, and a reader swap clears the cache, so a time limit evicts correct answers early and still leaves a window that clearing does not.

## Table sources

A source factbook has no provider row for is named directly.

| key | type | default | what it does |
|---|---|---|---|
| `url` | string | required | Where to fetch it |
| `file` | string | required | Name it is written under, inside `data_dir` |
| `format` | `csv` or `json` | required | How rows are encoded |
| `index` | `ip` or `{column: name}` | required | The key that reaches a row |
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

### How the formats differ on a bad row

A CSV row of the wrong width is refused, with the line number and both counts. A CSV states its width once, so a wrong row means an unquoted separator that would shift every later column without complaint.

Ragged JSON widens the table instead of failing it. Objects with differing keys are ordinary in real feeds, and a key an object lacks is an absent cell rather than an error.

## Bringing your own HTTP client

Both `GeoIpConfig` and `TableSource` accept a configured `reqwest::Client`, which is how a deployment behind a proxy or trusting a private certificate authority reaches a provider. An injected client is used exactly as it stands, so `connect_timeout_secs` and `read_timeout_secs` stop applying and the client's own timeouts govern. Every other setting above still applies.

A client is a handle rather than a setting, so it is excluded from equality: a consumer comparing an old config against a new one to decide whether to reload sees only what an operator can set.
