// Project:   factbook
// File:      benches/geoip_lookup.rs
// Purpose:   What a lookup costs on the two shapes the consumers run
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! What a lookup costs, on this crate rather than on a dependency.
//!
//! Two consumer shapes are measured. A batching loader flushes twenty thousand
//! rows through [`GeoIp::lookup_many`], and an expression transform calls
//! [`GeoIp::lookup`] once per event with no batching.
//!
//! # The addresses are drawn from the committed fixtures
//!
//! Every probe address is taken from a network the fixture databases actually
//! hold, and the harness asserts that before any timing runs, because a probe
//! the database does not hold would turn the hit-path benchmark into a second
//! miss-path benchmark.
//!
//! # The draw is frequency-biased, not uniform
//!
//! Address traffic is heavily skewed, so the draw is Zipf with exponent one --
//! the address at rank `r` is drawn proportional to `1/r`. A uniform draw would
//! measure a cache working set no deployment has.

use std::collections::HashSet;
use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use factbook::geoip::{CacheConfig, DatabasePaths, GeoIp};

/// A database in MaxMind's City schema, built by scripts/make_fixtures.py.
const CITY_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/city-test.mmdb");

/// A database in MaxMind's ASN schema, built by scripts/make_fixtures.py.
const ASN_DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/asn-test.mmdb");

/// Every IPv4 network the committed city fixture holds, base address and prefix.
///
/// Enumerated from the file itself rather than taken from MaxMind's
/// documentation, and the harness re-checks each one resolves before timing.
const LOCATED_NETWORKS: [(&str, u32); 12] = [
    ("2.125.160.216", 29),
    ("67.43.156.0", 24),
    ("81.2.69.142", 31),
    ("81.2.69.144", 28),
    ("81.2.69.160", 27),
    ("81.2.69.192", 28),
    ("89.160.20.112", 28),
    ("89.160.20.128", 25),
    ("175.16.199.0", 24),
    ("202.196.224.0", 20),
    ("214.78.0.0", 17),
    ("216.160.83.56", 29),
];

/// Base of the range the negative answers are drawn from.
///
/// `198.18.0.0/15` is benchmarking space: routable as far as the crate's
/// reserved-address check is concerned, and held by neither fixture database.
const ABSENT_BASE: &str = "198.18.0.0";

/// Addresses in the frequency-biased pool.
const POOL_SIZE: usize = 4096;

/// One pool slot in this many is an address no database holds.
const ABSENT_SHARE: usize = 4;

/// Rows in the batch, the size a batching loader flushes.
const BATCH_ROWS: usize = 20_000;

/// Addresses read per cold-read iteration, enough to swamp the timer.
const COLD_READS: usize = 256;

/// Hottest ranks whose share of the batch the harness reports.
const HEAD_RANKS: usize = 8;

/// Draws in the per-event probe sequence, a power of two so the cursor masks.
const PROBE_LEN: usize = 4096;

/// Mask that wraps the per-event probe cursor.
const PROBE_MASK: usize = PROBE_LEN - 1;

/// Reserved addresses cycled through by the short-circuit benchmark.
const RESERVED_LEN: usize = 8;

/// One address from each reserved range the short-circuit answers.
const RESERVED: [&str; RESERVED_LEN] = [
    "10.0.0.1",
    "172.16.5.4",
    "192.168.1.1",
    "127.0.0.1",
    "169.254.10.20",
    "100.64.0.1",
    "240.0.0.1",
    "fd12:3456:789a::1",
];

/// Mask that wraps the reserved probe cursor.
const RESERVED_MASK: usize = RESERVED_LEN - 1;

/// Fixed seed, so the pool and the batch are the same on every run.
const SEED: u64 = 0x0fac_7b00_0000_0001;

/// Numerator of the Zipf weight, large enough that rank 4096 still weighs more
/// than one.
const ZIPF_SCALE: u64 = 1 << 32;

/// SplitMix64, a deterministic generator with no dependency behind it.
struct SplitMix64(u64);

