<!-- Project:   factbook -->
<!-- File:      docs/metrics.md -->
<!-- Purpose:   Every metric emitted, and which feature emits it -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Metrics

Everything factbook emits, through the [`metrics`](https://crates.io/crates/metrics) facade. Nothing is recorded unless the host installs a recorder, so a consumer with no observability stack pays for the call and nothing else.

## Watch the database age above all else

`enrichment_database_age_seconds` is the metric that catches the failure nobody notices. A deployment whose downloads have been failing for months keeps answering: the cache still hits, lookups still return records, and the only complaint is a warn line in a log nobody reads. The data is just old, and quietly wrong about anything that moved.

Alert on it against the provider's own publish cadence -- daily sources should sit under a couple of days, monthly ones under a couple of months.

## Acquisition, on by default

The `metrics` feature is in the default set. It runs once per database per refresh, so the cost is a `stat` on a daily or monthly cadence.

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_database_age_seconds` | gauge | `type`, `kind` | Seconds since the database was written. The oldest file of a set, because a database is only as current as its stalest half |
| `enrichment_download_total` | counter | `type`, `kind`, `result` | One per download attempt, by how it ended |

`kind` is `city` or `asn`. `type` is `geoip` on every series.

### The `result` label separates three different problems

| value | means | what to do |
|---|---|---|
| `ok` | The download landed and replaced the file | nothing |
| `refused` | Bytes arrived and a check rejected them | the provider published something bad; the previous file is still being served |
| `failed` | Bytes never arrived | the network, the endpoint, or the credential |
| `busy` | Another process holds the lock on the file | expected where several processes share a data directory; sustained means one is stuck |

`refused` and `failed` are worth separating because they need different people. A `refused` run means the provider shipped a login page, a stub, or a database that answers nothing -- factbook did its job and kept the old file. A `failed` run never got that far.

## Lookup, off by default

The `metrics-lookup` feature is opt-in, on measurement rather than principle: it costs three to four times a cache hit, and the hit path is the one the crate exists to make fast. Take it when you want cache behaviour visible and can afford it.

```toml
factbook = { version = "0.1", features = ["metrics-lookup"] }
```

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_cache_hits_total` | counter | `type` | Addresses the cache already held |
| `enrichment_cache_misses_total` | counter | `type` | Addresses that cost a database read |
| `enrichment_cache_size` | gauge | `type` | Entries held, against the configured capacity |
| `enrichment_duration_seconds` | histogram | `type` | Time for one lookup, hit or miss |

The hit ratio these give is what tells you whether `cache.capacity` is set sensibly. A ratio that will not climb with more capacity means the traffic is not repeating, and the cache is not the thing to tune.

## Names are shared, not private

These four lookup names match the descriptors an existing consumer already registers, so the series land in the group it has rather than beside it. A rename here would not fail loudly -- the metric would simply be recorded without its description and appear as a stranger. Treat the names as a contract.

## Turning it all off

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip"] }
```

That drops the facade dependency entirely. The call sites compile to nothing.
