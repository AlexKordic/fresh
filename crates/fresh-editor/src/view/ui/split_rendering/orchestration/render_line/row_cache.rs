//! Rows rendered in an earlier frame, reused while nothing they were drawn
//! from has changed: a scroll redraws the rows that came into view, and the
//! rest come back from here.
//!
//! What is cached is a row's *content*: everything `render_view_lines` draws
//! after the gutter (the cell pass, the line-end fills, inline diagnostics,
//! synthesised guides), plus what the theme inspector recorded for those
//! cells. The gutter is drawn fresh every frame, so line numbers — relative
//! ones included — fold arrows, diagnostic marks and plugin margins never
//! need to be part of a key.
//!
//! A row's key ([`RowKeys::key`]) hashes everything its content reads: the
//! view line itself (text, source bytes, token styles, columns), the syntax
//! and semantic spans and the overlays touching its bytes, the part of the
//! selection inside it, its inline diagnostic and its guide columns. What
//! every row shares — theme, settings, widths, scroll column — is the
//! [`shared_key`], folded into each row's key, except the theme, which is
//! compared by value and clears the cache when it changes.
//!
//! Some rows are never cached (the caller decides; see `render_view_lines`):
//! rows on the cursor's logical line or holding any cursor, rows with ANSI
//! escapes, rows cut short by the long-line cap, and every row while a block
//! selection or the highlight debug view is on. Those all depend on state
//! that is cheaper to draw than to key.
//!
//! Debug builds draw every cached frame a second time without the cache and
//! compare the two, so a missing input fails the tests instead of leaving a
//! stale row on screen.

use super::super::contexts::{DecorationContext, SelectionContext};
use crate::app::types::CellThemeInfo;
use crate::view::theme::Theme;
use crate::view::ui::view_pipeline::ViewLine;
use ratatui::text::Span;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash, Hasher};

/// Rows kept for this many renders after the last one that drew them. A
/// buffer shown in two panes renders twice a frame, and each pane's rows
/// must survive the other's render.
const KEEP_RENDERS: u64 = 4;

/// One row's content as `render_view_lines` drew it after the gutter.
pub(super) struct CachedRow {
    /// The spans after the gutter's.
    pub spans: Vec<Span<'static>>,
    /// The source byte of each of those cells (`line_view_map` past the
    /// gutter's entries).
    pub view_map: Vec<Option<usize>>,
    /// The theme inspector's record of the content cells, from the gutter's
    /// right edge to the pane's, as runs of equal cells (a row's cells mostly
    /// repeat their neighbour's); empty while nothing records.
    theme_runs: Vec<(u16, CellThemeInfo)>,
    /// Where the row's text ends on screen (`locate_cursor_in_view_map`).
    pub text_end_x: Option<u16>,
    /// Cells the newline's line-ending indicator took.
    pub newline_indicator_cols: usize,
    used: u64,
}

impl CachedRow {
    pub(super) fn new(
        spans: Vec<Span<'static>>,
        view_map: Vec<Option<usize>>,
        theme_cells: &[CellThemeInfo],
        text_end_x: Option<u16>,
        newline_indicator_cols: usize,
    ) -> Self {
        let mut theme_runs: Vec<(u16, CellThemeInfo)> = Vec::new();
        for cell in theme_cells {
            match theme_runs.last_mut() {
                Some((n, last)) if last == cell && *n < u16::MAX => *n += 1,
                _ => theme_runs.push((1, cell.clone())),
            }
        }
        Self {
            spans,
            view_map,
            theme_runs,
            text_end_x,
            newline_indicator_cols,
            used: 0,
        }
    }

    /// How many theme inspector cells the row recorded.
    pub(super) fn theme_cell_count(&self) -> usize {
        self.theme_runs.iter().map(|(n, _)| *n as usize).sum()
    }

    /// Write the row's theme inspector cells into `cells`, which is
    /// `theme_cell_count` long.
    pub(super) fn write_theme_cells(&self, cells: &mut [CellThemeInfo]) {
        let mut at = 0;
        for (n, cell) in &self.theme_runs {
            for slot in &mut cells[at..at + *n as usize] {
                slot.clone_from(cell);
            }
            at += *n as usize;
        }
    }
}

