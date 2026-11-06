//! Deferred motions keep semantic destinations and already planned cursors.
//! Geometry windows may be disjoint; cursor events still commit as one batch.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Origin {
    position: usize,
    anchor: Option<usize>,
    deselect_on_move: bool,
    revision: u64,
    selection: u64,
    environment: u64,
    motion: Motion,
    select: bool,
}

#[derive(Clone, Copy)]
enum Progress {
    Destination {
        destination: Destination,
        goal: f64,
        column: Option<(usize, u32)>,
    },
    Ready(Memory),
}

#[derive(Clone, Copy)]
pub(super) struct CursorPlan {
    origin: Origin,
    progress: Progress,
}

impl InputContext {
    fn resolve(
        &mut self,
        id: CursorId,
        origin: Origin,
        from: usize,
        layout: &Layout,
    ) -> Result<Memory, Need> {
        let progress = self
            .plans
            .get(&id)
            .filter(|plan| {
                plan.origin == origin
                    && match plan.progress {
                        Progress::Destination {
                            destination: Destination::Page(page),
                            ..
                        } => page.environment == layout.environment_revision,
                        _ => true,
                    }
            })
            .map(|plan| plan.progress);
        if let Some(Progress::Ready(memory)) = progress {
            return Ok(memory);
        }
        // Changed source, selection or action must never apply a cached byte
        // destination. Every cursor is validated before emitting any event.
        self.plans.remove(&id);
        let vertical = matches!(
            origin.motion,
            Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown
        );
        let result = if let Some(Progress::Destination {
            destination,
            goal,
            column,
        }) = progress
        {
            match layout.resume(destination, goal, &self.tables) {
                Ok(row) => {
                    let stop = layout.nearest(row, goal);
                    Memory {
                        position: stop.source,
                        revision: origin.revision,
                        affinity: stop.affinity,
                        goal_x: Some(goal),
                        goal_column: layout.column_goal(row, row, column),
                    }
                }
                Err(need) => {
                    let step = Progress::Destination {
                        destination: need.destination.unwrap_or(destination),
                        goal,
                        column,
                    };
                    self.plans.insert(
                        id,
                        CursorPlan {
                            origin,
                            progress: step,
                        },
                    );
                    return Err(need);
                }
            }
        } else {
            let old = self.memory.get(&id).filter(|memory| {
                memory.position == origin.position && memory.revision == origin.revision
            });
            let affinity = old.map_or(Affinity::Downstream, |memory| memory.affinity);
            let (row, x) = layout
                .line_for(from, affinity)
                .ok_or_else(|| Need::region(from))?;
            let goal = old.and_then(|memory| memory.goal_x).unwrap_or(x);
            let previous_column = old
                .filter(|memory| memory.position == from)
                .and_then(|memory| memory.goal_column);
            let column = layout.column_goal(row, row, previous_column);
            let preferred = column.map(|(_, column)| column);
            let target_row =
                match layout.target_row(row, origin.motion, goal, &self.tables, preferred) {
                    Ok(row) => row,
                    Err(need) => {
                        if let Some(destination) = need.destination {
                            self.plans.insert(
                                id,
                                CursorPlan {
                                    origin,
                                    progress: Progress::Destination {
                                        destination,
                                        goal,
                                        column,
                                    },
                                },
                            );
                        }
                        return Err(need);
                    }
                };
            let backwards = matches!(origin.motion, Motion::Up | Motion::PageUp);
            let stop = match origin.motion {
                Motion::Start => layout.lines[row].start,
                Motion::End => layout.lines[row].end,
                Motion::PageUp
                    if target_row == row
                        && layout.source_start == 0
                        && layout.lines[row].y - layout.page_height < 0.0 =>
                {
                    layout.lines[row].start
                }
                Motion::PageDown
                    if target_row == row
                        && layout.source_end == layout.document_len
                        && layout.lines[row].y + layout.page_height >= layout.height
                        && layout.at_end(row) =>
                {
                    layout.lines[row].end
                }
                Motion::PageUp | Motion::PageDown => layout.nearest(target_row, goal),
                _ if target_row == row && backwards => layout.lines[row].start,
                _ if target_row == row && !backwards && layout.at_end(row) => layout.lines[row].end,
                _ => layout.nearest(target_row, goal),
            };
            Memory {
                position: stop.source,
                revision: origin.revision,
                affinity: stop.affinity,
                goal_x: vertical.then_some(goal),
                goal_column: if vertical {
                    layout.column_goal(row, target_row, previous_column)
                } else {
                    None
                },
            }
        };
        self.plans.insert(
            id,
            CursorPlan {
                origin,
                progress: Progress::Ready(result),
            },
        );
        Ok(result)
    }

