// Project:   factbook
// File:      src/table/parse.rs
// Purpose:   Turn a fetched CSV or JSON file into columns and rows
// Language:  Rust
//
// License:   Apache-2.0
// Copyright: (c) 2026 HYPERI PTY LIMITED

//! Reading a source file into columns and rows.
//!
//! Two encodings, two column policies. A CSV states its width once and every
//! row has to match it, because a row of the wrong width is a quoting fault
//! rather than a shape the file is entitled to. A JSON document states nothing,
//! so its columns are the union of every object's keys and a row simply has no
//! value under a key its object omitted -- ragged is the normal case there, not
//! an error.
//!
//! The CSV reader is written here rather than taken from a crate: it is one
//! state machine over four states, and the alternative is a dependency in the
//! graph of every consumer that only wanted the geo half.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use super::config::{Schema, TableFormat};
use super::{Cell, TableError};

/// Byte a CSV quotes a field with.
const QUOTE: u8 = b'"';

/// Byte a CSV separates fields with.
const DELIMITER: u8 = b',';

/// Marker some publishers open a UTF-8 file with, which would otherwise become
/// part of the first column's name.
const BOM: &str = "\u{feff}";

/// One CSV record, and the line it started on.
type Record = (usize, Vec<String>);

/// Columns and rows read from a source file.
#[derive(Debug)]
pub(super) struct Parsed {
    /// Column names, in the order a row's cells are stored.
    pub(super) columns: Vec<String>,

    /// One entry per row, each as wide as `columns`.
    pub(super) rows: Vec<Vec<Cell>>,
}

/// Read a source file into columns and rows.
///
/// # Errors
///
/// [`TableError::Malformed`] for a CSV the reader cannot make rows of,
/// [`TableError::NotAnArrayOfObjects`] for JSON that is not one,
/// [`TableError::NamesRequired`] for a headerless CSV that supplies no names,
/// or [`TableError::Empty`] when the file holds no rows.
pub(super) fn read(
    reader: impl BufRead,
    format: TableFormat,
    schema: &Schema,
) -> Result<Parsed, TableError> {
    match format {
        TableFormat::Csv { header } => read_csv(reader, header, schema),
        TableFormat::Json => read_json(reader, schema),
    }
}

/// Whether `path` holds rows of `format`, all the way to the end.
///
/// This runs against a staged file before it replaces the copy on disk, so a
/// fault it does not catch is one that destroys a good database and then fails
/// at load, with nothing left to fall back to.
///
/// # Errors
///
/// The errors [`read`] raises.
pub(crate) fn probe(path: &Path, format: TableFormat) -> Result<(), TableError> {
    let reader = BufReader::new(fs::File::open(path)?);

    match format {
        TableFormat::Csv { header } => validate_csv(reader, header),

        // A JSON document has to be read whole to be known valid at all, so the
        // probe is the same read the load performs.
        TableFormat::Json => read_json(reader, &Schema::Auto).map(drop),
    }
}

/// Whether every record is as wide as the first.
///
/// Records are checked and dropped as they are read, so the whole file is
/// covered without holding it in memory.
fn validate_csv(mut reader: impl BufRead, header: bool) -> Result<(), TableError> {
    let mut csv = Csv::new();
    let mut width: Option<usize> = None;
    let mut seen: usize = 0;

    loop {
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let consumed = chunk.len();
        for &byte in chunk {
            csv.byte(byte)?;
        }
        reader.consume(consumed);
        drain(&mut csv.records, &mut width, &mut seen)?;
    }

    let mut tail = csv.finish()?;
    drain(&mut tail, &mut width, &mut seen)?;

    // A header and nothing under it is not a table, and neither is an empty
    // file.
    if seen <= usize::from(header) {
        return Err(TableError::Empty);
    }
    Ok(())
}

