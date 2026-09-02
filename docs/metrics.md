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

The `metrics` feature is in the default set. The age gauge is read once per database on every `ensure_databases` call, including the calls that download nothing, and the download counter only where a download was attempted.

What the gauge costs depends on the build. A default build takes the age from the publisher's build stamp, which means opening the database as a memory map and parsing its metadata. A `geoip-download`-only build has no reader to do that with, and `age_from_source: false` asks for the local write time instead; either of those costs one `stat`.

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_database_age_seconds` | gauge | `type`, `kind` | Seconds since the publisher built the database. The oldest file of a set, because a database is only as current as its stalest half |
| `enrichment_download_total` | counter | `type`, `kind`, `result` | One per download attempt, by how it ended |
| `enrichment_database_backing` | gauge | `type`, `kind`, `backing` | Whether an open database was read onto the heap or mapped. Both `backing` series are written, 1 for the chosen one and 0 for the other |
| `enrichment_table_backing` | gauge | `type`, `file`, `backing` | Where a loaded table source's rows are. One backing today, so it says that the table loaded and is being held in memory |

`kind` is `city` or `asn`. `type` is `geoip` on the database series and `table`
on the table one.

### The `result` label separates five outcomes

| value | means | what to do |
|---|---|---|
| `ok` | The download landed and replaced the file | nothing |
| `refused` | Bytes arrived and a check rejected them | the provider published something bad; the previous file is still being served |
| `failed` | Bytes never arrived | the network, the endpoint, or the credential |
| `busy` | Another process holds the lock on the file | expected where several processes share a data directory; sustained means one is stuck |
| `unentitled` | The credential was accepted and the database refused | the account has no claim on the edition selected; check the tier against the account's products, not the key |

Separate the two when you alert. `refused` is the provider's problem: it shipped a login page, a stub, or a database that answers nothing, and the old file is still in place. `failed` is yours.

## The two backing gauges say where the bytes are

Both are in the default set. `file` is the name a source is written under, which separates one configured table from another.

`enrichment_database_backing` is `resident` or `mapped`, and both series are written -- 1 for the chosen one and 0 for the other -- so a refresh that flips a database leaves none still reporting the backing it no longer has. A mapped database can stall a lookup on a page fault where a resident one cannot, so this is the series to read when lookup latency has a tail the cache does not explain.

`enrichment_table_backing` has one backing, `resident`, because a table source that will not fit in memory is refused rather than moved anywhere. Watch it for absence: the series appears when a table loads, so a source that has grown past `auto_download.resident_max_bytes` stops reporting entirely rather than reporting something different. Serving an oversized table from a converted database is issue #2, and a second backing arrives with it.

## Lookup, off by default

The `metrics-lookup` feature is opt-in, on measurement rather than principle: it costs three to four times a cache hit, and the hit path is the one the crate exists to make fast. Take it when you want cache behaviour visible and can afford it.

```console
cargo add factbook --features metrics-lookup
```

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_cache_hits_total` | counter | `type` | Addresses the cache already held |
| `enrichment_cache_misses_total` | counter | `type` | Addresses that cost a database read |
| `enrichment_cache_size` | gauge | `type` | Entries held, against the configured capacity |
| `enrichment_duration_seconds` | histogram | `type` | Time for one lookup, hit or miss |

Watch the hit percentage. If it is low against data you expect repeat hits on, raise `cache.capacity` -- the working set is not fitting.

If raising it does not move the percentage, the traffic is not repeating and the cache is not what to tune. Check `enrichment_duration_seconds` instead: a miss costs a database read, so a workload that is mostly misses is bounded by the reader rather than by cache size.

## Treat the four lookup names as a contract

The names are bare and each carries a `type` label, so one recorder hosts factbook beside other enrichers without the series colliding. Where a host has already described a metric under one of these names, that description attaches to what factbook records. Rename one in a fork and it records without a description, under a name nothing is watching.

## Turning it all off

```console
cargo add factbook --no-default-features --features geoip
```

That drops the facade dependency entirely. The call sites compile to nothing.
