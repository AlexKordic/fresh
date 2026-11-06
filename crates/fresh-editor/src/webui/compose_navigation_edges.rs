//! Source flow boundaries, independent of a window's temporary line numbers.

use super::*;

impl Layout {
    pub(super) fn resume(
        &self,
        destination: Destination,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
    ) -> Result<usize, Need> {
        match destination {
            Destination::Cell(cell) => self.resume_cell(cell, goal, tables),
            Destination::Page(page) => self.resume_page(page, goal, tables),
            Destination::Source { edge, backwards } => self
                .source_entry(edge, backwards, goal, tables)
                .map_err(|need| need.destination(destination)),
        }
    }

    fn source_entry(
        &self,
        edge: usize,
        backwards: bool,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
    ) -> Result<usize, Need> {
        // The previous byte can be a CR/LF or a byte inside a UTF-8 scalar.
        // It chooses an owning flow interval, never an editable caret byte.
        let probe = if backwards {
            edge.saturating_sub(1)
        } else {
            edge
        };
        if probe < self.source_start
            || probe > self.source_end
            || (probe == self.source_end && probe != self.document_len)
        {
            return Err(Need::region(probe));
        }
        let band = self
            .bands
            .iter()
            .find(|band| {
                let line = &self.lines[band.lines.start];
                if let Some(cell) = &line.cell {
                    // Prefixes, pipes and EOLs belong to the parser's complete
                    // row, even though no individual cell owns those bytes.
                    return self
                        .table_row(cell, tables)
                        .is_some_and(|row| row.start <= probe && probe < row.end);
                }
                (line.source_start <= probe && probe < line.source_end)
                    || (probe == self.document_len
                        && line.source_start == probe
                        && line.source_end == probe)
            })
            .ok_or_else(|| Need::region(probe))?;
        self.enter(band, None, goal, backwards, tables)
    }
}