/// Check the records read so far against the first one's width, then drop them.
fn drain(
    records: &mut Vec<Record>,
    width: &mut Option<usize>,
    seen: &mut usize,
) -> Result<(), TableError> {
    for (line, fields) in records.drain(..) {
        let expected = *width.get_or_insert(fields.len());
        if fields.len() != expected {
            return Err(TableError::Malformed {
                line,
                detail: format!(
                    "{} fields against {expected} in the first row",
                    fields.len()
                ),
            });
        }
        *seen += 1;
    }
    Ok(())
}

/// Read a CSV into columns and rows.
fn read_csv(reader: impl BufRead, header: bool, schema: &Schema) -> Result<Parsed, TableError> {
    let mut records = records(reader, None)?.into_iter();

    // The header row is consumed whether or not its names are used, so naming
    // the columns explicitly overrides a header rather than shifting the rows.
    let columns = match (header, schema) {
        (true, Schema::Auto) => records.next().ok_or(TableError::Empty)?.1,
        (true, Schema::Named(named)) => {
            records.next().ok_or(TableError::Empty)?;
            named.clone()
        }
        (false, Schema::Auto) => return Err(TableError::NamesRequired),
        (false, Schema::Named(named)) => named.clone(),
    };
    if columns.is_empty() {
        return Err(TableError::NoNames);
    }

    let mut rows = Vec::new();
    for (line, fields) in records {
        if fields.len() != columns.len() {
            return Err(TableError::Malformed {
                line,
                detail: format!("{} fields against {} columns", fields.len(), columns.len()),
            });
        }
        rows.push(fields.into_iter().map(cell_of_text).collect());
    }
    if rows.is_empty() {
        return Err(TableError::Empty);
    }

    Ok(Parsed { columns, rows })
}

/// Read a JSON array of objects into columns and rows.
fn read_json(reader: impl BufRead, schema: &Schema) -> Result<Parsed, TableError> {
    let values: Vec<Value> =
        serde_json::from_reader(reader).map_err(|e| TableError::NotAnArrayOfObjects {
            detail: e.to_string(),
        })?;

    let mut objects = Vec::with_capacity(values.len());
    for (position, value) in values.iter().enumerate() {
        let object = value
            .as_object()
            .ok_or_else(|| TableError::NotAnArrayOfObjects {
                detail: format!("element {position} is not an object"),
            })?;
        objects.push(object);
    }

    // Checked before the columns are derived, so an empty document reports that
    // it holds no rows rather than that it names no columns.
    if objects.is_empty() {
        return Err(TableError::Empty);
    }

    // Sorted rather than first-seen: a JSON object has no key order to inherit,
    // so sorting is the one ordering that does not depend on which object
    // happened to come first.
    let columns = match schema {
        Schema::Auto => {
            let keys: BTreeSet<&str> = objects
                .iter()
                .flat_map(|object| object.keys().map(String::as_str))
                .collect();
            keys.into_iter().map(str::to_string).collect()
        }
        Schema::Named(named) => named.clone(),
    };
    if columns.is_empty() {
        return Err(TableError::NoNames);
    }

    let rows: Vec<Vec<Cell>> = objects
        .iter()
        .map(|object| {
            columns
                .iter()
                .map(|column| cell_of_value(object.get(column)))
                .collect()
        })
        .collect();

    Ok(Parsed { columns, rows })
}

/// A text field as a cell. An empty field is a value the source did not supply.
fn cell_of_text(text: String) -> Cell {
    if text.is_empty() {
        None
    } else {
        Some(text.into_boxed_str())
    }
}

/// A JSON value as a cell.
///
/// Scalars become their own text; an array or a nested object becomes its
/// compact JSON, because flattening it would invent a column layout the source
/// never stated.
fn cell_of_value(value: Option<&Value>) -> Cell {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => cell_of_text(text.clone()),
        Some(other) => Some(other.to_string().into_boxed_str()),
    }
}

