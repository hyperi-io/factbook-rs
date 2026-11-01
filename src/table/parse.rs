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
//!
//! # The memory ceiling is hit, never predicted
//!
//! Whether a source fits in memory is not knowable from its size on disk: an
//! encoding, a quoting style and a column count all move the answer. So the
//! readers fill memory and stop at the ceiling the caller names, reporting
//! [`TableError::OverResidentCeiling`] with everything they had accumulated
//! released as the error unwinds. What is counted is the rows, not the index
//! [`Table::build`](super::Table::build) later derives from them.
//!
//! Both encodings stop mid-record. A JSON array is walked element by element
//! rather than parsed whole -- a document is not valid until its last byte, but
//! one element of it is. The record being read is charged against what the rows
//! have left as it grows: a CSV field byte by byte, a JSON element by the length
//! of its text. So neither reader's peak is set by the file. An element's parsed
//! value costs a multiple of its text, which bounds the JSON peak at that
//! multiple of the ceiling.
//!
//! A JSON source that names no columns widens instead of collecting keys up
//! front. A key first seen on the hundredth object is inserted into the
//! ninety-nine rows already read, which reaches the same union of keys without
//! a first pass that would have to hold the document to make one.

use std::cell;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use serde::de;
use serde::{Deserialize as _, Deserializer as _};
use serde_json::Value;

use super::config::{Index, Schema, TableFormat};
use super::{Cell, Table, TableError};

/// Byte a CSV quotes a field with.
const QUOTE: u8 = b'"';

/// Byte a CSV separates fields with.
const DELIMITER: u8 = b',';

/// Marker some publishers open a UTF-8 file with, which would otherwise become
/// part of the first column's name.
const BOM: &str = "\u{feff}";

/// One CSV record, and the line it started on.
type Record = (usize, Vec<String>);

/// Bytes one cell costs beyond the text in it.
///
/// The `Option<Box<str>>` itself. An estimate, like everything the ceiling
/// counts: an allocator rounds a request up by an amount only it knows.
const CELL_OVERHEAD: usize = size_of::<Cell>();

/// Bytes one row costs beyond the cells in it.
///
/// The `Vec` holding them, and the slot it occupies in the table's own `Vec`.
const ROW_OVERHEAD: usize = 4 * size_of::<usize>();

/// Columns and rows read from a source file.
#[derive(Debug)]
pub(super) struct Parsed {
    /// Column names, in the order a row's cells are stored.
    pub(super) columns: Vec<String>,

    /// One entry per row, each as wide as `columns`.
    pub(super) rows: Vec<Vec<Cell>>,
}

/// Read a source file into columns and rows, stopping at `ceiling` bytes.
///
/// `u64::MAX` reads whatever is there, which is what a caller reading bytes it
/// already holds wants.
///
/// # Errors
///
/// [`TableError::Malformed`] for a CSV the reader cannot make rows of,
/// [`TableError::NotAnArrayOfObjects`] for JSON that is not one,
/// [`TableError::NamesRequired`] for a headerless CSV that supplies no names,
/// [`TableError::Empty`] when the file holds no rows, or
/// [`TableError::OverResidentCeiling`] when the rows outgrow `ceiling`.
pub(super) fn read(
    reader: impl BufRead,
    format: TableFormat,
    schema: &Schema,
    ceiling: u64,
) -> Result<Parsed, TableError> {
    match format {
        TableFormat::Csv { header } => read_csv(reader, header, schema, ceiling),
        TableFormat::Json => read_json(reader, schema, ceiling),
    }
}

/// Whether the file at `path` loads as the table the source states.
///
/// Runs against a staged file before it replaces the copy on disk, so it reads
/// the way the load does: the same width, ceiling and key column. A fault it
/// does not catch destroys a good database and then fails at load, with nothing
/// left to fall back to. The parsed table is released on return.
///
/// # Errors
///
/// The errors [`read`] and [`Table::build`](super::Table::build) raise.
pub(crate) fn probe(
    path: &Path,
    format: TableFormat,
    schema: &Schema,
    index: &Index,
    ceiling: u64,
) -> Result<(), TableError> {
    let reader = BufReader::new(fs::File::open(path)?);
    Table::build(read(reader, format, schema, ceiling)?, index).map(drop)
}

