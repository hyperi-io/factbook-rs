// Project:   factbook
// File:      benches/table_lookup.rs
// Purpose:   What a lookup costs on a resident table, which is the default path
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! What a lookup against a table held in memory costs.
//!
//! The resident backing is the default and the one nearly every source takes, so
//! it is the one that has to stay cheap. A row handed back from it carries the
//! cells and the column names it came from, and the whole point of measuring
//! here is that handing one back must not turn a hash and an index into an
//! allocation.
//!
//! # The draw is frequency-biased, not uniform
//!
//! Reference lookups are heavily skewed, so the draw is Zipf with exponent one
//! -- the key at rank `r` is drawn proportional to `1/r`. A uniform draw over
//! fifty thousand keys measures a cache-miss pattern no deployment has.
//!
//! # Every benchmark reads a field out of the row
//!
//! A row that is never read prices only the index. Reading one field is the
//! smallest honest unit of work, and it is what a consumer does with the row it
//! asked for.
//!
//! # The query group is the index against the walk
//!
//! A condition object can be answered two ways: narrowed by the index to the
//! rows carrying the key, or walked over every row in the table. Both are
//! measured on the same fifty thousand rows, at the same selectivity -- one
//! matching row either way -- so the difference between them is the index and
//! nothing else.

use std::hint::black_box;
use std::sync::LazyLock;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use factbook::table::{Condition, Index, Query, Schema, Table, TableFormat};

/// Keys the table holds, which is the size an ASN- or country-keyed side table
/// runs to.
const KEYS: u32 = 50_000;

/// Keys drawn from, a power of two so the cursor wraps with a mask.
const PROBE_COUNT: usize = 8192;

/// Mask that wraps the probe cursor.
const PROBE_MASK: usize = PROBE_COUNT - 1;

/// The lowest key the table holds, which is also the hottest.
const FIRST_KEY: u32 = 64_500;

/// A resident table and the keys it is asked about.
struct Fixture {
    /// The table itself, held in memory.
    table: Table,

    /// Keys to ask for, drawn frequency-biased.
    probe: Vec<String>,

    /// Operator names of the same draw, which no index reaches.
    named: Vec<String>,

    /// A key the table does not hold.
    absent: String,
}

/// Built once: parsing fifty thousand rows is not what is being measured.
static FIXTURE: LazyLock<Fixture> = LazyLock::new(build);

/// The table, and the keys it will be asked about.
fn build() -> Fixture {
    use std::fmt::Write as _;

    let mut csv = String::from("asn,name,country\n");
    for at in 0..KEYS {
        let key = FIRST_KEY + at;
        let _ = writeln!(csv, "{key},OPERATOR-{key},AU");
    }

    let table = Table::from_reader(
        csv.as_bytes(),
        TableFormat::Csv { header: true },
        &Schema::Auto,
        &Index::Column("asn".to_string()),
    )
    .expect("the benchmark table has to build");
    assert_eq!(table.len(), KEYS as usize);

    // Zipf with exponent one over the key ranks, so rank 1 takes about a tenth
    // of the draw and the tail is still reached.
    let mut probe = Vec::with_capacity(PROBE_COUNT);
    let mut rank = 1u32;
    while probe.len() < PROBE_COUNT {
        let repeats = (PROBE_COUNT / (rank as usize * 12)).max(1);
        for _ in 0..repeats {
            if probe.len() == PROBE_COUNT {
                break;
            }
            probe.push((FIRST_KEY + rank - 1).to_string());
        }
        rank = (rank % KEYS) + 1;
    }

    // The same rows by the column nothing is indexed on, so a query for one of
    // these has to walk the table to find the row the key would have named.
    let named = probe.iter().map(|key| format!("OPERATOR-{key}")).collect();

    Fixture {
        table,
        probe,
        named,
        absent: "1".to_string(),
    }
}