/// Read CSV records, stopping after `limit` of them when one is given.
fn records(mut reader: impl BufRead, limit: Option<usize>) -> Result<Vec<Record>, TableError> {
    let mut csv = Csv::new();
    let enough = |csv: &Csv| limit.is_some_and(|limit| csv.records.len() >= limit);

    loop {
        let mut consumed = 0;
        let mut stop = false;

        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        for &byte in chunk {
            csv.byte(byte)?;
            consumed += 1;
            if enough(&csv) {
                stop = true;
                break;
            }
        }
        reader.consume(consumed);

        if stop {
            return Ok(strip_bom(csv.records));
        }
    }

    Ok(strip_bom(csv.finish()?))
}

/// Drop the byte-order mark from the first field, where a publisher wrote one.
fn strip_bom(mut records: Vec<Record>) -> Vec<Record> {
    if let Some((_, fields)) = records.first_mut()
        && let Some(first) = fields.first_mut()
        && first.starts_with(BOM)
    {
        first.drain(..BOM.len());
    }
    records
}

/// Where the reader is in a record.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum State {
    /// Before the first byte of a field, which is where a quote opens one.
    #[default]
    FieldStart,

    /// Inside a field that was not opened with a quote.
    Unquoted,

    /// Inside a quoted field, where a delimiter or a newline is data.
    Quoted,

    /// On a quote inside a quoted field, which either escapes a quote or closes
    /// the field.
    QuoteInQuoted,
}

/// A CSV reader, one byte at a time.
///
/// Byte-wise rather than line-wise because a quoted field may hold the line
/// ending, so splitting on newlines first would cut records in half.
struct Csv {
    /// Records completed so far.
    records: Vec<Record>,

    /// Fields of the record being read.
    fields: Vec<String>,

    /// Bytes of the field being read.
    field: Vec<u8>,

    /// Where the reader is in the current record.
    state: State,

    /// Line the reader is on, counting from one.
    line: usize,

    /// Line the current record started on, which is what an error names.
    record_line: usize,

    /// Whether the current record has had any content, which is what tells a
    /// blank line from a record of one empty field.
    started: bool,
}

impl Csv {
    /// A reader positioned at the first byte of the first line.
    fn new() -> Self {
        Self {
            records: Vec::new(),
            fields: Vec::new(),
            field: Vec::new(),
            state: State::FieldStart,
            line: 1,
            record_line: 1,
            started: false,
        }
    }

    /// Feed one byte.
    fn byte(&mut self, byte: u8) -> Result<(), TableError> {
        match self.state {
            State::FieldStart => match byte {
                QUOTE => {
                    self.begin();
                    self.state = State::Quoted;
                }
                DELIMITER => {
                    self.begin();
                    self.end_field()?;
                }
                b'\n' => self.newline()?,
                // A bare carriage return outside a quoted field is the first
                // half of a CRLF and carries nothing of its own.
                b'\r' => {}
                _ => {
                    self.begin();
                    self.field.push(byte);
                    self.state = State::Unquoted;
                }
            },

            State::Unquoted => match byte {
                DELIMITER => self.end_field()?,
                b'\n' => self.newline()?,
                b'\r' => {}
                // A quote that did not open the field is data: some publishers
                // write inches and apostrophes unescaped.
                _ => self.field.push(byte),
            },

            State::Quoted => match byte {
                QUOTE => self.state = State::QuoteInQuoted,
                b'\n' => {
                    self.line += 1;
                    self.field.push(byte);
                }
                _ => self.field.push(byte),
            },

            State::QuoteInQuoted => match byte {
                QUOTE => {
                    self.field.push(QUOTE);
                    self.state = State::Quoted;
                }
                DELIMITER => self.end_field()?,
                b'\n' => self.newline()?,
                b'\r' => {}
                _ => {
                    return Err(TableError::Malformed {
                        line: self.record_line,
                        detail: "a closing quote is followed by more text".to_string(),
                    });
                }
            },
        }

        Ok(())
    }

