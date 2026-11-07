//! Completed formatter output retained independently of an in-flight command
//! stream. The Markdown parser remains the existing source of semantic roles.

use crate::model::coord_map::CoordMap;
use crate::view::conceal::ResolvedConceal;
use std::collections::HashMap;
use std::ops::Range;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(super) struct Semantic {
    pub source: Range<usize>,
    pub role: u32,
    pub flags: u32,
    pub content: Option<usize>,
}

pub(super) struct Snapshot {
    revision: u64,
    outline: Vec<Semantic>,
    records: Vec<ResolvedConceal>,
    capture: Option<(u32, Range<usize>, Vec<Range<usize>>)>,
}

impl Snapshot {
    pub fn new(revision: u64, outline: Vec<Semantic>) -> Self {
        Self {
            revision,
            outline,
            records: Vec::new(),
            capture: None,
        }
    }

    pub fn rebase(mut self, revision: u64, outline: Vec<Semantic>, coordinates: &CoordMap) -> Self {
        if revision < self.revision || coordinates.map(0, self.revision).is_none() {
            return Self::new(revision, outline);
        }
        // Compare the whole parsed outline once, rather than searching every
        // semantic span for every decoration. Changed structure invalidates
        // its affected old/new regions, not unrelated paragraphs and headings.
        let mapped: Vec<_> = self
            .outline
            .iter()
            .filter_map(|span| {
                Some(Semantic {
                    source: coordinates.map_with_gravity(span.source.start, self.revision, true)?
                        ..coordinates.map_with_gravity(span.source.end, self.revision, false)?,
                    role: span.role,
                    flags: span.flags,
                    content: match span.content {
                        Some(p) => Some(coordinates.map_with_gravity(p, self.revision, true)?),
                        None => None,
                    },
                })
            })
            .collect();
        let mut difference: HashMap<Semantic, isize> = HashMap::new();
        for span in mapped {
            *difference.entry(span).or_default() += 1;
        }
        for span in &outline {
            *difference.entry(span.clone()).or_default() -= 1;
        }
        let mut changed: Vec<_> = difference
            .into_iter()
            .filter(|(_, count)| *count != 0)
            .map(|(span, _)| span.source)
            .collect();
        changed.sort_by_key(|range| range.start);
        let mut invalid: Vec<Range<usize>> = Vec::new();
        for range in changed {
            if let Some(last) = invalid.last_mut().filter(|last| last.end >= range.start) {
                last.end = last.end.max(range.end);
            } else {
                invalid.push(range);
            }
        }
        let affected = |range: &Range<usize>| {
            let i = invalid.partition_point(|bad| bad.end < range.start);
            invalid.get(i).is_some_and(|bad| bad.start <= range.end)
        };
        self.records = self
            .records
            .into_iter()
            .filter_map(|mut entry| {
                entry.source = coordinates.map_untouched_range(entry.source, self.revision)?;
                if affected(&entry.source) {
                    return None;
                }
                if let Some((rule, scope)) = entry.activation {
                    let scope = coordinates.map_untouched_range(scope, self.revision)?;
                    if affected(&scope) {
                        return None;
                    }
                    entry.activation = Some((rule, scope));
                }
                Some(entry)
            })
            .collect();
        self.revision = revision;
        self.outline = outline;
        self.capture = None;
        self
    }

    pub fn outline(&self) -> &[Semantic] {
        &self.outline
    }

    pub fn needs_capture(
        &self,
        version: u32,
        region: &Range<usize>,
        complete: &[Range<usize>],
    ) -> bool {
        !self
            .capture
            .as_ref()
            .is_some_and(|(v, r, c)| *v == version && r == region && c == complete)
    }

    pub fn capture(
        &mut self,
        version: u32,
        region: Range<usize>,
        complete: Vec<Range<usize>>,
        length: usize,
        records: Vec<ResolvedConceal>,
    ) {
        let covered = |range: &Range<usize>| {
            let end = range.end.min(length);
            let i = complete.partition_point(|ready| ready.end < end);
            complete
                .get(i)
                .is_some_and(|ready| ready.start <= range.start && end <= ready.end)
        };
        self.records
            .retain(|entry| entry.source.end <= region.start || entry.source.start >= region.end);
        self.records.extend(records.into_iter().filter(|entry| {
            covered(&entry.source)
                && entry
                    .activation
                    .as_ref()
                    .is_none_or(|(_, scope)| covered(scope))
        }));
        self.records.sort_by_key(|entry| entry.source.start);
        self.capture = Some((version, region, complete));
    }