impl SplitMix64 {
    /// Seed the generator.
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Next value in the sequence.
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// Cumulative weights of a Zipf distribution over the pool ranks.
///
/// The weights are integers, so no float conversion sits between the draw and
/// the rank it selects.
struct Zipf {
    /// Inclusive prefix sums of the per-rank weights.
    cumulative: Vec<u64>,
}

impl Zipf {
    /// Build the table for `ranks` ranks, weighting rank `r` as `1/r`.
    fn new(ranks: usize) -> Self {
        let mut cumulative = Vec::with_capacity(ranks);
        let mut running = 0u64;
        for rank in 1..=ranks {
            running += ZIPF_SCALE / u64::try_from(rank).unwrap();
            cumulative.push(running);
        }
        Self { cumulative }
    }

    /// Draw one rank, as an index into the pool.
    fn draw(&self, rng: &mut SplitMix64) -> usize {
        let total = *self.cumulative.last().unwrap();
        let target = rng.next_u64() % total;
        self.cumulative.partition_point(|&edge| edge <= target)
    }
}

/// Everything the benchmarks read, built and checked once.
struct Fixture {
    /// The enricher under measurement.
    geoip: GeoIp,
    /// Pool addresses in rank order, rank 1 first.
    pool: Vec<IpAddr>,
    /// The rank-1 address, the one a repeat-hit benchmark uses.
    hottest: IpAddr,
    /// Zipf draws over the pool, the per-event probe sequence.
    probe: Vec<IpAddr>,
    /// Zipf draws over the pool, one loader flush.
    batch: Vec<IpAddr>,
    /// Distinct addresses in that batch, the count dedup collapses it to.
    batch_distinct: usize,
    /// Located addresses read cold, one per element of a cold-read iteration.
    cold_located: Vec<IpAddr>,
    /// Absent addresses read cold, the negative-answer half.
    cold_absent: Vec<IpAddr>,
    /// Reserved addresses, parsed once.
    reserved: [IpAddr; RESERVED_LEN],
}

impl Fixture {
    /// Open the fixtures, build the distribution and verify every probe.
    fn build() -> Self {
        let geoip = GeoIp::open(
            DatabasePaths {
                city: Some(Path::new(CITY_DB)),
                asn: Some(Path::new(ASN_DB)),
            },
            CacheConfig::default(),
        )
        .expect("the committed fixture databases open");

        let absent_count = POOL_SIZE / ABSENT_SHARE;
        let located = located_addresses(POOL_SIZE - absent_count);
        let absent = absent_addresses(absent_count);
        let pool = interleave(&located, &absent);

        let mut rng = SplitMix64::new(SEED);
        let zipf = Zipf::new(POOL_SIZE);
        let probe: Vec<IpAddr> = (0..PROBE_LEN).map(|_| pool[zipf.draw(&mut rng)]).collect();
        let batch: Vec<IpAddr> = (0..BATCH_ROWS).map(|_| pool[zipf.draw(&mut rng)]).collect();
        let batch_distinct = batch.iter().collect::<HashSet<_>>().len();

        let reserved = RESERVED.map(|literal| literal.parse().expect("a reserved literal parses"));

        let fixture = Self {
            hottest: pool[0],
            cold_located: located[..COLD_READS].to_vec(),
            cold_absent: absent[..COLD_READS].to_vec(),
            geoip,
            pool,
            probe,
            batch,
            batch_distinct,
            reserved,
        };
        fixture.verify(&located, &absent);
        fixture.report();
        fixture
    }

