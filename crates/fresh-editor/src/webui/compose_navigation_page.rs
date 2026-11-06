//! Pixel page intent survives a new source window via a repeatable flow point.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct Anchor {
    source: usize,
    y: f64,
    start: usize,
    end: usize,
    kind: u32,
}
impl Anchor {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut a = Self {
            source: 0,
            y: 0.0,
            start: 0,
            end: 0,
            kind: 0,
        };
        for field in fields(bytes) {
            match field? {
                (1, v) => a.source = v.uint() as usize,
                (2, v) => a.y = number(v)?,
                (3, v) => a.start = v.uint() as usize,
                (4, v) => a.end = v.uint() as usize,
                (5, v) => a.kind = u32::try_from(v.uint()).map_err(|_| "invalid flow kind")?,
                _ => {}
            }
        }
        Ok(a)
    }
}
pub(super) fn valid_geometry(anchors: &[Anchor], height: f64) -> bool {
    anchors.iter().all(|a| {
        a.kind <= 2 && a.y >= 0.0 && a.y <= height && a.start <= a.source && a.source <= a.end
    }) && anchors
        .windows(2)
        .all(|p| (p[0].source, p[0].kind) < (p[1].source, p[1].kind) && p[0].y <= p[1].y)
}
pub(super) fn valid_source(
    anchors: &[Anchor],
    layout: &Layout,
    valid: &impl Fn(usize) -> bool,
    tables: &[super::super::compose_tables::Table],
) -> bool {
    anchors.iter().all(|a| {
        valid(a.source)
            && valid(a.start)
            && valid(a.end)
            && match a.kind {
                1 => tables
                    .iter()
                    .flat_map(|t| &t.rows)
                    .any(|r| r.start == a.source && a.start == r.start && a.end == r.end),
                2 => a.end == layout.document_len,
                _ => a.start < a.end,
            }
    })
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Destination {
    anchor: Anchor,
    offset: f64,
    pub environment: u64,
    column: Option<(usize, u32)>,
}
impl Layout {
    fn page_need(&self, mut need: Need, y: f64, column: Option<(usize, u32)>) -> Need {
        let chosen = if need.region.is_some() {
            if y < 0.0 {
                self.anchors.first()
            } else {
                self.anchors.last()
            }
        } else {
            self.anchors
                .iter()
                .min_by(|a, b| (a.y - y).abs().total_cmp(&(b.y - y).abs()))
        };
        if let Some(&anchor) = chosen {
            need.destination = Some(super::Destination::Page(Destination {
                anchor,
                offset: y - anchor.y,
                environment: self.environment_revision,
                column,
            }));
            if need.region.is_some() {
                need.context = Some(anchor.start..anchor.end);
            }
        }
        need
    }

    pub(super) fn page_at(
        &self,
        y: f64,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
        column: Option<(usize, u32)>,
    ) -> Result<usize, Need> {
        if y < 0.0 && self.source_start > 0 {
            return Err(self.page_need(Need::region(self.source_start - 1), y, column));
        }
        if y >= self.height && self.source_end < self.document_len {
            return Err(self.page_need(Need::region(self.source_end), y, column));
        }
        let y = y.clamp(0.0, (self.height - 0.001).max(0.0));
        self.page_row(y, goal, tables, column)
            .map_err(|need| self.page_need(need, y, column))
    }

    pub(super) fn resume_page(
        &self,
        destination: Destination,
        goal: f64,
        tables: &[super::super::compose_tables::Table],
    ) -> Result<usize, Need> {
        let Some(anchor) = self
            .anchors
            .iter()
            .find(|a| a.source == destination.anchor.source && a.kind == destination.anchor.kind)
        else {
            let mut need = Need::region(destination.anchor.source)
                .destination(super::Destination::Page(destination));
            need.context = Some(destination.anchor.start..destination.anchor.end);
            return Err(need);
        };
        self.page_at(
            anchor.y + destination.offset,
            goal,
            tables,
            destination.column,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layout(start: usize, end: usize, height: f64, points: &[(usize, f64)]) -> Layout {
        let lines = points
            .iter()
            .enumerate()
            .map(|(number, &(source, y))| {
                let stop = Stop {
                    source,
                    x: 12.0,
                    affinity: Affinity::Downstream,
                };
                Line {
                    number,
                    y,
                    height: 10.0,
                    source_start: source,
                    source_end: source + 1,
                    start: stop,
                    end: stop,
                    stops: vec![stop],
                    cell: None,
                }
            })
            .collect::<Vec<_>>();
        Layout {
            id: 1,
            page_height: 200.0,
            source_start: start,
            source_end: end,
            document_len: 1000,
            total_lines: lines.len(),
            height,
            style_revision: 1,
            environment_revision: 7,
            anchors: points
                .iter()
                .map(|&(source, y)| Anchor {
                    source,
                    y,
                    start: source,
                    end: source + 1,
                    kind: 0,
                })
                .collect(),
            bands: cells::bands(&lines, height).unwrap(),
            lines,
        }
    }

    #[test]
    fn pages_rebase_remaining_pixels_over_multiple_windows_without_the_origin() {
        let first = layout(0, 100, 100.0, &[(20, 20.0), (80, 90.0)]);
        let need = first
            .target_row(0, Motion::PageDown, 12.0, &[], None)
            .unwrap_err();
        assert_eq!(need.region, Some(100));
        assert_eq!(need.context, Some(80..81));
        let second = layout(60, 200, 70.0, &[(80, 10.0), (120, 60.0)]);
        let need = second
            .resume(need.destination.unwrap(), 12.0, &[])
            .unwrap_err();
        assert_eq!(need.region, Some(200));
        assert_eq!(need.context, Some(120..121));
        let mut third = layout(100, 300, 150.0, &[(120, 5.0), (220, 85.0), (240, 110.0)]);
        third.page_height = 1.0;
        assert_eq!(
            third.resume(need.destination.unwrap(), 12.0, &[]).unwrap(),
            1,
            "the captured 80 remaining pixels are not a new page-height request"
        );
        let mut up = layout(100, 300, 150.0, &[(120, 5.0), (220, 85.0), (240, 110.0)]);
        up.page_height = 120.0;
        let need = up
            .target_row(1, Motion::PageUp, 12.0, &[], None)
            .unwrap_err();
        assert_eq!(need.region, Some(99));
        let previous = layout(0, 180, 100.0, &[(20, 20.0), (60, 50.0), (120, 90.0)]);
        assert_eq!(
            previous
                .resume(need.destination.unwrap(), 12.0, &[])
                .unwrap(),
            1
        );
    }

    #[test]
    fn source_selection_or_environment_changes_replan_the_original_page_before_any_move() {
        use crate::model::cursor::Cursor;
        for environment in [false, true] {
            let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
            crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"x".repeat(1000)}));
            *editor.active_cursors_mut().primary_mut() = Cursor::new(20);
            let window = editor.active_window();
            let mut context = InputContext::new(
                window.effective_active_split(),
                window.active_buffer(),
                Some(Arc::new(layout(0, 100, 100.0, &[(20, 20.0), (80, 90.0)]))),
                HashMap::new(),
            );
            assert_eq!(
                context
                    .plan(window, Motion::PageDown, true)
                    .unwrap_err()
                    .region,
                Some(100)
            );
            let mut next = layout(60, 300, 200.0, &[(80, 10.0), (220, 140.0)]);
            if environment {
                next.environment_revision = 8;
            } else {
                context.selection_revision = 1;
            }
            context.layout = Some(Arc::new(next));
            assert_eq!(
                context
                    .plan(window, Motion::PageDown, true)
                    .unwrap_err()
                    .region,
                Some(20)
            );
            assert_eq!(window.active_cursors().primary().position, 20);
        }
    }

    #[test]
    fn resolved_and_deferred_cursor_pages_cannot_mix_rendering_environments() {
        use crate::model::cursor::Cursor;
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"x".repeat(1000)}));
        *editor.active_cursors_mut().primary_mut() = Cursor::new(20);
        editor.active_cursors_mut().add(Cursor::new(120));
        let window = editor.active_window();
        let first = window.active_cursors().iter().next().unwrap().1.position;
        let other = if first == 20 { 120 } else { 20 };
        let mut before = layout(
            first,
            first + 60,
            100.0,
            &[(first, 20.0), (first + 10, 40.0), (first + 50, 90.0)],
        );
        before.page_height = 20.0;
        let mut context = InputContext::new(
            window.effective_active_split(),
            window.active_buffer(),
            Some(Arc::new(before)),
            HashMap::new(),
        );
        assert_eq!(
            context
                .plan(window, Motion::PageDown, true)
                .unwrap_err()
                .region,
            Some(other)
        );
        let mut after = layout(
            other,
            other + 60,
            100.0,
            &[(other, 20.0), (other + 10, 40.0)],
        );
        after.environment_revision = 8;
        context.layout = Some(Arc::new(after));
        assert_eq!(
            context
                .plan(window, Motion::PageDown, true)
                .unwrap_err()
                .region,
            Some(first)
        );
        assert!(context.memory.is_empty());
    }

    #[test]
    fn retained_context_cannot_cross_a_window_leaf_document_or_source_revision() {
        let original = crate::webui::compose::Scope {
            window: 1,
            leaf: LeafId(fresh_core::SplitId(2)),
            buffer: BufferId(3),
            revision: 4,
            projection: 5,
            selection: 6,
        };
        for changed in [
            crate::webui::compose::Scope {
                window: 2,
                ..original
            },
            crate::webui::compose::Scope {
                leaf: LeafId(fresh_core::SplitId(3)),
                ..original
            },
            crate::webui::compose::Scope {
                buffer: BufferId(4),
                ..original
            },
            crate::webui::compose::Scope {
                revision: 5,
                ..original
            },
        ] {
            let mut session = Session::default();
            session.identity = Some(Identity(original.window, original.leaf, original.buffer));
            session.pending_context = Some((original.revision, 30..40));
            session.request_region(&original, 50);
            assert_eq!(session.context(&original), Some(30..40));
            session.request_region(&changed, 50);
            assert_eq!(session.context(&changed), None);
        }
    }

    #[test]
    fn malformed_flow_geometry_and_ownership_are_refused() {
        let mut value = layout(0, 100, 100.0, &[(20, 20.0), (80, 90.0)]);
        assert!(valid_geometry(&value.anchors, value.height));
        value.anchors[1].y = 10.0;
        assert!(!valid_geometry(&value.anchors, value.height));
        value.anchors[1].y = 90.0;
        value.anchors[1].source = 20;
        assert!(!valid_geometry(&value.anchors, value.height));
        value.anchors.truncate(1);
        value.anchors[0].end = 21;
        assert!(
            !valid_source(&value.anchors, &value, &|at| at != 21, &[]),
            "UTF-8 interior context boundaries cannot become retained source"
        );
        value.anchors[0].kind = 1;
        assert!(
            !valid_source(&value.anchors, &value, &|_| true, &[]),
            "a row anchor needs parser-owned row context"
        );
    }

    #[test]
    fn a_page_within_first_line_padding_keeps_its_pixel_column() {
        use crate::model::cursor::Cursor;
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"x".repeat(1000)}));
        *editor.active_cursors_mut().primary_mut() = Cursor::new(15);
        let mut value = layout(0, 1000, 40.0, &[(11, 20.0)]);
        value.page_height = 5.0;
        value.lines[0].source_end = 20;
        value.lines[0].stops.push(Stop {
            source: 15,
            x: 30.0,
            affinity: Affinity::Downstream,
        });
        let window = editor.active_window();
        let mut context = InputContext::new(
            window.effective_active_split(),
            window.active_buffer(),
            Some(Arc::new(value)),
            HashMap::new(),
        );
        let (events, _) = context.plan(window, Motion::PageUp, false).unwrap();
        assert!(
            matches!(
                &events[0],
                Event::MoveCursor {
                    new_position: 15,
                    ..
                }
            ),
            "padding is not a document-edge jump: {events:?}"
        );
    }
}
