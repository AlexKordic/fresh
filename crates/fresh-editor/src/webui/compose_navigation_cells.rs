//! Two-dimensional cells over the same source-ordered navigation lines.
//! Row adjacency remains parser-owned; missing geometry always defers input.

use super::*;
use std::collections::HashSet;
use std::ops::Range;

#[derive(Clone, Debug)]
pub(super) struct Cell {
    pub table: usize,
    pub row: usize,
    pub column: Option<u32>,
    pub part: usize,
    pub parts: usize,
    pub x: f64,
    pub width: f64,
    pub y: f64,
    pub height: f64,
}

impl Cell {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut cell = Self {
            table: 0,
            row: 0,
            column: None,
            part: 0,
            parts: 0,
            x: 0.0,
            width: 0.0,
            y: 0.0,
            height: 0.0,
        };
        for f in fields(bytes) {
            match f? {
                (1, v) => cell.table = v.uint() as usize,
                (2, v) => cell.row = v.uint() as usize,
                (3, v) => {
                    cell.column = Some(u32::try_from(v.uint()).map_err(|_| "invalid table column")?)
                }
                (4, v) => cell.part = v.uint() as usize,
                (5, v) => cell.parts = v.uint() as usize,
                (6, v) => cell.x = number(v)?,
                (7, v) => cell.width = number(v)?,
                (8, v) => cell.y = number(v)?,
                (9, v) => cell.height = number(v)?,
                _ => {}
            }
        }
        if cell.parts == 0
            || cell.part >= cell.parts
            || cell.width <= 0.0
            || cell.height <= 0.0
            || cell.y < 0.0
            || (cell.x + cell.width).abs() > 100_000_000.0
        {
            return Err("incomplete graphical cell".into());
        }
        Ok(cell)
    }
    fn same_row(&self, other: &Self) -> bool {
        self.table == other.table && self.row == other.row
    }
    fn same_axis(&self, other: &Self) -> bool {
        self.same_row(other) && self.column == other.column
    }
}

#[derive(Debug)]
pub(super) struct Band {
    pub lines: Range<usize>,
    pub y: f64,
    pub bottom: f64,
}

/// A parser-owned neighbor survives replacing the source window which held
/// the motion's origin. Pixel line numbers do not survive that replacement.
#[derive(Clone, Copy, Debug)]
pub(super) struct CellDestination {
    table: usize,
    row: usize,
    column: Option<u32>,
    backwards: bool,
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.001
}

pub(super) fn bands(lines: &[Line], height: f64) -> Result<Vec<Band>, String> {
    let mut result: Vec<Band> = Vec::new();
    let mut rows = HashSet::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(previous) = index.checked_sub(1).map(|i| &lines[i]) {
            let start = line
                .cell
                .as_ref()
                .map_or(Some(line.number), |c| line.number.checked_sub(c.part));
            let previous_start = previous.cell.as_ref().map_or(Some(previous.number), |c| {
                previous.number.checked_sub(c.part)
            });
            let same = matches!((&previous.cell, &line.cell), (Some(a), Some(b)) if a.same_axis(b));
            let valid = match (previous_start, start) {
                (Some(a), Some(b)) if same => a == b,
                (Some(a), Some(b)) => a
                    .checked_add(previous.cell.as_ref().map_or(1, |c| c.parts))
                    .is_some_and(|end| b >= end),
                _ => false,
            };
            if !valid {
                return Err("graphical part ordinals disagree with line numbers".into());
            }
        }
        if let Some(cell) = &line.cell {
            if cell.y + cell.height > height + 0.001
                || line.y < cell.y - 0.001
                || line.y + line.height > cell.y + cell.height + 0.001
                || [line.start, line.end]
                    .iter()
                    .chain(&line.stops)
                    .any(|s| s.x < cell.x - 1.0 || s.x > cell.x + cell.width + 1.0)
            {
                return Err("graphical cell does not contain its lines".into());
            }
            if let Some(previous) = index
                .checked_sub(1)
                .and_then(|i| lines[i].cell.as_ref())
                .filter(|p| p.same_row(cell))
            {
                if !close(previous.y, cell.y) || !close(previous.height, cell.height) {
                    return Err("graphical row bounds disagree".into());
                }
                if previous.same_axis(cell) {
                    if previous.parts != cell.parts
                        || previous.part >= cell.part
                        || !close(previous.x, cell.x)
                        || !close(previous.width, cell.width)
                        || line.y < lines[index - 1].y + lines[index - 1].height - 0.001
                    {
                        return Err("graphical cell fragments overlap or disagree".into());
                    }
                } else if !matches!((previous.column, cell.column), (Some(a), Some(b)) if a < b)
                    || cell.x < previous.x + previous.width - 0.001
                {
                    return Err("graphical columns overlap or are out of order".into());
                }
                result.last_mut().unwrap().lines.end = index + 1;
                continue;
            }
            if !rows.insert((cell.table, cell.row)) {
                return Err("graphical row repeats".into());
            }
        }
        let (y, bottom) = line
            .cell
            .as_ref()
            .map_or((line.y, line.y + line.height), |c| (c.y, c.y + c.height));
        if result
            .last()
            .is_some_and(|previous| y < previous.bottom - 0.001)
        {
            return Err("graphical lines overlap".into());
        }
        result.push(Band {
            lines: index..index + 1,
            y,
            bottom,
        });
    }
    Ok(result)
}