    /// Fail before any timing runs if a probe address is not what it claims.
    ///
    /// A located address that stopped resolving would silently turn the hit-path
    /// benchmark into a miss-path one, which is the failure this rules out.
    fn verify(&self, located: &[IpAddr], absent: &[IpAddr]) {
        self.geoip.clear_cache();

        for &ip in located {
            assert!(
                self.geoip.lookup(ip).is_some(),
                "{ip} is not held by the fixture databases"
            );
        }
        for &ip in absent {
            assert!(
                self.geoip.lookup(ip).is_none(),
                "{ip} is held by a fixture database and cannot serve as a miss"
            );
        }
        for ip in self.reserved {
            assert!(
                self.geoip.lookup(ip).expect("reserved answers").is_private,
                "{ip} was not answered as reserved"
            );
        }

        // Reserved traffic reaching the cache would show up here as extra
        // entries, so this is the assertion behind the short-circuit claim.
        assert_eq!(
            self.geoip.cached_entries(),
            located.len() + absent.len(),
            "reserved addresses spent cache entries"
        );
        self.geoip.clear_cache();
    }

    /// Print what the distribution came out as, so the numbers can be read.
    fn report(&self) {
        let head = &self.pool[..HEAD_RANKS];
        let head_rows = self.batch.iter().filter(|ip| head.contains(ip)).count();
        println!(
            "fixture: pool={POOL_SIZE} located={} absent={} batch={BATCH_ROWS} \
             distinct={} top{HEAD_RANKS}_share={}%",
            POOL_SIZE - POOL_SIZE / ABSENT_SHARE,
            POOL_SIZE / ABSENT_SHARE,
            self.batch_distinct,
            head_rows * 100 / BATCH_ROWS,
        );
    }

    /// Load every pool address into the cache, so a hit benchmark measures hits.
    fn warm(&self) {
        for &ip in &self.pool {
            drop(self.geoip.lookup(ip));
        }
        assert_eq!(
            self.geoip.cached_entries(),
            POOL_SIZE,
            "the pool does not fit the cache, so a hit benchmark would be measuring evictions"
        );
    }
}

/// The fixture, shared by every group so one cache serves the whole run.
static FIXTURE: LazyLock<Fixture> = LazyLock::new(Fixture::build);

/// Distinct addresses taken round-robin from the fixture's own networks.
///
/// Round-robin rather than one big network, so the pool spans prefixes from /17
/// to /31 and the tree walk is not one fixed depth.
fn located_addresses(count: usize) -> Vec<IpAddr> {
    let networks: Vec<(u32, u32)> = LOCATED_NETWORKS
        .iter()
        .map(|&(base, prefix)| {
            let base: Ipv4Addr = base.parse().expect("a fixture network literal parses");
            (base.to_bits(), 1u32 << (32 - prefix))
        })
        .collect();

    let mut out = Vec::with_capacity(count);
    let mut host = 0u32;
    loop {
        let before = out.len();
        for &(base, size) in &networks {
            if host < size {
                out.push(IpAddr::V4(Ipv4Addr::from_bits(base + host)));
                if out.len() == count {
                    return out;
                }
            }
        }
        assert!(
            out.len() > before,
            "the fixture networks hold fewer than {count} addresses"
        );
        host += 1;
    }
}

/// Distinct addresses no database holds, taken in sequence from `198.18.0.0/15`.
fn absent_addresses(count: usize) -> Vec<IpAddr> {
    let base: Ipv4Addr = ABSENT_BASE.parse().expect("the absent base literal parses");
    (0..count)
        .map(|index| {
            IpAddr::V4(Ipv4Addr::from_bits(
                base.to_bits() + u32::try_from(index).unwrap(),
            ))
        })
        .collect()
}

/// Lay the two sets out in rank order, absent addresses in every fourth slot.
///
/// The hottest ranks are located, which is the real shape: the addresses a feed
/// repeats are its own egress and the CDNs in front of it, and a database holds
/// those.
fn interleave(located: &[IpAddr], absent: &[IpAddr]) -> Vec<IpAddr> {
    let mut pool = Vec::with_capacity(located.len() + absent.len());
    let mut next_located = 0;
    let mut next_absent = 0;
    for slot in 0..located.len() + absent.len() {
        if slot % ABSENT_SHARE == ABSENT_SHARE - 1 {
            pool.push(absent[next_absent]);
            next_absent += 1;
        } else {
            pool.push(located[next_located]);
            next_located += 1;
        }
    }
    pool
}

/// One address at a time, the shape a per-event transform takes.
fn per_event(c: &mut Criterion) {
    let fixture = &*FIXTURE;
    fixture.warm();

    let mut group = c.benchmark_group("per_event");

    group.bench_function("hit_hottest", |b| {
        let ip = fixture.hottest;
        b.iter(|| black_box(fixture.geoip.lookup(black_box(ip))));
    });

    group.bench_function("hit_zipf", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let ip = fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            black_box(fixture.geoip.lookup(black_box(ip)))
        });
    });

    // The same hit, plus the deep copy an owned return would have to make, which
    // is what prices the decision to hand back the `Arc`.
    group.bench_function("hit_zipf_owned", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let ip = fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            black_box(
                fixture
                    .geoip
                    .lookup(black_box(ip))
                    .map(|record| (*record).clone()),
            )
        });
    });

    group.bench_function("reserved", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let ip = fixture.reserved[cursor];
            cursor = (cursor + 1) & RESERVED_MASK;
            black_box(fixture.geoip.lookup(black_box(ip)))
        });
    });

    group.finish();
}

