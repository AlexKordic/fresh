//! Absolute source selections for accessibility and semantic navigation.
//! Validate and rebase the whole group before replacing any live cursor.

use super::{Input, InputTarget};
use crate::app::Editor;
use crate::model::cursor::Cursor;
use crate::webui::wire::{Field, Reader, WireError};

pub(super) const MAX_TARGETS: usize = 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Target {
    pub position: usize,
    pub anchor: Option<usize>,
}

impl Target {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let (mut position, mut anchor) = (None, None);
        for field in Reader::new(bytes) {
            let (tag, value) = field?;
            let slot = match tag {
                1 => &mut position,
                2 => &mut anchor,
                _ => continue,
            };
            if slot.is_some() {
                return Err(WireError::InvalidField(
                    "duplicate source selection endpoint",
                ));
            }
            let Field::Varint(value) = value else {
                return Err(WireError::InvalidField(
                    "source selection endpoint wire type",
                ));
            };
            *slot = Some(usize::try_from(value).map_err(|_| WireError::LongVarint)?);
        }
        Ok(Self {
            position: position.ok_or(WireError::InvalidField("source selection position"))?,
            anchor,
        })
    }

    fn range(self) -> std::ops::Range<usize> {
        let other = self.anchor.unwrap_or(self.position);
        self.position.min(other)..self.position.max(other)
    }

    fn cursor(self) -> Cursor {
        match self.anchor.filter(|anchor| *anchor != self.position) {
            Some(anchor) => Cursor::with_selection(anchor, self.position),
            None => Cursor::new(self.position),
        }
    }
}

