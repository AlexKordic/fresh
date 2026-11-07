//! Per-client graphical viewports. Their anchors are source markers owned by
//! the buffer; no pixel position or wheel event changes a terminal viewport.

use super::compose::Scope;
use super::wire::{Field, Reader, WireError};
use crate::app::{window::Window, Editor};
use crate::model::event::{BufferId, LeafId};
use crate::model::marker::MarkerId;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Value {
    pub position: usize,
    pub affinity: u32,
    /// Caret-fragment top relative to the native content top, in points.
    pub offset: f64,
    /// Horizontal translation of this client's native document, in points.
    pub horizontal_offset: f64,
    pub sequence: u64,
    pub epoch: usize,
    /// Source byte to load without changing the repeatable visible anchor.
    /// A probe within a UTF-8 scalar is normalized to that scalar's start.
    pub region: Option<usize>,
}

impl Value {
    pub fn requested_region(self) -> usize {
        self.region.unwrap_or(self.position)
    }

    pub fn context(self) -> Option<Range<usize>> {
        self.region.map(|_| self.position..self.position)
    }
}

pub(crate) struct Request {
    pub id: u64,
    pub scope: Scope,
    pub value: Value,
}

impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut id = 0;
        let mut scope = None;
        let mut value = Value::default();
        for field in Reader::new(bytes) {
            match field.map_err(|e| e.to_string())? {
                (1, v) => id = v.uint(),
                (2, Field::Bytes(bytes)) => {
                    scope = Some(Scope::parse(bytes).map_err(|e| e.to_string())?)
                }
                (3, v) => {
                    value.position =
                        usize::try_from(v.uint()).map_err(|_| "viewport position is too large")?
                }
                (4, v) => {
                    value.affinity =
                        u32::try_from(v.uint()).map_err(|_| "invalid viewport affinity")?
                }
                (5, v) => {
                    value.offset = match v {
                        Field::Fixed64(bits) => f64::from_bits(bits),
                        _ => return Err(WireError::Truncated.to_string()),
                    }
                }
                (6, v) => {
                    value.epoch =
                        usize::try_from(v.uint()).map_err(|_| "viewport epoch is too large")?
                }
                (7, v) => {
                    value.horizontal_offset = match v {
                        Field::Fixed64(bits) => f64::from_bits(bits),
                        _ => return Err(WireError::Truncated.to_string()),
                    }
                }
                (8, Field::Varint(region)) => {
                    value.region =
                        Some(usize::try_from(region).map_err(|_| "viewport region is too large")?);
                }
                (8, _) => return Err("viewport region must be a source byte".into()),
                _ => {}
            }
        }
        if id == 0
            || value.affinity > 1
            || !value.offset.is_finite()
            || value.offset.abs() > 100_000.0
            || !value.horizontal_offset.is_finite()
            || !(0.0..=100_000_000.0).contains(&value.horizontal_offset)
        {
            return Err("invalid graphical viewport".into());
        }
        value.sequence = id;
        Ok(Self {
            id,
            scope: scope.ok_or("viewport requires a pane")?,
            value,
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Reply {
    pub id: u64,
    pub accepted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    client: u64,
    leaf: LeafId,
    buffer: BufferId,
}

struct Entry {
    marker: MarkerId,
    region_marker: Option<MarkerId>,
    instance: usize,
    revision: u64,
    value: Value,
}

#[derive(Default)]
pub(crate) struct Registry {
    entries: HashMap<Key, Entry>,
}

impl Registry {
    fn value(&mut self, window: &mut Window, key: Key) -> Option<Value> {
        let top = window
            .buffers
            .splits()?
            .1
            .get(&key.leaf)?
            .buffer_state(key.buffer)?
            .viewport
            .top_byte();
        let state = window.buffers.get_mut(&key.buffer)?;
        let top = boundary(state, top.min(state.buffer.len()));
        let revision = state.buffer.version();
        let instance = state.marker_list.instance_id();
        let entry = self.entries.entry(key).or_insert_with(|| Entry {
            marker: state.marker_list.create_left_gravity(top),
            region_marker: None,
            instance,
            revision,
            value: Value {
                position: top,
                epoch: instance,
                ..Default::default()
            },
        });
        let reset = entry.instance != instance
            || revision < entry.revision
            || state.coord_map.crosses_barrier(entry.revision)
            || state.marker_list.get_position(entry.marker).is_none();
        if reset {
            if entry.instance == instance {
                state.marker_list.delete(entry.marker);
                if let Some(marker) = entry.region_marker {
                    state.marker_list.delete(marker);
                }
            }
            entry.region_marker = None;
            let top = boundary(state, top.min(state.buffer.len()));
            entry.marker = state.marker_list.create_left_gravity(top);
            entry.instance = instance;
            entry.value.position = top;
            entry.value.offset = 0.0;
            entry.value.horizontal_offset = 0.0;
            entry.value.affinity = 0;
            entry.value.epoch = instance;
        }
        entry.value.position = state
            .marker_list
            .get_position(entry.marker)?
            .min(state.buffer.len());
        entry.value.region = entry
            .region_marker
            .and_then(|marker| state.marker_list.get_position(marker))
            .map(|position| position.min(state.buffer.len()));
        if entry.value.region.is_none() {
            entry.region_marker = None;
        }
        entry.revision = revision;
        Some(entry.value)
    }

    fn update(&mut self, window: &mut Window, client: u64, request: &Request) -> bool {
        // A queued scroll can still belong to an open, inactive tab. It cannot
        // change the active tab, cursor, or current workspace.
        let open = window.buffers.splits().is_some_and(|(_, views)| {
            views
                .get(&request.scope.leaf)
                .is_some_and(|view| view.has_buffer(request.scope.buffer))
        });
        if !open {
            return false;
        }
        let key = Key {
            client,
            leaf: request.scope.leaf,
            buffer: request.scope.buffer,
        };
        if self.value(window, key).is_none() {
            return false;
        }
        let Some(state) = window.buffers.get_mut(&key.buffer) else {
            return false;
        };
        if request.scope.revision > state.buffer.version() {
            return false;
        }
        // Native source coordinates are signed machine integers. Reject
        // unrepresentable hints before replaying positive edit deltas.
        if request.value.position > isize::MAX as usize
            || request
                .value
                .region
                .is_some_and(|region| region > isize::MAX as usize)
        {
            return false;
        }
        if request.value.epoch != state.marker_list.instance_id() {
            return false;
        }
        let Some(position) =
            state
                .coord_map
                .map_with_gravity(request.value.position, request.scope.revision, true)
        else {
            return false;
        };
        if position > state.buffer.len() || boundary(state, position) != position {
            return false;
        }
        let region = match request.value.region {
            None => None,
            Some(region) => {
                let Some(region) =
                    state
                        .coord_map
                        .map_with_gravity(region, request.scope.revision, true)
                else {
                    return false;
                };
                if region > state.buffer.len() {
                    return false;
                }
                Some(boundary(state, region))
            }
        };
        let entry = self.entries.get_mut(&key).unwrap();
        if !state.marker_list.set_position(entry.marker, position) {
            return false;
        }
        if let Some(marker) = entry.region_marker.take() {
            state.marker_list.delete(marker);
        }
        entry.region_marker =
            region.map(|position| state.marker_list.create_left_gravity(position));
        entry.value = Value {
            position,
            region,
            ..request.value
        };
        entry.revision = state.buffer.version();
        true
    }

    fn retain(&mut self, window: &mut Window, clients: &HashSet<u64>) {
        self.entries.retain(|key, entry| {
            let open = clients.contains(&key.client)
                && window.buffers.splits().is_some_and(|(_, views)| {
                    views
                        .get(&key.leaf)
                        .is_some_and(|view| view.has_buffer(key.buffer))
                });
            if !open {
                if let Some(state) = window.buffers.get_mut(&key.buffer) {
                    if state.marker_list.instance_id() == entry.instance {
                        state.marker_list.delete(entry.marker);
                        if let Some(marker) = entry.region_marker {
                            state.marker_list.delete(marker);
                        }
                    }
                }
            }
            open
        });
    }
}

fn boundary(state: &crate::state::EditorState, mut position: usize) -> usize {
    while position > 0
        && state
            .buffer
            .slice_bytes(position..(position + 1).min(state.buffer.len()))
            .first()
            .is_some_and(|b| b & 0xc0 == 0x80)
    {
        position -= 1;
    }
    position
}

pub(crate) fn snapshot(
    editor: &mut Editor,
    client: u64,
    leaf: LeafId,
    buffer: BufferId,
) -> Option<Value> {
    let window = editor.active_window_mut();
    if window
        .buffers
        .splits()?
        .1
        .get(&leaf)?
        .buffer_state(buffer)?
        .view_mode
        != crate::state::ViewMode::PageView
    {
        return None;
    }
    let mut registry = std::mem::take(&mut window.compose_viewports);
    let value = registry.value(
        window,
        Key {
            client,
            leaf,
            buffer,
        },
    );
    window.compose_viewports = registry;
    value
}

pub(crate) fn update(editor: &mut Editor, client: u64, request: &Request) -> bool {
    let Some(window) = editor
        .windows
        .get_mut(&fresh_core::WindowId(request.scope.window))
    else {
        return false;
    };
    let mut registry = std::mem::take(&mut window.compose_viewports);
    let accepted = registry.update(window, client, request);
    window.compose_viewports = registry;
    accepted
}

pub(crate) fn retain(editor: &mut Editor, clients: &HashSet<u64>) {
    for window in editor.windows.values_mut() {
        let mut registry = std::mem::take(&mut window.compose_viewports);
        registry.retain(window, clients);
        window.compose_viewports = registry;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::marker::MarkerList;
    use serde_json::json;

    #[test]
    fn horizontal_translation_is_optional_finite_nonnegative_and_bounded() {
        use super::super::wire::field;
        let mut base = Vec::new();
        field::uint(&mut base, 1, 1);
        // An empty scope is sufficient for parsing; ownership is checked by update.
        field::raw_bytes(&mut base, 2, &[]);
        assert_eq!(Request::parse(&base).unwrap().value.horizontal_offset, 0.0);
        for value in [0.25, 100_000_000.0] {
            let mut bytes = base.clone();
            field::double(&mut bytes, 7, value);
            assert_eq!(
                Request::parse(&bytes).unwrap().value.horizontal_offset,
                value
            );
        }
        for value in [-0.25, f64::NAN, f64::INFINITY, 100_000_000.25] {
            let mut bytes = base.clone();
            field::double(&mut bytes, 7, value);
            assert!(Request::parse(&bytes).is_err());
        }
        let mut bytes = base;
        field::uint(&mut bytes, 7, 1);
        assert!(
            Request::parse(&bytes).is_err(),
            "a viewport coordinate must be a double"
        );
    }

    #[test]
    fn adjacent_region_is_optional_and_requires_an_integer_source_probe() {
        use super::super::wire::field;
        let mut base = Vec::new();
        field::uint(&mut base, 1, 1);
        field::raw_bytes(&mut base, 2, &[]);
        assert_eq!(Request::parse(&base).unwrap().value.region, None);
        for probe in [0, 90049] {
            let mut bytes = base.clone();
            field::opt_uint(&mut bytes, 8, Some(probe));
            assert_eq!(
                Request::parse(&bytes).unwrap().value.region,
                Some(probe as usize)
            );
        }
        for bytes in [
            {
                let mut bytes = base.clone();
                field::double(&mut bytes, 8, 12.0);
                bytes
            },
            {
                let mut bytes = base;
                field::raw_bytes(&mut bytes, 8, b"12");
                bytes
            },
        ] {
            assert!(Request::parse(&bytes).is_err());
        }
    }

    #[test]
    fn source_markers_survive_long_edit_sessions_and_clean_up_only_their_own_tree() {
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &json!({"type":"zero\nalpha café\nomega\n"}));
        let window = editor.active_window_mut();
        window.focused_view_mut().view_mode = crate::state::ViewMode::PageView;
        let leaf = window.effective_active_split();
        let buffer = window.active_buffer();
        let initial = snapshot(&mut editor, 42, leaf, buffer).unwrap();
        let revision = editor.active_window().active_state().buffer.version();
        let scope = Scope {
            window: editor.active_window().id.0,
            leaf,
            buffer,
            revision,
            projection: 0,
            selection: 0,
        };
        let request = Request {
            id: 1,
            scope,
            value: Value {
                position: 10,
                offset: -3.5,
                horizontal_offset: 129.75,
                sequence: 1,
                region: Some(15), // Interior byte of é: a probe selects its whole scalar.
                ..initial
            },
        };
        assert!(update(&mut editor, 42, &request));
        assert_eq!(
            snapshot(&mut editor, 42, leaf, buffer).unwrap().region,
            Some(14)
        );
        let id = editor.active_window().active_cursors().primary_id();
        editor
            .active_window_mut()
            .active_cursors_mut()
            .get_mut(id)
            .unwrap()
            .position = 0;
        crate::webui::apply_step(&mut editor, &json!({"type":"x".repeat(1100)}));
        assert!(editor
            .active_window()
            .active_state()
            .coord_map
            .map(10, revision)
            .is_none());
        let after = snapshot(&mut editor, 42, leaf, buffer).unwrap();
        assert_eq!(after.position, 1110);
        assert_eq!(after.offset, -3.5);
        assert_eq!(after.horizontal_offset, 129.75);
        assert_eq!(
            after.region,
            Some(1114),
            "the independent probe survives edits beyond the coordinate ring"
        );
        assert_eq!(
            snapshot(&mut editor, 43, leaf, buffer).unwrap().region,
            None
        );
        assert_eq!(
            snapshot(&mut editor, 43, leaf, buffer)
                .unwrap()
                .horizontal_offset,
            0.0,
            "another client cannot inherit the document translation"
        );
        assert!(
            !update(&mut editor, 42, &Request { id: 2, ..request }),
            "an unanchored request older than the ring must refresh"
        );
        let mut cache = crate::webui::compose::ComposeCache::default();
        let before = cache.gather_at(&mut editor, leaf, buffer, None).unwrap();
        let original_epoch = after.epoch;
        let (unrelated, instance) = {
            let state = editor.active_window_mut().active_state_mut();
            state.marker_list = MarkerList::new();
            let unrelated = state.marker_list.create(7);
            (unrelated, state.marker_list.instance_id())
        };
        let reset = snapshot(&mut editor, 42, leaf, buffer).unwrap();
        assert_ne!(reset.epoch, original_epoch);
        assert_eq!(reset.epoch, instance);
        assert_eq!(reset.region, None);
        let next = cache.gather_at(&mut editor, leaf, buffer, None).unwrap();
        assert_ne!(
            before.projection_revision, next.projection_revision,
            "replacement buffers invalidate old scene scopes"
        );
        assert_ne!(
            before.asset_document, next.asset_document,
            "replacement buffers invalidate image identities"
        );
        retain(&mut editor, &HashSet::new());
        let markers = &editor.active_window().active_state().marker_list;
        assert_eq!(markers.get_position(unrelated), Some(7));
        assert_eq!(
            markers.marker_count(),
            1,
            "only the unrelated marker remains"
        );
    }

    #[test]
    fn opaque_changes_reset_the_viewport_and_reject_an_unmappable_scroll() {
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &json!({"type":"first\nsecond\n"}));
        editor.active_window_mut().focused_view_mut().view_mode = crate::state::ViewMode::PageView;
        let leaf = editor.active_window().effective_active_split();
        let buffer = editor.active_window().active_buffer();
        let initial = snapshot(&mut editor, 7, leaf, buffer).unwrap();
        let scope = Scope {
            window: editor.active_window().id.0,
            leaf,
            buffer,
            revision: editor.active_window().active_state().buffer.version(),
            projection: 0,
            selection: 0,
        };
        let request = Request {
            id: 1,
            scope,
            value: Value {
                position: 6,
                offset: -5.0,
                horizontal_offset: 80.5,
                sequence: 1,
                region: Some(8),
                ..initial
            },
        };
        assert!(update(&mut editor, 7, &request));
        crate::webui::apply_step(&mut editor, &json!({"type":"X"}));
        let state = editor.active_window_mut().active_state_mut();
        let revision = state.buffer.version();
        state.coord_map.record_barrier(revision);
        let reset = snapshot(&mut editor, 7, leaf, buffer).unwrap();
        assert_eq!(reset.offset, 0.0);
        assert_eq!(reset.horizontal_offset, 0.0);
        assert_eq!(reset.region, None);
        assert!(!update(&mut editor, 7, &request));
    }

    #[test]
    fn invalid_region_cannot_move_the_anchor_and_normal_scroll_retires_the_probe() {
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &json!({"type":"first\nsecond café\nlast\n"}));
        editor.active_window_mut().focused_view_mut().view_mode = crate::state::ViewMode::PageView;
        let leaf = editor.active_window().effective_active_split();
        let buffer = editor.active_window().active_buffer();
        let initial = snapshot(&mut editor, 7, leaf, buffer).unwrap();
        let scope = Scope {
            window: editor.active_window().id.0,
            leaf,
            buffer,
            revision: editor.active_window().active_state().buffer.version(),
            projection: 0,
            selection: 0,
        };
        let mut request = Request {
            id: 1,
            scope,
            value: Value {
                region: Some(12),
                sequence: 1,
                ..initial
            },
        };
        let count = editor
            .active_window()
            .active_state()
            .marker_list
            .marker_count();
        assert!(update(&mut editor, 7, &request));
        assert_eq!(
            editor
                .active_window()
                .active_state()
                .marker_list
                .marker_count(),
            count + 1
        );
        let retained = snapshot(&mut editor, 7, leaf, buffer).unwrap();
        request.value.position = 6;
        request.value.region = Some(usize::MAX);
        assert!(!update(&mut editor, 7, &request));
        assert_eq!(snapshot(&mut editor, 7, leaf, buffer).unwrap(), retained);
        request.value.region = None;
        assert!(update(&mut editor, 7, &request));
        let moved = snapshot(&mut editor, 7, leaf, buffer).unwrap();
        assert_eq!(moved.position, 6);
        assert_eq!(moved.region, None);
        assert_eq!(
            editor
                .active_window()
                .active_state()
                .marker_list
                .marker_count(),
            count
        );
        let id = editor.active_window().active_cursors().primary_id();
        editor
            .active_window_mut()
            .active_cursors_mut()
            .get_mut(id)
            .unwrap()
            .position = 0;
        crate::webui::apply_step(&mut editor, &json!({"type":"é"}));
        let after_edit = snapshot(&mut editor, 7, leaf, buffer).unwrap();
        request.value.region = Some(usize::MAX);
        assert!(
            !update(&mut editor, 7, &request),
            "an old oversized probe must be refused before edit arithmetic"
        );
        request.value.region = None;
        request.value.position = usize::MAX;
        assert!(
            !update(&mut editor, 7, &request),
            "an old oversized anchor must be refused before edit arithmetic"
        );
        assert_eq!(snapshot(&mut editor, 7, leaf, buffer).unwrap(), after_edit);
    }
}
