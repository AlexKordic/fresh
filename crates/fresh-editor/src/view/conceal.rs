//! Conceal range infrastructure
//!
//! Provides a system for hiding or replacing ranges of buffer text during rendering.
//! Used for Typora-style "seamless canvas" markdown: hiding syntax markers like `**`
//! around bold text, `[](url)` around links, etc.
//!
//! ## Architecture
//!
//! Follows the same pattern as OverlayManager:
//! 1. Plugins add conceal ranges via `addConceal(bufferId, namespace, start, end, options)`
//! 2. Ranges are stored with marker-based position tracking (auto-adjust on edits)
//! 3. During token pipeline, concealed byte ranges are filtered/replaced
//!
//! ## Integration Point
//!
//! Conceal ranges are applied to the token stream in `split_rendering.rs` after
//! plugin view transforms but before wrapping. This means:
//! - Plugin transforms see the original (unconcealed) tokens
//! - Concealment happens transparently at the editor level
//! - Wrapping operates on the concealed (shorter) lines

use crate::model::marker::{MarkerId, MarkerList};
use crate::view::activation::ScopedActivation;
use fresh_core::api::MarkerActivation;
use fresh_core::overlay::OverlayNamespace;
use std::collections::HashMap;
use std::ops::Range;

/// A conceal range hides or replaces a byte range during rendering
#[derive(Debug, Clone)]
pub struct ConcealRange {
    /// Namespace for bulk operations (shared with overlay namespace system)
    pub namespace: OverlayNamespace,

    /// Start marker. Right gravity, like every marker `MarkerList::create`
    /// makes: text inserted exactly at the conceal's start is swallowed by
    /// the conceal rather than appearing before it.
    pub start_marker: MarkerId,

    /// End marker, also right gravity: text typed at the conceal's end
    /// extends it.
    pub end_marker: MarkerId,

    /// Optional replacement text to show instead of the concealed content.
    /// If None, the range is simply hidden (zero-width).
    pub replacement: Option<String>,

    /// Optional cursor-dependent activation rule, scope stored relative to
    /// the start marker. `None` = always active. Filtered at query time so
    /// cursor movement never mutates the marker set (see view/activation.rs).
    pub activation: Option<ScopedActivation>,
}

impl ConcealRange {
    /// Get the current byte range by resolving markers
    pub fn range(&self, marker_list: &MarkerList) -> Range<usize> {
        let start = marker_list.get_position(self.start_marker).unwrap_or(0);
        let end = marker_list.get_position(self.end_marker).unwrap_or(0);
        start..end
    }

    /// Check if this range overlaps with another range
    pub fn overlaps(&self, range: &Range<usize>, marker_list: &MarkerList) -> bool {
        let self_range = self.range(marker_list);
        self_range.start < range.end && range.start < self_range.end
    }
}

/// Manages conceal ranges for a buffer
#[derive(Debug, Clone)]
pub struct ConcealManager {
    ranges: Vec<ConcealRange>,
    /// `MarkerId -> index into ranges` for O(log N + k) `remove_in_range`.
    /// Both endpoints of each range are registered. Kept in sync with
    /// every push / swap_remove on `ranges`.
    marker_to_idx: HashMap<MarkerId, usize>,
    /// Monotonic counter bumped on every mutation. Consumers that cache derived
    /// data (e.g. `LineWrapCache`) fold this into their key so any mutation
    /// invalidates stale entries automatically.
    version: u32,
}

impl ConcealManager {
    /// Create a new empty conceal manager
    pub fn new() -> Self {
        Self {
            ranges: Vec::new(),
            marker_to_idx: HashMap::new(),
            version: 0,
        }
    }

    /// Monotonic version, bumped on every mutation.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// Add a conceal range
    pub fn add(
        &mut self,
        marker_list: &mut MarkerList,
        namespace: OverlayNamespace,
        range: Range<usize>,
        replacement: Option<String>,
    ) {
        self.add_with_activation(marker_list, namespace, range, replacement, None);
    }

