// Project:   factbook
// File:      src/table/query.rs
// Purpose:   Rows reached by several conditions at once, through the index
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Rows reached by a condition object rather than by one key.
//!
//! Enrichment asks one question: what is filed under this key. [`Table::get`]
//! and [`Table::all`] answer it in a hash lookup and a hand-back, and that is
//! the common case.
//!
//! A VRL host asks a different question. Its condition is an object -- country
//! is `AU`, status is `active`, and the licence had not expired on the day the
//! event happened -- and no single key reaches that. [`Table::find`] takes the
//! whole object.
//!
//! # It narrows before it tests
//!
//! Every condition has to hold, so any one of them that reaches the index
//! decides which rows are worth testing at all. An equality on the column the
//! source is indexed by does that: the index answers with the rows carrying
//! that value, and only those rows go through the rest of the conditions.
//!
//! A condition set that cannot be narrowed walks the table in file order. Two
//! things put a query on that path -- a case-insensitive comparison, which the
//! index cannot answer because it is keyed on exact text, and a condition set
//! that says nothing about the indexed column.
//!
//! # A wildcard is data, not syntax
//!
//! [`Query::wildcard`] names a value that stands for "any". It is a cell value
//! rather than a pattern: a row whose `country` is `*` answers a query for
//! `country == NZ` when the wildcard is `*`. Nothing is matched by prefix or by
//! glob, and the wildcard costs the index nothing -- it is a second bucket, not
//! a scan.
//!
//! # Dates are parsed from the cell
//!
//! A table holds text, so a date condition parses the cell it lands on. RFC
//! 3339 (`2026-07-01T00:00:00Z`) and a bare `2026-07-01`, read as midnight UTC,
//! are what parse; a cell that is neither never satisfies a date condition.
//!
//! # Example
//!
//! ```
//! use factbook::table::{Condition, Index, Query, Schema, Table, TableFormat};
//!
//! let csv = "country,city,status\nAU,Sydney,active\nAU,Perth,retired\n";
//! let table = Table::from_reader(
//!     csv.as_bytes(),
//!     TableFormat::Csv { header: true },
//!     &Schema::Auto,
//!     &Index::Column("country".to_string()),
//! )?;
//!
//! let conditions = [
//!     Condition::Equals { column: "country", value: "AU" },
//!     Condition::Equals { column: "status", value: "active" },
//! ];
//!
//! let cities: Vec<_> = table
//!     .find(Query::new(&conditions))
//!     .map(|matched| matched.row().get("city"))
//!     .collect();
//!
//! assert_eq!(cities, [Some("Sydney")]);
//! # Ok::<(), factbook::table::TableError>(())
//! ```

use std::ops::Range;
use std::slice;
use std::time::SystemTime;

use chrono::{DateTime, NaiveDate};

use super::{Cell, Keys, NO_ROWS, Row, Table, as_address, as_prefix, cell};

/// Lists of positions an index narrows to: the value, the wildcard, and the
/// rows the index could not file.
const LISTS: usize = 3;

/// A query narrowed to nothing, which is what an unknown column leaves.
const NOTHING: [&[usize]; LISTS] = [NO_ROWS; LISTS];

/// One rule a row has to satisfy.
///
/// Every rule in a [`Query`] has to hold at once, so a condition set is an AND
/// and there is no way to write an OR. That is deliberate: the enrichment hosts
/// this serves take a condition object, and an object is a conjunction.
///
/// [`Equals`](Self::Equals) compares the text of a cell. An address-keyed
/// source still compares text here -- `1.1.1.1` and `::ffff:1.1.1.1` are two
/// values, not one -- and [`Table::get_by_address`] is the lookup that knows
/// they are the same address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Condition<'a> {
    /// The column holds this text, or the query's wildcard.
    Equals {
        /// Column the rule tests.
        column: &'a str,

        /// Text the column has to hold.
        value: &'a str,
    },

    /// The column holds a date inside `[from, to]`, both ends included.
    BetweenDates {
        /// Column the rule tests.
        column: &'a str,

        /// Earliest date that satisfies it.
        from: SystemTime,

        /// Latest date that satisfies it.
        to: SystemTime,
    },

    /// The column holds a date at or after `from`.
    FromDate {
        /// Column the rule tests.
        column: &'a str,

        /// Earliest date that satisfies it.
        from: SystemTime,
    },

    /// The column holds a date at or before `to`.
    ToDate {
        /// Column the rule tests.
        column: &'a str,

        /// Latest date that satisfies it.
        to: SystemTime,
    },
}

impl<'a> Condition<'a> {
    /// Column this rule tests.
    #[must_use]
    pub const fn column(&self) -> &'a str {
        match *self {
            Self::Equals { column, .. }
            | Self::BetweenDates { column, .. }
            | Self::FromDate { column, .. }
            | Self::ToDate { column, .. } => column,
        }
    }
}

