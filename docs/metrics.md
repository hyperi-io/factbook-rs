<!-- Project:   factbook -->
<!-- File:      docs/metrics.md -->
<!-- Purpose:   Every metric emitted, and which feature emits it -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Metrics

Everything factbook emits, through the [`metrics`](https://crates.io/crates/metrics) facade. Nothing is recorded unless the host installs a recorder, so a consumer with no observability stack pays for the call and nothing else.

## Watch the database age above all else

`enrichment_database_age_seconds` is the one to alert on. When downloads stop working, nothing else shows it. Lookups keep returning records, they are just increasingly wrong.

Alert it against the provider's own cadence: daily sources under a couple of days, monthly ones under a couple of months.

It counts from the timestamp the publisher stamped into the database, not from when the file landed here. Those differ by however long the copy sat published before anyone fetched it -- a monthly database downloaded this morning is not new data. Set `auto_download.age_from_source: false` for a publisher whose stamp cannot be trusted, and the gauge falls back to time since the local write.

Freshness is a separate question and still counts from the local write, so a database whose build stamp predates the staleness window is not re-fetched on every run.

## Acquisition, on by default

The `metrics` feature is in the default set. It runs once per database per refresh, so the cost is a `stat` on a daily or monthly cadence.

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_database_age_seconds` | gauge | `type`, `kind` | Seconds since the publisher built the database. The oldest file of a set, because a database is only as current as its stalest half |
| `enrichment_download_total` | counter | `type`, `kind`, `result` | One per download attempt, by how it ended |

`kind` is `city` or `asn`. `type` is `geoip` on every series.

### The `result` label separates three different problems

| value | means | what to do |
|---|---|---|
| `ok` | The download landed and replaced the file | nothing |
| `refused` | Bytes arrived and a check rejected them | the provider published something bad; the previous file is still being served |
| `failed` | Bytes never arrived | the network, the endpoint, or the credential |
| `busy` | Another process holds the lock on the file | expected where several processes share a data directory; sustained means one is stuck |

Separate the two when you alert. `refused` is the provider's problem: it shipped a login page, a stub, or a database that answers nothing, and the old file is still in place. `failed` is yours.

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

These four lookup names match descriptors a consumer already registers, so the series land in the group it has rather than beside it. Renaming one does not fail loudly: it records without a description and shows up as an unrelated series. Treat the names as a contract.

## Turning it all off

```toml
factbook = { version = "0.1", default-features = false, features = ["geoip"] }
```

That drops the facade dependency entirely. The call sites compile to nothing.