    /// Add a conceal range with an optional cursor-dependent activation
    /// rule (wire format, absolute scope bytes — converted to the
    /// marker-relative form here).
    pub fn add_with_activation(
        &mut self,
        marker_list: &mut MarkerList,
        namespace: OverlayNamespace,
        range: Range<usize>,
        replacement: Option<String>,
        activation: Option<MarkerActivation>,
    ) {
        let start_marker = marker_list.create(range.start);
        let end_marker = marker_list.create(range.end);

        let idx = self.ranges.len();
        self.marker_to_idx.insert(start_marker, idx);
        self.marker_to_idx.insert(end_marker, idx);
        self.ranges.push(ConcealRange {
            namespace,
            start_marker,
            end_marker,
            replacement,
            activation: activation
                .map(|rule| ScopedActivation::from_absolute(&rule, range.start, marker_list)),
        });
        self.version = self.version.wrapping_add(1);
    }

    /// Remove all conceal ranges in a namespace
    pub fn clear_namespace(&mut self, namespace: &OverlayNamespace, marker_list: &mut MarkerList) {
        // Collect indices and markers up-front, then remove via swap_remove
        // (descending order so indices stay valid).
        let mut indices: Vec<usize> = self
            .ranges
            .iter()
            .enumerate()
            .filter_map(|(i, r)| (&r.namespace == namespace).then_some(i))
            .collect();
        if indices.is_empty() {
            return;
        }
        indices.sort_unstable_by(|a, b| b.cmp(a));
        for idx in indices {
            self.swap_remove_at(idx, marker_list);
        }
        self.version = self.version.wrapping_add(1);
    }

    /// Remove all conceal ranges that overlap with a byte range and clean up their markers
    pub fn remove_in_range(&mut self, range: &Range<usize>, marker_list: &mut MarkerList) {
        // O(log N + k): query the marker tree for endpoints near `range`,
        // map back to entries, then verify each candidate's true range
        // since `query_range` is closed-interval and the marker at the
        // exact upper bound represents a one-past-the-end position.
        // Spanning conceals (start < range.start && end > range.end) are
        // not detected; document the precondition in the type's contract.
        if range.start >= range.end {
            return;
        }
        let hits = marker_list.query_range(range.start, range.end);
        if hits.is_empty() {
            return;
        }
        let mut candidates: Vec<usize> = hits
            .iter()
            .filter_map(|(mid, _, _)| self.marker_to_idx.get(mid).copied())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();

        let mut to_remove: Vec<usize> = candidates
            .into_iter()
            .filter(|&idx| {
                let r = &self.ranges[idx];
                let start = marker_list.get_position(r.start_marker).unwrap_or(0);
                let end = marker_list.get_position(r.end_marker).unwrap_or(0);
                start < range.end && range.start < end
            })
            .collect();
        if to_remove.is_empty() {
            return;
        }
        // Descending so swap_remove doesn't shift earlier indices.
        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        for idx in to_remove {
            self.swap_remove_at(idx, marker_list);
        }
        self.version = self.version.wrapping_add(1);
    }

    /// Like [`remove_in_range`], but only removes ranges belonging to
    /// `namespace`. Lets one plugin rebuild its conceals for a line without
    /// destroying another plugin's ranges there (same motivation as
    /// `clear_overlays_in_range_for_namespace`, issue #2146).
    pub fn remove_in_range_for_namespace(
        &mut self,
        namespace: &OverlayNamespace,
        range: &Range<usize>,
        marker_list: &mut MarkerList,
    ) {
        if range.start >= range.end {
            return;
        }
        let hits = marker_list.query_range(range.start, range.end);
        if hits.is_empty() {
            return;
        }
        let mut candidates: Vec<usize> = hits
            .iter()
            .filter_map(|(mid, _, _)| self.marker_to_idx.get(mid).copied())
            .collect();
        candidates.sort_unstable();
        candidates.dedup();

        let mut to_remove: Vec<usize> = candidates
            .into_iter()
            .filter(|&idx| {
                let r = &self.ranges[idx];
                if &r.namespace != namespace {
                    return false;
                }
                let start = marker_list.get_position(r.start_marker).unwrap_or(0);
                let end = marker_list.get_position(r.end_marker).unwrap_or(0);
                start < range.end && range.start < end
            })
            .collect();
        if to_remove.is_empty() {
            return;
        }
        to_remove.sort_unstable_by(|a, b| b.cmp(a));
        for idx in to_remove {
            self.swap_remove_at(idx, marker_list);
        }
        self.version = self.version.wrapping_add(1);
    }

    /// Clear all conceal ranges and their markers
    pub fn clear(&mut self, marker_list: &mut MarkerList) {
        let had_any = !self.ranges.is_empty();
        for range in &self.ranges {
            marker_list.delete(range.start_marker);
            marker_list.delete(range.end_marker);
            if let Some(a) = &range.activation {
                a.release(marker_list);
            }
        }
        self.ranges.clear();
        self.marker_to_idx.clear();
        if had_any {
            self.version = self.version.wrapping_add(1);
        }
    }