    pub(super) fn plan(
        &mut self,
        window: &Window,
        motion: Motion,
        select: bool,
    ) -> Result<(Vec<Event>, HashMap<CursorId, Memory>), Need> {
        let layout = self.layout.clone().ok_or_else(Need::default)?;
        let revision = window
            .buffer_state(self.buffer)
            .ok_or_else(Need::default)?
            .buffer
            .version();
        let mut events = Vec::new();
        let mut memory = self.memory.clone();
        for (id, cursor) in window.active_cursors().iter() {
            let backwards = matches!(motion, Motion::Up | Motion::PageUp);
            let vertical = matches!(
                motion,
                Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown
            );
            let from = if vertical && !select && cursor.deselect_on_move {
                cursor.anchor.map_or(cursor.position, |anchor| {
                    if backwards {
                        anchor.min(cursor.position)
                    } else {
                        anchor.max(cursor.position)
                    }
                })
            } else {
                cursor.position
            };
            let target = self.resolve(
                id,
                Origin {
                    position: cursor.position,
                    anchor: cursor.anchor,
                    deselect_on_move: cursor.deselect_on_move,
                    revision,
                    selection: self.selection_revision,
                    environment: if matches!(motion, Motion::PageUp | Motion::PageDown) {
                        layout.environment_revision
                    } else {
                        0
                    },
                    motion,
                    select,
                },
                from,
                &layout,
            )?;
            let new_anchor = if select {
                Some(cursor.anchor.unwrap_or(cursor.position))
            } else if cursor.deselect_on_move {
                None
            } else {
                cursor.anchor
            };
            events.push(Event::MoveCursor {
                cursor_id: id,
                old_position: cursor.position,
                new_position: target.position,
                old_anchor: cursor.anchor,
                new_anchor,
                old_sticky_column: cursor.sticky_column,
                new_sticky_column: None,
            });
            memory.insert(id, target);
        }
        Ok((events, memory))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose(start: usize, end: usize, id: u64) -> Arc<Layout> {
        let first = Stop {
            source: start,
            x: 0.0,
            affinity: Affinity::Downstream,
        };
        let last = Stop {
            source: end,
            x: 80.0,
            affinity: Affinity::Upstream,
        };
        Arc::new(Layout {
            id,
            page_height: 40.0,
            source_start: start,
            source_end: end,
            document_len: 100,
            total_lines: 1,
            height: 20.0,
            style_revision: id,
            environment_revision: 0,
            anchors: Vec::new(),
            lines: vec![Line {
                number: 0,
                y: 0.0,
                height: 20.0,
                source_start: start,
                source_end: end,
                start: first,
                end: last,
                stops: vec![first, last],
                cell: None,
            }],
            bands: vec![cells::Band {
                lines: 0..1,
                y: 0.0,
                bottom: 20.0,
            }],
        })
    }

    #[test]
    fn disjoint_cursor_measurements_plan_one_atomic_event_batch() {
        use crate::model::cursor::Cursor;
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"x".repeat(100)}));
        *editor.active_cursors_mut().primary_mut() = Cursor::new(10);
        let primary = editor.active_cursors().primary_id();
        let other = editor.active_cursors_mut().add(Cursor::new(90));
        let window = editor.active_window();
        let (first, second, requested) =
            if window.active_cursors().iter().next().unwrap().1.position == 10 {
                (prose(0, 40, 1), prose(80, 100, 2), 90)
            } else {
                (prose(80, 100, 1), prose(0, 40, 2), 10)
            };
        let mut context = InputContext::new(
            window.effective_active_split(),
            window.active_buffer(),
            Some(first),
            HashMap::new(),
        );
        let need = context.plan(window, Motion::End, true).unwrap_err();
        assert_eq!(need.region, Some(requested));
        assert_eq!(window.active_cursors().get(primary).unwrap().position, 10);
        assert_eq!(window.active_cursors().get(other).unwrap().position, 90);
        assert!(
            context.memory.is_empty(),
            "deferral cannot install part of a cursor batch"
        );

        // Neither the old origin nor its destination exists in this window.
        context.layout = Some(second);
        let (events, memory) = context.plan(window, Motion::End, true).unwrap();
        assert_eq!(events.len(), 2);
        let mut destinations = events
            .into_iter()
            .map(|event| match event {
                Event::MoveCursor {
                    cursor_id,
                    new_position,
                    new_anchor,
                    ..
                } => (cursor_id.0, new_position, new_anchor),
                _ => panic!("only cursor events belong to a graphical motion"),
            })
            .collect::<Vec<_>>();
        destinations.sort_by_key(|(id, _, _)| *id);
        assert_eq!(
            destinations,
            vec![(primary.0, 40, Some(10)), (other.0, 100, Some(90))]
        );
        assert_eq!(memory[&primary].position, 40);
        assert_eq!(memory[&other].position, 100);
        assert_eq!(
            window.active_cursors().get(primary).unwrap().position,
            10,
            "Fresh applies the complete batch after action preparation"
        );
    }

    #[test]
    fn stale_cursor_source_selection_or_action_cannot_reuse_a_resolved_destination() {
        let origin = Origin {
            position: 10,
            anchor: None,
            deselect_on_move: true,
            revision: 1,
            selection: 0,
            environment: 0,
            motion: Motion::End,
            select: true,
        };
        let first = prose(0, 40, 1);
        let second = prose(80, 100, 2);
        for changed in [
            Origin {
                position: 11,
                ..origin
            },
            Origin {
                anchor: Some(5),
                ..origin
            },
            Origin {
                deselect_on_move: false,
                ..origin
            },
            Origin {
                revision: 2,
                ..origin
            },
            Origin {
                motion: Motion::Start,
                ..origin
            },
            Origin {
                select: false,
                ..origin
            },
        ] {
            let mut context = InputContext::new(
                LeafId(fresh_core::SplitId(0)),
                BufferId(0),
                None,
                HashMap::new(),
            );
            let id = CursorId(0);
            assert_eq!(
                context
                    .resolve(id, origin, origin.position, &first)
                    .unwrap()
                    .position,
                40
            );
            assert_eq!(
                context
                    .resolve(id, changed, changed.position, &second)
                    .unwrap_err()
                    .region,
                Some(changed.position)
            );
        }
    }
}