    pub fn active(
        &self,
        region: &Range<usize>,
        cursors: &[usize],
    ) -> Vec<(Range<usize>, Option<&str>)> {
        self.records
            .iter()
            .filter(|entry| entry.source.start < region.end && region.start < entry.source.end)
            .filter(|entry| {
                entry
                    .activation
                    .as_ref()
                    .is_none_or(|(rule, scope)| cursors.iter().any(|p| scope.contains(p)) == *rule)
            })
            .map(|entry| (entry.source.clone(), entry.replacement.as_deref()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn semantic(source: Range<usize>, role: u32) -> Semantic {
        Semantic {
            source,
            role,
            flags: 0,
            content: None,
        }
    }
    fn record(source: Range<usize>, scope: Range<usize>) -> ResolvedConceal {
        ResolvedConceal {
            source,
            replacement: None,
            activation: Some((false, scope)),
        }
    }

    #[test]
    fn source_edits_keep_unaffected_contexts_and_reevaluate_cursor_activation() {
        let outline = vec![
            semantic(0..9, 1),
            semantic(10..20, 11),
            semantic(21..31, 15),
        ];
        let mut snapshot = Snapshot::new(4, outline);
        snapshot.capture(
            1,
            0..31,
            vec![0..31],
            31,
            vec![
                record(0..2, 0..9),
                record(12..14, 10..20),
                record(21..23, 21..31),
            ],
        );
        assert_eq!(
            snapshot.active(&(0..31), &[0]).len(),
            2,
            "the completed snapshot includes inactive alternatives"
        );
        let mut coordinates = CoordMap::default();
        coordinates.record_insert(5, 15, 1);
        let current = vec![
            semantic(0..9, 1),
            semantic(10..21, 11),
            semantic(22..32, 15),
        ];
        let snapshot = snapshot.rebase(5, current, &coordinates);
        let ranges: Vec<_> = snapshot
            .active(&(0..32), &[15])
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(
            ranges,
            vec![0..2, 22..24],
            "only the edited cursor scope loses its old formatting"
        );
    }

    #[test]
    fn changed_parsed_context_invalidates_untouched_markup_and_flow_boundaries() {
        let mut snapshot = Snapshot::new(4, vec![semantic(0..8, 11), semantic(9..20, 11)]);
        snapshot.capture(
            1,
            0..20,
            vec![0..20],
            20,
            vec![record(0..2, 0..8), record(10..12, 9..20)],
        );
        let coordinates = CoordMap::default();
        let snapshot = snapshot.rebase(
            5,
            vec![semantic(0..8, 12), semantic(9..20, 11)],
            &coordinates,
        );
        assert_eq!(
            snapshot
                .active(&(0..20), &[])
                .iter()
                .map(|(r, _)| r.clone())
                .collect::<Vec<_>>(),
            vec![10..12]
        );
    }

    #[test]
    fn repeated_semantic_context_changes_are_not_lost_by_set_comparison() {
        let same = semantic(0..8, 11);
        let mut snapshot = Snapshot::new(4, vec![same.clone(), same.clone()]);
        snapshot.capture(1, 0..8, vec![0..8], 8, vec![record(0..2, 0..8)]);
        let snapshot = snapshot.rebase(5, vec![same], &CoordMap::default());
        assert!(snapshot.active(&(0..8), &[]).is_empty());
    }

    #[test]
    fn partial_completion_cannot_certify_a_scope_and_history_barriers_discard_old_output() {
        let mut snapshot = Snapshot::new(4, vec![]);
        snapshot.capture(1, 0..20, vec![0..10], 20, vec![record(2..4, 0..20)]);
        assert!(snapshot.active(&(0..20), &[]).is_empty());
        assert!(!snapshot.needs_capture(1, &(0..20), &[0..10]));
        assert!(
            snapshot.needs_capture(1, &(0..20), &[0..20]),
            "completion can change without a conceal mutation"
        );
        snapshot.capture(1, 0..20, vec![0..20], 20, vec![record(2..4, 0..20)]);
        let mut coordinates = CoordMap::default();
        coordinates.record_barrier(5);
        assert!(snapshot
            .rebase(5, vec![], &coordinates)
            .active(&(0..20), &[])
            .is_empty());
    }
}