    /// Note that the record has content, and where it started.
    fn begin(&mut self) {
        if !self.started {
            self.started = true;
            self.record_line = self.line;
        }
    }

    /// Close the field being read.
    fn end_field(&mut self) -> Result<(), TableError> {
        let bytes = std::mem::take(&mut self.field);
        let text = String::from_utf8(bytes).map_err(|_| TableError::Malformed {
            line: self.record_line,
            detail: "a field is not UTF-8".to_string(),
        })?;

        self.fields.push(text);
        self.state = State::FieldStart;
        Ok(())
    }

    /// Close the record at a line ending outside a quoted field.
    fn newline(&mut self) -> Result<(), TableError> {
        self.line += 1;

        // A blank line is not a record of one empty field: publishers pad the
        // end of a file, and every one of those would fail the width check.
        if !self.started {
            return Ok(());
        }

        self.end_record()
    }

    /// Close whatever record is open at the end of the file.
    fn finish(mut self) -> Result<Vec<Record>, TableError> {
        if self.state == State::Quoted {
            return Err(TableError::Malformed {
                line: self.record_line,
                detail: "a quoted field is never closed".to_string(),
            });
        }

        // A file that does not end with a newline still ends with a record.
        if self.started {
            self.end_record()?;
        }

        Ok(self.records)
    }