impl Input<'_> {
    pub(crate) fn select(&self, editor: &mut Editor) -> Result<(), String> {
        let scope = self.scope.ok_or("The selection has no editor view.")?;
        let revision = self
            .source_revision
            .ok_or("The selection has no source revision.")?;
        if self.selection_targets.is_empty()
            || self.selection_targets.len() > MAX_TARGETS
            || !editor.native_compose_pointer_allowed()
            || !(InputTarget {
                id: self.id,
                scope,
                epoch: self.buffer_epoch,
                pointer: false,
            })
            .valid(editor)
            || !super::document_keyboard(editor)
        {
            return Err(
                "The selection view changed or lost keyboard focus. Select the text again.".into(),
            );
        }
        let state = editor.active_window().buffer_state(scope.buffer).unwrap();
        if revision > state.buffer.version() {
            return Err("The selection refers to an unavailable source revision.".into());
        }
        let boundary = |position: usize| {
            position <= state.buffer.len()
                && (position == state.buffer.len()
                    || state
                        .buffer
                        .slice_bytes(position..position + 1)
                        .first()
                        .is_some_and(|byte| byte & 0xc0 != 0x80))
        };
        let mut targets = Vec::with_capacity(self.selection_targets.len());
        for target in &self.selection_targets {
            let range = state
                .coord_map
                .map_unchanged_range(target.range(), revision)
                .ok_or("The selected source changed. Select the text again.")?;
            if !boundary(range.start) || !boundary(range.end) {
                return Err("The selection is not at a UTF-8 source boundary.".into());
            }
            let reversed = target.anchor.is_some_and(|anchor| anchor > target.position);
            targets.push(Target {
                position: if reversed { range.start } else { range.end },
                anchor: target.anchor.filter(|_| !range.is_empty()).map(|_| {
                    if reversed {
                        range.end
                    } else {
                        range.start
                    }
                }),
            });
        }
        let mut ranges: Vec<_> = targets.iter().map(|target| target.range()).collect();
        ranges.sort_by_key(|range| (range.start, range.end));
        if ranges
            .windows(2)
            .any(|pair| pair[0].end > pair[1].start || pair[0].start == pair[1].start)
            || targets
                .iter()
                .map(|target| target.position)
                .collect::<std::collections::HashSet<_>>()
                .len()
                != targets.len()
        {
            return Err("Selection ranges overlap; no cursor was moved.".into());
        }
        // Keep the primary identity and the allocator's history. New secondary
        // IDs come from Fresh, never from an accessibility caller or a preview.
        let mut cursors = editor.active_cursors().clone();
        let primary = cursors.primary_id();
        for id in cursors.ids() {
            if id != primary {
                cursors.remove(id);
            }
        }
        for target in targets.iter().skip(1) {
            cursors.add(target.cursor());
        }
        cursors.insert_with_id(primary, targets[0].cursor());
        editor.dismiss_transient_popups();
        editor
            .active_window_mut()
            .set_buffer_cursor_in_splits_selecting(
                scope.buffer,
                targets[0].position,
                &[scope.leaf],
                false,
            );
        *editor.active_cursors_mut() = cursors;
        editor.active_window_mut().compose_input = None;
        editor.mark_plugin_snapshot_dirty();
        editor.shell_description_stale = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::webui::compose::{ComposeCache, Scope};
    use crate::webui::wire::field;

    fn fixture() -> (Editor, tempfile::TempDir, Scope, usize) {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.clipboard.use_osc52 = false;
        config.clipboard.use_system_clipboard = false;
        let mut editor = Editor::for_test(
            config,
            80,
            24,
            Some(root.path().into()),
            crate::config_io::DirectoryContext::for_testing(root.path()),
            crate::view::color_support::ColorCapability::TrueColor,
            std::sync::Arc::new(crate::model::filesystem::StdFileSystem),
            None,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        editor.insert_native_text("alpha café 👩🏽‍💻\r\nomega".into());
        editor.active_window_mut().focused_view_mut().view_mode = crate::state::ViewMode::PageView;
        let leaf = editor.active_window().effective_active_split();
        let buffer = editor.active_window().active_buffer();
        let pane = ComposeCache::default()
            .gather_at(&mut editor, leaf, buffer, Some(0))
            .unwrap();
        let scope = Scope {
            window: editor.active_window().id.0,
            leaf,
            buffer,
            revision: pane.buffer_revision,
            projection: pane.projection_revision,
            selection: pane.selection_revision,
        };
        let epoch = editor.active_state().marker_list.instance_id();
        (editor, root, scope, epoch)
    }

    fn bytes(scope: Scope, epoch: usize, targets: &[(usize, Option<usize>)]) -> Vec<u8> {
        let mut encoded = Vec::new();
        for (tag, value) in [
            (1, scope.window),
            (2, scope.leaf.0 .0 as u64),
            (3, scope.buffer.0 as u64),
            (4, scope.revision),
            (5, scope.projection),
            (6, scope.selection),
        ] {
            field::opt_uint(&mut encoded, tag, Some(value));
        }
        let mut body = Vec::new();
        field::raw_bytes(&mut body, 2, &encoded);
        field::opt_uint(&mut body, 13, Some(scope.revision));
        field::uint(&mut body, 14, epoch as u64);
        for &(position, anchor) in targets {
            let mut target = Vec::new();
            field::opt_uint(&mut target, 1, Some(position as u64));
            field::opt_uint(&mut target, 2, anchor.map(|a| a as u64));
            field::raw_bytes(&mut body, 22, &target);
        }
        body
    }

    #[test]
    fn selections_replace_the_complete_cursor_group_without_editing_source_or_reusing_ids() {
        let (mut editor, _root, scope, epoch) = fixture();
        let old = editor.active_cursors_mut().add(Cursor::new(2));
        let primary = editor.active_cursors().primary_id();
        let version = editor.active_state().buffer.version();
        let wire = bytes(scope, epoch, &[(5, Some(0)), (6, None)]);
        Input::parse(&wire).unwrap().select(&mut editor).unwrap();
        assert_eq!(editor.active_state().buffer.version(), version);
        assert_eq!(editor.active_cursors().count(), 2);
        assert_eq!(editor.active_cursors().primary_id(), primary);
        assert_eq!(
            editor.active_cursors().primary().selection_range(),
            Some(0..5)
        );
        assert!(editor
            .active_cursors()
            .iter()
            .any(|(id, c)| id.0 > old.0 && c.position == 6));
        let wire = bytes(scope, epoch, &[(0, None)]);
        Input::parse(&wire).unwrap().select(&mut editor).unwrap();
        assert_eq!(editor.active_cursors().count(), 1);
        assert_eq!(editor.active_cursors().primary_id(), primary);
        assert_eq!(editor.active_cursors().primary().position, 0);
    }

    #[test]
    fn invalid_later_targets_and_ambiguous_groups_cannot_partially_move_a_cursor() {
        let (mut editor, _root, scope, epoch) = fixture();
        let before = *editor.active_cursors().primary();
        for targets in [
            vec![(0, None), (99999, None)],
            vec![(3, Some(0)), (4, Some(2))],
            vec![(0, None), (0, None)],
            vec![(10, None)],
        ] {
            // Byte 10 is the continuation byte of é.
            let wire = bytes(scope, epoch, &targets);
            assert!(
                Input::parse(&wire).unwrap().select(&mut editor).is_err(),
                "{targets:?}"
            );
            assert_eq!(*editor.active_cursors().primary(), before);
            assert_eq!(editor.active_cursors().count(), 1);
        }
    }

    #[test]
    fn unchanged_ranges_rebase_but_modified_ranges_and_retired_sources_are_refused() {
        let (mut editor, _root, scope, epoch) = fixture();
        let wire = bytes(scope, epoch, &[(5, Some(0))]);
        editor
            .active_window_mut()
            .set_buffer_cursor_in_splits_selecting(scope.buffer, 0, &[scope.leaf], false);
        editor.insert_native_text("prefix ".into());
        Input::parse(&wire).unwrap().select(&mut editor).unwrap();
        assert_eq!(
            editor.active_cursors().primary().selection_range(),
            Some(7..12)
        );
        editor.insert_native_text("changed".into());
        assert!(Input::parse(&wire).unwrap().select(&mut editor).is_err());
        let wire = bytes(scope, epoch + 1, &[(0, None)]);
        assert!(Input::parse(&wire).unwrap().select(&mut editor).is_err());
    }

    #[test]
    fn source_selection_wire_requires_typed_endpoints_and_exclusive_revisioned_ownership() {
        let (editor, _root, scope, epoch) = fixture();
        drop(editor);
        for raw in [vec![], vec![0x08, 0, 0x08, 1], vec![0x0a, 0], vec![0x10, 0]] {
            assert!(Target::parse(&raw).is_err());
        }
        let wire = bytes(scope, epoch, &[(0, Some(0))]);
        let parsed = Input::parse(&wire).unwrap();
        assert_eq!(parsed.selection_targets[0].anchor, Some(0));
        let fixed = Target::parse(&[8, 5, 16, 0]).unwrap();
        assert_eq!(fixed.position, 5);
        assert_eq!(fixed.anchor, Some(0));
        for (tag, value) in [(3, 17), (5, 0), (6, 1), (7, 2), (11, 0), (12, 0), (18, 1)] {
            let mut mixed = wire.clone();
            field::opt_uint(&mut mixed, tag, Some(value));
            assert!(Input::parse(&mixed).is_err(), "field {tag}");
        }
        let huge = bytes(scope, epoch, &vec![(0, None); MAX_TARGETS + 1]);
        assert!(Input::parse(&huge).is_err());
        let mut wrong = Vec::new();
        field::uint(&mut wrong, 22, 1);
        assert!(Input::parse(&wrong).is_err());
    }

    #[test]
    fn read_only_source_keeps_selection_available_and_reports_editability_separately() {
        let (mut editor, _root, scope, epoch) = fixture();
        editor.active_state_mut().editing_disabled = true;
        let wire = bytes(scope, epoch, &[(5, Some(0))]);
        Input::parse(&wire).unwrap().select(&mut editor).unwrap();
        let pane = ComposeCache::default()
            .gather_at(&mut editor, scope.leaf, scope.buffer, Some(0))
            .unwrap();
        assert_eq!(pane.editable, Some(false));
        assert_eq!(
            editor.active_cursors().primary().selection_range(),
            Some(0..5)
        );
        let before = *editor.active_cursors().primary();
        let mut retired = scope;
        retired.window += 1;
        let wire = bytes(retired, epoch, &[(0, None)]);
        assert!(Input::parse(&wire).unwrap().select(&mut editor).is_err());
        assert_eq!(*editor.active_cursors().primary(), before);
    }
}