/// A buffer's cached rows. Lives on the buffer's state and is lent to
/// `render_view_lines` for each render of one of its panes.
#[derive(Default)]
pub(crate) struct RowCache {
    /// The theme the rows were drawn with.
    theme: Option<Theme>,
    rows: HashMap<u64, CachedRow>,
    /// Renders so far; a row remembers the last one that drew it.
    render: u64,
}

impl RowCache {
    /// Start a render. A changed theme drops every row.
    pub(super) fn begin(&mut self, theme: &Theme) {
        self.render += 1;
        if self.theme.as_ref() != Some(theme) {
            self.rows.clear();
            self.theme = Some(theme.clone());
        }
    }

    /// The row cached under `key`, marked as drawn by this render.
    pub(super) fn get(&mut self, key: u64) -> Option<&CachedRow> {
        let render = self.render;
        let row = self.rows.get_mut(&key)?;
        row.used = render;
        Some(row)
    }

    pub(super) fn insert(&mut self, key: u64, mut row: CachedRow) {
        row.used = self.render;
        self.rows.insert(key, row);
    }

    /// End a render: drop the rows no recent render drew.
    pub(super) fn end(&mut self) {
        let render = self.render;
        self.rows.retain(|_, row| row.used + KEEP_RENDERS > render);
    }
}

fn hasher() -> impl Hasher {
    foldhash::quality::FixedState::with_seed(0x5eed).build_hasher()
}

/// A `usize` slice's bytes, for hashing it in one call.
fn usize_bytes(values: &[usize]) -> &[u8] {
    // SAFETY: `usize` has no padding and any byte pattern is a valid `u8`;
    // the slice covers exactly the values' memory.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    }
}

/// What every row of one render shares, as a hash folded into each row's key.
#[derive(Hash)]
pub(super) struct SharedInputs<'a> {
    pub left_column: usize,
    pub render_width: u16,
    pub gutter_width: usize,
    pub line_wrap: bool,
    pub lsp_waiting: bool,
    pub is_active: bool,
    pub session_mode: bool,
    pub software_cursor_only: bool,
    pub highlight_current_line: bool,
    pub indentation_guide: &'a str,
    pub indentation_guide_glyph: &'a str,
    pub rainbow_indentation: bool,
    pub effective_editor_bg: ratatui::style::Color,
    pub records_theme: bool,
    pub tab_size: usize,
    pub whitespace: &'a str,
    pub whitespace_in_selection: bool,
    pub virtual_space: &'a str,
    pub line_ending: &'a str,
}

pub(super) fn shared_key(inputs: &SharedInputs<'_>) -> u64 {
    let mut h = hasher();
    inputs.hash(&mut h);
    h.finish()
}

/// One row, as far as its key is concerned.
pub(super) struct RowInputs<'r> {
    pub view_line: &'r ViewLine,
    /// The first and last source bytes the row draws.
    pub first_byte: usize,
    pub last_byte: usize,
    /// Overlays ending after this byte can reach the row: through its own
    /// bytes, or, on a wrap continuation, by still being active from the row
    /// before (the tail fill reads those).
    pub overlay_reach: usize,
    pub gutter_num: usize,
    pub is_continuation: bool,
    pub line_start_byte: Option<usize>,
    pub guide_columns: &'r [usize],
    pub active_guide_column: Option<usize>,
}

/// Builds rows' keys in screen order, walking the spans and overlays
/// forward the way the cell pass does.
pub(super) struct RowKeys<'a> {
    decorations: &'a DecorationContext,
    selection: &'a SelectionContext,
    shared: u64,
    highlight: usize,
    semantic: usize,
    overlay_next: usize,
    /// Overlays admitted (started at or before an earlier row's last byte)
    /// and not yet known to end before the rows still to come.
    overlay_active: Vec<usize>,
}

impl<'a> RowKeys<'a> {
    pub(super) fn new(
        decorations: &'a DecorationContext,
        selection: &'a SelectionContext,
        shared: u64,
    ) -> Self {
        Self {
            decorations,
            selection,
            shared,
            highlight: 0,
            semantic: 0,
            overlay_next: 0,
            overlay_active: Vec::new(),
        }
    }

