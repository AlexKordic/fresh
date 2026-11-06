//! Parser-owned table structure for native, measured cells. No widths,
//! padding or terminal frame strings are part of this model.

use pulldown_cmark::{Alignment, Event, Tag, TagEnd};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

#[path = "compose_table_measurements.rs"]
mod measurements;
pub(crate) use measurements::{Column, Measurement, Sample, Style};

#[derive(Clone, Debug)]
pub(crate) struct Table {
    pub start: usize,
    pub end: usize,
    pub alignments: Vec<u32>,
    pub rows: Vec<Row>,
    pub measurement_id: u64,
    pub measurement: Option<Arc<Measurement>>,
}

impl Hash for Table {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.end.hash(state);
        self.alignments.hash(state);
        self.rows.hash(state);
        self.measurement_id.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresh_core::compose::{Projection, SegmentKind};
    use pulldown_cmark::{Options, Parser};

    fn parse(text: &str) -> Tables {
        let mut tables = Tables::default();
        for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
            tables.event(text, &event, &range);
        }
        tables
    }

    fn project(text: &str, tables: &Tables, cursors: &[usize]) -> (Vec<Table>, Projection) {
        let snapshot = tables.snapshot(text, 0..text.len(), cursors);
        let projection =
            Projection::new(text, 0, &tables.conceals(text, 0..text.len(), &snapshot)).unwrap();
        (snapshot, projection)
    }

    #[test]
    fn parser_cells_alignments_empty_anchors_and_escapes_do_not_depend_on_columns() {
        let text = "| Head | Value |\r\n| :--- | ---: |\r\n| **café** | a\\|b |\r\n| | |\r\n";
        let tables = parse(text);
        let (snapshot, p) = project(text, &tables, &[]);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].alignments, [1, 3]);
        assert_eq!(snapshot[0].rows.len(), 3);
        assert!(snapshot[0].rows[0].header);
        assert!(snapshot[0]
            .rows
            .iter()
            .all(|r| r.cells.len() == 2 && !r.source));
        assert!(snapshot[0].rows[2].cells.iter().all(|c| !c.missing));
        assert_eq!(p.text, "Head\tValue\r\ncafé\ta|b\r\n\t\r\n");
        let café = text.find("café").unwrap();
        assert!(p.segments.iter().any(|s| s.kind == SegmentKind::Copy
            && s.source.start <= café
            && s.source.end >= café + "café".len()));
        let (revealed, p) = project(text, &tables, &[café]);
        assert!(revealed[0].rows[1].cells[0].revealed);
        assert_eq!(p.text, "Head\tValue\r\n **café** \ta|b\r\n\t\r\n");
        for eol in ["\n", "\r\n", ""] {
            let text = format!("A|B\n---|---\n|||{eol}");
            let tables = parse(&text);
            let (snapshot, p) = project(&text, &tables, &[]);
            assert!(snapshot[0].rows[1]
                .cells
                .iter()
                .all(|cell| !cell.missing && cell.start == cell.end));
            assert_eq!(p.text, format!("A\tB\n\t{eol}"));
        }
    }

    #[test]
    fn structural_rows_and_alignment_source_reveal_without_losing_cell_ownership() {
        let text = "| A | B |\n|:---|---:|\n| one | two |\n";
        let tables = parse(text);
        let (snapshot, p) = project(text, &tables, &[0]);
        assert!(snapshot[0].rows[0].source);
        assert!(p.text.starts_with("| A | B |\n"));
        let separator = text.find(":---").unwrap();
        let (snapshot, p) = project(text, &tables, &[separator]);
        assert_eq!(snapshot[0].rows.len(), 3);
        assert!(snapshot[0].rows[1].source && snapshot[0].rows[1].cells.is_empty());
        assert_eq!(p.text, "A\tB\n|:---|---:|\none\ttwo\n");
        let end = text.find("one").unwrap() + 3;
        let (snapshot, _) = project(text, &tables, &[end]);
        assert!(!snapshot[0].rows[1].source);
        assert!(snapshot[0].rows[1].cells[0].revealed);
    }

    #[test]
    fn quoted_cells_keep_image_markup_opaque_and_retain_unsupported_leaf_syntax() {
        let text = "> A | B\n> --- | ---\n> ![**pic**](a.png) | `code` &amp; $x$ [^note]\n";
        let tables = parse(text);
        let (snapshot, p) = project(text, &tables, &[]);
        assert_eq!(snapshot[0].start, 0);
        assert_eq!(snapshot[0].rows[1].start, text.find("> ![").unwrap());
        assert_eq!(p.text, "A\tB\n![**pic**](a.png)\tcode & $x$ [^note]\n");
    }

    #[test]
    fn short_rows_do_not_invent_editable_cells_and_windows_keep_complete_rows() {
        let text = "| A | B | C |\n|---|---|---|\n|x|\n|one|two|three|\n";
        let tables = parse(text);
        let (snapshot, p) = project(text, &tables, &[]);
        let row = &snapshot[0].rows[1];
        assert!(!row.cells[0].missing);
        assert!(row.cells[1..]
            .iter()
            .all(|c| c.missing && c.start == row.content_end && c.end == row.content_end));
        assert_eq!(p.text, "A\tB\tC\nx\none\ttwo\tthree\n");
        let last = text.find("|one").unwrap();
        let snapshot = tables.snapshot(text, last..text.len(), &[]);
        assert_eq!(snapshot[0].rows.len(), 1);
        assert_eq!(snapshot[0].rows[0].start, last);
        assert_eq!(
            snapshot[0].rows[0].previous,
            Some(text.find("|x|").unwrap())
        );
        assert_eq!(snapshot[0].rows[0].next, None);
        let p = Projection::new(
            &text[last..],
            last,
            &tables.conceals(text, last..text.len(), &snapshot),
        )
        .unwrap();
        assert_eq!(p.text, "one\ttwo\tthree\n");
        let snapshot = tables.snapshot(text, last + 1..text.len(), &[]);
        assert!(
            snapshot.is_empty(),
            "partial rows retain source fallback instead of hiding unclassified content"
        );
    }
}