/// What a lookup asks of a table.
///
/// The conditions are the question; the rest are settings on how they are
/// compared and how much of a matched row is handed back.
///
/// An empty condition set matches every row, which is what a host asking for a
/// whole table with a projection wants.
#[derive(Clone, Copy, Debug)]
pub struct Query<'a> {
    /// Rules a row has to satisfy, all of them.
    conditions: &'a [Condition<'a>],

    /// Whether text is compared exactly.
    case_sensitive: bool,

    /// Cell value that satisfies any equality rule.
    wildcard: Option<&'a str>,

    /// Columns a matched row hands back, or every column.
    select: Option<&'a [&'a str]>,
}

impl<'a> Query<'a> {
    /// A query over `conditions`, compared exactly, over every column.
    #[must_use]
    pub const fn new(conditions: &'a [Condition<'a>]) -> Self {
        Self {
            conditions,
            case_sensitive: true,
            wildcard: None,
            select: None,
        }
    }

    /// Whether text is compared exactly. Exact by default.
    ///
    /// Folding case costs the index: it is keyed on the bytes the source wrote,
    /// so a folded comparison walks the table instead.
    #[must_use]
    pub const fn case_sensitive(mut self, exactly: bool) -> Self {
        self.case_sensitive = exactly;
        self
    }

    /// Value a cell can hold to satisfy any equality rule.
    #[must_use]
    pub const fn wildcard(mut self, value: &'a str) -> Self {
        self.wildcard = Some(value);
        self
    }

    /// Columns a matched row hands back, in the order named.
    ///
    /// A name the table has not got is dropped rather than reported, the same
    /// as the enrichment hosts do. Only [`Matched::fields`] reads this --
    /// [`Matched::row`] is the whole row either way.
    #[must_use]
    pub const fn select(mut self, columns: &'a [&'a str]) -> Self {
        self.select = Some(columns);
        self
    }
}

impl Table {
    /// Every row satisfying all of a query's conditions, in file order.
    ///
    /// An equality on the indexed column narrows the walk to the rows carrying
    /// that value; anything else walks the table. Either way every condition is
    /// tested against every row that comes back, because a condition on a
    /// column outside the index is not encoded in the key.
    ///
    /// A condition naming a column the table has not got matches nothing at
    /// all, which is a typo behaving like an empty result for the life of the
    /// process. [`unknown_columns`](Self::unknown_columns) is how a host turns
    /// that into an error while it is still loading its config.
    #[must_use]
    pub fn find<'q>(&self, query: Query<'q>) -> Matches<'_, 'q> {
        // One column the table has not got takes the whole condition set with
        // it: everything has to hold, and that one never can.
        let resolved: Option<Vec<usize>> = query
            .conditions
            .iter()
            .map(|condition| self.position_of(condition.column()))
            .collect();

        let (plan, walk) = match resolved {
            None => (Vec::new(), Walk::nothing()),
            Some(plan) => {
                let walk = self.narrow(&query, &plan).map_or_else(
                    || Walk::All(0..self.rows.len()),
                    |lists| Walk::Listed { lists, next: 0 },
                );
                (plan, walk)
            }
        };

        Matches {
            table: self,
            query,
            plan,
            walk,
        }
    }

    /// Condition columns this table has not got, sorted.
    ///
    /// Empty is the answer a host wants at config load. A condition on a column
    /// the source does not have cannot match, so a misspelt column is a lookup
    /// that quietly answers nothing rather than a fault anyone sees.
    ///
    /// The [`select`](Query::select) list is not checked here: a projection
    /// naming a column the table has not got drops that name, which is what the
    /// enrichment hosts do with it.
    #[must_use]
    pub fn unknown_columns<'q>(&self, query: Query<'q>) -> Vec<&'q str> {
        let mut unknown: Vec<&str> = query
            .conditions
            .iter()
            .map(Condition::column)
            .filter(|column| self.position_of(column).is_none())
            .collect();

        unknown.sort_unstable();
        unknown.dedup();
        unknown
    }

    /// Position of a column, by name.
    pub(super) fn position_of(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|name| name == column)
    }

    /// Rows worth testing, when the index can name them.
    ///
    /// The lists are a superset of the answer, never the answer itself: they
    /// narrow the walk and every row in them still goes through
    /// [`satisfies`](Self::satisfies). That is what makes the address and
    /// prefix cases safe -- a row whose key cell did not parse is not in the
    /// map at all, so it rides along in its own list rather than being lost.
    fn narrow<'t>(&'t self, query: &Query<'_>, plan: &[usize]) -> Option<[&'t [usize]; LISTS]> {
        if !query.case_sensitive {
            return None;
        }

        // The source states one key column, so only an equality on that column
        // reaches the index.
        let value =
            query
                .conditions
                .iter()
                .zip(plan)
                .find_map(|(condition, &at)| match *condition {
                    Condition::Equals { value, .. } if at == self.key_column => Some(value),
                    _ => None,
                })?;

        let wildcard = query.wildcard;

        Some(match &self.keys {
            Keys::Text(filed) => [
                filed.get(value).map_or(NO_ROWS, Vec::as_slice),
                // Skipped when it is the value again, which would hand every
                // row in that bucket back twice.
                wildcard
                    .filter(|w| *w != value)
                    .and_then(|w| filed.get(w))
                    .map_or(NO_ROWS, Vec::as_slice),
                NO_ROWS,
            ],

            Keys::Address { filed, unindexed } => {
                let key = as_address(value);
                [
                    key.and_then(|address| filed.get(&address))
                        .map_or(NO_ROWS, Vec::as_slice),
                    wildcard
                        .and_then(as_address)
                        .filter(|address| Some(*address) != key)
                        .and_then(|address| filed.get(&address))
                        .map_or(NO_ROWS, Vec::as_slice),
                    unindexed,
                ]
            }

            Keys::Prefix { filed, unindexed } => {
                let key = as_prefix(value);
                [
                    key.and_then(|prefix| filed.get(&prefix))
                        .map_or(NO_ROWS, Vec::as_slice),
                    wildcard
                        .and_then(as_prefix)
                        .filter(|prefix| Some(*prefix) != key)
                        .and_then(|prefix| filed.get(&prefix))
                        .map_or(NO_ROWS, Vec::as_slice),
                    unindexed,
                ]
            }
        })
    }

    /// Whether the row at `at` satisfies every condition.
    ///
    /// `plan` holds each condition's column position, resolved once per query
    /// rather than per row.
    fn satisfies(&self, at: usize, query: &Query<'_>, plan: &[usize]) -> bool {
        let cells: &[Cell] = &self.rows[at];

        query
            .conditions
            .iter()
            .zip(plan)
            .all(|(condition, &column)| {
                // A cell the source did not supply satisfies nothing, including
                // an equality against empty text.
                let Some(text) = cell(cells, column) else {
                    return false;
                };

                match *condition {
                    Condition::Equals { value, .. } => {
                        equal(text, value, query.case_sensitive)
                            || query
                                .wildcard
                                .is_some_and(|w| equal(text, w, query.case_sensitive))
                    }
                    Condition::BetweenDates { from, to, .. } => {
                        as_date(text).is_some_and(|when| from <= when && when <= to)
                    }
                    Condition::FromDate { from, .. } => {
                        as_date(text).is_some_and(|when| from <= when)
                    }
                    Condition::ToDate { to, .. } => as_date(text).is_some_and(|when| when <= to),
                }
            })
    }
}

