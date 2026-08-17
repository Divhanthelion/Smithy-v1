//! Reading `rust-analyzer scip` output.
//!
//! ## Why a hand-rolled protobuf reader
//!
//! SCIP is a Protocol Buffers schema with a few dozen messages. We need four
//! fields from it — a document's path, and each occurrence's range, symbol and
//! roles — and nothing else. Pulling in `prost` plus a `build.rs` codegen step to
//! decode four fields would add a compile-time dependency and a generated module
//! far larger than the code below.
//!
//! Protobuf's wire format makes this safe to do by hand, and that is the actual
//! justification rather than mere thrift: every field is length-prefixed or
//! self-delimiting, so **unknown fields skip correctly without knowing what they
//! are**. A reader that understands four fields and ignores the rest stays
//! correct when the schema grows, which is exactly the property a hand-rolled
//! parser usually lacks.
//!
//! ## Why SCIP rather than LSIF
//!
//! Both are emitted by the same rust-analyzer subcommands. Measured on the same
//! project: SCIP 1.2 MB, LSIF 6.2 MB, for equivalent information. This file is
//! read on every launch, so five times smaller wins.
//!
//! ## What is deliberately not here
//!
//! `Occurrence.enclosing_range` — the field whose own specification names call
//! hierarchies as its purpose. **rust-analyzer does not emit it**, verified by
//! walking 25 documents of real output: occurrences carry only `range`, `symbol`
//! and `symbol_roles`. Enclosure therefore comes from
//! [`crate::symbols::SymbolIndex::enclosing`], built from tree-sitter spans.

/// One token, and what it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    /// The SCIP moniker, e.g.
    /// `rust-analyzer cargo kernelosv2 0.2.0 components/desktop/Desktop#restore_session().`
    pub symbol: String,
    /// 1-based, converted from SCIP's 0-based rows so it can be compared
    /// directly against [`crate::symbols::Symbol::line`].
    pub line: usize,
    /// `SymbolRole` bitmask.
    pub roles: u32,
}

impl Occurrence {
    /// `SymbolRole.Definition`. Everything else is a use of the symbol — which,
    /// for a function, is a call.
    pub fn is_definition(&self) -> bool {
        self.roles & 0x1 != 0
    }

    /// `SymbolRole.Import`. A `use` statement mentions a symbol without calling
    /// it, so these must not become edges.
    pub fn is_import(&self) -> bool {
        self.roles & 0x2 != 0
    }
}

/// One source file's occurrences.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    /// Relative to the project root, matching how [`crate::symbols`] names files.
    pub relative_path: String,
    pub occurrences: Vec<Occurrence>,
}

/// A parsed SCIP index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScipIndex {
    pub documents: Vec<Document>,
}

impl ScipIndex {
    pub fn occurrence_count(&self) -> usize {
        self.documents.iter().map(|d| d.occurrences.len()).sum()
    }

    /// How many occurrences carry any role at all.
    ///
    /// Pinned by a test against real output, because it is the cheapest single
    /// number that would change if the reader started skipping fields wrongly.
    pub fn roled_count(&self) -> usize {
        self.documents
            .iter()
            .flat_map(|d| d.occurrences.iter())
            .filter(|o| o.roles != 0)
            .count()
    }

    /// Parse a `.scip` file.
    pub fn from_file(path: &std::path::Path) -> Result<ScipIndex, String> {
        let bytes =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        ScipIndex::parse(&bytes)
    }

    /// Parse SCIP bytes.
    ///
    /// Tolerant by design. A truncated or partly unreadable index yields the
    /// documents that did parse rather than an error: `rust-analyzer scip` was
    /// observed logging `ERROR Bug: definition … should have been in an SCIP
    /// document but was not` on a real project, so treating the index as
    /// all-or-nothing would throw away a working graph over two bad entries.
    pub fn parse(bytes: &[u8]) -> Result<ScipIndex, String> {
        let mut index = ScipIndex::default();
        let mut reader = Reader::new(bytes);

        while let Some((field, value)) = reader.next_field() {
            // Index.documents == 2
            if field == 2 {
                if let Value::Bytes(buf) = value {
                    if let Some(document) = parse_document(buf) {
                        index.documents.push(document);
                    }
                }
            }
        }
        Ok(index)
    }
}

fn parse_document(bytes: &[u8]) -> Option<Document> {
    let mut document = Document::default();
    let mut reader = Reader::new(bytes);

    while let Some((field, value)) = reader.next_field() {
        match (field, value) {
            // Document.relative_path == 1
            (1, Value::Bytes(buf)) => {
                document.relative_path = String::from_utf8_lossy(buf).into_owned();
            }
            // Document.occurrences == 2
            (2, Value::Bytes(buf)) => {
                if let Some(occurrence) = parse_occurrence(buf) {
                    document.occurrences.push(occurrence);
                }
            }
            _ => {}
        }
    }
    Some(document)
}