#[derive(Clone, Debug, Hash)]
pub(crate) struct Row {
    pub start: usize,
    pub end: usize,
    pub content_end: usize,
    pub header: bool,
    /// Structural source editing, including the alignment delimiter line.
    pub source: bool,
    pub cells: Vec<Cell>,
    /// Parser-owned neighbors, including rows outside the published window.
    /// These stay on the backend; native geometry addresses rows by source.
    pub previous: Option<usize>,
    pub next: Option<usize>,
}

#[derive(Clone, Debug, Hash)]
pub(crate) struct Cell {
    pub start: usize,
    pub end: usize,
    pub revealed: bool,
    /// The parser pads a short row with cells that have no source bytes.
    pub missing: bool,
}

struct Part {
    source: Range<usize>,
    text: Option<String>,
}

struct ParsedCell {
    source: Range<usize>,
    parts: Vec<Part>,
}

struct ParsedRow {
    source: Range<usize>,
    header: bool,
    cells: Vec<ParsedCell>,
}

struct ParsedTable {
    source: Range<usize>,
    alignments: Vec<u32>,
    rows: Vec<ParsedRow>,
    measurement_id: u64,
    measurement: Option<Arc<Measurement>>,
}

#[derive(Default)]
pub(super) struct Tables {
    parsed: Vec<ParsedTable>,
    active: bool,
    image_depth: usize,
}

fn physical_start(text: &str, position: usize) -> usize {
    text[..position].rfind(['\r', '\n']).map_or(0, |p| p + 1)
}

fn row_start(text: &str, source: &Range<usize>) -> usize {
    let start = physical_start(text, source.start);
    // Consume container indentation/quotes, but leave an item's own marker
    // to the enclosing list presentation.
    if text[start..source.start]
        .bytes()
        .all(|c| matches!(c, b' ' | b'\t' | b'>'))
    {
        start
    } else {
        source.start
    }
}