/// Rows a query walks.
#[derive(Debug)]
enum Walk<'a> {
    /// Every row, in file order.
    All(Range<usize>),

    /// The lists the index narrowed to, walked in order.
    Listed {
        /// Positions still to walk, per list.
        lists: [&'a [usize]; LISTS],

        /// List the walk is on.
        next: usize,
    },
}

impl Walk<'_> {
    /// A walk over no rows at all.
    const fn nothing() -> Self {
        Self::Listed {
            lists: NOTHING,
            next: 0,
        }
    }

    /// The next position to test.
    fn pop(&mut self) -> Option<usize> {
        match self {
            Self::All(all) => all.next(),
            Self::Listed { lists, next } => {
                while *next < LISTS {
                    if let Some((first, rest)) = lists[*next].split_first() {
                        lists[*next] = rest;
                        return Some(*first);
                    }
                    *next += 1;
                }
                None
            }
        }
    }

    /// How many positions are left to test, which is the most that can match.
    fn remaining(&self) -> usize {
        match self {
            Self::All(all) => all.len(),
            Self::Listed { lists, next } => lists[*next..].iter().map(|list| list.len()).sum(),
        }
    }
}

/// Rows a query matched, in file order.
#[derive(Debug)]
pub struct Matches<'t, 'q> {
    /// Table the rows belong to.
    table: &'t Table,

    /// Question being asked.
    query: Query<'q>,

    /// Column position of each condition, resolved once.
    plan: Vec<usize>,

    /// Rows still to test.
    walk: Walk<'t>,
}

impl<'t, 'q> Iterator for Matches<'t, 'q> {
    type Item = Matched<'t, 'q>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(at) = self.walk.pop() {
            if self.table.satisfies(at, &self.query, &self.plan) {
                return Some(Matched {
                    row: self.table.row(at),
                    select: self.query.select,
                });
            }
        }
        None
    }

    /// Lower bound zero: what is left to walk is what is left to test, and a
    /// test can refuse all of it.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.walk.remaining()))
    }
}

/// One row a query matched, and the columns it was asked for.
///
/// The row is borrowed from the table, so it costs a pointer pair rather than a
/// copy of the cells.
#[derive(Clone, Copy, Debug)]
pub struct Matched<'t, 'q> {
    /// The row itself.
    row: Row<'t>,

    /// Columns [`fields`](Self::fields) walks, or every column.
    select: Option<&'q [&'q str]>,
}

impl<'t, 'q> Matched<'t, 'q> {
    /// The whole row, whatever the query selected.
    #[must_use]
    pub const fn row(&self) -> Row<'t> {
        self.row
    }

    /// Column names and their values, in the order the query asked for them.
    ///
    /// Every column in file order when the query selected none. This is what a
    /// host copying a row into an event of its own walks.
    #[must_use]
    pub fn fields(&self) -> Fields<'t, 'q> {
        Fields {
            table: self.row.table,
            cells: self.row.cells,
            walk: match self.select {
                Some(named) => FieldWalk::Named(named.iter()),
                None => FieldWalk::All(0..self.row.table.columns.len()),
            },
        }
    }
}