    /// Move the fields read so far into a completed record.
    fn end_record(&mut self) -> Result<(), TableError> {
        self.end_field()?;
        let fields = std::mem::take(&mut self.fields);
        self.records.push((self.record_line, fields));
        self.started = false;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field of a parsed table, as text, for comparing against a literal.
    fn cells(parsed: &Parsed) -> Vec<Vec<&str>> {
        parsed
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.as_deref().unwrap_or_default())
                    .collect()
            })
            .collect()
    }

    /// Read a CSV with a header and derived column names.
    fn csv(body: &str) -> Result<Parsed, TableError> {
        read(
            body.as_bytes(),
            TableFormat::Csv { header: true },
            &Schema::Auto,
        )
    }

    #[test]
    fn a_header_names_the_columns() {
        let parsed = csv("asn,name\n13335,CLOUDFLARENET\n15169,GOOGLE\n").unwrap();

        assert_eq!(parsed.columns, ["asn", "name"]);
        assert_eq!(
            cells(&parsed),
            [["13335", "CLOUDFLARENET"], ["15169", "GOOGLE"]]
        );
    }

    #[test]
    fn a_quoted_field_holds_the_delimiter_and_the_line_ending() {
        // The two things that make a line-wise reader wrong.
        let parsed = csv("asn,name\n13335,\"Cloudflare, Inc.\"\n15169,\"Google\nLLC\"\n").unwrap();

        assert_eq!(
            cells(&parsed),
            [["13335", "Cloudflare, Inc."], ["15169", "Google\nLLC"]]
        );
    }

    #[test]
    fn a_doubled_quote_is_one_quote() {
        let parsed = csv("asn,name\n13335,\"the \"\"net\"\"\"\n").unwrap();
        assert_eq!(cells(&parsed), [["13335", "the \"net\""]]);
    }

    #[test]
    fn crlf_endings_and_blank_lines_are_not_rows() {
        // Padding at the end of a file would otherwise fail the width check.
        let parsed = csv("asn,name\r\n13335,CLOUDFLARENET\r\n\r\n").unwrap();
        assert_eq!(cells(&parsed), [["13335", "CLOUDFLARENET"]]);
    }

    #[test]
    fn a_final_row_without_a_newline_is_still_a_row() {
        let parsed = csv("asn,name\n13335,CLOUDFLARENET").unwrap();
        assert_eq!(cells(&parsed), [["13335", "CLOUDFLARENET"]]);
    }

    #[test]
    fn an_empty_field_is_a_value_the_source_did_not_supply() {
        let parsed = csv("asn,name,country\n13335,,AU\n").unwrap();

        assert_eq!(parsed.rows[0][1], None);
        assert_eq!(parsed.rows[0][2].as_deref(), Some("AU"));
    }

    #[test]
    fn a_byte_order_mark_is_not_part_of_the_first_column_name() {
        let parsed = csv("\u{feff}asn,name\n13335,CLOUDFLARENET\n").unwrap();
        assert_eq!(parsed.columns, ["asn", "name"]);
    }

    #[test]
    fn a_row_of_the_wrong_width_is_refused_by_line() {
        // An unquoted comma in a free-text field lands here, and admitting it
        // would silently shift every later column.
        let err = csv("asn,name\n13335,Cloudflare, Inc.\n").unwrap_err();

        assert!(
            matches!(err, TableError::Malformed { line: 2, .. }),
            "{err:?}"
        );
        assert!(
            err.to_string().contains("3 fields against 2 columns"),
            "{err}"
        );
    }

    #[test]
    fn an_unterminated_quote_is_refused() {
        let err = csv("asn,name\n13335,\"Cloudflare\n").unwrap_err();
        assert!(matches!(err, TableError::Malformed { .. }), "{err:?}");
    }

    #[test]
    fn text_after_a_closing_quote_is_refused() {
        let err = csv("asn,name\n13335,\"Cloudflare\" Inc\n").unwrap_err();
        assert!(matches!(err, TableError::Malformed { .. }), "{err:?}");
    }

    #[test]
    fn a_headerless_csv_takes_its_names_from_the_config() {
        // The edge case the whole `schema` field exists for.
        let names = Schema::Named(vec!["asn".to_string(), "name".to_string()]);
        let parsed = read(
            "13335,CLOUDFLARENET\n15169,GOOGLE\n".as_bytes(),
            TableFormat::Csv { header: false },
            &names,
        )
        .unwrap();

        assert_eq!(parsed.columns, ["asn", "name"]);
        // The first line is data, not names.
        assert_eq!(
            cells(&parsed),
            [["13335", "CLOUDFLARENET"], ["15169", "GOOGLE"]]
        );
    }

    #[test]
    fn a_headerless_csv_with_no_names_is_refused() {
        let err = read(
            "13335,CLOUDFLARENET\n".as_bytes(),
            TableFormat::Csv { header: false },
            &Schema::Auto,
        )
        .unwrap_err();

        assert!(matches!(err, TableError::NamesRequired), "{err:?}");
    }

    #[test]
    fn named_columns_override_a_header_rather_than_shifting_the_rows() {
        let names = Schema::Named(vec!["number".to_string(), "operator".to_string()]);
        let parsed = read(
            "asn,name\n13335,CLOUDFLARENET\n".as_bytes(),
            TableFormat::Csv { header: true },
            &names,
        )
        .unwrap();

        assert_eq!(parsed.columns, ["number", "operator"]);
        assert_eq!(cells(&parsed), [["13335", "CLOUDFLARENET"]]);
    }

    #[test]
    fn a_csv_with_only_a_header_holds_no_rows() {
        let err = csv("asn,name\n").unwrap_err();
        assert!(matches!(err, TableError::Empty), "{err:?}");
    }

    #[test]
    fn json_objects_name_the_columns() {
        let body = r#"[{"asn": 13335, "name": "CLOUDFLARENET"}]"#;
        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto).unwrap();

        assert_eq!(parsed.columns, ["asn", "name"]);
        assert_eq!(cells(&parsed), [["13335", "CLOUDFLARENET"]]);
    }

    #[test]
    fn ragged_objects_widen_the_table_rather_than_failing_it() {
        // Real JSON feeds omit keys per record, so the columns are the union
        // and a missing key is a missing value rather than a broken file.
        let body = r#"[
            {"ip": "1.1.1.1", "country": "AU"},
            {"ip": "8.8.8.8", "asn": 15169}
        ]"#;
        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto).unwrap();

        assert_eq!(parsed.columns, ["asn", "country", "ip"]);
        assert_eq!(parsed.rows[0][0], None);
        assert_eq!(parsed.rows[0][1].as_deref(), Some("AU"));
        assert_eq!(parsed.rows[1][0].as_deref(), Some("15169"));
        assert_eq!(parsed.rows[1][1], None);
    }

    #[test]
    fn named_columns_project_a_json_document() {
        // Pinning the schema is what stops a provider adding a key and changing
        // the shape of the table under a consumer.
        let body = r#"[{"ip": "1.1.1.1", "country": "AU", "extra": 1}]"#;
        let names = Schema::Named(vec!["ip".to_string(), "country".to_string()]);
        let parsed = read(body.as_bytes(), TableFormat::Json, &names).unwrap();

        assert_eq!(parsed.columns, ["ip", "country"]);
        assert_eq!(cells(&parsed), [["1.1.1.1", "AU"]]);
    }

    #[test]
    fn json_scalars_are_text_and_nested_values_keep_their_json() {
        let body = r#"[{"n": 42, "t": true, "z": null, "a": [1, 2], "o": {"k": "v"}}]"#;
        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto).unwrap();

        assert_eq!(parsed.columns, ["a", "n", "o", "t", "z"]);
        assert_eq!(
            cells(&parsed),
            [["[1,2]", "42", r#"{"k":"v"}"#, "true", ""]]
        );
    }

    #[test]
    fn json_that_is_not_an_array_of_objects_is_refused() {
        for body in [r#"{"relays": []}"#, "[1, 2]", "not json at all"] {
            let err = read(body.as_bytes(), TableFormat::Json, &Schema::Auto).unwrap_err();
            assert!(
                matches!(err, TableError::NotAnArrayOfObjects { .. }),
                "{body}: {err:?}"
            );
        }
    }

    #[test]
    fn an_empty_json_array_holds_no_rows() {
        let err = read("[]".as_bytes(), TableFormat::Json, &Schema::Auto).unwrap_err();
        assert!(matches!(err, TableError::Empty), "{err:?}");
    }

    #[test]
    fn a_probe_reads_a_csv_to_the_end() {
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.csv");
        let mut body = String::from("asn,name\n");
        for row in 0..5_000 {
            let _ = writeln!(body, "{row},operator");
        }
        // Deep enough that a bounded check would admit the file, replace a good
        // database with it, and only then fail at load.
        body.push_str("1,too,many,fields\n");
        fs::write(&file, &body).unwrap();

        let err = probe(&file, TableFormat::Csv { header: true }).unwrap_err();
        assert!(
            matches!(err, TableError::Malformed { line: 5002, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_probe_admits_a_long_well_formed_csv() {
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.csv");
        let mut body = String::from("asn,name\n");
        for row in 0..5_000 {
            let _ = writeln!(body, "{row},operator");
        }
        fs::write(&file, &body).unwrap();

        probe(&file, TableFormat::Csv { header: true }).unwrap();
    }

    #[test]
    fn a_probe_refuses_a_file_that_is_not_the_stated_format() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.csv");

        fs::write(&file, b"asn,name\n").unwrap();
        assert!(matches!(
            probe(&file, TableFormat::Csv { header: true }).unwrap_err(),
            TableError::Empty
        ));

        fs::write(&file, b"asn,name\n1,two,three\n").unwrap();
        assert!(matches!(
            probe(&file, TableFormat::Csv { header: true }).unwrap_err(),
            TableError::Malformed { .. }
        ));

        fs::write(&file, b"{\"relays\": []}").unwrap();
        assert!(matches!(
            probe(&file, TableFormat::Json).unwrap_err(),
            TableError::NotAnArrayOfObjects { .. }
        ));
    }
}