    /// Swap-remove the entry at `idx`, deleting its markers and patching
    /// `marker_to_idx` for whatever entry got swapped in. Does NOT bump
    /// the version — callers do that at batch boundaries.
    fn swap_remove_at(&mut self, idx: usize, marker_list: &mut MarkerList) {
        let removed = self.ranges.swap_remove(idx);
        self.marker_to_idx.remove(&removed.start_marker);
        self.marker_to_idx.remove(&removed.end_marker);
        marker_list.delete(removed.start_marker);
        marker_list.delete(removed.end_marker);
        if let Some(a) = &removed.activation {
            a.release(marker_list);
        }
        if let Some(moved) = self.ranges.get(idx) {
            self.marker_to_idx.insert(moved.start_marker, idx);
            self.marker_to_idx.insert(moved.end_marker, idx);
        }
    }

    /// Query conceal ranges that overlap a viewport range.
    /// Returns ranges sorted by start position for efficient token filtering.
    ///
    /// `cursors` are the rendering split's cursor byte positions, used to
    /// evaluate cursor-dependent activation rules. Pass `&[]` for
    /// cursor-blind consumers (scroll math, `WrapIndex`) — they see
    /// the canonical "no cursor anywhere" rendering.
    pub fn query_viewport(
        &self,
        start: usize,
        end: usize,
        marker_list: &MarkerList,
        cursors: &[usize],
    ) -> Vec<(Range<usize>, Option<&str>)> {
        self.query_viewport_excluding(start, end, marker_list, None, cursors)
    }

    /// The same activated conceals as the grid, with a complete source map
    /// for a graphical Compose view. The caller supplies a complete source
    /// slice: replacements spanning its edges are refused, never clipped into
    /// misleading partial attachments. This performs no I/O or Markdown parse.
    pub fn project_text(
        &self,
        source: &str,
        source_start: usize,
        marker_list: &MarkerList,
        cursors: &[usize],
    ) -> Result<fresh_core::compose::Projection, fresh_core::compose::ProjectionError> {
        self.project_text_with_objects(source, source_start, marker_list, cursors, &[])
    }

    /// Graphical objects take priority over a cell decoration crossing their
    /// source range. Such a decoration cannot be cut into meaningful halves;
    /// retain the original surrounding source and let native text layout wrap
    /// it, rather than inventing partial replacement mappings. The terminal
    /// projection continues to use its original decorations unchanged.
    pub fn project_text_with_objects(
        &self,
        source: &str,
        source_start: usize,
        marker_list: &MarkerList,
        cursors: &[usize],
        objects: &[Range<usize>],
    ) -> Result<fresh_core::compose::Projection, fresh_core::compose::ProjectionError> {
        self.project_text_with_revealed_objects(
            source,
            source_start,
            marker_list,
            cursors,
            objects,
            &[],
        )
    }

    /// A graphical object's source editor reveals its entire markup, including
    /// nested emphasis or a crossing table-cell replacement. Drop a crossing
    /// decoration whole, just as for rendered objects; never invent fragments
    /// of replacement text. Revealed objects remain ordinary copied source.
    pub fn project_text_with_revealed_objects(
        &self,
        source: &str,
        source_start: usize,
        marker_list: &MarkerList,
        cursors: &[usize],
        objects: &[Range<usize>],
        revealed: &[Range<usize>],
    ) -> Result<fresh_core::compose::Projection, fresh_core::compose::ProjectionError> {
        let objects: Vec<_> = objects
            .iter()
            .cloned()
            .map(|source| fresh_core::compose::Conceal {
                source,
                replacement: Some("\u{fffc}"),
            })
            .collect();
        self.project_text_with_overrides(
            source,
            source_start,
            marker_list,
            cursors,
            &objects,
            revealed,
        )
    }