/// Columns a [`Matched`] walks.
#[derive(Debug)]
enum FieldWalk<'q> {
    /// Every column, in file order.
    All(Range<usize>),

    /// The columns the query named, in that order.
    Named(slice::Iter<'q, &'q str>),
}

/// Column names and values of one matched row.
#[derive(Debug)]
pub struct Fields<'t, 'q> {
    /// Table the row belongs to, which is what names its cells.
    table: &'t Table,

    /// Cells of the row being walked.
    cells: &'t [Cell],

    /// Columns still to hand back.
    walk: FieldWalk<'q>,
}

impl<'t> Iterator for Fields<'t, '_> {
    type Item = (&'t str, Option<&'t str>);

    fn next(&mut self) -> Option<Self::Item> {
        let table = self.table;
        let cells = self.cells;

        match &mut self.walk {
            FieldWalk::All(all) => {
                let at = all.next()?;
                Some((table.columns[at].as_str(), cell(cells, at)))
            }
            FieldWalk::Named(named) => loop {
                // A selected column the table has not got is dropped rather
                // than handed back empty.
                if let Some(at) = table.position_of(named.next()?) {
                    return Some((table.columns[at].as_str(), cell(cells, at)));
                }
            },
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.walk {
            FieldWalk::All(all) => all.size_hint(),
            FieldWalk::Named(named) => (0, Some(named.len())),
        }
    }
}

/// Whether a cell holds the value a condition asked for.
///
/// Folding is Unicode-aware, and the ASCII path is taken when both sides are
/// ASCII because that is what reference data mostly is.
fn equal(held: &str, value: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        return held == value;
    }

    if held.is_ascii() && value.is_ascii() {
        return held.eq_ignore_ascii_case(value);
    }

    held.chars()
        .flat_map(char::to_lowercase)
        .eq(value.chars().flat_map(char::to_lowercase))
}