fn parse_occurrence(bytes: &[u8]) -> Option<Occurrence> {
    let mut symbol = String::new();
    let mut roles = 0u32;
    let mut line: Option<usize> = None;
    let mut reader = Reader::new(bytes);

    while let Some((field, value)) = reader.next_field() {
        match (field, value) {
            // Occurrence.range == 1, a packed `repeated int32`.
            //
            // Three or four elements: `[startLine, startChar, endLine, endChar]`
            // or `[startLine, startChar, endChar]` when it is all on one line.
            // Only the start line is wanted, and it is first either way.
            (1, Value::Bytes(buf)) => {
                let mut packed = Reader::new(buf);
                if let Some(start_row) = packed.varint() {
                    // SCIP rows are 0-based; `Symbol::line` is 1-based.
                    line = Some(start_row as usize + 1);
                }
            }
            // A non-packed encoder would send each element separately.
            (1, Value::Varint(v)) if line.is_none() => line = Some(v as usize + 1),
            // Occurrence.symbol == 2
            (2, Value::Bytes(buf)) => symbol = String::from_utf8_lossy(buf).into_owned(),
            // Occurrence.symbol_roles == 3
            (3, Value::Varint(v)) => roles = v as u32,
            _ => {}
        }
    }

    // An occurrence with no symbol refers to nothing and cannot become an edge.
    if symbol.is_empty() {
        return None;
    }
    Some(Occurrence {
        symbol,
        line: line.unwrap_or(0),
        roles,
    })
}