/// Read a CSV into columns and rows.
fn read_csv(
    reader: impl BufRead,
    header: bool,
    schema: &Schema,
    ceiling: u64,
) -> Result<Parsed, TableError> {
    let mut records = records(reader, ceiling)?.into_iter();

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
///
/// Streamed element by element. Parsing the document whole and then measuring
/// the rows built out of it would put the ceiling behind the allocation it
/// exists to stop, which is no ceiling at all.
fn read_json(reader: impl BufRead, schema: &Schema, ceiling: u64) -> Result<Parsed, TableError> {
    let meter = Meter::default();
    let mut document = serde_json::Deserializer::from_reader(Metered {
        inner: reader,
        meter: &meter,
    });
    let mut stopped = None;

    let read = document.deserialize_seq(Elements {
        schema,
        ceiling,
        meter: &meter,
        stopped: &mut stopped,
    });

    // A reader that stops mid-array leaves the deserializer standing on a comma,
    // which it then reports as a malformed document. What stopped the read is
    // the answer, so it is taken ahead of that.
    if let Some(e) = stopped {
        return Err(e);
    }

    let parsed = read.map_err(|e| not_an_array(&e))?;
    // Anything after the array is a malformed document rather than a second one.
    document.end().map_err(|e| not_an_array(&e))?;

    Ok(parsed)
}

/// A `serde_json` refusal as the refusal it caused.
fn not_an_array(e: &serde_json::Error) -> TableError {
    TableError::NotAnArrayOfObjects {
        detail: e.to_string(),
    }
}

/// How many bytes of the document the element being read may still take.
///
/// One element is parsed whole before its row can be measured, so its text is
/// what the ceiling bounds while it is read: a single string or array the size
/// of the file would otherwise be held in full before the first check.
#[derive(Default)]
struct Meter {
    /// Bytes left for the element being read, `None` between elements.
    room: cell::Cell<Option<u64>>,

    /// Whether an element ran out of room.
    tripped: cell::Cell<bool>,
}

/// A document reader that refuses to read an element past its room.
struct Metered<'m, R> {
    inner: R,
    meter: &'m Meter,
}

impl<R: io::Read> io::Read for Metered<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(room) = self.meter.room.get() else {
            return self.inner.read(buf);
        };
        if room == 0 {
            self.meter.tripped.set(true);
            return Err(io::Error::other("an element outgrew the resident ceiling"));
        }

        let take = buf.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..take])?;
        self.meter
            .room
            .set(Some(room - u64::try_from(read).unwrap_or(room)));
        Ok(read)
    }
}

/// One array element, read with `room` bytes of the document to take.
///
/// The separator and whitespace before it are read before the room is set, so
/// a pretty-printed document is charged for its elements and not its layout.
struct Element<'m> {
    meter: &'m Meter,
    room: u64,
}

impl<'de> de::DeserializeSeed<'de> for Element<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(self, element: D) -> Result<Value, D::Error> {
        self.meter.room.set(Some(self.room));
        let value = Value::deserialize(element);
        self.meter.room.set(None);
        value
    }
}

/// Abandon the walk, keeping the reason for the caller to read.
///
/// The error handed back to serde is the one that ends the walk; the caller
/// never sees it, because it checks what was kept here first.
fn stop<E: de::Error>(kept: &mut Option<TableError>, reason: TableError) -> E {
    let text = reason.to_string();
    *kept = Some(reason);
    E::custom(text)
}

/// The elements of a JSON array, taken one at a time and turned into rows.
///
/// A document that names its columns nowhere widens as it goes: a key first seen
/// on the hundredth object is inserted into the ninety-nine rows already read,
/// which is how the union of every object's keys is reached without holding
/// every object to compute it.
struct Elements<'a> {
    /// Where the column names come from.
    schema: &'a Schema,

    /// Bytes the rows may cost before the read is abandoned.
    ceiling: u64,

    /// What the element being read may still take.
    meter: &'a Meter,

    /// Why the walk stopped, where it stopped before the array ended.
    stopped: &'a mut Option<TableError>,
}