/// A cell as an instant, when it reads as one.
///
/// RFC 3339, or a bare date read as midnight UTC. A source writing anything
/// else answers no date condition, which is visible as an empty result rather
/// than as a wrong one.
fn as_date(text: &str) -> Option<SystemTime> {
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(SystemTime::from(at));
    }

    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    Some(SystemTime::from(date.and_hms_opt(0, 0, 0)?.and_utc()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::table::{Index, Schema, TableFormat};

    /// A table of three columns, keyed by country.
    const PLACES: &str = "country,city,status\n\
                          AU,Sydney,active\n\
                          AU,Perth,retired\n\
                          NZ,Auckland,active\n";

    /// Read a table from a CSV with a header, indexed by `index`.
    fn table(body: &str, index: &Index) -> Table {
        Table::from_reader(
            body.as_bytes(),
            TableFormat::Csv { header: true },
            &Schema::Auto,
            index,
        )
        .unwrap()
    }

    /// The places table, keyed by country.
    fn places() -> Table {
        table(PLACES, &Index::Column("country".to_string()))
    }

    /// An equality on `column`.
    const fn eq<'a>(column: &'a str, value: &'a str) -> Condition<'a> {
        Condition::Equals { column, value }
    }

    /// One column of every row a query matched.
    fn found(table: &Table, query: Query<'_>, column: &str) -> Vec<String> {
        table
            .find(query)
            .map(|matched| matched.row().get(column).unwrap_or_default().to_string())
            .collect()
    }

    /// An instant that many seconds after the epoch.
    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    /// Midnight UTC on a date, as the cells in these tests write it.
    fn day(text: &str) -> SystemTime {
        as_date(text).unwrap()
    }

    // -----------------------------------------------------------------------
    // Several conditions at once
    // -----------------------------------------------------------------------

    #[test]
    fn every_condition_has_to_hold() {
        let table = places();

        let both = [eq("country", "AU"), eq("status", "active")];
        assert_eq!(found(&table, Query::new(&both), "city"), ["Sydney"]);

        // The same key, the other status: the index narrows to two rows and the
        // second condition is what refuses one of them.
        let neither = [eq("country", "AU"), eq("status", "dissolved")];
        assert!(found(&table, Query::new(&neither), "city").is_empty());
    }

    #[test]
    fn a_condition_on_the_key_alone_answers_every_row_under_it() {
        let table = places();
        let one = [eq("country", "AU")];

        assert_eq!(found(&table, Query::new(&one), "city"), ["Sydney", "Perth"]);
    }

    #[test]
    fn conditions_away_from_the_key_still_answer() {
        // Nothing here names the indexed column, so the walk is the table and
        // the answer has to be the same one the index would have given.
        let table = places();
        let away = [eq("status", "active")];

        assert_eq!(
            found(&table, Query::new(&away), "city"),
            ["Sydney", "Auckland"]
        );
    }

    #[test]
    fn no_conditions_at_all_answers_every_row() {
        let table = places();

        assert_eq!(table.find(Query::new(&[])).count(), 3);
    }

    #[test]
    fn a_query_answers_the_same_rows_the_index_and_the_walk_both_reach() {
        // The one property that matters: narrowing must not change the answer.
        let table = places();
        let conditions = [eq("country", "AU"), eq("status", "retired")];

        let indexed = found(&table, Query::new(&conditions), "city");
        // Folding case takes the identical query off the index and onto a walk.
        let walked = found(
            &table,
            Query::new(&conditions).case_sensitive(false),
            "city",
        );

        assert_eq!(indexed, ["Perth"]);
        assert_eq!(indexed, walked);
    }

    #[test]
    fn a_cell_the_source_did_not_supply_satisfies_nothing() {
        let table = table(
            "country,city\nAU,\nNZ,Auckland\n",
            &Index::Column("country".to_string()),
        );

        let empty = [eq("city", "")];
        assert!(table.find(Query::new(&empty)).next().is_none());
    }

    #[test]
    fn a_repeated_key_hands_back_every_row_under_it() {
        let table = table(
            "asn,prefix\n13335,1.1.1.0/24\n13335,1.0.0.0/24\n",
            &Index::Column("asn".to_string()),
        );

        let one = [eq("asn", "13335")];
        assert_eq!(
            found(&table, Query::new(&one), "prefix"),
            ["1.1.1.0/24", "1.0.0.0/24"]
        );
    }

    // -----------------------------------------------------------------------
    // Columns the table has not got
    // -----------------------------------------------------------------------

    #[test]
    fn a_condition_on_an_unknown_column_matches_nothing_and_is_reportable() {
        let table = places();
        let typo = [eq("country", "AU"), eq("stauts", "active")];
        let query = Query::new(&typo);

        assert_eq!(table.find(query).count(), 0);
        assert_eq!(table.unknown_columns(query), ["stauts"]);
    }

    #[test]
    fn unknown_columns_is_sorted_and_says_each_once() {
        let table = places();
        let conditions = [
            eq("zeta", "1"),
            eq("alpha", "2"),
            eq("zeta", "3"),
            eq("country", "AU"),
        ];

        assert_eq!(
            table.unknown_columns(Query::new(&conditions)),
            ["alpha", "zeta"]
        );
    }

    #[test]
    fn a_query_a_table_can_answer_reports_nothing_unknown() {
        let table = places();
        let conditions = [eq("country", "AU"), eq("city", "Perth")];

        assert!(table.unknown_columns(Query::new(&conditions)).is_empty());
    }

    #[test]
    fn a_date_condition_on_an_unknown_column_is_reported_too() {
        // Vector leaves date fields out of its dataset check, so a misspelt one
        // is never reported at all. Here they are checked the same as any other.
        let table = places();
        let conditions = [Condition::FromDate {
            column: "isseud",
            from: at(0),
        }];

        assert_eq!(table.unknown_columns(Query::new(&conditions)), ["isseud"]);
    }

    // -----------------------------------------------------------------------
    // Case
    // -----------------------------------------------------------------------

    #[test]
    fn text_is_compared_exactly_by_default() {
        let table = places();
        let lower = [eq("city", "sydney")];

        assert!(table.find(Query::new(&lower)).next().is_none());
    }

    #[test]
    fn folding_case_matches_either_way_round() {
        let table = places();

        for value in ["sydney", "SYDNEY", "SyDnEy"] {
            let conditions = [eq("city", value)];
            let query = Query::new(&conditions).case_sensitive(false);
            assert_eq!(found(&table, query, "city"), ["Sydney"], "{value}");
        }
    }

    #[test]
    fn folding_case_reaches_past_ascii() {
        let table = table(
            "city,country\nZ\u{00fc}rich,CH\n",
            &Index::Column("city".to_string()),
        );
        let conditions = [eq("city", "Z\u{00dc}RICH")];

        let query = Query::new(&conditions).case_sensitive(false);
        assert_eq!(found(&table, query, "country"), ["CH"]);
    }

    #[test]
    fn folding_case_reaches_rows_the_index_files_apart() {
        // The index is keyed on the bytes the source wrote, so a folded query
        // on the key column has to leave it rather than answer one bucket.
        let table = table(
            "country,city\nau,Sydney\nAU,Perth\n",
            &Index::Column("country".to_string()),
        );
        let conditions = [eq("country", "Au")];

        let query = Query::new(&conditions).case_sensitive(false);
        assert_eq!(found(&table, query, "city"), ["Sydney", "Perth"]);
    }

    // -----------------------------------------------------------------------
    // Wildcard
    // -----------------------------------------------------------------------

    #[test]
    fn a_wildcard_cell_satisfies_any_value() {
        let table = table(
            "country,rate\nAU,0.10\n*,0.20\n",
            &Index::Column("country".to_string()),
        );
        let conditions = [eq("country", "NZ")];

        let query = Query::new(&conditions).wildcard("*");
        assert_eq!(found(&table, query, "rate"), ["0.20"]);
    }

    #[test]
    fn a_wildcard_row_is_reached_through_the_index_rather_than_a_walk() {
        // The condition pins the key column, so the value's bucket is the whole
        // of what the index would answer -- the wildcard bucket has to be walked
        // as well or the fallback row is invisible.
        let table = table(
            "country,rate\nAU,0.10\n*,0.20\n",
            &Index::Column("country".to_string()),
        );
        let conditions = [eq("country", "AU")];

        let query = Query::new(&conditions).wildcard("*");
        assert_eq!(found(&table, query, "rate"), ["0.10", "0.20"]);
    }

    #[test]
    fn a_wildcard_equal_to_the_value_hands_each_row_back_once() {
        let table = table(
            "country,rate\n*,0.20\n",
            &Index::Column("country".to_string()),
        );
        let conditions = [eq("country", "*")];

        let query = Query::new(&conditions).wildcard("*");
        assert_eq!(found(&table, query, "rate"), ["0.20"]);
    }

    #[test]
    fn a_wildcard_applies_to_columns_away_from_the_key() {
        let plain = places();
        let conditions = [eq("country", "AU"), eq("status", "*")];

        // No cell holds "*", so nothing matches even though the wildcard is set.
        let query = Query::new(&conditions).wildcard("*");
        assert!(found(&plain, query, "city").is_empty());

        // A wildcard the data does carry is what the fallback is for.
        let carried = table(
            "country,city,status\nAU,Sydney,*\n",
            &Index::Column("country".to_string()),
        );
        let conditions = [eq("country", "AU"), eq("status", "active")];
        let query = Query::new(&conditions).wildcard("*");
        assert_eq!(found(&carried, query, "city"), ["Sydney"]);
    }

    // -----------------------------------------------------------------------
    // Dates
    // -----------------------------------------------------------------------

    /// Rows carrying a date column, keyed by identifier.
    const LICENCES: &str = "id,issued\n\
                            1,1985-06-15T00:00:00Z\n\
                            2,1990-01-01T00:00:00Z\n\
                            3,not-a-date\n";

    #[test]
    fn a_date_range_takes_both_ends() {
        let table = table(LICENCES, &Index::Column("id".to_string()));
        let conditions = [Condition::BetweenDates {
            column: "issued",
            from: day("1980-01-01"),
            to: day("1989-12-31"),
        }];

        assert_eq!(found(&table, Query::new(&conditions), "id"), ["1"]);
    }

    #[test]
    fn a_date_range_includes_the_instant_it_names() {
        let table = table(LICENCES, &Index::Column("id".to_string()));
        let conditions = [Condition::BetweenDates {
            column: "issued",
            from: day("1985-06-15"),
            to: day("1985-06-15"),
        }];

        assert_eq!(found(&table, Query::new(&conditions), "id"), ["1"]);
    }

    #[test]
    fn an_open_ended_range_answers_from_each_side() {
        let table = table(LICENCES, &Index::Column("id".to_string()));

        let from = [Condition::FromDate {
            column: "issued",
            from: day("1986-01-01"),
        }];
        assert_eq!(found(&table, Query::new(&from), "id"), ["2"]);

        let to = [Condition::ToDate {
            column: "issued",
            to: day("1986-01-01"),
        }];
        assert_eq!(found(&table, Query::new(&to), "id"), ["1"]);
    }

    #[test]
    fn a_cell_that_is_not_a_date_answers_no_date_condition() {
        let table = table(LICENCES, &Index::Column("id".to_string()));
        let conditions = [Condition::FromDate {
            column: "issued",
            from: at(0),
        }];

        let ids = found(&table, Query::new(&conditions), "id");
        assert!(!ids.contains(&"3".to_string()), "{ids:?}");
    }

    #[test]
    fn a_bare_date_reads_as_midnight_utc() {
        let table = table(
            "id,issued\n1,2026-07-01\n",
            &Index::Column("id".to_string()),
        );
        let conditions = [Condition::BetweenDates {
            column: "issued",
            from: day("2026-07-01"),
            to: day("2026-07-01"),
        }];

        assert_eq!(found(&table, Query::new(&conditions), "id"), ["1"]);
    }

    #[test]
    fn an_offset_is_the_instant_it_names_rather_than_the_text_it_wrote() {
        // Midnight in Sydney is the previous afternoon in UTC, so a comparison
        // on text would put this row on the wrong side of the range.
        let table = table(
            "id,issued\n1,2026-07-01T00:00:00+10:00\n",
            &Index::Column("id".to_string()),
        );
        let conditions = [Condition::ToDate {
            column: "issued",
            to: day("2026-07-01"),
        }];

        assert_eq!(found(&table, Query::new(&conditions), "id"), ["1"]);
    }

    #[test]
    fn a_date_condition_rides_beside_an_indexed_equality() {
        let table = table(
            "country,issued\nAU,1985-06-15\nAU,2026-07-01\nNZ,1985-06-15\n",
            &Index::Column("country".to_string()),
        );
        let conditions = [
            eq("country", "AU"),
            Condition::ToDate {
                column: "issued",
                to: day("1990-01-01"),
            },
        ];

        assert_eq!(
            found(&table, Query::new(&conditions), "issued"),
            ["1985-06-15"]
        );
    }

    #[test]
    fn a_wildcard_is_not_read_as_a_date() {
        // The wildcard is an equality fallback; a date range is not an equality.
        let table = table("id,issued\n1,*\n", &Index::Column("id".to_string()));
        let conditions = [Condition::FromDate {
            column: "issued",
            from: at(0),
        }];

        let query = Query::new(&conditions).wildcard("*");
        assert!(table.find(query).next().is_none());
    }

    // -----------------------------------------------------------------------
    // Projection
    // -----------------------------------------------------------------------

    #[test]
    fn fields_hands_back_every_column_when_nothing_is_selected() {
        let table = places();
        let conditions = [eq("city", "Sydney")];

        let matched = table.find(Query::new(&conditions)).next().unwrap();
        let fields: Vec<_> = matched.fields().collect();

        assert_eq!(
            fields,
            [
                ("country", Some("AU")),
                ("city", Some("Sydney")),
                ("status", Some("active")),
            ]
        );
    }

    #[test]
    fn a_selection_limits_the_columns_and_keeps_its_own_order() {
        let table = places();
        let conditions = [eq("city", "Sydney")];

        let query = Query::new(&conditions).select(&["status", "country"]);
        let matched = table.find(query).next().unwrap();

        assert_eq!(
            matched.fields().collect::<Vec<_>>(),
            [("status", Some("active")), ("country", Some("AU"))]
        );
    }

    #[test]
    fn a_selected_column_the_table_has_not_got_is_dropped() {
        let table = places();
        let conditions = [eq("city", "Sydney")];

        let query = Query::new(&conditions).select(&["city", "nosuchcolumn"]);
        let matched = table.find(query).next().unwrap();

        assert_eq!(
            matched.fields().collect::<Vec<_>>(),
            [("city", Some("Sydney"))]
        );
    }

    #[test]
    fn a_selection_never_limits_the_row_itself() {
        let table = places();
        let conditions = [eq("city", "Sydney")];

        let query = Query::new(&conditions).select(&["city"]);
        let matched = table.find(query).next().unwrap();

        assert_eq!(matched.row().get("status"), Some("active"));
        assert_eq!(matched.row().key(), Some("AU"));
    }

    #[test]
    fn a_selected_cell_the_source_did_not_supply_is_handed_back_empty() {
        // Absent is not the same as unselected: the column exists, so the name
        // is reported with nothing under it.
        let table = table("country,city\nAU,\n", &Index::Column("country".to_string()));
        let conditions = [eq("country", "AU")];

        let query = Query::new(&conditions).select(&["city"]);
        let matched = table.find(query).next().unwrap();

        assert_eq!(matched.fields().collect::<Vec<_>>(), [("city", None)]);
    }

    // -----------------------------------------------------------------------
    // Address and prefix indexes
    // -----------------------------------------------------------------------

    #[test]
    fn an_address_keyed_table_answers_an_equality_on_its_key() {
        let table = table(
            "ip,operator\n1.1.1.1,CLOUDFLARENET\n8.8.8.8,GOOGLE\n",
            &Index::Ip,
        );
        let conditions = [eq("ip", "8.8.8.8")];

        assert_eq!(
            found(&table, Query::new(&conditions), "operator"),
            ["GOOGLE"]
        );
    }

    /// A CSV whose key column holds `count` good values and then `odd`.
    ///
    /// Detection samples the head of the file, so a value that is not an
    /// address has to arrive past the sample for the source to be accepted at
    /// all -- which is exactly how a junk row reaches a real prefix list.
    fn with_an_odd_one_out(column: &str, good: impl Fn(u32) -> String, odd: &str) -> String {
        use std::fmt::Write as _;

        let mut body = format!("{column},operator\n");
        for at in 0..70 {
            let _ = writeln!(body, "{},OPERATOR-{at}", good(at));
        }
        let _ = writeln!(body, "{odd},PLACEHOLDER");
        body
    }

    #[test]
    fn an_address_keyed_table_reaches_a_row_its_index_could_not_file() {
        // The key cell does not parse, so the row is not in the address map at
        // all. A query for that text still has to find it.
        let body = with_an_odd_one_out("ip", |at| format!("10.0.0.{at}"), "unknown");
        let table = table(&body, &Index::Ip);

        let unparsed = [eq("ip", "unknown")];
        assert_eq!(
            found(&table, Query::new(&unparsed), "operator"),
            ["PLACEHOLDER"]
        );

        // And the parseable ones are still answered from the map.
        let parsed = [eq("ip", "10.0.0.7")];
        assert_eq!(
            found(&table, Query::new(&parsed), "operator"),
            ["OPERATOR-7"]
        );
        assert_eq!(
            table
                .get_by_address("10.0.0.7".parse().unwrap())
                .unwrap()
                .get("operator"),
            Some("OPERATOR-7")
        );
    }

    #[test]
    fn an_address_keyed_equality_compares_text_not_addresses() {
        let table = table(
            "ip,operator\n2606:4700:4700::1111,CLOUDFLARENET\n",
            &Index::Ip,
        );

        // The same address written out in full, which get_by_address answers and
        // a text equality does not.
        let long = [eq("ip", "2606:4700:4700:0000:0000:0000:0000:1111")];
        assert!(table.find(Query::new(&long)).next().is_none());
        assert!(
            table
                .get_by_address("2606:4700:4700:0000:0000:0000:0000:1111".parse().unwrap())
                .is_some()
        );
    }

    #[test]
    fn a_prefix_keyed_table_answers_an_equality_on_its_range() {
        let table = table(
            "prefix,operator\n8.0.0.0/8,LEVEL3\n8.8.8.0/24,GOOGLE\n",
            &Index::Prefix,
        );
        let conditions = [eq("prefix", "8.8.8.0/24")];

        assert_eq!(
            found(&table, Query::new(&conditions), "operator"),
            ["GOOGLE"]
        );
    }

    #[test]
    fn a_prefix_keyed_query_is_the_range_written_down_not_the_one_containing_it() {
        // get_by_address walks the trie; an equality reads the cell, so a range
        // that contains the address but is not the text asked for stays out.
        let table = table(
            "prefix,operator\n8.0.0.0/8,LEVEL3\n8.8.8.0/24,GOOGLE\n",
            &Index::Prefix,
        );
        let conditions = [eq("prefix", "8.8.8.8")];

        assert!(table.find(Query::new(&conditions)).next().is_none());
    }

    #[test]
    fn a_prefix_keyed_table_reaches_a_row_its_index_could_not_file() {
        let body = with_an_odd_one_out("prefix", |at| format!("10.0.{at}.0/24"), "pending");
        let table = table(&body, &Index::Prefix);

        let unparsed = [eq("prefix", "pending")];
        assert_eq!(
            found(&table, Query::new(&unparsed), "operator"),
            ["PLACEHOLDER"]
        );

        let parsed = [eq("prefix", "10.0.7.0/24")];
        assert_eq!(
            found(&table, Query::new(&parsed), "operator"),
            ["OPERATOR-7"]
        );
    }

    #[test]
    fn a_bare_address_and_its_single_host_range_file_together() {
        // Both parse to the same range, so the index hands the bucket to either
        // query and the text comparison is what separates them.
        let table = table(
            "prefix,operator\n1.1.1.1,HOST\n1.1.1.1/32,RANGE\n",
            &Index::Prefix,
        );

        let bare = [eq("prefix", "1.1.1.1")];
        assert_eq!(found(&table, Query::new(&bare), "operator"), ["HOST"]);

        let written = [eq("prefix", "1.1.1.1/32")];
        assert_eq!(found(&table, Query::new(&written), "operator"), ["RANGE"]);
    }

    // -----------------------------------------------------------------------
    // Iterator behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn the_size_hint_bounds_what_is_left_without_promising_it() {
        let table = places();
        let conditions = [eq("country", "AU")];

        let matches = table.find(Query::new(&conditions));
        let (low, high) = matches.size_hint();

        assert_eq!(low, 0, "a test can refuse every candidate");
        assert_eq!(high, Some(2), "the index narrowed to the two AU rows");
    }

    #[test]
    fn a_walked_query_bounds_itself_by_the_whole_table() {
        let table = places();
        let conditions = [eq("status", "active")];

        assert_eq!(
            table.find(Query::new(&conditions)).size_hint(),
            (0, Some(3))
        );
    }

    #[test]
    fn a_matched_row_prints_its_own_cells() {
        let table = places();
        let conditions = [eq("city", "Perth")];

        let matched = table.find(Query::new(&conditions)).next().unwrap();
        let rendered = format!("{matched:?}");

        assert!(rendered.contains("\"city\": Some(\"Perth\")"), "{rendered}");
    }

    // -----------------------------------------------------------------------
    // Comparison helpers
    // -----------------------------------------------------------------------

    #[test]
    fn a_date_reads_from_either_form_and_nothing_else() {
        assert_eq!(as_date("1970-01-01T00:00:00Z"), Some(at(0)));
        assert_eq!(as_date("1970-01-01"), Some(at(0)));
        assert_eq!(as_date("1970-01-01T01:00:00+01:00"), Some(at(0)));

        assert!(as_date("01/01/1970").is_none());
        assert!(as_date("1970-01-01 00:00:00").is_none());
        assert!(as_date("").is_none());
    }

    #[test]
    fn a_date_before_the_epoch_reads_as_an_earlier_instant() {
        let before = as_date("1969-12-31").unwrap();

        assert!(before < SystemTime::UNIX_EPOCH);
    }

    #[test]
    fn folding_is_off_the_ascii_path_only_when_it_has_to_be() {
        assert!(equal("AU", "AU", true));
        assert!(!equal("AU", "au", true));
        assert!(equal("AU", "au", false));
        assert!(equal("\u{00dc}ber", "\u{00fc}ber", false));
        assert!(!equal("\u{00dc}ber", "uber", false));
    }
}