pub(super) fn valid_source(line: &Line, tables: &[super::super::compose_tables::Table]) -> bool {
    let Some(cell) = &line.cell else { return true };
    let Some(row) = tables
        .iter()
        .find(|t| t.start == cell.table)
        .and_then(|t| t.rows.iter().find(|r| r.start == cell.row))
    else {
        return false;
    };
    let source = match cell.column {
        None if row.source => row.start..row.end,
        Some(column) if !row.source => match row.cells.get(column as usize) {
            Some(c) if c.missing => row.content_end..row.content_end,
            Some(c) => c.start..c.end,
            None => return false,
        },
        _ => return false,
    };
    line.source_start >= source.start
        && line.source_end <= source.end
        && cell.parts <= source.len().saturating_add(1)
}

impl Layout {
    pub(super) fn resume_cell(
        &self,
        destination: CellDestination,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
    ) -> Result<usize, Need> {
        let result = self
            .bands
            .iter()
            .find(|band| {
                self.lines[band.lines.start]
                    .cell
                    .as_ref()
                    .is_some_and(|cell| {
                        cell.table == destination.table && cell.row == destination.row
                    })
            })
            .ok_or_else(|| Need::region(destination.row))
            .and_then(|band| {
                self.enter(
                    band,
                    destination.column,
                    goal,
                    destination.backwards,
                    tables,
                )
            });
        result.map_err(|need| need.destination(Destination::Cell(destination)))
    }

    fn band_for(&self, line: usize) -> &Band {
        self.bands.iter().find(|b| b.lines.contains(&line)).unwrap()
    }