/// One key at a time, the shape a per-event transform takes.
fn resident(c: &mut Criterion) {
    let fixture = &*FIXTURE;
    assert!(fixture.table.get(&fixture.probe[0]).is_some());
    assert!(fixture.table.get(&fixture.absent).is_none());

    let mut group = c.benchmark_group("resident");

    // The hottest key, over and over: the index lookup and the row hand-back
    // with nothing else moving.
    group.bench_function("get_hottest", |b| {
        let key = FIRST_KEY.to_string();
        b.iter(|| {
            black_box(
                fixture
                    .table
                    .get(black_box(&key))
                    .map(|row| row.at(1).map(str::len)),
            )
        });
    });

    group.bench_function("get_zipf", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            black_box(
                fixture
                    .table
                    .get(black_box(key))
                    .map(|row| row.at(1).map(str::len)),
            )
        });
    });

    // By name rather than by position, which is what a consumer writes and which
    // adds the column scan on top of the hand-back.
    group.bench_function("get_zipf_by_name", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            black_box(
                fixture
                    .table
                    .get(black_box(key))
                    .map(|row| row.get("country").map(str::len)),
            )
        });
    });

    group.bench_function("absent", |b| {
        b.iter(|| {
            black_box(
                fixture
                    .table
                    .get(black_box(&fixture.absent))
                    .map(|row| row.at(1).map(str::len)),
            )
        });
    });

    // Every row filed under one key, which is the iterator rather than the
    // single hand-back.
    group.bench_function("all_zipf", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            black_box(
                fixture
                    .table
                    .all(black_box(key))
                    .map(|row| row.at(1).map(str::len))
                    .last(),
            )
        });
    });

    group.finish();
}

/// A condition object, which is what a VRL host asks with.
fn query(c: &mut Criterion) {
    let fixture = &*FIXTURE;

    let mut group = c.benchmark_group("query");

    // One equality on the indexed column: the index names the row and the walk
    // is one position long.
    group.bench_function("indexed", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            let conditions = [Condition::Equals {
                column: "asn",
                value: key,
            }];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)))
                    .map(|matched| matched.row().at(1).map(str::len))
                    .last(),
            )
        });
    });

    // The same question about the same row, asked by a column no index reaches:
    // every row in the table is tested. This is what the index is measured
    // against.
    group.bench_function("walked", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let name = &fixture.named[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            let conditions = [Condition::Equals {
                column: "name",
                value: name,
            }];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)))
                    .map(|matched| matched.row().at(1).map(str::len))
                    .last(),
            )
        });
    });

    // Folding case takes the identical indexed query off the index, so this
    // prices the same condition on the walk.
    group.bench_function("indexed_folded", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            let conditions = [Condition::Equals {
                column: "asn",
                value: key,
            }];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)).case_sensitive(false))
                    .map(|matched| matched.row().at(1).map(str::len))
                    .last(),
            )
        });
    });

    // A second condition on a column outside the index, which every candidate
    // the index hands over is still tested against.
    group.bench_function("indexed_two_conditions", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            let conditions = [
                Condition::Equals {
                    column: "asn",
                    value: key,
                },
                Condition::Equals {
                    column: "country",
                    value: "AU",
                },
            ];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)))
                    .map(|matched| matched.row().at(1).map(str::len))
                    .last(),
            )
        });
    });

    // A wildcard adds its own bucket to the walk rather than taking the query
    // off the index.
    group.bench_function("indexed_wildcard", |b| {
        let mut cursor = 0usize;
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            let conditions = [Condition::Equals {
                column: "asn",
                value: key,
            }];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)).wildcard("*"))
                    .map(|matched| matched.row().at(1).map(str::len))
                    .last(),
            )
        });
    });

    // What a host actually does with a match: copy the columns it asked for out
    // of the row.
    group.bench_function("indexed_selected", |b| {
        let mut cursor = 0usize;
        let select = ["name", "country"];
        b.iter(|| {
            let key = &fixture.probe[cursor];
            cursor = (cursor + 1) & PROBE_MASK;
            let conditions = [Condition::Equals {
                column: "asn",
                value: key,
            }];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)).select(&select))
                    .map(|matched| {
                        matched
                            .fields()
                            .map(|(_, value)| value.map(str::len))
                            .last()
                    })
                    .last(),
            )
        });
    });

    group.bench_function("absent", |b| {
        b.iter(|| {
            let conditions = [Condition::Equals {
                column: "asn",
                value: &fixture.absent,
            }];
            black_box(
                fixture
                    .table
                    .find(Query::new(black_box(&conditions)))
                    .map(|matched| matched.row().at(1).map(str::len))
                    .last(),
            )
        });
    });

    group.finish();
}

/// Short samples and short measurement windows, so a full run finishes in
/// minutes rather than the quarter hour criterion's defaults would take.
fn configured() -> Criterion {
    Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3))
}

criterion_group! {
    name = benches;
    config = configured();
    targets = resident, query
}
criterion_main!(benches);