    /// Native ornaments and objects replace whole crossing grid decorations.
    /// Revealed source wins by the caller's choice of disjoint overrides.
    pub fn project_text_with_overrides(
        &self,
        source: &str,
        source_start: usize,
        marker_list: &MarkerList,
        cursors: &[usize],
        overrides: &[fresh_core::compose::Conceal<'_>],
        revealed: &[Range<usize>],
    ) -> Result<fresh_core::compose::Projection, fresh_core::compose::ProjectionError> {
        use fresh_core::compose::{Conceal, Projection, ProjectionError};
        let source_end = source_start
            .checked_add(source.len())
            .ok_or(ProjectionError::CoordinateOverflow)?;
        let ranges = self.query_viewport(source_start, source_end, marker_list, cursors);
        let mut covered = None;
        let mut conceals: Vec<_> = ranges
            .into_iter()
            .filter(|(range, _)| {
                !overrides
                    .iter()
                    .map(|c| &c.source)
                    .chain(revealed)
                    .any(|object| range.start < object.end && range.end > object.start)
            })
            // The grid consumes ranges in source order. A containing range
            // wins until its end; an overlapping range extending past that
            // end emits its replacement once on the still-uncovered suffix.
            .filter_map(|(mut range, replacement)| {
                if let Some(end) = covered {
                    if range.end <= end {
                        return None;
                    }
                    range.start = range.start.max(end);
                }
                covered = Some(range.end);
                Some((range, replacement))
            })
            .map(|(source, replacement)| Conceal {
                source,
                replacement,
            })
            .collect();
        conceals.extend(overrides.iter().cloned());
        Projection::new(source, source_start, &conceals)
    }

    /// Earliest conceal in `start..end` whose cursor-dependent activation
    /// currently differs from the canonical (cursor-less) evaluation.
    ///
    /// The wrap index is canonical, so rows at and after this byte can wrap
    /// differently on screen than the index says. An anchored build whose
    /// anchor sits past this byte must re-anchor here — the model's
    /// `_stable_anchor` — or the frame draws rows the placement never meant.
    pub fn earliest_cursor_divergence(
        &self,
        start: usize,
        end: usize,
        marker_list: &MarkerList,
        cursors: &[usize],
    ) -> Option<usize> {
        self.ranges
            .iter()
            .filter_map(|r| {
                let a = r.activation.as_ref()?;
                let range = r.range(marker_list);
                if range.start < start || range.start >= end {
                    return None;
                }
                (a.is_active(marker_list, cursors) != a.is_active(marker_list, &[]))
                    .then_some(range.start)
            })
            .min()
    }

    /// Like [`query_viewport`](Self::query_viewport), but skips ranges whose
    /// namespace equals `exclude_ns`.
    ///
    /// Conceals live on the *buffer*, so they are visible to every split that
    /// shows it. A compose-only conceal namespace (markdown_compose's
    /// `md-syntax`) must therefore be suppressed when rendering a Source-mode
    /// split even though a sibling split composes the same buffer — the split's
    /// view mode is the only place that scoping can happen. Pass `None` to
    /// include everything (the Compose-mode path and all other callers).
    pub fn query_viewport_excluding(
        &self,
        start: usize,
        end: usize,
        marker_list: &MarkerList,
        exclude_ns: Option<&OverlayNamespace>,
        cursors: &[usize],
    ) -> Vec<(Range<usize>, Option<&str>)> {
        let mut results: Vec<(Range<usize>, Option<&str>)> = self
            .ranges
            .iter()
            .filter(|r| exclude_ns != Some(&r.namespace))
            .filter_map(|r| {
                let range = r.range(marker_list);
                if let Some(a) = &r.activation {
                    if !a.is_active(marker_list, cursors) {
                        return None;
                    }
                }
                if range.start < end && start < range.end {
                    Some((range, r.replacement.as_deref()))
                } else {
                    None
                }
            })
            .collect();

        // Sort by start position for sequential processing
        results.sort_by_key(|(range, _)| range.start);

        // Debug: log conceal ranges being applied during render
        if !results.is_empty() {
            let summary: Vec<String> = results
                .iter()
                .map(|(r, repl)| format!("{}..{}={}", r.start, r.end, repl.unwrap_or("hide")))
                .collect();
            tracing::trace!(
                "[conceal] query_viewport({start}..{end}): {} ranges: {}",
                results.len(),
                summary.join(", ")
            );
        }

        results
    }

    /// Check if a byte position is inside any conceal range.
    /// Returns the conceal info if concealed.
    pub fn is_concealed(
        &self,
        position: usize,
        marker_list: &MarkerList,
    ) -> Option<(Range<usize>, Option<&str>)> {
        for r in &self.ranges {
            let range = r.range(marker_list);
            if range.contains(&position) {
                return Some((range, r.replacement.as_deref()));
            }
        }
        None
    }

    /// Number of stored conceal ranges. Every viewport query walks all of
    /// them (see [`query_viewport_excluding`](Self::query_viewport_excluding)),
    /// so this is the size of a per-frame scan.
    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    /// Returns true if there are no conceal ranges
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Test-only: assert `marker_to_idx` is consistent with `ranges`.
    /// Panics on any divergence. Used by property tests.
    #[cfg(test)]
    fn check_invariants(&self) {
        assert_eq!(
            self.marker_to_idx.len(),
            self.ranges.len() * 2,
            "marker_to_idx size != 2 * ranges.len()"
        );
        for (i, r) in self.ranges.iter().enumerate() {
            assert_eq!(
                self.marker_to_idx.get(&r.start_marker).copied(),
                Some(i),
                "start_marker {:?} of range {} mismapped",
                r.start_marker,
                i,
            );
            assert_eq!(
                self.marker_to_idx.get(&r.end_marker).copied(),
                Some(i),
                "end_marker {:?} of range {} mismapped",
                r.end_marker,
                i,
            );
        }
    }
}

impl Default for ConcealManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_graphical_object_reveals_nested_markup_and_crossing_replacements() {
        let source = "| ![**café**](a.png) | **other** |\r\n";
        let start = source.find('!').unwrap();
        let end = source.find(')').unwrap() + 1;
        let mut markers = MarkerList::new();
        markers.set_buffer_size(source.len());
        let mut manager = ConcealManager::new();
        manager.add(&mut markers, ns(), 0..end + 2, Some("padded cell".into()));
        let bold = source.find("**").unwrap();
        manager.add(&mut markers, ns(), bold..bold + 2, None);
        let projected = manager
            .project_text_with_revealed_objects(
                source,
                0,
                &markers,
                &[start + 2],
                &[],
                &[start..end],
            )
            .unwrap();
        assert_eq!(
            projected.text, source,
            "a preview contributes no source text"
        );
        assert!(projected
            .segments
            .iter()
            .all(|s| s.kind == fresh_core::compose::SegmentKind::Copy));
        assert_eq!(
            manager.project_text(source, 0, &markers, &[]).unwrap().text,
            format!("padded cell{}", &source[end + 2..]),
            "terminal conceals remain unchanged"
        );
    }