impl<'de> de::Visitor<'de> for Elements<'_> {
    type Value = Parsed;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of objects")
    }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let stated = matches!(self.schema, Schema::Named(_));
        let mut columns: Vec<String> = match self.schema {
            Schema::Auto => Vec::new(),
            Schema::Named(named) => named.clone(),
        };
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut held: u64 = 0;
        let mut seen = 0usize;

        loop {
            let element = Element {
                meter: self.meter,
                room: self.ceiling.saturating_sub(held),
            };
            let value = match seq.next_element_seed(element) {
                Ok(Some(value)) => value,
                Ok(None) => break,
                Err(_) if self.meter.tripped.get() => {
                    return Err(stop(
                        self.stopped,
                        TableError::OverResidentCeiling {
                            ceiling: self.ceiling,
                        },
                    ));
                }
                Err(e) => return Err(e),
            };

            let Some(object) = value.as_object() else {
                return Err(stop(
                    self.stopped,
                    TableError::NotAnArrayOfObjects {
                        detail: format!("element {seen} is not an object"),
                    },
                ));
            };

            if !stated {
                // Sorted rather than first-seen: a JSON object has no key order
                // to inherit, so sorting is the one ordering that does not
                // depend on which object happened to come first.
                for key in object.keys() {
                    let Err(at) = columns.binary_search(key) else {
                        continue;
                    };
                    if columns.len() >= MAX_FIELDS {
                        return Err(stop(
                            self.stopped,
                            TableError::NotAnArrayOfObjects {
                                detail: format!("the objects carry more than {MAX_FIELDS} keys"),
                            },
                        ));
                    }
                    columns.insert(at, key.clone());
                    for row in &mut rows {
                        row.insert(at, None);
                    }
                    held = held.saturating_add(widened(rows.len()));
                }
            }

            let row: Vec<Cell> = columns
                .iter()
                .map(|column| cell_of_value(object.get(column)))
                .collect();

            held = held.saturating_add(footprint(&row));
            if held > self.ceiling {
                return Err(stop(
                    self.stopped,
                    TableError::OverResidentCeiling {
                        ceiling: self.ceiling,
                    },
                ));
            }

            rows.push(row);
            seen += 1;
        }

        // Checked before the columns are, so an empty document reports that it
        // holds no rows rather than that it names no columns.
        if seen == 0 {
            return Err(stop(self.stopped, TableError::Empty));
        }
        if columns.is_empty() {
            return Err(stop(self.stopped, TableError::NoNames));
        }

        Ok(Parsed { columns, rows })
    }
}

/// What widening `rows` rows by one absent cell costs.
fn widened(rows: usize) -> u64 {
    u64::try_from(rows * CELL_OVERHEAD).unwrap_or(u64::MAX)
}