    pub(super) fn table_row<'a>(
        &self,
        cell: &Cell,
        tables: &'a [super::super::compose_tables::Table],
    ) -> Option<&'a super::super::compose_tables::Row> {
        tables
            .iter()
            .find(|t| t.start == cell.table)?
            .rows
            .iter()
            .find(|r| r.start == cell.row)
    }

    fn cell_axis(
        &self,
        band: &Band,
        preferred: Option<u32>,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
    ) -> Result<Vec<usize>, Need> {
        let first = self.lines[band.lines.start].cell.as_ref().unwrap();
        let row = self
            .table_row(first, tables)
            .ok_or_else(|| Need::region(first.row))?;
        let column = if row.source {
            None
        } else if let Some(column) = preferred {
            Some(column)
        } else {
            let count = row.cells.len();
            let mut axes = Vec::new();
            for column in 0..count {
                let axis = band.lines.clone().find_map(|i| {
                    self.lines[i]
                        .cell
                        .as_ref()
                        .filter(|c| c.column == Some(column as u32))
                });
                axes.push(axis.ok_or_else(|| Need::region(first.row))?);
            }
            axes.into_iter()
                .min_by(|a, b| {
                    let distance = |c: &Cell| (c.x - goal).max(goal - c.x - c.width).max(0.0);
                    distance(a)
                        .total_cmp(&distance(b))
                        .then_with(|| a.column.cmp(&b.column))
                })
                .and_then(|c| c.column)
        };
        let found: Vec<_> = band
            .lines
            .clone()
            .filter(|&i| self.lines[i].cell.as_ref().unwrap().column == column)
            .collect();
        if found.is_empty() {
            return Err(Need::region(first.row));
        }
        Ok(found)
    }

    pub(super) fn enter(
        &self,
        band: &Band,
        preferred: Option<u32>,
        goal: f64,
        backwards: bool,
        tables: &[super::super::compose_tables::Table],
    ) -> Result<usize, Need> {
        if self.lines[band.lines.start].cell.is_none() {
            return Ok(band.lines.start);
        }
        let axis = self.cell_axis(band, preferred, goal, tables)?;
        let index = if backwards {
            *axis.last().unwrap()
        } else {
            axis[0]
        };
        let cell = self.lines[index].cell.as_ref().unwrap();
        if (backwards && cell.part + 1 != cell.parts) || (!backwards && cell.part != 0) {
            return Err(Need::y(if backwards {
                band.bottom - 0.1
            } else {
                band.y + 0.1
            }));
        }
        Ok(index)
    }

    fn vertical(
        &self,
        row: usize,
        backwards: bool,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
        preferred: Option<u32>,
    ) -> Result<usize, Need> {
        let line = &self.lines[row];
        let number = if let Some(cell) = &line.cell {
            let part = if backwards {
                cell.part.checked_sub(1)
            } else {
                (cell.part + 1 < cell.parts).then_some(cell.part + 1)
            };
            if let Some(part) = part {
                return self
                    .band_for(row)
                    .lines
                    .clone()
                    .find(|&i| {
                        self.lines[i]
                            .cell
                            .as_ref()
                            .is_some_and(|c| c.same_axis(cell) && c.part == part)
                    })
                    .ok_or_else(|| {
                        Need::y(if backwards {
                            line.y - 0.1
                        } else {
                            line.y + line.height + 0.1
                        })
                    });
            }
            let source_row = self
                .table_row(cell, tables)
                .ok_or_else(|| Need::region(cell.row))?;
            let neighbor = if backwards {
                source_row.previous
            } else {
                source_row.next
            };
            if let Some(start) = neighbor {
                return self.resume_cell(
                    CellDestination {
                        table: cell.table,
                        row: start,
                        column: cell.column.or(preferred),
                        backwards,
                    },
                    goal,
                    tables,
                );
            }
            // Table bounds remain authoritative when a short cell's origin
            // is absent from the measurement beyond a taller sibling.
            let table = tables
                .iter()
                .find(|t| t.start == cell.table)
                .ok_or_else(|| Need::region(cell.row))?;
            if (backwards && table.start == 0)
                || (!backwards && self.source_end == self.document_len && self.at_end(row))
            {
                return Ok(row);
            }
            return self.resume(
                Destination::Source {
                    edge: if backwards { table.start } else { table.end },
                    backwards,
                },
                goal,
                tables,
            );
        } else {
            line.number
        };
        let wanted = if backwards {
            number.checked_sub(1)
        } else {
            (number + 1 < self.total_lines).then_some(number + 1)
        };
        let Some(wanted) = wanted else {
            if backwards && self.source_start > 0 {
                return self.resume(
                    Destination::Source {
                        edge: self.source_start,
                        backwards,
                    },
                    goal,
                    tables,
                );
            }
            if !backwards && self.source_end < self.document_len {
                return self.resume(
                    Destination::Source {
                        edge: self.source_end,
                        backwards,
                    },
                    goal,
                    tables,
                );
            }
            return Ok(row);
        };
        let target = self
            .lines
            .iter()
            .position(|l| l.number == wanted)
            .ok_or_else(|| {
                Need::y(if backwards {
                    self.band_for(row).y - 0.1
                } else {
                    self.band_for(row).bottom + 0.1
                })
            })?;
        let target_line = &self.lines[target];
        if !backwards
            && self.source_end < self.document_len
            && target_line.source_start == self.source_end
        {
            return self.resume(
                Destination::Source {
                    edge: self.source_end,
                    backwards,
                },
                goal,
                tables,
            );
        }
        if let Some(cell) = &target_line.cell {
            return self.resume_cell(
                CellDestination {
                    table: cell.table,
                    row: cell.row,
                    column: None,
                    backwards,
                },
                goal,
                tables,
            );
        }
        self.enter(self.band_for(target), None, goal, backwards, tables)
    }

    fn page_band(&self, y: f64) -> Result<&Band, Need> {
        if let Some(band) = self.bands.iter().find(|b| y >= b.y && y < b.bottom) {
            return Ok(band);
        }
        let before = self.bands.iter().rev().find(|b| b.bottom <= y);
        let after = self.bands.iter().find(|b| b.y > y);
        match (before, after) {
            (Some(a), Some(b))
                if self.lines[a.lines.end - 1].number + 1 == self.lines[b.lines.start].number =>
            {
                Ok(if y - a.bottom <= b.y - y { a } else { b })
            }
            (None, Some(b)) if self.lines[b.lines.start].number == 0 => Ok(b),
            (Some(a), None) if self.lines[a.lines.end - 1].number + 1 == self.total_lines => Ok(a),
            _ => Err(Need::y(y)),
        }
    }

    fn page(
        &self,
        row: usize,
        backwards: bool,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
        column: Option<u32>,
    ) -> Result<usize, Need> {
        let y = self.lines[row].y
            + if backwards {
                -self.page_height
            } else {
                self.page_height
            };
        if y < 0.0 && self.source_start > 0 {
            return Err(Need::region(self.source_start - 1));
        }
        if y >= self.height && self.source_end < self.document_len {
            return Err(Need::region(self.source_end));
        }
        let y = y.clamp(0.0, (self.height - 0.001).max(0.0));
        let band = self.page_band(y)?;
        let Some(target) = self.lines[band.lines.start].cell.as_ref() else {
            return Ok(band.lines.start);
        };
        let preferred = self.lines[row]
            .cell
            .as_ref()
            .filter(|c| c.table == target.table)
            .and_then(|c| c.column.or(column));
        let axis = self.cell_axis(band, preferred, goal, tables)?;
        if let Some(&index) = axis
            .iter()
            .find(|&&i| y >= self.lines[i].y && y < self.lines[i].y + self.lines[i].height)
        {
            return Ok(index);
        }
        let before = axis
            .iter()
            .rev()
            .copied()
            .find(|&i| self.lines[i].y + self.lines[i].height <= y);
        let after = axis.iter().copied().find(|&i| self.lines[i].y > y);
        let target = match (before, after) {
            (Some(a), Some(b))
                if self.lines[a].cell.as_ref().unwrap().part + 1
                    == self.lines[b].cell.as_ref().unwrap().part =>
            {
                if y - self.lines[a].y - self.lines[a].height <= self.lines[b].y - y {
                    a
                } else {
                    b
                }
            }
            (None, Some(b)) if self.lines[b].cell.as_ref().unwrap().part == 0 => b,
            (Some(a), None)
                if self.lines[a].cell.as_ref().unwrap().part + 1
                    == self.lines[a].cell.as_ref().unwrap().parts =>
            {
                a
            }
            _ => return Err(Need::y(y)),
        };
        Ok(target)
    }

    pub(super) fn target_row(
        &self,
        row: usize,
        motion: Motion,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
        preferred: Option<u32>,
    ) -> Result<usize, Need> {
        match motion {
            Motion::Up | Motion::Down => {
                self.vertical(row, matches!(motion, Motion::Up), goal, tables, preferred)
            }
            Motion::PageUp | Motion::PageDown => self.page(
                row,
                matches!(motion, Motion::PageUp),
                goal,
                tables,
                preferred,
            ),
            _ => Ok(row),
        }
    }

    pub(super) fn column_goal(
        &self,
        from: usize,
        to: usize,
        previous: Option<(usize, u32)>,
    ) -> Option<(usize, u32)> {
        let target = self.lines[to].cell.as_ref()?;
        let column = target
            .column
            .or_else(|| {
                self.lines[from]
                    .cell
                    .as_ref()
                    .filter(|c| c.table == target.table)
                    .and_then(|c| c.column)
            })
            .or_else(|| {
                previous
                    .filter(|(table, _)| *table == target.table)
                    .map(|(_, column)| column)
            })?;
        Some((target.table, column))
    }

    pub(super) fn at_end(&self, row: usize) -> bool {
        let line = &self.lines[row];
        line.number + 1 == self.total_lines
            || line.cell.as_ref().is_some_and(|cell| {
                cell.part + 1 == cell.parts
                    && self
                        .lines
                        .iter()
                        .filter_map(|l| l.cell.as_ref())
                        .filter(|c| c.table == cell.table)
                        .map(|c| c.row)
                        .max()
                        == Some(cell.row)
                    && self
                        .lines
                        .iter()
                        .filter(|l| l.cell.as_ref().is_some_and(|c| c.table == cell.table))
                        .map(|l| l.number)
                        .max()
                        .is_some_and(|last| last + 1 == self.total_lines)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::compose_tables::{Cell as SourceCell, Row, Table};
    use super::*;

    fn fixture() -> (Layout, Vec<Table>) {
        let stop = |source, x, affinity| Stop {
            source,
            x,
            affinity,
        };
        let line = |number, y, source: Range<usize>, cell: Option<Cell>| {
            let x = cell.as_ref().map_or(0.0, |c| c.x);
            let start = stop(source.start, x, Affinity::Downstream);
            let end = stop(source.end, x + 60.0, Affinity::Upstream);
            Line {
                number,
                y,
                height: 10.0,
                source_start: source.start,
                source_end: source.end,
                start,
                end,
                stops: vec![start, end],
                cell,
            }
        };
        let cell = |row, column, part, parts, y, height| {
            Some(Cell {
                table: 10,
                row,
                column: Some(column),
                part,
                parts,
                x: 8.0 + f64::from(column) * 100.0,
                width: 80.0,
                y,
                height,
            })
        };
        let lines = vec![
            line(0, 0.0, 0..10, None),
            line(1, 18.0, 11..15, cell(10, 0, 0, 2, 10.0, 40.0)),
            line(2, 28.0, 15..20, cell(10, 0, 1, 2, 10.0, 40.0)),
            line(3, 18.0, 21..30, cell(10, 1, 0, 1, 10.0, 40.0)),
            line(4, 58.0, 41..60, cell(40, 0, 0, 1, 50.0, 30.0)),
            line(5, 58.0, 61..70, cell(40, 1, 0, 1, 50.0, 30.0)),
            line(6, 90.0, 80..100, None),
        ];
        let bands = bands(&lines, 100.0).unwrap();
        let layout = Layout {
            id: 1,
            page_height: 40.0,
            source_start: 0,
            source_end: 100,
            document_len: 100,
            total_lines: lines.len(),
            height: 100.0,
            style_revision: 1,
            lines,
            bands,
        };
        let sc = |start, end| SourceCell {
            start,
            end,
            missing: false,
            revealed: false,
        };
        let tables = vec![Table {
            measurement_id: 0,
            measurement: None,
            start: 10,
            end: 80,
            alignments: vec![0, 0],
            rows: vec![
                Row {
                    start: 10,
                    end: 40,
                    content_end: 39,
                    header: true,
                    source: false,
                    cells: vec![sc(11, 20), sc(21, 30)],
                    previous: None,
                    next: Some(40),
                },
                Row {
                    start: 40,
                    end: 80,
                    content_end: 79,
                    header: false,
                    source: false,
                    cells: vec![sc(41, 60), sc(61, 70)],
                    previous: Some(10),
                    next: None,
                },
            ],
        }];
        (layout, tables)
    }

    #[test]
    fn vertical_motion_keeps_the_column_wraps_and_enters_and_leaves_whole_tables() {
        let (layout, tables) = fixture();
        for (from, motion, goal, wanted) in [
            (0, Motion::Down, 130.0, 3),
            (3, Motion::Down, 130.0, 5),
            (5, Motion::Up, 130.0, 3),
            (3, Motion::Up, 130.0, 0),
            (1, Motion::Down, 40.0, 2),
            (2, Motion::Down, 40.0, 4),
            (4, Motion::Up, 40.0, 2),
            (4, Motion::Down, 40.0, 6),
            (6, Motion::Up, 130.0, 5),
            (6, Motion::Up, 40.0, 4),
        ] {
            assert_eq!(
                layout
                    .target_row(from, motion, goal, &tables, None)
                    .unwrap(),
                wanted
            );
        }
        assert!(layout.lines.iter().all(|l| valid_source(l, &tables)));
    }

    #[test]
    fn paging_uses_full_row_bounds_and_does_not_jump_into_a_taller_sibling() {
        let (mut layout, tables) = fixture();
        assert_eq!(
            layout
                .target_row(3, Motion::PageDown, 130.0, &tables, None)
                .unwrap(),
            5
        );
        assert_eq!(
            layout
                .target_row(5, Motion::PageUp, 130.0, &tables, None)
                .unwrap(),
            3
        );
        layout.page_height = 20.0;
        assert_eq!(
            layout
                .target_row(3, Motion::PageDown, 130.0, &tables, None)
                .unwrap(),
            3,
            "the target is below this short cell's text but still in its full row"
        );
        layout.page_height = 15.0;
        assert_eq!(
            layout
                .target_row(0, Motion::PageDown, 130.0, &tables, None)
                .unwrap(),
            3,
            "row padding chooses a real caret rather than deferring forever"
        );
    }

    #[test]
    fn logical_column_survives_a_full_width_source_row_and_pixel_goal_reset() {
        let (mut layout, mut tables) = fixture();
        let mut last = layout.lines[5].clone();
        layout.lines.truncate(5);
        let raw = &mut layout.lines[4];
        raw.source_start = 40;
        raw.source_end = 80;
        raw.start.source = 40;
        raw.end.source = 79;
        raw.stops = vec![raw.start, raw.end];
        raw.cell.as_mut().unwrap().column = None;
        raw.cell.as_mut().unwrap().width = 180.0;
        let mut first = raw.clone();
        first.number = 5;
        first.y = 88.0;
        first.source_start = 81;
        first.source_end = 90;
        first.start.source = 81;
        first.end.source = 90;
        first.stops = vec![first.start, first.end];
        first.cell = Some(Cell {
            table: 10,
            row: 80,
            column: Some(0),
            part: 0,
            parts: 1,
            x: 8.0,
            width: 80.0,
            y: 80.0,
            height: 30.0,
        });
        last.number = 6;
        last.y = 88.0;
        last.source_start = 91;
        last.source_end = 100;
        last.start.source = 91;
        last.end.source = 100;
        last.stops = vec![last.start, last.end];
        last.cell.as_mut().unwrap().row = 80;
        last.cell.as_mut().unwrap().y = 80.0;
        layout.lines.extend([first, last]);
        tables[0].end = 120;
        tables[0].rows[1].source = true;
        tables[0].rows[1].cells.clear();
        tables[0].rows[1].next = Some(80);
        tables[0].rows.push(Row {
            start: 80,
            end: 120,
            content_end: 119,
            header: false,
            source: false,
            previous: Some(40),
            next: None,
            cells: vec![
                SourceCell {
                    start: 81,
                    end: 90,
                    missing: false,
                    revealed: false,
                },
                SourceCell {
                    start: 91,
                    end: 100,
                    missing: false,
                    revealed: false,
                },
            ],
        });
        layout.height = 110.0;
        layout.source_end = 120;
        layout.document_len = 120;
        layout.total_lines = 7;
        layout.bands = bands(&layout.lines, layout.height).unwrap();
        assert_eq!(layout.column_goal(3, 4, None), Some((10, 1)));
        assert_eq!(
            layout
                .target_row(4, Motion::Down, 0.0, &tables, None)
                .unwrap(),
            5
        );
        assert_eq!(
            layout
                .target_row(4, Motion::Down, 0.0, &tables, Some(1))
                .unwrap(),
            6
        );
        assert_eq!(
            layout
                .target_row(4, Motion::PageDown, 0.0, &tables, Some(1))
                .unwrap(),
            6
        );
    }

    #[test]
    fn sparse_parts_and_rows_outside_the_window_request_measurements() {
        let (mut layout, tables) = fixture();
        layout.lines.remove(2);
        layout.bands = bands(&layout.lines, layout.height).unwrap();
        let need = layout
            .target_row(1, Motion::Down, 40.0, &tables, None)
            .unwrap_err();
        assert_eq!(need.y, Some(28.1));
        let (mut layout, mut tables) = fixture();
        layout.lines.truncate(4);
        layout.total_lines = 4;
        layout.height = 50.0;
        layout.source_end = 40;
        layout.bands = bands(&layout.lines, layout.height).unwrap();
        tables[0].rows.truncate(1);
        let need = layout
            .target_row(3, Motion::Down, 130.0, &tables, None)
            .unwrap_err();
        assert_eq!(
            need.region,
            Some(40),
            "parser adjacency survives a bounded source window"
        );
    }

    #[test]
    fn neighbor_destination_survives_disjoint_windows_and_a_second_sparse_measurement() {
        let (mut origin, mut tables) = fixture();
        origin.lines.truncate(4);
        origin.total_lines = 4;
        origin.source_end = 40;
        origin.height = 50.0;
        origin.bands = bands(&origin.lines, origin.height).unwrap();
        tables[0].rows.truncate(1);
        let destination = origin
            .target_row(3, Motion::Down, 130.0, &tables, None)
            .unwrap_err()
            .destination
            .unwrap();

        let (mut target, tables) = fixture();
        target.lines = target.lines[4..].to_vec();
        target.source_start = 40;
        target.bands = bands(&target.lines, target.height).unwrap();
        assert_eq!(
            target.resume(destination, 130.0, &tables).unwrap(),
            1,
            "the origin need not remain measurable in the next source window"
        );
        // The desired column exists, but its leading part is absent.
        target.lines[1].cell.as_mut().unwrap().part = 1;
        target.lines[1].cell.as_mut().unwrap().parts = 2;
        let need = target.resume(destination, 130.0, &tables).unwrap_err();
        assert!(need.y.is_some() && need.destination.is_some());
        target.lines[1].cell.as_mut().unwrap().part = 0;
        assert_eq!(
            target
                .resume(need.destination.unwrap(), 130.0, &tables)
                .unwrap(),
            1,
            "another measurement must resume the same logical destination"
        );
    }

    #[test]
    fn source_window_boundaries_retain_table_exit_and_prose_entry_destinations() {
        let (mut table, tables) = fixture();
        table.lines = table.lines[1..6].to_vec();
        for line in &mut table.lines {
            line.number -= 1;
        }
        table.source_start = 10;
        table.source_end = 80;
        table.total_lines = 5;
        table.bands = bands(&table.lines, table.height).unwrap();
        let (full, _) = fixture();
        for (row, motion, probe) in [(2, Motion::Up, 9), (4, Motion::Down, 80)] {
            let need = table
                .target_row(row, motion, 130.0, &tables, None)
                .unwrap_err();
            assert_eq!(need.region, Some(probe));
            assert!(
                need.destination.is_some(),
                "table exit must survive losing its origin window"
            );
            let index = if probe == 9 { 0 } else { 6 };
            let mut prose = Layout {
                id: 2,
                page_height: 40.0,
                source_start: if index == 0 { 0 } else { 80 },
                source_end: if index == 0 { 10 } else { 100 },
                document_len: 100,
                lines: vec![full.lines[index].clone()],
                total_lines: 1,
                height: 100.0,
                style_revision: 1,
                bands: Vec::new(),
            };
            prose.lines[0].number = 0;
            prose.bands = bands(&prose.lines, prose.height).unwrap();
            assert_eq!(
                prose.resume(need.destination.unwrap(), 130.0, &[]).unwrap(),
                0,
                "the destination flow is measurable without the table's origin"
            );
        }
        for (index, motion, start, end, probe) in
            [(0, Motion::Down, 0, 10, 10), (6, Motion::Up, 80, 100, 79)]
        {
            let mut prose = Layout {
                id: 2,
                page_height: 40.0,
                source_start: start,
                source_end: end,
                document_len: 100,
                lines: vec![full.lines[index].clone()],
                total_lines: 1,
                height: 100.0,
                style_revision: 1,
                bands: Vec::new(),
            };
            prose.lines[0].number = 0;
            prose.bands = bands(&prose.lines, prose.height).unwrap();
            let need = prose.target_row(0, motion, 130.0, &[], None).unwrap_err();
            assert_eq!(need.region, Some(probe));
            assert!(
                need.destination.is_some(),
                "prose entry must survive losing its origin window"
            );
            let wanted = if matches!(motion, Motion::Down) { 2 } else { 4 };
            assert_eq!(
                table
                    .resume(need.destination.unwrap(), 130.0, &tables)
                    .unwrap(),
                wanted,
                "entry chooses the intended column and first/last wrapped part"
            );
        }
    }

    #[test]
    fn an_incomplete_source_window_cannot_use_its_empty_eof_line_as_the_next_line() {
        let (mut layout, _) = fixture();
        layout.lines.truncate(1);
        let mut eof = layout.lines[0].clone();
        eof.number = 1;
        eof.y = 10.0;
        eof.source_start = 10;
        eof.source_end = 10;
        eof.start.source = 10;
        eof.end.source = 10;
        eof.stops = vec![eof.start];
        layout.lines.push(eof);
        layout.total_lines = 2;
        layout.source_end = 10;
        layout.height = 20.0;
        layout.bands = bands(&layout.lines, layout.height).unwrap();
        let need = layout
            .target_row(0, Motion::Down, 130.0, &[], None)
            .unwrap_err();
        assert_eq!(need.region, Some(10));
        assert!(need.destination.is_some());
    }

    #[test]
    fn overlapping_columns_rows_and_inconsistent_parts_are_refused() {
        let (layout, _) = fixture();
        let mut bad = layout.lines.clone();
        bad[3].cell.as_mut().unwrap().x = 20.0;
        assert!(bands(&bad, layout.height).is_err());
        let mut bad = layout.lines.clone();
        bad[2].cell.as_mut().unwrap().parts = 3;
        assert!(bands(&bad, layout.height).is_err());
        let mut bad = layout.lines.clone();
        bad[2].cell.as_mut().unwrap().part = 0;
        assert!(bands(&bad, layout.height).is_err());
        let mut bad = layout.lines.clone();
        bad[2].number += 1;
        assert!(bands(&bad, layout.height).is_err());
        let mut bad = layout.lines.clone();
        bad[3].cell.as_mut().unwrap().height = 45.0;
        assert!(bands(&bad, layout.height).is_err());
        let mut bad = layout.lines.clone();
        bad[4].y = 40.0;
        assert!(bands(&bad, layout.height).is_err());
    }

    #[test]
    fn source_ownership_includes_missing_anchors_but_never_a_sibling_cells_bytes() {
        let (layout, mut tables) = fixture();
        let mut line = layout.lines[5].clone();
        line.source_start = 41;
        assert!(!valid_source(&line, &tables));
        line = layout.lines[5].clone();
        line.cell.as_mut().unwrap().column = None;
        assert!(!valid_source(&line, &tables));
        line = layout.lines[5].clone();
        line.cell.as_mut().unwrap().table = 9;
        assert!(!valid_source(&line, &tables));
        tables[0].rows[1].cells[1] = SourceCell {
            start: 79,
            end: 79,
            missing: true,
            revealed: false,
        };
        line = layout.lines[5].clone();
        line.source_start = 79;
        line.source_end = 79;
        line.start.source = 79;
        line.end.source = 79;
        for stop in &mut line.stops {
            stop.source = 79;
        }
        assert!(valid_source(&line, &tables));
        line.cell.as_mut().unwrap().parts = 2;
        assert!(!valid_source(&line, &tables));
    }

    #[test]
    fn wire_cell_presence_and_malformed_geometry_are_checked() {
        use super::super::super::wire::field;
        let mut bytes = Vec::new();
        field::opt_uint(&mut bytes, 3, Some(0));
        field::uint(&mut bytes, 5, 1);
        field::double(&mut bytes, 7, 80.0);
        field::double(&mut bytes, 9, 40.0);
        assert_eq!(Cell::parse(&bytes).unwrap().column, Some(0));
        let mut bad = bytes.clone();
        field::double(&mut bad, 7, -1.0);
        assert!(Cell::parse(&bad).is_err());
        let mut bad = bytes.clone();
        field::double(&mut bad, 6, f64::NAN);
        assert!(Cell::parse(&bad).is_err());
        let mut bad = bytes.clone();
        field::opt_uint(&mut bad, 3, Some(u64::MAX));
        assert!(Cell::parse(&bad).is_err());
        let mut bad = bytes.clone();
        field::uint(&mut bad, 4, 1);
        assert!(Cell::parse(&bad).is_err());
    }

    #[test]
    fn the_end_of_a_table_column_is_a_document_edge_only_on_the_final_row() {
        let (mut layout, _) = fixture();
        layout.lines.pop();
        layout.total_lines = 6;
        layout.height = 80.0;
        layout.source_end = 80;
        layout.document_len = 80;
        layout.bands = bands(&layout.lines, layout.height).unwrap();
        assert!(layout.at_end(4));
        assert!(!layout.at_end(2));
    }
}