    #[test]
    fn graphical_objects_replace_crossing_cell_decorations_without_losing_source() {
        let source = "before ![x](a.png) after\n";
        let start = source.find('!').unwrap();
        let end = source.find(")").unwrap() + 1;
        let mut markers = MarkerList::new();
        markers.set_buffer_size(source.len());
        let mut manager = ConcealManager::new();
        manager.add(
            &mut markers,
            ns(),
            0..source.len(),
            Some("cell replacement".into()),
        );
        let native = manager
            .project_text_with_objects(source, 0, &markers, &[], &[start..end])
            .unwrap();
        assert_eq!(native.text, "before \u{fffc} after\n");
        assert_eq!(
            native.source_position(start, fresh_core::compose::Affinity::Downstream),
            Some(start)
        );
        assert_eq!(
            native.source_position(start + 3, fresh_core::compose::Affinity::Downstream),
            Some(end)
        );
        assert_eq!(
            manager.project_text(source, 0, &markers, &[]).unwrap().text,
            "cell replacement"
        );
    }

    #[test]
    fn rich_projection_keeps_the_grid_precedence_for_crossing_and_nested_ranges() {
        let mut markers = MarkerList::new();
        markers.set_buffer_size(9);
        let mut manager = ConcealManager::new();
        manager.add(&mut markers, ns(), 1..5, Some("X".into()));
        manager.add(&mut markers, ns(), 2..4, Some("nested".into()));
        manager.add(&mut markers, ns(), 3..7, Some("Y".into()));
        let projected = manager.project_text("abcdefghi", 0, &markers, &[]).unwrap();
        assert_eq!(projected.text, "aXYhi");
        assert!(
            manager.project_text("cdefghi", 2, &markers, &[]).is_err(),
            "a requested window must still include the entire first conceal"
        );
    }