/// What one row costs in memory, near enough for the ceiling to act on.
fn footprint(row: &[Cell]) -> u64 {
    let cells: usize = row
        .iter()
        .map(|cell| cell.as_deref().map_or(0, str::len) + CELL_OVERHEAD)
        .sum();

    u64::try_from(cells + ROW_OVERHEAD).unwrap_or(u64::MAX)
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

/// Read every CSV record in the reader.
///
/// One exit, through `finish`, because that is where an unterminated quoted
/// field is caught: a second exit that skipped it would accept a malformed file.
fn records(mut reader: impl BufRead, ceiling: u64) -> Result<Vec<Record>, TableError> {
    let mut csv = Csv::new(ceiling);

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

    /// Whether the previous byte ended a record as the CR of a CRLF.
    after_cr: bool,

    /// Bytes the completed records will cost once they are rows.
    held: u64,

    /// Bytes the closed fields of the current record will cost as cells.
    record: u64,

    /// Bytes the records may cost, the one being read included, before the
    /// read is abandoned.
    ceiling: u64,
}

/// Longest one field may grow before the input is called malformed.
///
/// A field is held whole until it closes, so without a ceiling an unclosed
/// quote buffers the entire file into one field. No real column is near this.
const MAX_FIELD: usize = 8 * 1024 * 1024;

/// Most fields one record may carry before the input is called malformed.
///
/// Records are only checked and dropped once they close, so a record that never
/// closes is the other way the reader grows without bound.
const MAX_FIELDS: usize = 4096;

impl Csv {
    /// A reader positioned at the first byte of the first line, holding at most
    /// `ceiling` bytes of completed records.
    fn new(ceiling: u64) -> Self {
        Self {
            records: Vec::new(),
            fields: Vec::new(),
            field: Vec::new(),
            state: State::FieldStart,
            line: 1,
            record_line: 1,
            started: false,
            after_cr: false,
            held: 0,
            record: 0,
            ceiling,
        }
    }

    /// Refuse once the completed records and the record being read cost more
    /// than the ceiling, `open` being what its unclosed field will cost.
    fn charge(&self, open: usize) -> Result<(), TableError> {
        let row = self
            .record
            .saturating_add(u64::try_from(ROW_OVERHEAD + open).unwrap_or(u64::MAX));
        if self.held.saturating_add(row) > self.ceiling {
            return Err(TableError::OverResidentCeiling {
                ceiling: self.ceiling,
            });
        }
        Ok(())
    }

    /// Hold one byte of the field being read.
    fn push(&mut self, byte: u8) -> Result<(), TableError> {
        if self.field.len() >= MAX_FIELD {
            return Err(TableError::Malformed {
                line: self.record_line,
                detail: format!("a field is longer than {MAX_FIELD} bytes"),
            });
        }
        self.charge(self.field.len() + 1 + CELL_OVERHEAD)?;
        self.field.push(byte);
        Ok(())
    }

    /// Feed one byte.
    fn byte(&mut self, byte: u8) -> Result<(), TableError> {
        // A record ends at CR, LF or CRLF. The CR of a CRLF ends it, so the LF
        // behind it is already spoken for. Inside a quoted field both are data.
        if self.state != State::Quoted {
            if byte == b'\n' && std::mem::take(&mut self.after_cr) {
                return Ok(());
            }
            self.after_cr = false;
        }

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
                b'\r' => {
                    self.after_cr = true;
                    self.newline()?;
                }
                _ => {
                    self.begin();
                    self.push(byte)?;
                    self.state = State::Unquoted;
                }
            },

            State::Unquoted => match byte {
                DELIMITER => self.end_field()?,
                b'\n' => self.newline()?,
                b'\r' => {
                    self.after_cr = true;
                    self.newline()?;
                }
                // A quote that did not open the field is data: some publishers
                // write inches and apostrophes unescaped.
                _ => self.push(byte)?,
            },

            State::Quoted => match byte {
                QUOTE => self.state = State::QuoteInQuoted,
                b'\n' => {
                    self.line += 1;
                    self.push(byte)?;
                }
                _ => self.push(byte)?,
            },

            State::QuoteInQuoted => match byte {
                QUOTE => {
                    self.push(QUOTE)?;
                    self.state = State::Quoted;
                }
                DELIMITER => self.end_field()?,
                b'\n' => self.newline()?,
                b'\r' => {
                    self.after_cr = true;
                    self.newline()?;
                }
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
        if self.fields.len() >= MAX_FIELDS {
            return Err(TableError::Malformed {
                line: self.record_line,
                detail: format!("a record carries more than {MAX_FIELDS} fields"),
            });
        }

        let bytes = std::mem::take(&mut self.field);
        let text = String::from_utf8(bytes).map_err(|_| TableError::Malformed {
            line: self.record_line,
            detail: "a field is not UTF-8".to_string(),
        })?;

        let cost = u64::try_from(text.len() + CELL_OVERHEAD).unwrap_or(u64::MAX);
        self.record = self.record.saturating_add(cost);
        self.fields.push(text);
        self.state = State::FieldStart;
        // Empty fields cost a cell each and push no byte, so a record of them
        // is charged here rather than in `push`.
        self.charge(0)
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
    ///
    /// The ceiling was charged as the record grew, so the read stops partway
    /// through one that will not fit rather than after holding it whole.
    fn end_record(&mut self) -> Result<(), TableError> {
        self.end_field()?;
        let fields = std::mem::take(&mut self.fields);

        let row = std::mem::take(&mut self.record)
            .saturating_add(u64::try_from(ROW_OVERHEAD).unwrap_or(u64::MAX));
        self.held = self.held.saturating_add(row);

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
            u64::MAX,
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
            u64::MAX,
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
            u64::MAX,
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
            u64::MAX,
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
        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap();

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
        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap();

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
        let parsed = read(body.as_bytes(), TableFormat::Json, &names, u64::MAX).unwrap();

        assert_eq!(parsed.columns, ["ip", "country"]);
        assert_eq!(cells(&parsed), [["1.1.1.1", "AU"]]);
    }

    #[test]
    fn json_scalars_are_text_and_nested_values_keep_their_json() {
        let body = r#"[{"n": 42, "t": true, "z": null, "a": [1, 2], "o": {"k": "v"}}]"#;
        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap();

        assert_eq!(parsed.columns, ["a", "n", "o", "t", "z"]);
        assert_eq!(
            cells(&parsed),
            [["[1,2]", "42", r#"{"k":"v"}"#, "true", ""]]
        );
    }

    #[test]
    fn json_that_is_not_an_array_of_objects_is_refused() {
        for body in [r#"{"relays": []}"#, "[1, 2]", "not json at all"] {
            let err =
                read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap_err();
            assert!(
                matches!(err, TableError::NotAnArrayOfObjects { .. }),
                "{body}: {err:?}"
            );
        }
    }

    #[test]
    fn a_json_read_stops_at_the_ceiling_rather_than_parsing_the_document() {
        // The document is malformed well past the point the ceiling is reached,
        // so a read that reported that had parsed all of it -- which is the
        // allocation the ceiling exists not to make.
        use std::fmt::Write as _;

        let mut body = String::from("[");
        for at in 0..200u32 {
            let asn = 64_500 + at;
            let _ = write!(body, r#"{{"asn": {asn}, "name": "OPERATOR-{asn}"}},"#);
        }
        body.push_str(r#"{"asn": ]"#);

        let stopped = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, 512).unwrap_err();
        assert!(
            matches!(stopped, TableError::OverResidentCeiling { ceiling: 512 }),
            "{stopped:?}"
        );

        // Unbounded, the same document is parsed far enough to meet the fault,
        // so it is the ceiling doing the stopping.
        let read_out =
            read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap_err();
        assert!(
            matches!(read_out, TableError::NotAnArrayOfObjects { .. }),
            "{read_out:?}"
        );
    }

    #[test]
    fn one_element_is_held_to_the_ceiling_while_it_is_read() {
        // Each document is malformed past the point a single element crosses
        // the ceiling, so a read that reported the fault had buffered the whole
        // element first -- the allocation the ceiling exists to stop.
        let long_string = format!(r#"[{{"name": "{}"#, "x".repeat(1 << 20));
        let wide_array = format!(r#"[{{"asn": 1}}, {{"seen": [{}"#, "1,".repeat(1 << 19));
        let many_keys: String = std::iter::once("[{".to_string())
            .chain((0..1 << 16).map(|key| format!(r#""k{key}": null,"#)))
            .collect();

        for (shape, body) in [
            ("one long string", long_string),
            ("one wide array", wide_array),
            ("one wide object", many_keys),
        ] {
            let err = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, 4096).unwrap_err();
            assert!(
                matches!(err, TableError::OverResidentCeiling { ceiling: 4096 }),
                "{shape}: {err:?}"
            );

            // Unbounded, the same document is read far enough to meet the
            // fault, so it is the ceiling doing the stopping.
            let unbounded =
                read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap_err();
            assert!(
                matches!(unbounded, TableError::NotAnArrayOfObjects { .. }),
                "{shape}: {unbounded:?}"
            );
        }
    }

    #[test]
    fn an_element_that_fits_beside_the_rows_already_read_is_admitted() {
        // The element is charged against what the ceiling has left, so a table
        // that fits still loads rather than being refused for its last row.
        let body = r#"[{"asn": 13335, "name": "CLOUDFLARENET"}, {"asn": 15169, "name": "GOOGLE"}]"#;
        let whole = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap();
        let cost: u64 = whole.rows.iter().map(|row| footprint(row)).sum();

        let parsed = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, cost).unwrap();
        assert_eq!(cells(&parsed), cells(&whole));
    }

    #[test]
    fn one_record_is_held_to_the_ceiling_while_it_is_read() {
        // As for JSON: each record is malformed past the point it crosses the
        // ceiling, so reporting that fault means the record was held whole.
        let long_field = format!("asn,name\n1,\"{}", "x".repeat(1 << 20));
        let many_fields = format!("asn,name\n{}", "a,".repeat(MAX_FIELDS + 1));

        for (shape, body) in [
            ("one long field", long_field),
            ("one wide record", many_fields),
        ] {
            let err = read(
                body.as_bytes(),
                TableFormat::Csv { header: true },
                &Schema::Auto,
                1024,
            )
            .unwrap_err();
            assert!(
                matches!(err, TableError::OverResidentCeiling { ceiling: 1024 }),
                "{shape}: {err:?}"
            );

            let unbounded = csv(&body).unwrap_err();
            assert!(
                matches!(unbounded, TableError::Malformed { .. }),
                "{shape}: {unbounded:?}"
            );
        }
    }

    #[test]
    fn a_json_element_that_is_not_an_object_is_named_by_its_position() {
        // The walk stops on it, which leaves the document mid-array. Reporting
        // what the deserializer then says about the remainder would lose the
        // element that was actually wrong.
        let body = r#"[{"ip": "1.1.1.1"}, {"ip": "8.8.8.8"}, 3]"#;

        let err = read(body.as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap_err();

        assert!(
            matches!(&err, TableError::NotAnArrayOfObjects { detail } if detail == "element 2 is not an object"),
            "{err:?}"
        );
    }

    #[test]
    fn an_empty_json_array_holds_no_rows() {
        let err = read("[]".as_bytes(), TableFormat::Json, &Schema::Auto, u64::MAX).unwrap_err();
        assert!(matches!(err, TableError::Empty), "{err:?}");
    }

    /// Probe `file` as a headed CSV keyed by `asn`, under `ceiling`.
    fn probe_csv(file: &Path, schema: &Schema, ceiling: u64) -> Result<(), TableError> {
        probe(
            file,
            TableFormat::Csv { header: true },
            schema,
            &Index::Column("asn".to_string()),
            ceiling,
        )
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
        probe_csv(file_with(&file, &body), &Schema::Auto, u64::MAX).unwrap();

        // Deep enough that a bounded check would admit the file, replace a good
        // database with it, and only then fail at load.
        body.push_str("1,too,many,fields\n");
        let err = probe_csv(file_with(&file, &body), &Schema::Auto, u64::MAX).unwrap_err();
        assert!(
            matches!(err, TableError::Malformed { line: 5002, .. }),
            "{err:?}"
        );
    }

    /// Write `body` to `file` and hand the path back.
    fn file_with<'a>(file: &'a Path, body: &str) -> &'a Path {
        fs::write(file, body).unwrap();
        file
    }

    #[test]
    fn a_probe_refuses_a_file_the_load_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rows.csv");
        let named = Schema::Named(vec!["asn".to_string(), "name".to_string()]);

        for (body, schema, ceiling) in [
            ("asn,name\n", &Schema::Auto, u64::MAX),
            ("asn,name\n1,two,three\n", &Schema::Auto, u64::MAX),
            // Consistent at three columns, and the schema names two.
            ("asn,name,extra\n1,one,x\n", &named, u64::MAX),
            // The index column is not one the file has.
            ("number,name\n1,one\n", &Schema::Auto, u64::MAX),
            // The rows cost more than the ceiling allows.
            ("asn,name\n1,one\n2,two\n", &Schema::Auto, 64),
        ] {
            let result = probe_csv(file_with(&file, body), schema, ceiling);
            assert!(result.is_err(), "{body:?} was admitted");
        }

        fs::write(&file, b"{\"relays\": []}").unwrap();
        assert!(matches!(
            probe(
                &file,
                TableFormat::Json,
                &Schema::Auto,
                &Index::Column("asn".to_string()),
                u64::MAX
            )
            .unwrap_err(),
            TableError::NotAnArrayOfObjects { .. }
        ));
    }

    #[test]
    fn a_lone_carriage_return_ends_a_record_rather_than_vanishing() {
        // Classic Mac line endings. Dropping the byte instead would silently
        // glue every row into one.
        let parsed = csv("asn,name\r1,one\r2,two\r").unwrap();

        assert_eq!(parsed.columns, ["asn", "name"]);
        assert_eq!(cells(&parsed), [["1", "one"], ["2", "two"]]);
    }

    #[test]
    fn a_crlf_line_ending_is_one_record_and_one_line() {
        let err = csv("asn,name\r\n1,one\r\n2,three,too many\r\n").unwrap_err();

        // Line 3, not 5: the CR and the LF of one ending are one line between
        // them.
        assert!(
            matches!(err, TableError::Malformed { line: 3, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_carriage_return_inside_a_quoted_field_stays_data() {
        let parsed = csv("asn,name\n1,\"a\rb\"\n").unwrap();

        assert_eq!(cells(&parsed), [["1", "a\rb"]]);
    }

    #[test]
    fn a_field_that_never_closes_is_refused_rather_than_buffered() {
        // An unclosed quote otherwise holds the whole file in one field, so a
        // large download decides how much memory the reader takes.
        let mut body = String::from("asn,name\n1,\"");
        body.push_str(&"x".repeat(MAX_FIELD + 1));

        let err = csv(&body).unwrap_err();
        assert!(
            matches!(&err, TableError::Malformed { detail, .. } if detail.contains("longer than")),
            "{err:?}"
        );
    }

    #[test]
    fn a_record_that_never_ends_is_refused_rather_than_buffered() {
        let mut body = String::from("asn,name\n");
        body.push_str(&"a,".repeat(MAX_FIELDS + 1));

        let err = csv(&body).unwrap_err();
        assert!(
            matches!(&err, TableError::Malformed { detail, .. } if detail.contains("more than")),
            "{err:?}"
        );
    }
}