    /// The key of a row. Rows are asked for in screen order.
    pub(super) fn key(&mut self, row: &RowInputs<'_>) -> u64 {
        let (lo, hi) = (row.first_byte, row.last_byte);
        let mut h = hasher();
        h.write_u64(self.shared);

        // The view line.
        let line = row.view_line;
        h.write(line.text.as_bytes());
        h.write_usize(line.text.len());
        line.source_start_byte.hash(&mut h);
        for byte in &line.char_source_bytes {
            h.write_usize(byte.map_or(usize::MAX, |b| b));
        }
        if line.char_styles.iter().any(Option::is_some) {
            for (i, style) in line.char_styles.iter().enumerate() {
                if let Some(style) = style {
                    h.write_usize(i);
                    style.hash(&mut h);
                }
            }
        }
        h.write(usize_bytes(&line.char_visual_cols));
        h.write_usize(line.char_visual_cols.len());
        h.write(usize_bytes(&line.visual_to_char));
        h.write_usize(line.visual_to_char.len());
        let mut tabs: Vec<usize> = line.tab_starts.iter().copied().collect();
        tabs.sort_unstable();
        tabs.hash(&mut h);
        h.write_u8(line.line_start as u8);
        line.ends_with_newline.hash(&mut h);
        line.virtual_gutter_glyph.hash(&mut h);
        line.virtual_line_style.hash(&mut h);

        // Where it sits.
        h.write_usize(row.gutter_num);
        row.is_continuation.hash(&mut h);
        row.line_start_byte.hash(&mut h);
        row.guide_columns.hash(&mut h);
        row.active_guide_column.hash(&mut h);

        // Syntax and semantic spans touching its bytes.
        for (spans, cursor) in [
            (&self.decorations.highlight_spans, &mut self.highlight),
            (&self.decorations.semantic_token_spans, &mut self.semantic),
        ] {
            while *cursor < spans.len() && spans[*cursor].range.end <= lo {
                *cursor += 1;
            }
            h.write_u8(0xff);
            for span in spans[*cursor..].iter().take_while(|s| s.range.start <= hi) {
                if span.range.end > lo {
                    h.write_usize(span.range.start);
                    h.write_usize(span.range.end);
                    span.color.hash(&mut h);
                    span.bg.hash(&mut h);
                    span.category
                        .as_ref()
                        .map(std::mem::discriminant)
                        .hash(&mut h);
                }
            }
        }

        // Overlays that can reach it: started by its last byte, ending after
        // the reach. Every field but the handle and markers, which change
        // when an overlay is recreated as it was.
        let overlays = &self.decorations.viewport_overlays;
        let index = &self.decorations.overlay_position_index;
        while self.overlay_next < index.len() && overlays[index[self.overlay_next]].1.start <= hi {
            self.overlay_active.push(index[self.overlay_next]);
            self.overlay_next += 1;
        }
        let reach = row.overlay_reach;
        self.overlay_active.retain(|&i| overlays[i].1.end > reach);
        h.write_u8(0xfe);
        for &i in &self.overlay_active {
            let (overlay, range) = &overlays[i];
            h.write_usize(range.start);
            h.write_usize(range.end);
            overlay.namespace.hash(&mut h);
            overlay.face.hash(&mut h);
            overlay.priority.hash(&mut h);
            overlay.message.hash(&mut h);
            overlay.extend_to_line_end.hash(&mut h);
            overlay.url.hash(&mut h);
            overlay.theme_key.hash(&mut h);
            overlay.above_selection.hash(&mut h);
        }

        // The selection inside it.
        h.write_u8(0xfd);
        for range in &self.selection.ranges {
            if range.start <= hi && range.end > lo {
                h.write_usize(range.start.max(lo));
                h.write_usize(range.end.min(hi + 1));
            }
        }

        // Its inline diagnostic.
        if let Some((message, style)) = row
            .line_start_byte
            .and_then(|b| self.decorations.diagnostic_inline_texts.get(&b))
        {
            message.hash(&mut h);
            style.hash(&mut h);
        }
        h.finish()
    }
}