    #[test]
    fn rich_projection_uses_the_same_cursor_reveal_rules_without_marker_churn() {
        use fresh_core::compose::Affinity;
        let source = "# Héllo\r\n\r\nbody";
        let mut markers = MarkerList::new();
        markers.set_buffer_size(source.len());
        let mut manager = ConcealManager::new();
        manager.add_with_activation(
            &mut markers,
            ns(),
            0..2,
            None,
            Some(MarkerActivation {
                if_cursor_in: false,
                scope_start: 0,
                scope_end: 9,
            }),
        );
        let version = manager.version();
        let rendered = manager.project_text(source, 0, &markers, &[12]).unwrap();
        assert_eq!(rendered.text, "Héllo\r\n\r\nbody");
        assert_eq!(rendered.source_position(0, Affinity::Downstream), Some(2));
        let editing = manager.project_text(source, 0, &markers, &[5]).unwrap();
        assert_eq!(editing.text, source);
        assert_eq!(editing.source_position(5, Affinity::Downstream), Some(5));
        assert_eq!(
            manager.version(),
            version,
            "Moving a caret must not rebuild the decorations"
        );
    }

    #[test]
    fn rich_projection_tracks_source_edits_through_the_existing_markers() {
        use fresh_core::compose::Affinity;
        let mut markers = MarkerList::new();
        markers.set_buffer_size(9);
        let mut manager = ConcealManager::new();
        manager.add(&mut markers, ns(), 3..5, None);
        assert_eq!(
            manager
                .project_text("a  **word", 0, &markers, &[])
                .unwrap()
                .text,
            "a  word"
        );
        // The native presentation does not maintain a second decoration map.
        markers.adjust_for_insert(0, 2);
        let p = manager
            .project_text("éa  **word", 0, &markers, &[])
            .unwrap();
        assert_eq!(p.text, "éa  word");
        assert_eq!(p.source_position(5, Affinity::Downstream), Some(7));
    }

    fn ns() -> OverlayNamespace {
        OverlayNamespace::from_string("test".to_string())
    }

    /// Typing at the end of a scoped line must not flip an
    /// `unless-cursor-in` conceal on for a frame.
    ///
    /// The reported flicker, at its smallest: a buffer holding `# a`, compose
    /// mode, markdown_compose's conceal over `# ` scoped to the heading's line.
    /// Type one more character and the editor renders the next frame straight
    /// away — the plugin has not seen the edit yet, so this conceal, with the
    /// scope it was emitted with, is what the frame asks. The scope has to have
    /// grown with the text the cursor just typed, or the cursor reads as off
    /// the line, the `# ` conceals itself for that frame and the whole line
    /// re-lays out; the next frame, built from the plugin's rebuilt conceal,
    /// puts it back.
    ///
    /// Driven through the manager rather than [`ScopedActivation`] directly,
    /// because the marker tree's *shape* is part of the bug: the conceal's own
    /// two markers sit beside the scope's, and a span marker in that company
    /// stops being widened at all.
    #[test]
    fn a_conceal_scoped_to_its_line_stays_off_while_the_line_is_typed_on() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(4);
        let mut manager = ConcealManager::new();

        // `# a`: conceal `# `, scoped to the line and one byte past it — the
        // position a cursor at the end of the line holds.
        manager.add_with_activation(
            &mut marker_list,
            ns(),
            0..2,
            None,
            Some(MarkerActivation {
                if_cursor_in: false,
                scope_start: 0,
                scope_end: 4,
            }),
        );
        let concealed = |marker_list: &MarkerList, cursors: &[usize]| {
            !manager
                .query_viewport(0, 1000, marker_list, cursors)
                .is_empty()
        };

        assert!(
            !concealed(&marker_list, &[3]),
            "editing the line: syntax shown"
        );
        assert!(
            concealed(&marker_list, &[]),
            "nothing on the line: concealed"
        );

        // Type `b` at the end of the line. Markers are adjusted before the
        // buffer, exactly as `EditorState::apply_insert` does it, and the
        // cursor lands on 4.
        marker_list.adjust_for_insert(3, 1);
        assert!(
            !concealed(&marker_list, &[4]),
            "the cursor is at the end of the character it just typed, still on \
             the heading's line, so the `# ` must stay visible for this frame"
        );

        // And through a burst, which is what outran the single byte of slack
        // an emitted scope carries.
        marker_list.adjust_for_insert(4, 6);
        assert!(!concealed(&marker_list, &[10]));