/// Addresses the cache has never held, so every read reaches the database.
fn cold_read(c: &mut Criterion) {
    let fixture = &*FIXTURE;

    let mut group = c.benchmark_group("cold_read");
    group.throughput(Throughput::Elements(u64::try_from(COLD_READS).unwrap()));

    group.bench_function("located", |b| {
        b.iter_batched(
            || fixture.geoip.clear_cache(),
            |()| {
                for &ip in &fixture.cold_located {
                    black_box(fixture.geoip.lookup(black_box(ip)));
                }
            },
            BatchSize::PerIteration,
        );
    });

    // The negative answer costs both traversals and builds no record, so it is
    // the cheaper half of the miss path rather than the same measurement twice.
    group.bench_function("absent", |b| {
        b.iter_batched(
            || fixture.geoip.clear_cache(),
            |()| {
                for &ip in &fixture.cold_absent {
                    black_box(fixture.geoip.lookup(black_box(ip)));
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

/// One loader flush, batched against the same rows one at a time.
fn batch(c: &mut Criterion) {
    let fixture = &*FIXTURE;

    let mut group = c.benchmark_group("batch_20k");
    group.throughput(Throughput::Elements(u64::try_from(BATCH_ROWS).unwrap()));
    group.measurement_time(Duration::from_secs(5));

    fixture.warm();

    group.bench_function("lookup_many_warm", |b| {
        b.iter(|| black_box(fixture.geoip.lookup_many(black_box(&fixture.batch))));
    });

    group.bench_function("lookup_loop_warm", |b| {
        b.iter(|| {
            black_box(
                fixture
                    .batch
                    .iter()
                    .map(|&ip| fixture.geoip.lookup(ip))
                    .collect::<Vec<_>>(),
            )
        });
    });

    group.bench_function("lookup_many_cold", |b| {
        b.iter_batched(
            || fixture.geoip.clear_cache(),
            |()| black_box(fixture.geoip.lookup_many(&fixture.batch)),
            BatchSize::PerIteration,
        );
    });

    group.bench_function("lookup_loop_cold", |b| {
        b.iter_batched(
            || fixture.geoip.clear_cache(),
            |()| {
                black_box(
                    fixture
                        .batch
                        .iter()
                        .map(|&ip| fixture.geoip.lookup(ip))
                        .collect::<Vec<_>>(),
                )
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

/// Short samples and short measurement windows, so a full run finishes in
/// minutes rather than the quarter hour criterion's defaults would take.
///
/// The group macro applies the command line on top of this, so `--sample-size`
/// still overrides it.
fn configured() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
}

criterion_group! {
    name = benches;
    config = configured();
    targets = per_event, cold_read, batch
}
criterion_main!(benches);