fn content_end(text: &str, source: &Range<usize>) -> usize {
    source.start + text[source.clone()].trim_end_matches(['\r', '\n']).len()
}

impl Tables {
    pub fn event(&mut self, text: &str, event: &Event<'_>, source: &Range<usize>) {
        match event {
            Event::Start(Tag::Table(alignments)) => {
                self.active = true;
                self.parsed.push(ParsedTable {
                    source: source.clone(),
                    alignments: alignments
                        .iter()
                        .map(|a| match a {
                            Alignment::None => 0,
                            Alignment::Left => 1,
                            Alignment::Center => 2,
                            Alignment::Right => 3,
                        })
                        .collect(),
                    rows: Vec::new(),
                    measurement_id: 0,
                    measurement: None,
                });
            }
            Event::End(TagEnd::Table) => self.active = false,
            _ if !self.active => {}
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                self.parsed.last_mut().unwrap().rows.push(ParsedRow {
                    source: source.clone(),
                    header: matches!(event, Event::Start(Tag::TableHead)),
                    cells: Vec::new(),
                });
            }
            Event::Start(Tag::TableCell) => {
                self.parsed
                    .last_mut()
                    .unwrap()
                    .rows
                    .last_mut()
                    .unwrap()
                    .cells
                    .push(ParsedCell {
                        source: source.clone(),
                        parts: Vec::new(),
                    });
            }
            Event::Start(Tag::Image { .. }) => {
                if self.image_depth == 0 {
                    self.part(text, source, None);
                }
                self.image_depth += 1;
            }
            Event::End(TagEnd::Image) => self.image_depth = self.image_depth.saturating_sub(1),
            _ if self.image_depth != 0 => {}
            Event::Text(value) | Event::Code(value) | Event::InlineHtml(value) => {
                self.part(text, source, Some(value))
            }
            Event::Start(_) | Event::End(_) => {}
            _ => self.part(text, source, None),
        }
    }

    fn part(&mut self, text: &str, source: &Range<usize>, displayed: Option<&str>) {
        if let Some(cell) = self
            .parsed
            .last_mut()
            .and_then(|t| t.rows.last_mut())
            .and_then(|r| r.cells.last_mut())
        {
            cell.parts.push(Part {
                source: source.clone(),
                text: displayed
                    .filter(|value| *value != &text[source.clone()])
                    .map(str::to_owned),
            });
        }
    }

    fn separator(text: &str, table: &ParsedTable) -> Range<usize> {
        let start = table.rows[0].source.end;
        start..super::compose::source_line_end(text, start).min(table.source.end)
    }

    pub fn snapshot(&self, text: &str, window: Range<usize>, cursors: &[usize]) -> Vec<Table> {
        self.parsed
            .iter()
            .filter_map(|table| {
                let start = row_start(text, &table.source);
                if start >= window.end || table.source.end <= window.start {
                    return None;
                }
                let separator = Self::separator(text, table);
                let reveal_separator = cursors.iter().any(|p| separator.contains(p));
                let mut starts: Vec<_> = table
                    .rows
                    .iter()
                    .map(|row| row_start(text, &row.source))
                    .collect();
                if reveal_separator {
                    starts.push(separator.start);
                    starts.sort_unstable();
                }
                let neighbors = |start| {
                    let index = starts.binary_search(&start).unwrap();
                    (
                        index.checked_sub(1).map(|i| starts[i]),
                        starts.get(index + 1).copied(),
                    )
                };
                let mut rows: Vec<_> = table
                    .rows
                    .iter()
                    .filter_map(|row| {
                        let start = row_start(text, &row.source);
                        if start < window.start || row.source.end > window.end {
                            return None;
                        }
                        let end = content_end(text, &row.source);
                        let structural = cursors.iter().any(|&p| {
                            p >= start
                                && p <= end
                                && ((p == end
                                    && !row
                                        .cells
                                        .iter()
                                        .any(|c| c.source.start < end && c.source.end == end))
                                    || (text.as_bytes().get(p) == Some(&b'|')
                                        && !row.cells.iter().any(|c| c.source.contains(&p)))
                                    || p < row.source.start)
                        });
                        let cells = row
                            .cells
                            .iter()
                            .map(|cell| {
                                let missing = cell.source.start >= row.source.end;
                                Cell {
                                    start: cell.source.start.min(end),
                                    end: cell.source.end.min(end),
                                    missing,
                                    revealed: !missing
                                        && cursors.iter().any(|p| {
                                            cell.source.start <= *p && *p <= cell.source.end
                                        }),
                                }
                            })
                            .collect();
                        Some(Row {
                            start,
                            end: row.source.end,
                            content_end: end,
                            header: row.header,
                            source: structural,
                            cells,
                            previous: neighbors(start).0,
                            next: neighbors(start).1,
                        })
                    })
                    .collect();
                if separator.start >= window.start
                    && separator.end <= window.end
                    && reveal_separator
                {
                    rows.push(Row {
                        start: separator.start,
                        end: separator.end,
                        content_end: content_end(text, &separator),
                        header: false,
                        source: true,
                        cells: Vec::new(),
                        previous: neighbors(separator.start).0,
                        next: neighbors(separator.start).1,
                    });
                    rows.sort_by_key(|row| row.start);
                }
                (!rows.is_empty()).then_some(Table {
                    start,
                    end: table.source.end,
                    alignments: table.alignments.clone(),
                    rows,
                    measurement_id: table.measurement_id,
                    measurement: table.measurement.clone(),
                })
            })
            .collect()
    }

    /// Source/display projection uses parser text events, never the terminal
    /// table's padding, truncation or wrapped-cell replacement strings.
    pub fn conceals<'a>(
        &'a self,
        text: &'a str,
        window: Range<usize>,
        tables: &[Table],
    ) -> Vec<fresh_core::compose::Conceal<'a>> {
        use fresh_core::compose::Conceal;
        let mut out = Vec::new();
        fn hidden<'a>(out: &mut Vec<Conceal<'a>>, source: Range<usize>) {
            if !source.is_empty() {
                out.push(Conceal {
                    source,
                    replacement: None,
                });
            }
        }
        for table in tables {
            let Some(parsed) = self
                .parsed
                .iter()
                .find(|p| row_start(text, &p.source) == table.start && p.source.end == table.end)
            else {
                continue;
            };
            let separator = Self::separator(text, parsed);
            if !table
                .rows
                .iter()
                .any(|row| row.source && row.start == separator.start)
            {
                hidden(
                    &mut out,
                    separator.start.max(window.start)..separator.end.min(window.end),
                );
            }
            for row in &table.rows {
                if row.source {
                    continue;
                }
                let Some(parsed) = parsed
                    .rows
                    .iter()
                    .find(|p| row_start(text, &p.source) == row.start)
                else {
                    continue;
                };
                let mut from = row.start;
                let mut first = true;
                for (cell, parsed) in row.cells.iter().zip(&parsed.cells) {
                    if cell.missing {
                        continue;
                    }
                    if first {
                        hidden(&mut out, from..cell.start);
                    } else if from < cell.start {
                        out.push(Conceal {
                            source: from..cell.start,
                            replacement: Some("\t"),
                        });
                    }
                    first = false;
                    if !cell.revealed {
                        let mut at = cell.start;
                        for part in &parsed.parts {
                            hidden(&mut out, at..part.source.start);
                            if let Some(value) = &part.text {
                                out.push(Conceal {
                                    source: part.source.clone(),
                                    replacement: Some(value),
                                });
                            }
                            at = part.source.end;
                        }
                        hidden(&mut out, at..cell.end);
                    }
                    from = cell.end;
                }
                hidden(&mut out, from..row.content_end);
            }
        }
        out
    }
}