        // Deleting back off the line hands the conceal back.
        marker_list.adjust_for_delete(2, 8);
        assert!(concealed(&marker_list, &[20]));
    }

    #[test]
    fn test_conceal_remove_in_range_keeps_only_disjoint() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(200);
        let mut manager = ConcealManager::new();

        manager.add(&mut marker_list, ns(), 0..5, None);
        manager.add(&mut marker_list, ns(), 10..20, None);
        manager.add(&mut marker_list, ns(), 30..40, None);
        manager.add(&mut marker_list, ns(), 50..60, None);

        manager.remove_in_range(&(15..35), &mut marker_list);

        let kept: Vec<_> = manager
            .query_viewport(0, 1000, &marker_list, &[])
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(kept, vec![0..5, 50..60]);
    }

    #[test]
    fn test_conceal_remove_in_range_deletes_markers_and_bumps_version() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(100);
        let mut manager = ConcealManager::new();

        manager.add(&mut marker_list, ns(), 10..20, None);
        let v0 = manager.version();

        manager.remove_in_range(&(0..50), &mut marker_list);
        assert!(manager.is_empty());
        assert_ne!(manager.version(), v0);
    }

    #[test]
    fn test_conceal_remove_in_range_no_match_keeps_version() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(100);
        let mut manager = ConcealManager::new();

        manager.add(&mut marker_list, ns(), 10..20, None);
        let v0 = manager.version();

        manager.remove_in_range(&(50..60), &mut marker_list);
        assert!(!manager.is_empty());
        assert_eq!(manager.version(), v0);
    }

    #[test]
    fn test_conceal_remove_in_range_endpoint_semantics() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(100);
        let mut manager = ConcealManager::new();

        manager.add(&mut marker_list, ns(), 10..20, None);

        manager.remove_in_range(&(20..30), &mut marker_list);
        assert!(!manager.is_empty());
        manager.remove_in_range(&(0..10), &mut marker_list);
        assert!(!manager.is_empty());
        manager.remove_in_range(&(19..21), &mut marker_list);
        assert!(manager.is_empty());
    }

    fn ns_named(name: &str) -> OverlayNamespace {
        OverlayNamespace::from_string(name.to_string())
    }

    #[test]
    fn test_remove_in_range_for_namespace_only_touches_that_namespace() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(200);
        let mut manager = ConcealManager::new();

        // Two plugins conceal overlapping bytes in the same range, plus an
        // out-of-range entry in the target namespace that must survive.
        manager.add(&mut marker_list, ns_named("md"), 10..20, None);
        manager.add(&mut marker_list, ns_named("other"), 12..18, None);
        manager.add(&mut marker_list, ns_named("md"), 100..110, None);

        manager.remove_in_range_for_namespace(&ns_named("md"), &(0..50), &mut marker_list);

        // Only "md"'s in-range range is gone; "other" and the out-of-range
        // "md" entry remain.
        let kept: Vec<_> = manager
            .query_viewport(0, 1000, &marker_list, &[])
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(kept, vec![12..18, 100..110]);
    }

    #[test]
    fn test_remove_in_range_for_namespace_version_and_endpoints() {
        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(100);
        let mut manager = ConcealManager::new();

        manager.add(&mut marker_list, ns_named("md"), 10..20, None);
        let v0 = manager.version();

        // No overlap with this namespace's range → no-op, version unchanged.
        manager.remove_in_range_for_namespace(&ns_named("md"), &(20..30), &mut marker_list);
        assert_eq!(manager.version(), v0);
        assert!(!manager.is_empty());

        // Wrong namespace over an overlapping range → no-op.
        manager.remove_in_range_for_namespace(&ns_named("other"), &(0..50), &mut marker_list);
        assert_eq!(manager.version(), v0);
        assert!(!manager.is_empty());

        // Right namespace, overlapping range → removed, version bumped.
        manager.remove_in_range_for_namespace(&ns_named("md"), &(15..25), &mut marker_list);
        assert!(manager.is_empty());
        assert_ne!(manager.version(), v0);
    }

    /// Mirrors the production cycle in `markdown_compose.ts`: for each line
    /// in a `lines_changed` batch, clear conceals in the line's byte range,
    /// then re-add the line's conceals. Steady-state entry count holds
    /// throughout, exactly like the plugin's per-line rebuild.
    ///
    /// Run with:
    ///   cargo nextest run -p fresh-editor --no-capture \
    ///     view::conceal::tests::perf_full_buffer_rebuild_pass
    #[test]
    fn perf_full_buffer_rebuild_pass() {
        const LINES: usize = 500;
        const LINE_BYTES: usize = 50;
        const CONCEALS_PER_LINE: usize = 5;

        let mut marker_list = MarkerList::new();
        marker_list.set_buffer_size(LINES * LINE_BYTES);
        let mut manager = ConcealManager::new();

        let conceal_byte = |line: usize, k: usize| -> usize {
            line * LINE_BYTES + k * (LINE_BYTES / CONCEALS_PER_LINE)
        };

        // Populate steady state (mirrors first-render: every line has its conceals).
        for line in 0..LINES {
            for k in 0..CONCEALS_PER_LINE {
                let s = conceal_byte(line, k);
                manager.add(&mut marker_list, ns(), s..(s + 2), None);
            }
        }
        let initial = LINES * CONCEALS_PER_LINE;

        // One full-buffer `lines_changed` pass: per line, clear + re-add.
        let start = std::time::Instant::now();
        for line in 0..LINES {
            let line_range = (line * LINE_BYTES)..((line + 1) * LINE_BYTES);
            manager.remove_in_range(&line_range, &mut marker_list);
            for k in 0..CONCEALS_PER_LINE {
                let s = conceal_byte(line, k);
                manager.add(&mut marker_list, ns(), s..(s + 2), None);
            }
        }
        let elapsed = start.elapsed();

        eprintln!(
            "[perf] conceal full-buffer rebuild ({LINES} lines, {} entries steady): \
             {:?} total, {:?}/line",
            initial,
            elapsed,
            elapsed / LINES as u32,
        );
        // Steady state preserved.
        let still_present = manager
            .query_viewport(0, LINES * LINE_BYTES, &marker_list, &[])
            .len();
        assert_eq!(still_present, initial);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum Op {
            // Length capped so the spanning-conceal precondition holds:
            // every conceal is shorter than the smallest query range.
            Add {
                start: usize,
                len: usize,
                ns_idx: u8,
            },
            RemoveInRange {
                start: usize,
                end: usize,
            },
            ClearNamespace {
                ns_idx: u8,
            },
        }

        const BUFFER_SIZE: usize = 200;
        const MAX_CONCEAL_LEN: usize = 4;
        const MIN_QUERY_LEN: usize = MAX_CONCEAL_LEN + 1;

        fn arb_op() -> impl Strategy<Value = Op> {
            prop_oneof![
                3 => (0..(BUFFER_SIZE - MAX_CONCEAL_LEN), 1..=MAX_CONCEAL_LEN, 0u8..3u8)
                    .prop_map(|(start, len, ns_idx)| Op::Add { start, len, ns_idx }),
                2 => (0..BUFFER_SIZE, MIN_QUERY_LEN..=BUFFER_SIZE)
                    .prop_map(|(start, qlen)| {
                        let s = start.min(BUFFER_SIZE - 1);
                        let e = (s + qlen).min(BUFFER_SIZE);
                        Op::RemoveInRange { start: s, end: e }
                    }),
                1 => (0u8..3u8).prop_map(|ns_idx| Op::ClearNamespace { ns_idx }),
            ]
        }

        fn nsf(idx: u8) -> OverlayNamespace {
            OverlayNamespace::from_string(format!("ns{idx}"))
        }

        proptest! {
            /// Invariants must hold after every sequence of operations.
            /// Plus: after `remove_in_range(r)`, no surviving conceal's
            /// range overlaps `r` — given the precondition that conceals
            /// are no longer than the query range.
            #[test]
            fn prop_marker_index_consistent(ops in prop::collection::vec(arb_op(), 0..40)) {
                let mut marker_list = MarkerList::new();
                marker_list.set_buffer_size(BUFFER_SIZE);
                let mut manager = ConcealManager::new();

                for op in ops {
                    match op {
                        Op::Add { start, len, ns_idx } => {
                            manager.add(&mut marker_list, nsf(ns_idx), start..(start + len), None);
                        }
                        Op::RemoveInRange { start, end } => {
                            manager.remove_in_range(&(start..end), &mut marker_list);
                            for (rng, _) in manager.query_viewport(0, BUFFER_SIZE, &marker_list, &[]) {
                                let overlaps = rng.start < end && start < rng.end;
                                prop_assert!(
                                    !overlaps,
                                    "conceal {:?} survived remove_in_range({start}..{end})",
                                    rng,
                                );
                            }
                        }
                        Op::ClearNamespace { ns_idx } => {
                            manager.clear_namespace(&nsf(ns_idx), &mut marker_list);
                        }
                    }
                    manager.check_invariants();
                }
            }
        }
    }
}