/// A decoded field value. Only the wire types that carry data we read.
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// A minimal protobuf field reader.
///
/// Stops at the first malformed byte rather than panicking or looping: the input
/// is a file on disk that another program wrote, and a parser that can hang on
/// it is a parser that can hang the editor.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *self.buf.get(self.pos)?;
            self.pos += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
            // A varint longer than ten bytes cannot fit in a u64 and means the
            // stream is corrupt. Bail rather than shifting into oblivion.
            if shift >= 64 {
                return None;
            }
        }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    /// The next `(field_number, value)`, skipping wire types we do not read.
    fn next_field(&mut self) -> Option<(u32, Value<'a>)> {
        loop {
            if self.pos >= self.buf.len() {
                return None;
            }
            let key = self.varint()?;
            let field = (key >> 3) as u32;
            match key & 7 {
                0 => return Some((field, Value::Varint(self.varint()?))),
                1 => {
                    self.take(8)?;
                }
                2 => {
                    let len = self.varint()? as usize;
                    return Some((field, Value::Bytes(self.take(len)?)));
                }
                5 => {
                    self.take(4)?;
                }
                // Groups (3, 4) are deprecated and never appear in SCIP. An
                // unknown wire type means the stream is not what we think it is.
                _ => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- encoding helpers, so the fixtures are readable ---

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn tag(field: u32, wire: u8) -> Vec<u8> {
        varint(((field as u64) << 3) | wire as u64)
    }

    fn delimited(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = tag(field, 2);
        out.extend(varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn varint_field(field: u32, value: u64) -> Vec<u8> {
        let mut out = tag(field, 0);
        out.extend(varint(value));
        out
    }

    /// `[startLine, startChar, endLine, endChar]`, packed.
    fn range(start_line: u64, start_char: u64, end_line: u64, end_char: u64) -> Vec<u8> {
        let mut packed = Vec::new();
        for v in [start_line, start_char, end_line, end_char] {
            packed.extend(varint(v));
        }
        delimited(1, &packed)
    }

    fn occurrence(symbol: &str, start_line: u64, roles: u64) -> Vec<u8> {
        let mut body = range(start_line, 4, start_line, 12);
        body.extend(delimited(2, symbol.as_bytes()));
        if roles != 0 {
            body.extend(varint_field(3, roles));
        }
        body
    }

    fn document(path: &str, occurrences: &[Vec<u8>]) -> Vec<u8> {
        let mut body = delimited(1, path.as_bytes());
        for o in occurrences {
            body.extend(delimited(2, o));
        }
        body
    }

    fn index(documents: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for d in documents {
            out.extend(delimited(2, d));
        }
        out
    }

    // --- the reader ---

    #[test]
    fn a_document_and_its_occurrences_round_trip() {
        let bytes = index(&[document(
            "src/lib.rs",
            &[
                occurrence("crate/foo().", 10, 0x1),
                occurrence("crate/bar().", 20, 0),
            ],
        )]);
        let parsed = ScipIndex::parse(&bytes).unwrap();

        assert_eq!(parsed.documents.len(), 1);
        let doc = &parsed.documents[0];
        assert_eq!(doc.relative_path, "src/lib.rs");
        assert_eq!(doc.occurrences.len(), 2);
        assert_eq!(doc.occurrences[0].symbol, "crate/foo().");
        assert!(doc.occurrences[0].is_definition());
        assert!(!doc.occurrences[1].is_definition());
    }

    /// SCIP rows are 0-based; every line number elsewhere in this crate is
    /// 1-based. Getting this wrong shifts every edge by one line and attributes
    /// calls to whatever is above them.
    #[test]
    fn scip_rows_are_converted_to_one_based_lines() {
        let bytes = index(&[document("a.rs", &[occurrence("crate/f().", 0, 0)])]);
        let parsed = ScipIndex::parse(&bytes).unwrap();
        assert_eq!(parsed.documents[0].occurrences[0].line, 1);
    }

    /// A `use` statement mentions a symbol without calling it.
    #[test]
    fn imports_are_distinguishable_from_calls() {
        let bytes = index(&[document("a.rs", &[occurrence("crate/f().", 3, 0x2)])]);
        let parsed = ScipIndex::parse(&bytes).unwrap();
        let o = &parsed.documents[0].occurrences[0];
        assert!(o.is_import());
        assert!(!o.is_definition());
    }

    /// The property that makes a hand-rolled reader defensible: fields we do not
    /// know are length-prefixed or self-delimiting, so they skip cleanly and the
    /// reader keeps working when the schema grows.
    #[test]
    fn unknown_fields_are_skipped_without_disturbing_the_rest() {
        let mut body = range(7, 0, 7, 5);
        body.extend(varint_field(99, 12345)); // unknown varint
        body.extend(delimited(98, b"something we do not read")); // unknown bytes
        body.extend(tag(97, 5)); // unknown 32-bit
        body.extend([0, 0, 0, 0]);
        body.extend(tag(96, 1)); // unknown 64-bit
        body.extend([0, 0, 0, 0, 0, 0, 0, 0]);
        body.extend(delimited(2, b"crate/survivor()."));
        body.extend(varint_field(3, 0x1));

        let bytes = index(&[document("a.rs", &[body])]);
        let parsed = ScipIndex::parse(&bytes).unwrap();
        let o = &parsed.documents[0].occurrences[0];
        assert_eq!(o.symbol, "crate/survivor().");
        assert_eq!(o.line, 8);
        assert!(o.is_definition());
    }

    /// A three-element range omits the end line. The start line is first either
    /// way, which is the only element read.
    #[test]
    fn a_three_element_range_still_yields_its_start_line() {
        let mut packed = Vec::new();
        for v in [42u64, 4, 12] {
            packed.extend(varint(v));
        }
        let mut body = delimited(1, &packed);
        body.extend(delimited(2, b"crate/f()."));
        let bytes = index(&[document("a.rs", &[body])]);
        let parsed = ScipIndex::parse(&bytes).unwrap();
        assert_eq!(parsed.documents[0].occurrences[0].line, 43);
    }

    /// An occurrence naming nothing cannot become an edge.
    #[test]
    fn an_occurrence_without_a_symbol_is_dropped() {
        let bytes = index(&[document("a.rs", &[range(1, 0, 1, 5)])]);
        let parsed = ScipIndex::parse(&bytes).unwrap();
        assert!(parsed.documents[0].occurrences.is_empty());
    }

    /// `rust-analyzer scip` was observed logging `ERROR Bug:` on a real project.
    /// Losing a whole graph over a truncated tail would be the wrong trade.
    #[test]
    fn a_truncated_index_yields_what_did_parse() {
        let bytes = index(&[
            document("good.rs", &[occurrence("crate/f().", 1, 0x1)]),
            document("also_good.rs", &[occurrence("crate/g().", 1, 0x1)]),
        ]);
        let cut = &bytes[..bytes.len() - 5];
        let parsed = ScipIndex::parse(cut).unwrap();
        assert_eq!(parsed.documents.len(), 1, "the intact document survives");
        assert_eq!(parsed.documents[0].relative_path, "good.rs");
    }

    /// Garbage must terminate, not hang. This file is written by another
    /// program and a parser that can loop on it can hang the editor.
    #[test]
    fn malformed_input_terminates_rather_than_looping() {
        for junk in [
            vec![0xff; 64],         // varints that never terminate
            vec![0x0a, 0xff, 0xff], // a length prefix past the end
            vec![0x07],             // an unknown wire type
            vec![],
        ] {
            let parsed = ScipIndex::parse(&junk).unwrap();
            assert!(parsed.documents.is_empty() || parsed.occurrence_count() == 0);
        }
    }

    #[test]
    fn counts_are_summed_across_documents() {
        let bytes = index(&[
            document(
                "a.rs",
                &[
                    occurrence("crate/f().", 1, 0x1),
                    occurrence("crate/g().", 2, 0),
                ],
            ),
            document("b.rs", &[occurrence("crate/h().", 1, 0x8)]),
        ]);
        let parsed = ScipIndex::parse(&bytes).unwrap();
        assert_eq!(parsed.occurrence_count(), 3);
        assert_eq!(parsed.roled_count(), 2);
    }
}
