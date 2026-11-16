<!-- Project:   factbook -->
<!-- File:      docs/metrics.md -->
<!-- Purpose:   Every metric emitted, and which feature emits it -->
<!-- License:   Apache-2.0 -->
<!-- Copyright: (c) 2026 HYPERI PTY LIMITED -->

# Metrics

Everything factbook emits, through the [`metrics`](https://crates.io/crates/metrics) facade. Nothing is recorded unless the host installs a recorder, so a consumer with no observability stack pays for the call and nothing else.

## Watch the database age above all else

`enrichment_database_age_seconds` is the one to alert on. The download counter says an attempt failed, and the age says the data is going stale, whatever the cause. Lookups keep returning records, they are just increasingly wrong.

Alert it against the provider's cadence: daily sources under a couple of days, monthly ones under a couple of months.

It counts from the timestamp the publisher stamped into the database, not from when the file landed here. Those differ by however long the copy sat published before anyone fetched it, and a monthly database downloaded this morning is not fresh data. Set `auto_download.age_from_source: false` for a publisher with a stamp that cannot be trusted, and the gauge falls back to time since the local write.

Freshness is a separate question and still counts from the local write, so a database with a build stamp older than the staleness window is not re-fetched on every run.

## Acquisition, on by default

The `metrics` feature is in the default set. The age gauge is set for each database an `ensure_databases` call returns, including the calls that download nothing. The download counter moves when a download was attempted. A table source emits neither of those two, and has its backing gauge alone.

What the gauge costs depends on the build. A default build takes the age from the publisher's build stamp, which means opening the database as a memory map and parsing its metadata. A `geoip-download`-only build has no reader to do that with, and `age_from_source: false` asks for the local write time instead. Either of those costs one `stat`.

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_database_age_seconds` | gauge | `type`, `kind` | Seconds since the publisher built the database. The oldest file of a set, because a database is as current as its stalest half |
| `enrichment_download_total` | counter | `type`, `kind`, `result` | One per download attempt, by how it ended |
| `enrichment_database_backing` | gauge | `type`, `kind`, `backing` | Whether an open database was read onto the heap or mapped. Both `backing` series are written, 1 for the chosen one and 0 for the other |
| `enrichment_table_backing` | gauge | `type`, `file`, `backing` | Where a loaded table source's rows are. There is one backing, so it says that the table loaded and is held in memory |

`kind` is `city` or `asn`. `type` is `geoip` on the database series and `table` on the table one.

The `result` label separates five outcomes:

| value | means | what to do |
|---|---|---|
| `ok` | The download landed and replaced the file | nothing |
| `refused` | Bytes arrived and a check rejected them | the provider published something bad, or a check the configuration sets refused it, and a copy on disk keeps serving |
| `failed` | Bytes never arrived | the network, the endpoint, or the credential |
| `busy` | Another process holds the lock on the file | expected where several processes share a data directory, and sustained means one is stuck |
| `unentitled` | The credential was accepted and the database refused | the account has no claim on the edition selected, so check the tier against the account's products, not the key |

Separate the two when you alert. `refused` is usually the provider's problem: it shipped a login page, a stub, a body past the 4 GiB ceiling, or a database that answers nothing. A size floor, a table schema or a resident ceiling the configuration sets can refuse a download too. `failed` is yours.

`cargo add factbook --no-default-features --features geoip` drops the facade dependency, and every call site compiles to nothing.

The two backing gauges say where the bytes are. `enrichment_database_backing` is `resident` or `mapped`, and both series are written, 1 for the chosen one and 0 for the other. So a refresh that flips a database leaves none still reporting the backing it no longer has. A mapped database can stall a lookup while a page is read from the file, where a resident one has the file in memory, so this is the series to read when lookup latency has a tail the cache does not explain.

`enrichment_table_backing` has the `file` a source is written under, and has one backing, `resident`, because a table source that will not fit in memory is refused rather than moved anywhere. The series is set when a table loads. A download that has grown past `auto_download.resident_max_bytes` is refused and the copy on disk keeps loading, so the gauge does not change: the refusal shows as a `warn` log line. Serving an oversized table from a converted database is not built, tracked in [issue #2](https://github.com/hyperi-io/factbook-rs/issues/2).

## Lookup, off by default

The `metrics-lookup` feature is opt-in: it costs three to four times a cache hit, and the hit path is the one the crate exists to make fast. Take it when you want cache behaviour visible and can afford it.

```console
cargo add factbook --features metrics-lookup
```

| metric | type | labels | what it says |
|---|---|---|---|
| `enrichment_cache_hits_total` | counter | `type` | Addresses the cache held |
| `enrichment_cache_misses_total` | counter | `type` | Addresses that cost a database read |
| `enrichment_cache_size` | gauge | `type` | Entries held, against the configured capacity |
| `enrichment_duration_seconds` | histogram | `type` | Time for one lookup, hit or miss |

Watch the hit percentage. If it is low against data you expect repeat hits on, raise `CacheConfig::capacity`, because the working set is not fitting. If raising it does not move the percentage, the traffic is not repeating and the cache is not what to tune. Check `enrichment_duration_seconds` instead: a miss costs a database read, so a workload that is mostly misses is bounded by the reader rather than by cache size.

Treat the four names as a contract. They are bare and each has a `type` label, so one recorder hosts factbook beside other enrichers without the series colliding. Rename one in a fork and it records under a name nothing is watching.
