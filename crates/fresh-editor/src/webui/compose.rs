//! Graphical Compose data before terminal wrapping and cell flattening.
//!
//! The existing Compose conceal manager remains the text projection. The
//! Markdown parser already used by Fresh supplies semantic ranges (including
//! properly parsed image destinations), never replacement document contents.

use crate::app::Editor;
use crate::model::event::{BufferId, LeafId};
use crate::state::ViewMode;
use fresh_core::compose::{Conceal, Projection, SegmentKind};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Weak};

const BACKGROUND_PARSE_BYTES: usize = 64 << 10;
const WINDOW_BYTES: usize = 64 << 10;
const CACHE_LIMIT: usize = 8;

#[path = "compose_parse.rs"]
mod parsing;

#[path = "compose_prefix.rs"]
mod prefixes;
pub(crate) use prefixes::Context as ComposeHangingContext;

#[path = "compose_flow.rs"]
mod flow;

#[path = "compose_links.rs"]
pub(super) mod links;

#[path = "compose_selection.rs"]
mod selection;
pub(crate) use selection::Target as SelectionTarget;

#[path = "compose_semantics.rs"]
pub(super) mod semantics;

#[cfg(test)]
#[path = "compose_profile.rs"]
mod profile;

pub(super) fn document_keyboard(editor: &mut Editor) -> bool {
    use crate::input::keybindings::KeyContext;
    editor.menu_state().active_menu.is_none()
        && matches!(
            editor.get_key_context(),
            KeyContext::Normal | KeyContext::Mode(_) | KeyContext::CompositeBuffer
        )
}

#[derive(Clone, Default)]
pub(crate) struct InputReply {
    pub id: u64,
    pub accepted: bool,
    pub needs_layout: bool,
    pub need_region: Option<usize>,
    pub need_y: Option<f64>,
    pub carets: Vec<super::compose_navigation::CaretState>,
    pub compose: Option<ComposePane>,
    pub edit: Option<EditReceipt>,
    /// Correlated source/cursor group after a scoped action, not edited ownership.
    pub result: Option<EditReceipt>,
    /// Terminal outcome: this request will never be retried or applied.
    pub cancelled: bool,
    pub error: String,
}

/// Immutable state captured immediately after this client's native edit.
/// Other clients or later frames cannot replace the accepted cursor context.
#[derive(Clone, Debug)]
pub(crate) struct EditReceipt {
    pub window: u64,
    pub leaf: usize,
    pub buffer: usize,
    pub epoch: usize,
    pub pane: ComposePane,
    pub cursors: Vec<usize>,
    pub preserved_cursors: Vec<usize>,
}

pub(crate) struct Input<'a> {
    pub id: u64,
    pub scope: Option<Scope>,
    pub kind: u64,
    pub payload: &'a [u8],
    pub position: Option<usize>,
    pub extend: bool,
    pub count: u32,
    pub layout: Option<&'a [u8]>,
    pub layout_id: u64,
    pub affinity: super::compose_navigation::Affinity,
    pub region: Option<usize>,
    pub attachment_start: Option<usize>,
    pub source_revision: Option<u64>,
    pub buffer_epoch: usize,
    pub cancel: bool,
    pub replacement: Option<Range<usize>>,
    pub replacement_cursor: Option<usize>,
    pub text_input: bool,
    pub replacements: Vec<TextReplacement>,
    pub observed_cursors: Vec<CursorAnchor>,
    pub selection_targets: Vec<SelectionTarget>,
}

/// Event-time source ownership for carets not necessarily changed by this edit.
pub(crate) struct CursorAnchor {
    pub id: usize,
    pub position: usize,
    pub anchor: Option<usize>,
}

impl CursorAnchor {
    fn parse(bytes: &[u8]) -> Result<Self, super::wire::WireError> {
        use super::wire::{Field, Reader, WireError};
        let (mut id, mut position, mut anchor) = (None, None, None);
        for field in Reader::new(bytes) {
            let (tag, value) = field?;
            let slot = match tag {
                1 => &mut id,
                2 => &mut position,
                3 => &mut anchor,
                _ => continue,
            };
            if slot.is_some() {
                return Err(WireError::InvalidField("duplicate observed cursor anchor"));
            }
            let Field::Varint(value) = value else {
                return Err(WireError::InvalidField("observed cursor anchor wire type"));
            };
            *slot = Some(usize::try_from(value).map_err(|_| WireError::LongVarint)?);
        }
        match (id, position) {
            (Some(id), Some(position)) => Ok(Self {
                id,
                position,
                anchor,
            }),
            _ => Err(WireError::InvalidField("incomplete observed cursor anchor")),
        }
    }
}

pub(crate) struct TextReplacement {
    pub range: Range<usize>,
    pub cursor: usize,
    pub text: Option<String>,
}

impl TextReplacement {
    fn parse(bytes: &[u8]) -> Result<Self, super::wire::WireError> {
        use super::wire::{Reader, WireError};
        let (mut start, mut end, mut cursor, mut text) = (None, None, None, None);
        for field in Reader::new(bytes) {
            let (tag, value) = field?;
            let slot = match tag {
                1 => &mut start,
                2 => &mut end,
                3 => &mut cursor,
                4 => {
                    if text.is_some() {
                        return Err(WireError::InvalidField("duplicate replacement text"));
                    }
                    text = Some(value.text()?.to_owned());
                    continue;
                }
                _ => continue,
            };
            if slot.is_some() {
                return Err(WireError::InvalidField("duplicate replacement target"));
            }
            *slot = Some(usize::try_from(value.uint()).map_err(|_| WireError::LongVarint)?);
        }
        match (start, end, cursor) {
            (Some(start), Some(end), Some(cursor)) if start <= end => Ok(Self {
                range: start..end,
                cursor,
                text,
            }),
            _ => Err(WireError::InvalidField("replacement target")),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct InputTarget {
    pub id: u64,
    pub scope: Scope,
    pub epoch: usize,
    pub pointer: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PointerState {
    Ready,
    Pending,
}

impl InputTarget {
    pub fn valid(&self, editor: &Editor) -> bool {
        let window = editor.active_window();
        window.id.0 == self.scope.window
            && (self.pointer || window.effective_active_split() == self.scope.leaf)
            && window.pane_buffer(self.scope.leaf) == Some(self.scope.buffer)
            && window
                .buffer_state(self.scope.buffer)
                .is_some_and(|state| state.marker_list.instance_id() == self.epoch)
            && window.buffers.splits().is_some_and(|(_, views)| {
                views
                    .get(&self.scope.leaf)
                    .and_then(|view| view.buffer_state(self.scope.buffer))
                    .is_some_and(|view| view.view_mode == ViewMode::PageView)
            })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Scope {
    pub window: u64,
    pub leaf: LeafId,
    pub buffer: BufferId,
    pub revision: u64,
    pub projection: u64,
    pub selection: u64,
}

impl Scope {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, super::wire::WireError> {
        let mut scope = Self {
            window: 0,
            leaf: LeafId(fresh_core::SplitId(0)),
            buffer: BufferId(0),
            revision: 0,
            projection: 0,
            selection: 0,
        };
        for field in super::wire::Reader::new(bytes) {
            let (tag, value) = field?;
            match tag {
                1 => scope.window = value.uint(),
                2 => scope.leaf = LeafId(fresh_core::SplitId(value.uint() as usize)),
                3 => scope.buffer = BufferId(value.uint() as usize),
                4 => scope.revision = value.uint(),
                5 => scope.projection = value.uint(),
                6 => scope.selection = value.uint(),
                _ => {}
            }
        }
        Ok(scope)
    }

    pub fn matches(&self, editor: &Editor, pane: &ComposePane) -> bool {
        editor.active_window().id.0 == self.window
            && editor.active_window().pane_buffer(self.leaf) == Some(self.buffer)
            && pane.buffer_revision == self.revision
            && pane.projection_revision == self.projection
            && pane.selection_revision == self.selection
            && (pane.ready != Some(false) || pane.current_projection)
    }
}

impl<'a> Input<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, super::wire::WireError> {
        use super::wire::{Field, Reader};
        let mut input = Self {
            id: 0,
            scope: None,
            kind: 0,
            payload: &[],
            position: None,
            extend: false,
            count: 1,
            layout: None,
            layout_id: 0,
            affinity: super::compose_navigation::Affinity::Downstream,
            region: None,
            attachment_start: None,
            source_revision: None,
            buffer_epoch: 0,
            cancel: false,
            replacement: None,
            replacement_cursor: None,
            text_input: false,
            replacements: Vec::new(),
            observed_cursors: Vec::new(),
            selection_targets: Vec::new(),
        };
        let mut start = None;
        let mut end = None;
        for field in Reader::new(bytes) {
            match field? {
                (1, value) => input.id = value.uint(),
                (2, Field::Bytes(bytes)) => input.scope = Some(Scope::parse(bytes)?),
                (3, value) => input.kind = value.uint(),
                (4, Field::Bytes(bytes)) => input.payload = bytes,
                (5, value) => input.position = Some(value.uint() as usize),
                (6, value) => input.extend = value.flag(),
                (7, value) => input.count = value.uint() as u32,
                (8, Field::Bytes(bytes)) => input.layout = Some(bytes),
                (9, value) => input.layout_id = value.uint(),
                (10, value) if value.uint() == 1 => {
                    input.affinity = super::compose_navigation::Affinity::Upstream
                }
                (11, value) => input.region = Some(value.uint() as usize),
                (12, value) => input.attachment_start = Some(value.uint() as usize),
                (13, value) => input.source_revision = Some(value.uint()),
                (14, value) => {
                    input.buffer_epoch = usize::try_from(value.uint())
                        .map_err(|_| super::wire::WireError::LongVarint)?
                }
                (15, value) => input.cancel = value.flag(),
                (16, value) => {
                    start = Some(
                        usize::try_from(value.uint())
                            .map_err(|_| super::wire::WireError::LongVarint)?,
                    )
                }
                (17, value) => {
                    end = Some(
                        usize::try_from(value.uint())
                            .map_err(|_| super::wire::WireError::LongVarint)?,
                    )
                }
                (18, value) => input.text_input = value.flag(),
                (19, value) => {
                    input.replacement_cursor = Some(
                        usize::try_from(value.uint())
                            .map_err(|_| super::wire::WireError::LongVarint)?,
                    )
                }
                (20, Field::Bytes(bytes)) => {
                    input.replacements.push(TextReplacement::parse(bytes)?)
                }
                (21, Field::Bytes(bytes)) => {
                    input.observed_cursors.push(CursorAnchor::parse(bytes)?)
                }
                (22, Field::Bytes(bytes)) => {
                    if input.selection_targets.len() >= selection::MAX_TARGETS {
                        return Err(super::wire::WireError::InvalidField(
                            "too many source selections",
                        ));
                    }
                    input.selection_targets.push(SelectionTarget::parse(bytes)?);
                }
                (22, _) => {
                    return Err(super::wire::WireError::InvalidField(
                        "source selection target wire type",
                    ))
                }
                _ => {}
            }
        }
        if start.is_some() || end.is_some() {
            let (Some(start), Some(end)) = (start, end) else {
                return Err(super::wire::WireError::InvalidField("replacement range"));
            };
            if start > end
                || input.position.is_some()
                || input.kind != super::wire::kind::PASTE
                || input.scope.is_none()
                || input.source_revision.is_none()
                || input.buffer_epoch == 0
                || input.attachment_start.is_some()
                || input.extend
                || input.region.is_some()
                || input.layout.is_some()
                || input.layout_id != 0
            {
                return Err(super::wire::WireError::InvalidField("replacement input"));
            }
            if input.replacement_cursor.is_none() {
                return Err(super::wire::WireError::InvalidField("replacement cursor"));
            }
            input.replacement = Some(start..end);
        }
        if input.text_input
            && (input.kind != super::wire::kind::PASTE
                || input.scope.is_none()
                || input.buffer_epoch == 0
                || input.position.is_some()
                || input.attachment_start.is_some()
                || input.extend
                || input.layout.is_some()
                || input.layout_id != 0
                || input.region.is_some())
        {
            return Err(super::wire::WireError::InvalidField("text input"));
        }
        if input.replacement.is_none()
            && (input.replacement_cursor.is_some()
                || !input.replacements.is_empty()
                || input.text_input && input.source_revision.is_some())
        {
            return Err(super::wire::WireError::InvalidField("replacement metadata"));
        }
        if !input.observed_cursors.is_empty() {
            let unique: std::collections::HashSet<_> =
                input.observed_cursors.iter().map(|c| c.id).collect();
            if unique.len() != input.observed_cursors.len()
                || input.replacement.is_none() && !input.text_input
            {
                return Err(super::wire::WireError::InvalidField(
                    "observed cursors require unique native text targets",
                ));
            }
        }
        if !input.selection_targets.is_empty()
            && (input.scope.is_none()
                || input.source_revision.is_none()
                || input.buffer_epoch == 0
                || input.kind != 0
                || !input.payload.is_empty()
                || input.position.is_some()
                || input.attachment_start.is_some()
                || input.extend
                || input.count != 1
                || input.region.is_some()
                || input.layout.is_some()
                || input.layout_id != 0
                || input.replacement.is_some()
                || input.text_input
                || !input.observed_cursors.is_empty())
        {
            return Err(super::wire::WireError::InvalidField(
                "source selection input",
            ));
        }
        Ok(input)
    }

    /// Match event-time source ranges through retained coordinate history, then
    /// compare the exact live caret/selection before this transaction. Merely
    /// appearing in the later receipt does not establish cursor ownership.
    pub fn preserved_cursors(&self, editor: &Editor) -> Vec<usize> {
        let Some(scope) = self.scope else {
            return Vec::new();
        };
        if !(InputTarget {
            id: self.id,
            scope,
            epoch: self.buffer_epoch,
            pointer: false,
        })
        .valid(editor)
        {
            return Vec::new();
        }
        let window = editor.active_window();
        let Some(state) = window.buffer_state(scope.buffer) else {
            return Vec::new();
        };
        let Some(view) = window
            .buffers
            .splits()
            .and_then(|(_, views)| views.get(&scope.leaf))
            .and_then(|v| v.buffer_state(scope.buffer))
        else {
            return Vec::new();
        };
        let revision = self.source_revision.unwrap_or(scope.revision);
        if revision > state.buffer.version() {
            return Vec::new();
        }
        self.observed_cursors
            .iter()
            .filter_map(|observed| {
                let anchor = observed.anchor.unwrap_or(observed.position);
                if observed.position > isize::MAX as usize || anchor > isize::MAX as usize {
                    return None;
                }
                let range = state.coord_map.map_unchanged_range(
                    observed.position.min(anchor)..observed.position.max(anchor),
                    revision,
                )?;
                if range.end > state.buffer.len() {
                    return None;
                }
                let position = if observed.position < anchor {
                    range.start
                } else {
                    range.end
                };
                let anchor = observed.anchor.map(|_| {
                    if observed.position < anchor {
                        range.end
                    } else {
                        range.start
                    }
                });
                let cursor = view
                    .cursors
                    .get(crate::model::event::CursorId(observed.id))?;
                (cursor.position == position && cursor.anchor == anchor).then_some(observed.id)
            })
            .collect()
    }

    pub fn replace(&self, editor: &mut Editor) -> Result<(), String> {
        let scope = self
            .scope
            .ok_or("The text replacement has no editor view.")?;
        if !(InputTarget {
            id: self.id,
            scope,
            epoch: self.buffer_epoch,
            pointer: false,
        })
        .valid(editor)
        {
            return Err(
                "The text input view was closed or replaced; text was not inserted.".into(),
            );
        }
        if self.replacement.is_none() {
            if !document_keyboard(editor) {
                return Err(
                    "Text was not inserted because an editor dialog has the keyboard.".into(),
                );
            }
            editor.focus_split(scope.leaf, scope.buffer);
            if editor.active_window().is_editing_disabled() {
                return Err("The document is read-only; text was not inserted.".into());
            }
            editor.insert_native_text(self.text()?);
            return Ok(());
        }
        let range = self.replacement.clone().unwrap();
        let revision = self
            .source_revision
            .ok_or("The text replacement has no source revision.")?;
        if !document_keyboard(editor) {
            return Err("Text was not inserted because an editor dialog has the keyboard. Retry your text input.".into());
        }
        let state = editor
            .active_window()
            .buffer_state(scope.buffer)
            .ok_or("The replaced file is no longer available.")?;
        if revision > state.buffer.version() {
            return Err("The replacement refers to an unavailable file revision.".into());
        }
        let valid = |p| {
            p <= state.buffer.len()
                && (p == state.buffer.len()
                    || state
                        .buffer
                        .slice_bytes(p..p + 1)
                        .first()
                        .is_some_and(|b| b & 0xc0 != 0x80))
        };
        let text = self.text()?;
        let mut replacements = Vec::with_capacity(self.replacements.len() + 1);
        for (range, cursor, text) in std::iter::once((
            range,
            self.replacement_cursor
                .ok_or("The text replacement has no cursor.")?,
            text.clone(),
        ))
        .chain(self.replacements.iter().map(|target| {
            (
                target.range.clone(),
                target.cursor,
                target.text.clone().unwrap_or_else(|| text.clone()),
            )
        })) {
            let range = state.coord_map.map_unchanged_range(range, revision).ok_or(
                "Text was not inserted because an original range changed. Retry your text input.",
            )?;
            if !valid(range.start) || !valid(range.end) {
                return Err("The replacement is not at a UTF-8 source boundary.".into());
            }
            replacements.push((range, text, crate::model::event::CursorId(cursor)));
        }
        editor.focus_split(scope.leaf, scope.buffer);
        editor.replace_native_texts(replacements)
    }

    fn text(&self) -> Result<String, String> {
        let mut text = None;
        for field in super::wire::Reader::new(self.payload) {
            let (tag, value) = field.map_err(|e| e.to_string())?;
            if tag == 1 {
                if text.is_some() {
                    return Err("Text replacement has duplicate text.".into());
                }
                text = Some(value.text().map_err(|e| e.to_string())?.to_owned());
            }
        }
        Ok(text.unwrap_or_default())
    }

    /// A graphical click addresses the source seen when it happened. Scrolling
    /// or another edit may have replaced the visible projection by the time it
    /// is dispatched; never hit-test that old screen point a second time.
    pub fn rebase_pointer(
        &mut self,
        editor: &mut Editor,
        cache: &mut ComposeCache,
    ) -> Result<PointerState, String> {
        let scope = self
            .scope
            .ok_or("The clicked file is no longer available.")?;
        let revision = self
            .source_revision
            .ok_or("The click has no source revision.")?;
        if self.buffer_epoch == 0
            || !(InputTarget {
                id: self.id,
                scope,
                epoch: self.buffer_epoch,
                pointer: true,
            })
            .valid(editor)
        {
            return Err("The clicked file was closed or replaced. Click again.".into());
        }
        let state = editor.active_window().buffer_state(scope.buffer).unwrap();
        let position = self.position.ok_or("The click has no source position.")?;
        if revision > state.buffer.version() {
            return Err("The click refers to an unavailable file revision. Click again.".into());
        }
        let mapped = state
            .coord_map
            .map_with_gravity(
                position,
                revision,
                self.affinity == super::compose_navigation::Affinity::Upstream,
            )
            .ok_or("The file changed before this click could be applied. Click again.")?;
        let valid = |position: usize| {
            position <= state.buffer.len()
                && (position == state.buffer.len()
                    || state
                        .buffer
                        .slice_bytes(position..position + 1)
                        .first()
                        .is_none_or(|byte| byte & 0xc0 != 0x80))
        };
        if !valid(mapped) {
            return Err("The clicked source position is no longer available. Click again.".into());
        }
        if let Some(start) = self.attachment_start {
            if self.extend {
                return Err("The image selection is no longer available. Click again.".into());
            }
            let start = state
                .coord_map
                .map(start, revision)
                .ok_or("The image changed before this click could be applied. Click again.")?;
            let end = state
                .coord_map
                .map_with_gravity(position, revision, true)
                .ok_or("The image changed before this click could be applied. Click again.")?;
            if start >= end || !valid(start) || !valid(end) {
                return Err(
                    "The image changed before this click could be applied. Click again.".into(),
                );
            }
            let pane = cache
                .gather_at(editor, scope.leaf, scope.buffer, Some(start))
                .ok_or("The image is no longer available. Click again.")?;
            if cache.parsing_pending(scope.window, scope.buffer.0) {
                return Ok(PointerState::Pending);
            }
            if !pane
                .images
                .iter()
                .any(|image| image.start == start && image.end == end)
            {
                return Err(
                    "The image changed before this click could be applied. Click again.".into(),
                );
            }
            self.attachment_start = Some(start);
            self.position = Some(end);
        } else {
            self.position = Some(mapped);
        }
        Ok(PointerState::Ready)
    }

    pub fn pointer(&self, editor: &mut Editor) -> bool {
        if !editor.native_compose_pointer_allowed() {
            return false;
        }
        let (Some(scope), Some(mut position)) = (&self.scope, self.position) else {
            return false;
        };
        if let Some(start) = self.attachment_start {
            if self.count >= 2 {
                position = start + 2;
            }
        }
        let Some(state) = editor.active_window().buffer_state(scope.buffer) else {
            return false;
        };
        if position > state.buffer.len() {
            return false;
        }
        if position < state.buffer.len()
            && state
                .buffer
                .slice_bytes(position..position + 1)
                .first()
                .is_some_and(|byte| byte & 0xc0 == 0x80)
        {
            return false;
        }
        editor.dismiss_transient_popups();
        editor.focus_split(scope.leaf, scope.buffer);
        if let Some(start) = self.attachment_start.filter(|_| self.count < 2) {
            editor
                .active_window_mut()
                .set_buffer_cursor_in_splits_selecting(scope.buffer, start, &[scope.leaf], false);
        }
        editor
            .active_window_mut()
            .set_buffer_cursor_in_splits_selecting(
                scope.buffer,
                position,
                &[scope.leaf],
                self.extend || self.attachment_start.is_some() && self.count < 2,
            );
        editor.mark_plugin_snapshot_dirty();
        editor.shell_description_stale = true;
        use crate::input::keybindings::Action;
        match if self.attachment_start.is_some() {
            1
        } else {
            self.count
        } {
            2 => {
                let _ = editor.handle_action(Action::SelectWord);
            }
            3.. => {
                let _ = editor.handle_action(Action::SelectLine);
            }
            _ => {}
        }
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ComposeSegment {
    pub source_start: usize,
    pub source_end: usize,
    pub display_start: usize,
    pub display_end: usize,
    pub kind: u32,
}

#[derive(Clone, Debug, Hash)]
pub(crate) struct ComposeSpan {
    pub start: usize,
    pub end: usize,
    /// 0 body; 1..6 heading; 7 inline code; 8 table; 9 quote;
    /// 10 list; 11 paragraph; 12 code block; 13 list item; 14 thematic rule.
    pub role: u32,
    /// Bold 1, italic 2, underline 4, strikethrough 8, link 16.
    pub flags: u32,
    /// First content on an item's opening line, after its list/task marker.
    /// Other spans and empty items omit this source-byte anchor.
    pub content_start: Option<usize>,
    /// Parsed destination for a link span, including an explicitly empty one.
    /// It is metadata, never permission to fetch or open the target.
    pub link_target: Option<Arc<str>>,
    /// Full parsed size when only a bounded tooltip preview is sent.
    pub link_target_bytes: Option<usize>,
    /// An explicitly incomplete destination, never an actionable URI.
    pub link_target_preview: Option<Arc<str>>,
}

#[derive(Clone, Debug, Hash)]
pub(crate) struct ComposeImage {
    pub start: usize,
    pub end: usize,
    pub target: String,
    pub alt: String,
    pub title: String,
    pub block: bool,
    pub rendered: bool,
    /// The exact markup is editable text, with a separate flow preview.
    pub source_preview: bool,
}

#[derive(Clone, Debug, Hash)]
pub(crate) struct ComposeSelection {
    pub position: usize,
    pub anchor: Option<usize>,
    pub id: usize,
    pub primary: bool,
    pub utf16_position: Option<usize>,
    pub utf16_anchor: Option<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct ComposePane {
    pub buffer_revision: u64,
    pub projection_revision: u64,
    pub selection_revision: u64,
    pub source_start: usize,
    pub source_end: usize,
    pub text: String,
    pub segments: Vec<ComposeSegment>,
    pub spans: Vec<ComposeSpan>,
    pub images: Vec<ComposeImage>,
    pub cursors: Vec<ComposeSelection>,
    pub focused: bool,
    pub keyboard: bool,
    pub top_byte: usize,
    pub document_len: usize,
    pub asset_authority: u64,
    pub asset_document: u64,
    pub viewport: Option<super::compose_viewport::Value>,
    pub ready: Option<bool>,
    pub error: String,
    pub text_context: Option<super::compose_text::Context>,
    pub text_chunks: Vec<super::compose_text::Chunk>,
    pub tables: Vec<super::compose_tables::Table>,
    /// Native insertion separator: 0 unknown, 1 LF (including CR disk files), 2 CRLF.
    pub native_newline: u32,
    pub current_projection: bool,
    pub hanging_contexts: Vec<ComposeHangingContext>,
    /// Complete source regions required by this pane's formatter, including
    /// independently prepared offscreen marker lines. Not a wire payload.
    pub format_regions: Vec<Range<usize>>,
    pub editable: Option<bool>,
    pub decoration_revision: Option<u64>,
}

struct Document {
    semantics: Arc<semantics::Inventory>,
    revision: u64,
    epoch: usize,
    text: Arc<String>,
    text_index: Arc<super::compose_text::Index>,
    spans: Vec<ComposeSpan>,
    images: Vec<ComposeImage>,
    paragraphs: Vec<Range<usize>>,
    tight_paragraphs: Vec<Range<usize>>,
    item_paragraphs: HashMap<usize, usize>,
    prefix_cache: prefixes::Cache,
    underlines: Vec<(Range<usize>, Range<usize>)>,
    heading_openers: Vec<Range<usize>>,
    code: Vec<CodeBlock>,
    tables: super::compose_tables::Tables,
    used: u64,
    decorations: super::compose_decorations::Snapshot,
    parsed: bool,
    parse_error: Option<String>,
    previous_decorations: Option<super::compose_decorations::Snapshot>,
}

struct CodeBlock {
    source: Range<usize>,
    content: Vec<Range<usize>>,
    fenced: bool,
}

impl Document {
    fn parse(text: String, revision: u64, used: u64, epoch: usize) -> Self {
        let text = Arc::new(text);
        let index = Arc::new(super::compose_text::Index::new(&text));
        Self::parse_shared(text, index, revision, used, epoch, None).unwrap()
    }

    /// Current source/caret context remains available for immutable edit
    /// receipts while the semantic parser works on its own shared snapshot.
    fn pending(text: String, revision: u64, used: u64, epoch: usize) -> Self {
        let index = Arc::new(super::compose_text::Index::new(&text));
        Self {
            revision,
            epoch,
            text: Arc::new(text),
            semantics: Default::default(),
            text_index: index,
            spans: Vec::new(),
            images: Vec::new(),
            paragraphs: Vec::new(),
            tight_paragraphs: Vec::new(),
            item_paragraphs: HashMap::new(),
            prefix_cache: Default::default(),
            underlines: Vec::new(),
            heading_openers: Vec::new(),
            code: Vec::new(),
            tables: Default::default(),
            used,
            decorations: super::compose_decorations::Snapshot::new(revision, Vec::new()),
            parsed: false,
            parse_error: None,
            previous_decorations: None,
        }
    }

    fn parse_shared(
        text: Arc<String>,
        text_index: Arc<super::compose_text::Index>,
        revision: u64,
        used: u64,
        epoch: usize,
        cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<Self> {
        let cancelled =
            || cancel.is_some_and(|token| token.load(std::sync::atomic::Ordering::Acquire));
        if cancelled() {
            return None;
        }
        let mut spans = Vec::new();
        let mut semantics = semantics::Collector::default();
        // Repeated reference links share one parsed destination allocation.
        let mut link_targets: HashSet<Arc<str>> = HashSet::new();
        let mut images = Vec::new();
        let mut image_stack: Vec<ComposeImage> = Vec::new();
        let mut paragraphs = Vec::new();
        let mut tight_flows = flow::TightFlows::default();
        let mut item_paragraphs = HashMap::new();
        let mut underlines = Vec::new();
        let mut heading_openers = Vec::new();
        let mut code: Vec<CodeBlock> = Vec::new();
        let mut active_code = None;
        let mut tables = super::compose_tables::Tables::default();
        let mut items: Vec<(usize, usize)> = Vec::new();
        for (index, (event, range)) in Parser::new_ext(&text, Options::all())
            .into_offset_iter()
            .enumerate()
        {
            if index % 128 == 0 && cancelled() {
                return None;
            }
            tables.event(&text, &event, &range);
            semantics.event(&event, &range);
            if let Some(image) = image_stack.last_mut() {
                if let Some(value) = semantics::plain_text(&event) {
                    image.alt.push_str(&value);
                }
            }
            tight_flows.event(&text, &event, &range);
            // Use the parser's item ownership and inline ranges. Tight lists
            // have no Paragraph event; task markers precede their body. An
            // empty item whose first child is on another line has no prefix
            // anchor, rather than a multiline "marker" guessed from text.
            if !matches!(
                &event,
                Event::End(_)
                    | Event::TaskListMarker(_)
                    | Event::SoftBreak
                    | Event::HardBreak
                    | Event::Start(
                        Tag::Item
                            | Tag::List(_)
                            | Tag::Paragraph
                            | Tag::BlockQuote(_)
                            | Tag::Heading { .. }
                            | Tag::CodeBlock(_)
                    )
            ) {
                if let Some(&(index, line_end)) = items.last() {
                    let span: &mut ComposeSpan = &mut spans[index];
                    if span.content_start.is_none() && range.start < line_end {
                        span.content_start = Some(range.start);
                    }
                }
            }
            let new_item = matches!(&event, Event::Start(Tag::Item));
            if matches!(&event, Event::End(TagEnd::Item)) {
                items.pop();
            }
            let mut role = 0;
            let mut flags = 0;
            let mut link_target = None;
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    role = level as u32;
                    // The parser establishes heading ownership. Strip only
                    // its ATX opener; a literal hash in prose/code is untouched.
                    let raw = &text[range.clone()];
                    let lead = raw
                        .bytes()
                        .take_while(|b| matches!(b, b' ' | b'\t'))
                        .count();
                    let hashes = raw[lead..].bytes().take_while(|b| *b == b'#').count();
                    if (1..=6).contains(&hashes) {
                        let after = lead + hashes;
                        let space = raw[after..]
                            .bytes()
                            .take_while(|b| matches!(b, b' ' | b'\t'))
                            .count();
                        // A setext heading may begin with a literal #word.
                        // Only ATX syntax allows the marker followed by space/EOL.
                        if space > 0
                            || raw
                                .as_bytes()
                                .get(after)
                                .is_none_or(|b| matches!(b, b'\r' | b'\n'))
                        {
                            heading_openers.push(range.start..range.start + after + space);
                        }
                    }
                    if let Some(underline) = heading_underline(&text, &range) {
                        let line_start = text[..range.start].rfind('\n').map_or(0, |n| n + 1);
                        underlines.push((line_start..range.end, underline));
                    }
                }
                Event::Rule => role = 14,
                Event::Start(Tag::CodeBlock(kind)) => {
                    role = 12;
                    active_code = Some(code.len());
                    code.push(CodeBlock {
                        source: range.clone(),
                        content: Vec::new(),
                        fenced: matches!(kind, pulldown_cmark::CodeBlockKind::Fenced(_)),
                    });
                }
                Event::End(TagEnd::CodeBlock) => active_code = None,
                Event::Code(_) => {
                    role = 7;
                }
                Event::Start(Tag::Table(_)) => role = 8,
                Event::Start(Tag::BlockQuote(_)) => role = 9,
                Event::Start(Tag::List(_)) => role = 10,
                Event::Start(Tag::Item) => role = 13,
                Event::Start(Tag::Link {
                    link_type,
                    dest_url,
                    ..
                }) => {
                    flags = 4 | 16;
                    let target = if link_type == pulldown_cmark::LinkType::Email {
                        std::borrow::Cow::Owned(format!("mailto:{dest_url}"))
                    } else {
                        std::borrow::Cow::Borrowed(dest_url.as_ref())
                    };
                    link_target = Some(if let Some(shared) = link_targets.get(target.as_ref()) {
                        Arc::clone(shared)
                    } else {
                        let shared: Arc<str> = target.as_ref().into();
                        link_targets.insert(Arc::clone(&shared));
                        shared
                    });
                }
                Event::Start(Tag::Strong) => flags = 1,
                Event::Start(Tag::Emphasis) => flags = 2,
                Event::Start(Tag::Strikethrough) => flags = 8,
                Event::Start(Tag::Paragraph) => {
                    role = 11;
                    paragraphs.push(range.clone());
                    if let Some(&(index, _)) = items.last() {
                        *item_paragraphs.entry(spans[index].start).or_insert(0usize) += 1;
                    }
                }
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let before = text[..range.start].rsplit('\n').next().unwrap_or("");
                    let after = text[range.end..].split('\n').next().unwrap_or("");
                    image_stack.push(ComposeImage {
                        start: range.start,
                        end: range.end,
                        target: dest_url.into_string(),
                        title: title.into_string(),
                        alt: String::new(),
                        block: before.trim().is_empty() && after.trim().is_empty(),
                        rendered: false,
                        source_preview: false,
                    });
                }
                Event::Text(_) => {
                    if let Some(index) = active_code {
                        code[index].content.push(range.clone());
                    }
                }
                Event::End(TagEnd::Image) => {
                    if let Some(image) = image_stack.pop() {
                        if let Some(parent) = image_stack.last_mut() {
                            parent.alt.push_str(&image.alt);
                        } else {
                            images.push(image);
                        }
                    }
                }
                _ => {}
            }
            if role != 0 || flags != 0 {
                spans.push(ComposeSpan {
                    start: range.start,
                    end: if role == 14 {
                        range.start + text[range.clone()].trim_end_matches(['\r', '\n']).len()
                    } else {
                        range.end
                    },
                    role,
                    flags,
                    content_start: None,
                    link_target,
                    link_target_bytes: None,
                    link_target_preview: None,
                });
                if new_item {
                    let line_end = text[range.start..]
                        .find('\n')
                        .map_or(text.len(), |n| range.start + n);
                    items.push((spans.len() - 1, line_end));
                }
            }
        }
        if cancelled() {
            return None;
        }
        let tight_paragraphs = tight_flows
            .finish(&text)
            .into_iter()
            .map(|(owner, range)| {
                *item_paragraphs.entry(owner).or_insert(0usize) += 1;
                range
            })
            .collect();
        tables.finish(&text, &spans, &images);
        if cancelled() {
            return None;
        }
        let outline = spans
            .iter()
            .map(|span| super::compose_decorations::Semantic {
                source: span.start..span.end,
                role: span.role,
                flags: span.flags,
                content: span.content_start,
            })
            .chain(
                images
                    .iter()
                    .map(|image| super::compose_decorations::Semantic {
                        source: image.start..image.end,
                        role: 15,
                        flags: 0,
                        content: None,
                    }),
            )
            .collect();
        Some(Self {
            revision,
            epoch,
            semantics: Arc::new(semantics.finish()),
            text_index,
            text,
            spans,
            images,
            paragraphs,
            tight_paragraphs,
            item_paragraphs,
            prefix_cache: Default::default(),
            underlines,
            heading_openers,
            code,
            tables,
            used,
            decorations: super::compose_decorations::Snapshot::new(revision, outline),
            parsed: true,
            parse_error: None,
            previous_decorations: None,
        })
    }

    /// Native-only overrides are resolved from parsed block ownership and
    /// active grid conceals. Column-formatted tables retain editable source
    /// until native cell layout is available. Quote joins remain whole:
    /// removing a marker inside a join would accidentally unflow the prose.
    #[cfg(test)]
    fn native_projection(
        &self,
        state: &crate::state::EditorState,
        start: usize,
        end: usize,
        cursors: &[usize],
        objects: &[Range<usize>],
        revealed: &[Range<usize>],
    ) -> Option<Projection> {
        self.native_table_projection(state, start, end, cursors, objects, revealed, &[])
    }

    #[cfg(test)]
    fn source_window(&self, top_byte: usize, region: Option<usize>) -> Range<usize> {
        self.source_window_with_context(top_byte, region, None)
    }

    #[cfg(test)]
    fn source_window_with_context(
        &self,
        top_byte: usize,
        region: Option<usize>,
        keep: Option<Range<usize>>,
    ) -> Range<usize> {
        self.source_window_policy(top_byte, region, keep, false)
    }

    fn source_window_policy(
        &self,
        top_byte: usize,
        region: Option<usize>,
        keep: Option<Range<usize>>,
        paragraph_windows: bool,
    ) -> Range<usize> {
        let mut anchor = region.unwrap_or(top_byte).min(self.text.len());
        while !self.text.is_char_boundary(anchor) {
            anchor -= 1;
        }
        // Older clients keep the pre-capability policy. For capable clients,
        // direct inline flows are complete paragraphs even when the parser
        // omits Paragraph tags in a tight list.
        let paragraphs = || {
            self.paragraphs
                .iter()
                .chain(self.tight_paragraphs.iter().filter(|_| paragraph_windows))
        };
        let context_start = |mut at: usize| {
            while !self.text.is_char_boundary(at) {
                at -= 1;
            }
            // Preserve paragraph joins, but do not let an unrelated preceding
            // paragraph pull the requested byte beyond the window's end.
            let physical_start = |byte| self.text[..byte].rfind('\n').map_or(0, |n| n + 1);
            let parsed_paragraph = paragraphs().find(|r| {
                r.contains(&at) || paragraph_windows && physical_start(r.start) <= at && at < r.end
            });
            let paragraph = parsed_paragraph.map_or_else(
                || self.text[..at].rfind('\n').map_or(0, |n| n + 1),
                |r| r.start,
            );
            // Parsed explicit paragraphs and tight inline flows provide their
            // complete shaping context. A capable client receives offscreen
            // ancestor markers separately; no soft-wrap slice is inferred.
            if paragraph_windows && parsed_paragraph.is_some() {
                // Include source-owned leading whitespace so Home/Left can
                // still address it and native hanging layout charges it once.
                return physical_start(paragraph);
            }
            if paragraph_windows {
                let line_start = physical_start(at);
                let line_end = self.text[at..]
                    .find('\n')
                    .map_or(self.text.len(), |n| at + n + 1);
                let owner = self
                    .spans
                    .iter()
                    .filter(|span| span.role == 13 && span.start <= at && at < span.end)
                    .min_by_key(|span| span.end - span.start);
                let independent_paragraphs = owner.is_none_or(|item| {
                    self.item_paragraphs.get(&item.start).copied().unwrap_or(0) > 1
                });
                if independent_paragraphs
                    && self.text[line_start..line_end]
                        .bytes()
                        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                {
                    return line_start;
                }
            }
            // Structural bytes outside a known paragraph retain their item
            // context so hanging measurement remains repeatable on a page.
            if self
                .spans
                .iter()
                .any(|s| matches!(s.role, 8 | 12) && s.start <= at && at < s.end)
            {
                paragraph
            } else {
                self.spans
                    .iter()
                    .filter(|s| s.role == 13 && s.start <= at && at < s.end)
                    .map(|s| s.start)
                    .min()
                    .unwrap_or(paragraph)
                    .min(paragraph)
            }
        };
        let mut start = context_start(if region.is_some() {
            anchor.saturating_sub(WINDOW_BYTES / 3)
        } else {
            anchor
        });
        if anchor.saturating_sub(start) >= WINDOW_BYTES {
            start = context_start(anchor);
        }
        if let Some(keep) = &keep {
            start = start.min(context_start(keep.start.min(self.text.len())));
        }
        let mut end = (start + WINDOW_BYTES)
            .max(anchor.saturating_add(if region.is_some() {
                WINDOW_BYTES * 2 / 3
            } else {
                1
            }))
            .max(keep.map_or(0, |range| range.end))
            .min(self.text.len());
        while !self.text.is_char_boundary(end) {
            end += 1;
        }
        if end < self.text.len() {
            end += self.text[end..].find('\n').map_or(0, |n| n + 1);
        }
        if paragraph_windows {
            if let Some(paragraph) = paragraphs().find(|r| r.start < end && end < r.end) {
                end = paragraph.end;
            }
        }
        start..end
    }

    #[cfg(test)]
    fn native_table_projection(
        &self,
        state: &crate::state::EditorState,
        start: usize,
        end: usize,
        cursors: &[usize],
        objects: &[Range<usize>],
        revealed: &[Range<usize>],
        tables: &[super::compose_tables::Table],
    ) -> Option<Projection> {
        let active = state
            .conceals
            .query_viewport(start, end, &state.marker_list, cursors);
        self.project_native(start, end, cursors, objects, revealed, tables, active)
    }

    fn project_native(
        &self,
        start: usize,
        end: usize,
        cursors: &[usize],
        objects: &[Range<usize>],
        revealed: &[Range<usize>],
        tables: &[super::compose_tables::Table],
        active: Vec<(Range<usize>, Option<&str>)>,
    ) -> Option<Projection> {
        let conceals = self.native_conceals(start, end, cursors, objects, revealed, tables, active);
        Projection::new(self.text.get(start..end)?, start, &conceals).ok()
    }

    /// Borrow the exact presentation inputs without allocating projected text.
    /// Source and semantic spans belong to this immutable parsed Document.
    fn native_conceals<'a>(
        &'a self,
        start: usize,
        end: usize,
        cursors: &[usize],
        objects: &[Range<usize>],
        revealed: &[Range<usize>],
        tables: &[super::compose_tables::Table],
        active: Vec<(Range<usize>, Option<&'a str>)>,
    ) -> Vec<Conceal<'a>> {
        let mut overrides: Vec<_> = objects
            .iter()
            .cloned()
            .map(|source| Conceal {
                source,
                replacement: Some("\u{fffc}"),
            })
            .collect();
        let mut revealed = revealed.to_vec();
        revealed.extend(
            tables
                .iter()
                .flat_map(|t| t.rows.iter())
                .filter(|r| r.source)
                .map(|r| r.start..r.end),
        );
        overrides.extend(self.tables.conceals(&self.text, start..end, tables));
        // Grid table replacements can pad, truncate or wrap whole cells to a
        // terminal measure; code fences become character-sized frame strings.
        // None is content for a pixel-measured client. Retain the exact source
        // of these parsed blocks instead of adopting that terminal layout.
        // Explicit native image overrides still apply within a source table.
        revealed.extend(
            self.spans
                .iter()
                .filter(|s| [8, 12].contains(&s.role))
                .map(|s| s.start..s.end),
        );
        for block in self
            .code
            .iter()
            .filter(|b| b.source.start < end && b.source.end >= start)
        {
            let line_start = self.text[..block.source.start]
                .rfind(['\r', '\n'])
                .map_or(0, |p| p + 1);
            let prefix = line_start..block.source.start;
            // The parser excludes a first line's container prefix from the
            // code block range. Never consume a list marker on that line.
            let strip_prefix = !prefix.is_empty()
                && self.text[prefix.clone()]
                    .bytes()
                    .all(|c| matches!(c, b' ' | b'\t' | b'>'));
            if strip_prefix {
                revealed.push(prefix.clone());
            }
            let first_part = block.content.partition_point(|part| part.end <= start);
            let mut from = if first_part == 0 {
                block.source.start
            } else {
                block.content[first_part - 1].end
            };
            let mut gaps = Vec::new();
            if strip_prefix {
                gaps.push(prefix);
            }
            // A hidden opener on its own line does not need a blank header
            // row. Keep its EOL for an empty block or a preceding list marker.
            if block.fenced
                && !block.content.is_empty()
                && (strip_prefix || line_start == block.source.start)
                && !source_line_has_cursor(&self.text, block.source.start, cursors)
            {
                let opener_end = source_line_end(&self.text, block.source.start);
                if opener_end <= block.content[0].start
                    && block.source.start >= start
                    && opener_end <= end
                {
                    overrides.push(Conceal {
                        source: from..opener_end,
                        replacement: None,
                    });
                    from = opener_end;
                }
            }
            for part in block.content[first_part..]
                .iter()
                .take_while(|part| part.start < end)
            {
                if from < part.start {
                    gaps.push(from..part.start);
                }
                from = part.end;
            }
            if from < block.source.end {
                gaps.push(from..block.source.end);
            }
            // The parser excludes a closing fence's final EOL. Conceal that
            // syntax-owned line ending as well, leaving code EOL bytes copied.
            if block.fenced
                && self
                    .text
                    .as_bytes()
                    .get(block.source.end)
                    .is_some_and(|b| matches!(b, b'\r' | b'\n'))
                && !source_line_has_cursor(&self.text, block.source.end - 1, cursors)
            {
                let to = source_line_end(&self.text, block.source.end);
                if block.source.end >= start && to <= end {
                    overrides.push(Conceal {
                        source: block.source.end..to,
                        replacement: None,
                    });
                }
            }
            for gap in gaps {
                let gap = gap.start.max(start)..gap.end.min(end);
                if gap.is_empty() {
                    continue;
                }
                // Copy the original CR/LF bytes. These source-owned line
                // boundaries are not terminal wrapping or generated padding.
                let mut at = gap.start;
                while at < gap.end {
                    let to = self.text[at..gap.end]
                        .find(['\r', '\n'])
                        .map_or(gap.end, |p| at + p);
                    if at < to
                        && at >= start
                        && to <= end
                        && !source_line_has_cursor(&self.text, at, cursors)
                    {
                        overrides.push(Conceal {
                            source: at..to,
                            replacement: None,
                        });
                    }
                    at = to + 1;
                }
            }
            // An empty, unterminated fence at EOF still needs one measurable
            // native line. Its placeholder never enters the source buffer.
            if block.content.is_empty()
                && !self.text[block.source.clone()].contains(['\r', '\n'])
                && block.source.start >= start
                && block.source.end <= end
                && !source_line_has_cursor(&self.text, block.source.start, cursors)
            {
                overrides.retain(|c| {
                    !(c.source.start < block.source.end && block.source.start < c.source.end)
                });
                overrides.push(Conceal {
                    source: block.source.clone(),
                    replacement: Some("\n"),
                });
            }
        }
        for opener in &self.heading_openers {
            if opener.start >= start
                && opener.end <= end
                && !source_line_has_cursor(&self.text, opener.start, cursors)
            {
                overrides.push(Conceal {
                    source: opener.clone(),
                    replacement: None,
                });
            }
        }
        for (heading, underline) in &self.underlines {
            if underline.start < start || underline.end > end {
                continue;
            }
            if cursors.iter().any(|p| {
                heading.contains(p) || (*p == heading.end && heading.end == self.text.len())
            }) {
                revealed.push(underline.clone());
            } else {
                overrides.push(Conceal {
                    source: underline.clone(),
                    replacement: None,
                });
            }
        }
        for rule in self
            .spans
            .iter()
            .filter(|s| s.role == 14 && s.start >= start && s.end <= end)
        {
            let line_start = self.text[..rule.start].rfind('\n').map_or(0, |n| n + 1);
            let line_end = self.text[rule.end..]
                .find('\n')
                .map_or(self.text.len(), |n| rule.end + n + 1);
            let source = rule.start..rule.end;
            // A grid flow join can swallow a quoted rule's line break. The
            // parsed block must still have its own native line, including
            // while its markup is revealed. Preserve the original EOL bytes.
            for (join, replacement) in &active {
                if overrides
                    .iter()
                    .any(|c| c.source.start < join.end && join.start < c.source.end)
                {
                    continue;
                }
                if *replacement == Some(" ") && join.end == rule.start && join.start < line_start {
                    let raw = &self.text[join.clone()];
                    if let Some(n) = raw.find('\n') {
                        let from = if n > 0 && raw.as_bytes()[n - 1] == b'\r' {
                            n - 1
                        } else {
                            n
                        };
                        overrides.push(Conceal {
                            source: join.clone(),
                            replacement: Some(&raw[from..n + 1]),
                        });
                    }
                }
            }
            if cursors.iter().any(|p| {
                (line_start..line_end).contains(p)
                    || (*p == line_end && line_end == self.text.len())
            }) {
                revealed.push(source);
            } else {
                overrides.push(Conceal {
                    source,
                    replacement: None,
                });
            }
        }
        for (range, replacement) in &active {
            // The grid expands a nested item's leading whitespace according
            // to its column measure. Native indentation already comes from
            // parsed container depth, so keep only the original whitespace.
            if replacement.is_some()
                && self.text.get(range.clone()).is_some_and(|raw| {
                    !raw.is_empty() && raw.bytes().all(|c| c == b' ' || c == b'\t')
                })
                && self.text[..range.start].rfind('\n').map_or(0, |p| p + 1) == range.start
                && self
                    .spans
                    .iter()
                    .any(|s| s.role == 13 && s.start == range.end && s.content_start.is_some())
            {
                revealed.push(range.clone());
                continue;
            }
            if *replacement != Some("▌")
                || self.text.get(range.clone()) != Some(">")
                || !self
                    .spans
                    .iter()
                    .any(|s| s.role == 9 && s.start <= range.start && range.end <= s.end)
            {
                continue;
            }
            let code = self
                .spans
                .iter()
                .any(|s| [7, 12].contains(&s.role) && s.start <= range.start && range.end <= s.end);
            let to =
                if !code && self.text.as_bytes().get(range.end) == Some(&b' ') && range.end < end {
                    range.end + 1
                } else {
                    range.end
                };
            let source = range.start..to;
            if overrides
                .iter()
                .any(|c| c.source.start < source.end && source.start < c.source.end)
                || revealed
                    .iter()
                    .any(|r| r.start < source.end && source.start < r.end)
            {
                continue;
            }
            if code {
                revealed.push(source);
            } else {
                overrides.push(Conceal {
                    source,
                    replacement: None,
                });
            }
        }
        crate::view::conceal::ConcealManager::resolve_ranges(&active, &overrides, &revealed)
    }
}

fn source_line_has_cursor(text: &str, position: usize, cursors: &[usize]) -> bool {
    let start = text[..position].rfind(['\r', '\n']).map_or(0, |p| p + 1);
    let end = source_line_end(text, position);
    let ends_without_newline = text
        .as_bytes()
        .last()
        .is_some_and(|b| !matches!(b, b'\r' | b'\n'));
    cursors.iter().any(|&p| {
        start <= p && (p < end || (p == end && end == text.len() && ends_without_newline))
    })
}

fn document_identity(
    path: Option<&std::path::Path>,
    buffer: usize,
    authority: u64,
    epoch: usize,
) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hash);
    buffer.hash(&mut hash);
    authority.hash(&mut hash);
    epoch.hash(&mut hash);
    hash.finish()
}

pub(super) fn source_line_end(text: &str, position: usize) -> usize {
    let mut end = text[position..]
        .find(['\r', '\n'])
        .map_or(text.len(), |p| position + p + 1);
    if text.as_bytes().get(end.wrapping_sub(1)) == Some(&b'\r')
        && text.as_bytes().get(end) == Some(&b'\n')
    {
        end += 1;
    }
    end
}

/// The Markdown parser has already classified a heading. Its final source
/// line is a setext underline only when it is a separate delimiter line;
/// quoted/list headings can have container whitespace and `>` before it.
fn heading_underline(text: &str, heading: &Range<usize>) -> Option<Range<usize>> {
    let body = text[heading.clone()].trim_end_matches(['\r', '\n']);
    let end = heading.start + body.len();
    let start = text[..end].rfind('\n')? + 1;
    if start <= heading.start {
        return None;
    }
    let line = text[start..end].trim_end();
    let delimiter = line.as_bytes().last().copied()?;
    if ![b'=', b'-'].contains(&delimiter) {
        return None;
    }
    let marker = line
        .as_bytes()
        .iter()
        .rposition(|b| *b != delimiter)
        .map_or(0, |p| p + 1);
    if !line[..marker]
        .bytes()
        .all(|b| [b' ', b'\t', b'>'].contains(&b))
    {
        return None;
    }
    Some(start..heading.end)
}

/// Bounded per-bridge semantic cache; only composing buffers enter it. Parsing
/// is keyed by source revision, so caret moves do not reparse the document.
#[derive(Default)]
pub(crate) struct ComposeCache {
    documents: HashMap<(u64, usize), Document>,
    authorities: HashMap<
        u64,
        (
            Weak<dyn crate::model::filesystem::FileSystem + Send + Sync>,
            u64,
        ),
    >,
    clock: u64,
    parser: Option<Result<parsing::Worker, String>>,
    parsing: HashMap<parsing::Key, Arc<std::sync::atomic::AtomicBool>>,
    parse_wanted: std::collections::HashSet<parsing::Key>,
}

impl ComposeCache {
    pub(crate) fn validate_document_source(
        &self,
        editor: &Editor,
        owner: &super::compose_index::Owner,
    ) -> Result<(), String> {
        if !(InputTarget {
            id: 0,
            scope: owner.scope(),
            epoch: owner.epoch,
            pointer: true,
        })
        .valid(editor)
        {
            return Err("Document navigation source was closed or replaced.".into());
        }
        let window = editor.active_window();
        let state = window.buffer_state(BufferId(owner.buffer)).unwrap();
        let authority = self
            .authorities
            .get(&owner.window)
            .filter(|(fs, generation)| {
                *generation == owner.authority
                    && fs
                        .upgrade()
                        .is_some_and(|fs| Arc::ptr_eq(&fs, &window.authority().filesystem))
            })
            .ok_or("Document navigation authority changed.")?
            .1;
        if state.buffer.version() != owner.revision
            || state.buffer.len() != owner.length
            || document_identity(
                state.buffer.file_path(),
                owner.buffer,
                authority,
                owner.epoch,
            ) != owner.document
        {
            return Err("Document navigation source changed; request its current revision.".into());
        }
        Ok(())
    }

    /// The same fingerprint as a projected pane, without rebuilding its text.
    /// Source/reveal ownership must be checked while a snapshot waits for ACKs.
    pub(crate) fn document_selection(
        &self,
        editor: &mut Editor,
        owner: &super::compose_index::Owner,
    ) -> Option<u64> {
        let context = format!("{:?}", editor.get_key_context());
        let window = editor.active_window();
        let (_, views) = window.buffers.splits()?;
        let view = views
            .get(&owner.scope().leaf)?
            .buffer_state(BufferId(owner.buffer))?;
        let doc = self.documents.get(&(owner.window, owner.buffer))?;
        if doc.revision != owner.revision || doc.epoch != owner.epoch {
            return None;
        }
        let mut cursors: Vec<_> = view
            .cursors
            .iter()
            .map(|(id, c)| ComposeSelection {
                position: c.position,
                anchor: c.anchor,
                id: id.0,
                primary: id == view.cursors.primary_id(),
                utf16_position: doc.text_index.utf16_at(&doc.text, c.position),
                utf16_anchor: c.anchor.and_then(|p| doc.text_index.utf16_at(&doc.text, p)),
            })
            .collect();
        cursors.sort_by_key(|c| c.id);
        Some(selection_fingerprint(&cursors, &context))
    }

    /// A read-only complete document snapshot, independent of this client's
    /// displayed source window. Main scene gathering prepares the cache.
    pub(crate) fn semantic_inventory(
        &self,
        editor: &Editor,
        owner: &super::compose_index::Owner,
    ) -> Result<Option<Arc<semantics::Inventory>>, String> {
        self.validate_document_source(editor, owner)?;
        let Some(document) = self
            .documents
            .get(&(owner.window, owner.buffer))
            .filter(|doc| doc.revision == owner.revision && doc.epoch == owner.epoch)
        else {
            return Ok(None);
        };
        if let Some(error) = &document.parse_error {
            return Err(error.clone());
        }
        if !document.parsed {
            return Ok(None);
        }
        if let Some(error) = &document.semantics.error {
            return Err(error.clone());
        }
        Ok(Some(Arc::clone(&document.semantics)))
    }

    pub fn gather_at(
        &mut self,
        editor: &mut Editor,
        leaf: LeafId,
        buffer: BufferId,
        region: Option<usize>,
    ) -> Option<ComposePane> {
        self.gather_tables_at(editor, leaf, buffer, region, false, false)
    }

    pub fn gather_tables_at(
        &mut self,
        editor: &mut Editor,
        leaf: LeafId,
        buffer: BufferId,
        region: Option<usize>,
        measured_tables: bool,
        paragraph_windows: bool,
    ) -> Option<ComposePane> {
        self.gather_tables_with_context(
            editor,
            leaf,
            buffer,
            region,
            measured_tables,
            paragraph_windows,
            None,
        )
    }

    pub fn gather_tables_with_context(
        &mut self,
        editor: &mut Editor,
        leaf: LeafId,
        buffer: BufferId,
        region: Option<usize>,
        measured_tables: bool,
        paragraph_windows: bool,
        keep: Option<Range<usize>>,
    ) -> Option<ComposePane> {
        let context = editor.get_key_context();
        let owns_keyboard = document_keyboard(editor);
        let context = format!("{context:?}");
        let window_id = editor.active_window().id.0;
        let window = editor.active_window();
        let (_, states) = window.buffers.splits()?;
        let view = states.get(&leaf)?.buffer_state(buffer)?;
        if view.view_mode != ViewMode::PageView {
            return None;
        }
        let top_byte = view.viewport.top_byte();
        let mut cursors: Vec<_> = view
            .cursors
            .iter()
            .map(|(id, c)| ComposeSelection {
                position: c.position,
                anchor: c.anchor,
                id: id.0,
                primary: id == view.cursors.primary_id(),
                utf16_position: None,
                utf16_anchor: None,
            })
            .collect();
        cursors.sort_by_key(|c| c.id);
        let cursor_positions: Vec<_> = cursors.iter().map(|c| c.position).collect();
        let focused = window.effective_active_split() == leaf;
        let fs = window.authority().filesystem.clone();
        self.authorities
            .retain(|window, _| editor.windows.contains_key(&fresh_core::WindowId(*window)));
        if !self
            .authorities
            .get(&window_id)
            .is_some_and(|(previous, _)| {
                previous
                    .upgrade()
                    .is_some_and(|previous| Arc::ptr_eq(&previous, &fs))
            })
        {
            self.clock += 1;
            self.authorities
                .insert(window_id, (Arc::downgrade(&fs), self.clock));
            self.documents.retain(|(window, _), _| *window != window_id);
        }
        let asset_authority = self.authorities[&window_id].1;
        let readiness_enabled = editor.compose_plugins_active();
        let (epoch, current_revision) = {
            let state = editor.active_window().buffer_state(buffer)?;
            (state.marker_list.instance_id(), state.buffer.version())
        };
        if readiness_enabled {
            let window = editor.active_window_mut();
            if window
                .compose_readiness
                .track(buffer, epoch, current_revision)
            {
                window.seen_byte_ranges.remove(&buffer);
                editor.request_plugin_render();
            }
        }
        let window = editor.active_window_mut();
        let readiness = &mut window.compose_readiness;
        let state = window.buffers.get_mut(&buffer)?;
        let revision = state.buffer.version();
        let asset_document = document_identity(
            state.buffer.file_path(),
            buffer.0,
            asset_authority,
            state.marker_list.instance_id(),
        );
        let key = (window_id, buffer.0);
        self.clock += 1;
        if !self
            .documents
            .get(&key)
            .is_some_and(|d| d.revision == revision && d.epoch == state.marker_list.instance_id())
        {
            let bytes = state
                .buffer
                .get_text_range_mut(0, state.buffer.len())
                .ok()?;
            let text = String::from_utf8(bytes).ok()?;
            let previous = self
                .documents
                .remove(&key)
                .filter(|doc| doc.epoch == state.marker_list.instance_id());
            self.cancel_buffer_parse(window_id, buffer.0);
            if self.documents.len() >= CACHE_LIMIT {
                if let Some(oldest) = self
                    .documents
                    .iter()
                    .min_by_key(|(_, d)| d.used)
                    .map(|(k, _)| *k)
                {
                    self.documents.remove(&oldest);
                }
            }
            let mut document = if text.len() > BACKGROUND_PARSE_BYTES {
                Document::pending(text, revision, self.clock, state.marker_list.instance_id())
            } else {
                Document::parse(text, revision, self.clock, state.marker_list.instance_id())
            };
            if let Some(previous) = previous {
                let snapshot = previous
                    .previous_decorations
                    .unwrap_or(previous.decorations);
                if document.parsed {
                    let outline = document.decorations.outline().to_vec();
                    document.decorations = snapshot.rebase(revision, outline, &state.coord_map);
                } else {
                    document.previous_decorations = Some(snapshot);
                }
            }
            self.documents.insert(key, document);
            self.request_parse(window_id, buffer.0);
            self.schedule();
        }
        let doc = self.documents.get_mut(&key)?;
        doc.used = self.clock;
        let window = doc.source_window_policy(top_byte, region, keep, paragraph_windows);
        let (mut start, mut end) = (window.start, window.end);
        // Conceal replacements must travel whole. Extend the requested slice
        // rather than clipping a range into an invalid source map.
        use crate::app::compose_readiness::Status;
        let mut status = |start, end| {
            if readiness_enabled {
                readiness.demand(buffer, start..end)
            } else {
                Status::Ready
            }
        };
        for _ in 0..4 {
            let ranges = if status(start, end) == Status::Ready {
                state
                    .conceals
                    .query_viewport(start, end, &state.marker_list, &cursor_positions)
            } else {
                doc.decorations.active(&(start..end), &cursor_positions)
            };
            let old = (start, end);
            for (range, _) in ranges {
                start = start.min(range.start);
                end = end.max(range.end);
            }
            if (start, end) == old {
                break;
            }
        }
        doc.text.get(start..end)?;
        let objects: Vec<_> = doc
            .images
            .iter()
            .filter(|image| {
                image.start >= start
                    && image.end <= end
                    && !cursor_positions
                        .iter()
                        .any(|&position| position > image.start && position < image.end)
            })
            .map(|image| image.start..image.end)
            .collect();
        let revealed: Vec<_> = doc
            .images
            .iter()
            .filter(|image| {
                image.start >= start
                    && image.end <= end
                    && cursor_positions
                        .iter()
                        .any(|&position| position > image.start && position < image.end)
            })
            .map(|image| image.start..image.end)
            .collect();
        let visible_status = status(start, end);
        let prefixes = if paragraph_windows && doc.parsed && visible_status != Status::Failed {
            prefixes::prepare(
                doc,
                state,
                readiness,
                buffer,
                readiness_enabled,
                measured_tables,
                start..end,
                &cursor_positions,
            )
        } else {
            Default::default()
        };
        let status = if visible_status == Status::Failed || prefixes.failed {
            Status::Failed
        } else if visible_status == Status::Pending || prefixes.pending {
            Status::Pending
        } else {
            Status::Ready
        };
        if visible_status == Status::Ready && doc.parsed {
            let complete = if readiness_enabled {
                readiness.completed_ranges(buffer)
            } else {
                vec![0..doc.text.len()]
            };
            let region = start..end;
            if doc
                .decorations
                .needs_capture(state.conceals.version(), &region, &complete)
            {
                let records = state
                    .conceals
                    .resolved_snapshot(&state.marker_list, &region);
                doc.decorations.capture(
                    state.conceals.version(),
                    region,
                    complete,
                    doc.text.len(),
                    records,
                );
            }
        }
        // A ready body is not a coherent rich presentation while a required
        // offscreen marker is still missing. Keep the same source fallback
        // as failed formatting until all required prefix contexts are ready.
        let source_fallback = status == Status::Failed || prefixes.pending;
        let tables = if measured_tables && !source_fallback {
            doc.tables
                .snapshot(&doc.text, start..end, &cursor_positions)
        } else {
            Vec::new()
        };
        let projection = if !source_fallback && doc.parsed {
            let active = doc.decorations.active(&(start..end), &cursor_positions);
            doc.project_native(
                start,
                end,
                &cursor_positions,
                &objects,
                &revealed,
                &tables,
                active,
            )?
        } else {
            // Never publish a partially applied replacement/conceal map.
            Projection::new(doc.text.get(start..end)?, start, &[]).ok()?
        };
        let mut pane = ComposePane::from_projection(
            projection, doc, revision, cursors, focused, top_byte, &context, tables,
        );
        pane.editable = Some(!state.editing_disabled);
        pane.decoration_revision = Some(u64::from(state.conceals.version()));
        pane.format_regions.extend(prefixes.regions);
        if !prefixes.contexts.is_empty() {
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            pane.projection_revision.hash(&mut hash);
            prefixes.contexts.hash(&mut hash);
            pane.projection_revision = hash.finish();
            pane.hanging_contexts.extend(prefixes.contexts);
            pane.hanging_contexts
                .sort_by_key(|context| context.item_start);
        }
        pane.native_newline = if state.buffer.line_ending().insertion_str() == "\r\n" {
            2
        } else {
            1
        };
        pane.keyboard = owns_keyboard && focused;
        pane.asset_authority = asset_authority;
        pane.asset_document = asset_document;
        pane.ready = Some(status != Status::Pending && (doc.parsed || doc.parse_error.is_some()));
        // The raw source/index is sufficient for receipts, but semantic
        // geometry must wait for the worker before accepting measured actions.
        pane.current_projection = !source_fallback && (doc.parsed || doc.parse_error.is_some());
        if source_fallback {
            for image in &mut pane.images {
                image.rendered = false;
                image.source_preview = false;
            }
        }
        if status == Status::Failed {
            pane.error = "Formatting is unavailable. The Markdown source remains editable.".into();
        }
        if let Some(error) = &doc.parse_error {
            pane.error = error.clone();
        }
        Some(pane)
    }
}

fn selection_fingerprint(cursors: &[ComposeSelection], context: &str) -> u64 {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    cursors.hash(&mut hash);
    context.hash(&mut hash);
    hash.finish()
}

impl ComposePane {
    fn from_projection(
        p: Projection,
        doc: &Document,
        revision: u64,
        mut cursors: Vec<ComposeSelection>,
        focused: bool,
        top_byte: usize,
        context: &str,
        tables: Vec<super::compose_tables::Table>,
    ) -> Self {
        let mut spans: Vec<_> = doc
            .spans
            .iter()
            .filter(|s| s.start < p.source.end && s.end > p.source.start)
            .cloned()
            .collect();
        let mut link_metadata_budget = links::SCENE_METADATA_BYTES;
        links::limit(&mut spans, &mut link_metadata_budget);
        let images: Vec<_> = doc
            .images
            .iter()
            .filter(|s| s.start >= p.source.start && s.end <= p.source.end)
            .cloned()
            .map(|mut image| {
                image.rendered = !cursors
                    .iter()
                    .any(|cursor| cursor.position > image.start && cursor.position < image.end);
                image.source_preview = !image.rendered;
                image
            })
            .collect();
        let hanging_contexts = prefixes::contexts(&p, &spans, &doc.text);
        let format_regions = vec![p.source.clone()];
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        p.source.hash(&mut hash);
        doc.epoch.hash(&mut hash);
        p.text.hash(&mut hash);
        spans.hash(&mut hash);
        images.hash(&mut hash);
        tables.hash(&mut hash);
        hanging_contexts.hash(&mut hash);
        let segments = p
            .segments
            .iter()
            .map(|s| {
                s.source.hash(&mut hash);
                s.display.hash(&mut hash);
                let kind = match s.kind {
                    SegmentKind::Copy => 0,
                    SegmentKind::Hidden => 1,
                    SegmentKind::Replacement => 2,
                    SegmentKind::Insertion => 3,
                };
                kind.hash(&mut hash);
                ComposeSegment {
                    source_start: s.source.start,
                    source_end: s.source.end,
                    display_start: s.display.start,
                    display_end: s.display.end,
                    kind,
                }
            })
            .collect();
        let projection_revision = hash.finish();
        for cursor in &mut cursors {
            cursor.utf16_position = doc.text_index.utf16_at(&doc.text, cursor.position);
            cursor.utf16_anchor = cursor
                .anchor
                .and_then(|p| doc.text_index.utf16_at(&doc.text, p));
        }
        let selection_revision = selection_fingerprint(&cursors, context);
        let text_context = cursors
            .iter()
            .find(|cursor| cursor.primary)
            .and_then(|cursor| {
                doc.text_index
                    .context(&doc.text, cursor.position, cursor.anchor)
            });
        let text_chunks = doc.text_index.chunks(
            &doc.text,
            &cursors.iter().map(|c| c.position).collect::<Vec<_>>(),
            text_context.as_ref(),
        );
        Self {
            buffer_revision: revision,
            projection_revision,
            selection_revision,
            source_start: p.source.start,
            source_end: p.source.end,
            text: p.text,
            segments,
            spans,
            images,
            cursors,
            focused,
            keyboard: false,
            native_newline: 0,
            current_projection: false,
            top_byte,
            document_len: doc.text.len(),
            asset_authority: 0,
            asset_document: 0,
            viewport: None,
            ready: Some(true),
            error: String::new(),
            text_context,
            text_chunks,
            tables,
            hanging_contexts,
            format_regions,
            editable: None,
            decoration_revision: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_offscreen_prefix_uses_complete_source_until_its_context_is_certified() {
        use crate::app::compose_readiness::Status;
        let root = tempfile::tempdir().unwrap();
        let mut editor = Editor::for_test(
            crate::config::Config::default(),
            80,
            24,
            Some(root.path().into()),
            crate::config_io::DirectoryContext::for_testing(root.path()),
            crate::view::color_support::ColorCapability::TrueColor,
            Arc::new(crate::model::filesystem::StdFileSystem),
            None,
            None,
            false,
            false,
            false,
        )
        .unwrap();
        let source = "100) **Parent**\n\n".to_owned()
            + &"     Child **word** ![tiny](tiny.png)\n\n".repeat(1100);
        assert!(source.len() < BACKGROUND_PARSE_BYTES);
        let anchor = source.rfind("Child").unwrap();
        let cursor = editor.active_cursors().primary_id();
        editor
            .replace_native_texts(vec![(0..0, source.clone(), cursor)])
            .unwrap();
        editor.active_cursors_mut().primary_mut().position = anchor;
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        // This queue makes the normal readiness path active without starting
        // a plugin thread that could complete the controlled batch for us.
        editor.plugin_manager_mut().test_inject_command(
            fresh_core::api::PluginCommand::SetStatus {
                message: "controlled formatter".into(),
            },
        );
        let leaf = editor.active_window().effective_active_split();
        let buffer = editor.active_buffer();
        let state = editor.active_window_mut().active_state_mut();
        let epoch = state.marker_list.instance_id();
        let revision = state.buffer.version();
        let doc = Document::parse(source.clone(), revision, 0, epoch);
        let window = doc.source_window_policy(0, Some(anchor), None, true);
        assert!(window.start > source.find("Parent").unwrap());
        let namespace =
            crate::view::overlay::OverlayNamespace::from_string("prefix-ready-test".into());
        let open = source.find("**").unwrap();
        let close = source.find("**\n").unwrap();
        for at in [open, close] {
            state
                .conceals
                .add(&mut state.marker_list, namespace.clone(), at..at + 2, None);
        }
        let readiness = &mut editor.active_window_mut().compose_readiness;
        readiness.track(buffer, epoch, revision);
        let visible = readiness
            .begin(1, buffer, &[(window.start, window.end)])
            .unwrap();
        readiness.complete(&visible, true);
        assert_eq!(readiness.status(buffer, window.clone()), Status::Ready);
        let mut cache = ComposeCache::default();
        let pending = cache
            .gather_tables_at(&mut editor, leaf, buffer, Some(anchor), true, true)
            .unwrap();
        assert_eq!(pending.ready, Some(false));
        assert!(!pending.current_projection);
        assert!(
            pending.segments.iter().all(|segment| segment.kind == 0),
            "uncertified prefix context must not expose a partial rich projection"
        );
        assert_eq!(
            pending.text,
            source[pending.source_start..pending.source_end]
        );
        assert!(!pending.images.is_empty());
        assert!(pending
            .images
            .iter()
            .all(|image| !image.rendered && !image.source_preview));
        assert!(pending.tables.is_empty());
        let header = pending
            .format_regions
            .iter()
            .find(|range| range.start == 0)
            .unwrap();
        let readiness = &mut editor.active_window_mut().compose_readiness;
        let batch = readiness
            .begin(1, buffer, &[(header.start, header.end)])
            .unwrap();
        readiness.complete(&batch, true);
        let complete = cache
            .gather_tables_at(&mut editor, leaf, buffer, Some(anchor), true, true)
            .unwrap();
        assert_eq!(complete.ready, Some(true));
        assert!(complete.current_projection);
        assert!(complete.segments.iter().any(|segment| segment.kind != 0));
        assert!(complete.images.iter().any(|image| image.rendered));
        assert!(complete
            .hanging_contexts
            .iter()
            .any(|context| context.text == "100) P"));
        let state = editor.active_window().active_state();
        assert_eq!(
            state.buffer.slice_bytes(0..state.buffer.len()),
            source.as_bytes()
        );
    }

    #[test]
    fn complete_table_font_inputs_are_deduplicated_stable_across_windows_and_independent_of_reveal()
    {
        let text =
            "| H | V |\n|---|---|\n| **café** | `a\\|b` &amp; |\n| ![**pic**](x.png) | far |\n"
                .to_owned()
                + &"| repeat | x |\n".repeat(100);
        let doc = Document::parse(text.clone(), 0, 0, 1);
        let start = text.find("| repeat").unwrap();
        let visible = doc.tables.snapshot(&text, start..text.len(), &[start + 2]);
        let all = doc.tables.snapshot(&text, 0..text.len(), &[]);
        assert_eq!(visible[0].measurement_id, all[0].measurement_id);
        assert!(Arc::ptr_eq(
            visible[0].measurement.as_ref().unwrap(),
            all[0].measurement.as_ref().unwrap()
        ));
        let measurement = visible[0].measurement.as_ref().unwrap();
        assert!(
            measurement.columns[0].samples.len() < 10,
            "repeated rows are not repeated on the wire"
        );
        let bold = measurement.columns[0]
            .samples
            .iter()
            .find(|s| s.text == "café")
            .unwrap();
        assert!(bold.spans.iter().any(|s| s.flags == 1));
        let code = measurement.columns[1]
            .samples
            .iter()
            .find(|s| !s.minimum_only && s.text.contains("a|b"))
            .unwrap();
        assert!(code.spans.iter().any(|s| s.code));
        assert!(code.text.contains('&') && !code.text.contains("&amp;"));
        assert!(measurement.columns[0]
            .samples
            .iter()
            .any(|s| s.image && !s.text.contains("![")));
        assert!(measurement.columns[0].samples.iter().any(|s| s.minimum_only
            && s.text.contains("![")
            && s.spans.iter().any(|span| span.code)));
        let edited = Document::parse(text.replace("far", "changed far"), 1, 0, 1);
        assert_ne!(
            edited
                .tables
                .snapshot(&edited.text, start..edited.text.len(), &[])[0]
                .measurement_id,
            all[0].measurement_id
        );
    }

    #[test]
    fn requested_source_window_contains_anchor_after_a_long_preceding_paragraph() {
        let text = "x".repeat(WINDOW_BYTES + 20_000) + "\n\nwanted\n";
        let anchor = text.find("wanted").unwrap();
        let doc = Document::parse(text, 0, 0, 1);
        let window = doc.source_window(0, Some(anchor));
        assert!(
            window.contains(&anchor),
            "requested byte {anchor} is outside returned source window {window:?}"
        );
        assert!(
            window.start >= anchor,
            "an unrelated giant paragraph must not expand the requested new paragraph"
        );
    }

    #[test]
    fn requested_source_windows_keep_unicode_joins_and_eof_when_context_exceeds_the_target_size() {
        let text = "café 👩🏽‍💻 soft line\r\n".repeat(5000) + "end";
        let doc = Document::parse(text, 0, 0, 1);
        let anchor = doc.text.rfind("👩🏽‍💻").unwrap();
        let window = doc.source_window(0, Some(anchor));
        assert!(window.contains(&anchor));
        assert_eq!(
            window.start, 0,
            "a long joined paragraph retains its own context"
        );
        assert!(doc.text.is_char_boundary(window.start) && doc.text.is_char_boundary(window.end));
        let eof = doc.source_window(0, Some(doc.text.len()));
        assert_eq!(eof.end, doc.text.len());
    }

    #[test]
    fn page_measurements_retain_complete_row_and_tight_list_context() {
        let text = "| Head | Image |\n|---|---|\n| tiny | ![p](".to_owned()
            + &"x".repeat(90_000)
            + ".png) |\n\nTail paragraph\n";
        let doc = Document::parse(text, 0, 0, 1);
        let row = doc.tables.snapshot(&doc.text, 0..doc.text.len(), &[])[0].rows[1].clone();
        let window = doc.source_window_with_context(0, Some(row.end), Some(row.start..row.end));
        assert_eq!(window.start, row.start);
        assert_eq!(window.end, doc.text.len());
        assert!(
            window.start > 0,
            "only the pivot context travels, not the original header cursor"
        );

        let prefix = "Unrelated paragraph.\n\n".repeat(5000);
        let item_start = prefix.len();
        let text = prefix
            + "- first line\n"
            + &"  café continuation words.\n".repeat(5000)
            + "\n- next item\n";
        let doc = Document::parse(text, 0, 0, 1);
        let anchor = doc.text.rfind("café").unwrap();
        assert!(doc
            .spans
            .iter()
            .any(|s| s.role == 13 && s.start == item_start && s.end > anchor));
        let window =
            doc.source_window_with_context(0, Some(anchor), Some(anchor..anchor + "café".len()));
        assert_eq!(
            window.start, item_start,
            "a tight list retains its hanging marker when paging into a continuation"
        );
        assert!(window.contains(&anchor));
    }

    #[test]
    fn measured_table_projection_keeps_image_objects_and_revealed_image_source_disjoint() {
        let text = "Intro\n\n| A | B |\n|---|---|\n| ![**pic**](x.png) | `code` |\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        let image = doc.images[0].start..doc.images[0].end;
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let table = doc.spans.iter().find(|s| s.role == 8).unwrap();
        state.conceals.add(
            &mut state.marker_list,
            crate::view::overlay::OverlayNamespace::from_string("md-syntax".into()),
            table.start..table.end,
            Some("TRUNCATED".into()),
        );
        let tables = doc.tables.snapshot(text, 0..text.len(), &[]);
        let p = doc
            .native_table_projection(&state, 0, text.len(), &[], &[image.clone()], &[], &tables)
            .unwrap();
        assert_eq!(p.text, "Intro\n\nA\tB\n\u{fffc}\tcode\n");
        let cursor = image.start + 3;
        let tables = doc.tables.snapshot(text, 0..text.len(), &[cursor]);
        let p = doc
            .native_table_projection(
                &state,
                0,
                text.len(),
                &[cursor],
                &[],
                &[image.clone()],
                &tables,
            )
            .unwrap();
        assert!(p.text.contains(&text[image.clone()]));
        assert!(!p.text.contains("TRUNCATED"));
        assert!(p
            .segments
            .iter()
            .filter(|s| s.source.start < image.end && s.source.end > image.start)
            .all(|s| s.kind == SegmentKind::Copy));
        let legacy = doc
            .native_projection(&state, 0, text.len(), &[], &[], &[])
            .unwrap();
        assert_eq!(
            legacy.text, text,
            "clients without table capability keep editable source"
        );
    }

    #[test]
    fn additional_replacements_require_complete_unambiguous_captured_targets() {
        use super::super::wire::field;
        let target = |fields: &[(u32, u64)], texts: &[&str]| {
            let mut bytes = Vec::new();
            for &(tag, value) in fields {
                field::opt_uint(&mut bytes, tag, Some(value));
            }
            for text in texts {
                field::raw_bytes(&mut bytes, 4, text.as_bytes());
            }
            bytes
        };
        let complete = [(1, 0), (2, 3), (3, 0)];
        let bytes = target(&complete, &[""]);
        let parsed = TextReplacement::parse(&bytes).unwrap();
        assert_eq!(parsed.range, 0..3);
        assert_eq!(parsed.cursor, 0);
        assert_eq!(parsed.text.as_deref(), Some(""));
        assert!(TextReplacement::parse(&target(&complete, &[]))
            .unwrap()
            .text
            .is_none());
        for missing in [1, 2, 3] {
            let fields: Vec<_> = complete
                .into_iter()
                .filter(|(tag, _)| *tag != missing)
                .collect();
            assert!(TextReplacement::parse(&target(&fields, &[])).is_err());
        }
        for duplicate in complete {
            let mut fields = complete.to_vec();
            fields.push(duplicate);
            assert!(TextReplacement::parse(&target(&fields, &[])).is_err());
        }
        assert!(TextReplacement::parse(&target(&complete, &["a", "b"])).is_err());
        assert!(TextReplacement::parse(&target(&[(1, 4), (2, 3), (3, 0)], &[])).is_err());
        let mut orphan = Vec::new();
        field::raw_bytes(&mut orphan, 20, &bytes);
        assert!(Input::parse(&orphan).is_err());
    }

    #[test]
    fn text_input_requires_captured_targets_and_refuses_pointer_geometry() {
        use super::super::wire::field;
        let encode = |fields: &[(u32, u64)], scope: bool| {
            let mut bytes = Vec::new();
            if scope {
                field::raw_bytes(&mut bytes, 2, &[]);
            }
            for &(tag, value) in fields {
                field::opt_uint(&mut bytes, tag, Some(value));
            }
            bytes
        };
        let replacement = [(3, 20), (13, 0), (14, 1), (16, 0), (17, 3), (19, 0)];
        let bytes = encode(&replacement, true);
        let parsed = Input::parse(&bytes).unwrap();
        assert_eq!(parsed.replacement, Some(0..3));
        assert_eq!(parsed.replacement_cursor, Some(0));
        assert_eq!(
            parsed.text().unwrap(),
            "",
            "an omitted proto3 string deletes its range"
        );
        assert!(Input::parse(&encode(&replacement, false)).is_err());
        for missing in [3, 13, 14, 16, 17, 19] {
            let fields: Vec<_> = replacement
                .into_iter()
                .filter(|(tag, _)| *tag != missing)
                .collect();
            assert!(
                Input::parse(&encode(&fields, true)).is_err(),
                "missing field {missing}"
            );
        }
        for bad in [(3, 17), (5, 0), (6, 1), (9, 7), (11, 0), (12, 0), (16, 4)] {
            let mut fields = replacement.to_vec();
            fields.push(bad);
            assert!(
                Input::parse(&encode(&fields, true)).is_err(),
                "mixed replacement field {bad:?}"
            );
        }
        let literal = [(3, 20), (14, 1), (18, 1)];
        assert!(Input::parse(&encode(&literal, true)).is_ok());
        assert!(Input::parse(&encode(&literal, false)).is_err());
        for bad in [(3, 17), (5, 0), (9, 1), (11, 0), (13, 0), (19, 0)] {
            let mut fields = literal.to_vec();
            fields.push(bad);
            assert!(
                Input::parse(&encode(&fields, true)).is_err(),
                "mixed literal field {bad:?}"
            );
        }
    }

    #[test]
    fn native_code_uses_parser_owned_content_and_preserves_source_endings_and_reveal() {
        let text = "Intro\r\n\r\n> ```rs\r\n> let a = \"café 👩🏽‍💻\";\r\n> > literal\r\n> ```\r\n\r\n    café\r\n    second\r\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        assert_eq!(doc.code.len(), 2);
        let state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        let projection = doc
            .native_projection(&state, 0, text.len(), &[], &[], &[])
            .unwrap();
        assert!(!projection.text.contains("```"));
        assert!(projection
            .text
            .contains("let a = \"café 👩🏽‍💻\";\r\n> literal\r\n"));
        assert!(projection.text.ends_with("café\r\nsecond\r\n"));
        assert_eq!(
            projection.text.matches("\r\n").count(),
            text.matches("\r\n").count() - 2,
            "only fence-owned line endings disappear"
        );
        for block in &doc.code {
            for part in &block.content {
                assert!(
                    projection
                        .segments
                        .iter()
                        .any(|s| s.kind == SegmentKind::Copy
                            && s.source.start <= part.start
                            && s.source.end >= part.end),
                    "code bytes remain individually addressable: {part:?}"
                );
            }
        }
        let opening = text.find("```rs").unwrap();
        let editing = doc
            .native_projection(&state, 0, text.len(), &[opening + 1], &[], &[])
            .unwrap();
        assert!(
            editing.text.contains("> ```rs\r\n"),
            "editing reveals the exact fence/container line"
        );
        assert_eq!(editing.text.matches("```").count(), 1);
        let body = text.find("let a").unwrap();
        let editing = doc
            .native_projection(&state, 0, text.len(), &[body + 1], &[], &[])
            .unwrap();
        assert!(
            editing.text.contains("> let a"),
            "container source reveals on the edited body line"
        );
        let start = text.find("literal").unwrap();
        let end = doc.code[0].content.last().unwrap().end;
        let clipped = doc
            .native_projection(&state, start, end, &[], &[], &[])
            .unwrap();
        assert_eq!(
            clipped.text,
            &text[start..end],
            "a window inside a code event retains the original content"
        );
        assert_eq!(clipped.source, start..end);
        for text in [
            "```",
            "```\n```",
            "```\r\n",
            "```\nbody",
            "- ```\n  code\n  ```\n",
        ] {
            let doc = Document::parse(text.into(), 0, 0, 1);
            let p = doc
                .native_projection(&state, 0, text.len(), &[], &[], &[])
                .unwrap();
            assert!(!p.text.contains("```"));
            assert!(!p.text.is_empty(), "empty code keeps a native line");
            assert_eq!(doc.text.as_str(), text);
        }
        let text = "Intro\r\r    café\r    second\r";
        let doc = Document::parse(text.into(), 0, 0, 1);
        let native = doc
            .native_projection(&state, 0, text.len(), &[], &[], &[])
            .unwrap();
        // Pulldown owns the continuation spaces as literal code for CR-only
        // input. Preserve that parser-owned content rather than guessing.
        assert_eq!(native.text, "Intro\r\rcafé\r    second\r");
        let text = "```rs\r\nbody\r\n```\r\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        assert!(
            !doc.native_projection(&state, 0, text.len(), &[text.len()], &[], &[])
                .unwrap()
                .text
                .contains("```"),
            "EOF after a newline belongs to the next line"
        );
        assert!(doc
            .native_projection(&state, 0, text.len(), &[text.len() - 2], &[], &[])
            .unwrap()
            .text
            .contains("```\r\n"));
    }

    #[test]
    fn native_blocks_do_not_inherit_terminal_column_padding_or_truncation() {
        let text = "| Header | More |\r\n| --- | --- |\r\n| all café content | tail |\r\n\r\n```rust\r\nlet value = 1;\r\n```\r\n\r\n- outer\r\n  - inner\r\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        let table = doc.spans.iter().find(|s| s.role == 8).unwrap();
        let code = doc.spans.iter().find(|s| s.role == 12).unwrap();
        let mut native_text = None;
        for columns in [12, 160] {
            let mut state = crate::state::EditorState::new(
                columns,
                24,
                1 << 20,
                Arc::new(crate::model::filesystem::StdFileSystem),
            );
            state.marker_list.set_buffer_size(text.len());
            let ns = crate::view::overlay::OverlayNamespace::from_string("md-syntax".into());
            state.conceals.add(
                &mut state.marker_list,
                ns.clone(),
                table.start..table.end,
                Some(format!("│ truncated{}│", " ".repeat(columns as usize))),
            );
            state.conceals.add(
                &mut state.marker_list,
                ns.clone(),
                code.start..code.end,
                Some(format!("┌{}┐", "─".repeat(columns as usize))),
            );
            let indent = text.find("  - inner").unwrap();
            state.conceals.add(
                &mut state.marker_list,
                ns,
                indent..indent + 2,
                Some(" ".repeat(columns as usize / 4)),
            );
            let projection = doc
                .native_projection(&state, 0, text.len(), &[], &[], &[])
                .unwrap();
            assert!(projection.text.contains("| all café content | tail |\r\n"));
            assert!(projection.text.contains("\r\nlet value = 1;\r\n"));
            assert!(!projection.text.contains("```"));
            if let Some(previous) = &native_text {
                assert_eq!(&projection.text, previous);
            }
            native_text = Some(projection.text);
            let grid = state
                .conceals
                .project_text(text, 0, &state.marker_list, &[])
                .unwrap();
            assert!(grid.text.contains("truncated") && grid.text.contains('─'));
            let cell = text.find("all café content").unwrap();
            let clipped = doc
                .native_projection(&state, cell, code.end, &[], &[], &[])
                .unwrap();
            assert!(clipped.text.contains("all café content | tail |\r\n"));
            assert!(clipped.text.contains("let value = 1;\r\n"));
            assert!(!clipped.text.contains(['┌', '─', '`']));
            let object = cell..cell + "all café content".len();
            let images = doc
                .native_projection(&state, 0, text.len(), &[], &[object], &[])
                .unwrap();
            assert!(images.text.contains("| \u{fffc} | tail |"));
        }
        assert_eq!(doc.text.as_str(), text);
    }

    #[test]
    fn native_ornaments_preserve_grid_decorations_joins_and_exact_source_reveal() {
        let text = "Title\r\n-----\r\n\r\n> first\r\n> continued\r\n\r\n***\r\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        assert_eq!(doc.underlines.len(), 1);
        assert_eq!(doc.spans.iter().filter(|s| s.role == 14).count(), 1);
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let ns = crate::view::overlay::OverlayNamespace::from_string("md-syntax".into());
        let underline = doc.underlines[0].1.clone();
        state.conceals.add(
            &mut state.marker_list,
            ns.clone(),
            underline.start..underline.end - 2,
            Some("─".repeat(78)),
        );
        let quote = text.find('>').unwrap();
        state.conceals.add(
            &mut state.marker_list,
            ns.clone(),
            quote..quote + 1,
            Some("▌".into()),
        );
        let join = text.find("first").unwrap() + 5..text.find("continued").unwrap();
        state
            .conceals
            .add(&mut state.marker_list, ns.clone(), join, Some(" ".into()));
        let rule = text.find("***").unwrap();
        state.conceals.add(
            &mut state.marker_list,
            ns,
            rule..rule + 3,
            Some("─".repeat(78)),
        );
        let native = doc
            .native_projection(&state, 0, text.len(), &[], &[], &[])
            .unwrap();
        assert!(native.text.starts_with("Title\r\n\r\n"));
        assert!(
            native.text.contains("first continued"),
            "quote joins must not be dropped while removing a rail"
        );
        assert!(!native.text.contains(['─', '▌', '>']));
        let editing = doc
            .native_projection(&state, 0, text.len(), &[1], &[], &[])
            .unwrap();
        assert!(
            editing.text.contains("-----\r\n"),
            "heading source is revealed rather than a terminal ruler"
        );
        let grid = state
            .conceals
            .project_text(text, 0, &state.marker_list, &[])
            .unwrap();
        assert!(grid.text.contains('─') && grid.text.contains('▌'));
        assert_eq!(doc.text.as_str(), text);
    }

    #[test]
    fn current_parser_headings_and_images_do_not_need_mutable_formatter_output() {
        let source = "# Title\n\nBefore ![dot](dot.png) after.\n\nTail\n";
        let doc = Document::parse(source.into(), 0, 0, 1);
        let image = doc.images[0].start..doc.images[0].end;
        let projection = doc
            .project_native(0, source.len(), &[source.len()], &[image], &[], &[], vec![])
            .unwrap();
        assert_eq!(projection.text, "Title\n\nBefore \u{fffc} after.\n\nTail\n");
        let editing = doc
            .project_native(0, source.len(), &[0], &[], &[], &[], vec![])
            .unwrap();
        assert!(
            editing.text.starts_with("# Title"),
            "the active heading's exact source opener is still editable"
        );
        let quoted = Document::parse("> ## Quoted\n\n`# literal`\n".into(), 0, 0, 1);
        let editing = quoted
            .project_native(0, quoted.text.len(), &[0], &[], &[], &[], vec![])
            .unwrap();
        assert!(
            editing.text.starts_with("> ## Quoted"),
            "a cursor in the quote prefix reveals the heading line"
        );
        assert!(editing.text.contains("# literal"));
        let setext = Document::parse("#Literal\n--------\n".into(), 0, 0, 1);
        let projection = setext
            .project_native(0, setext.text.len(), &[], &[], &[], &[], vec![])
            .unwrap();
        assert!(
            projection.text.starts_with("#Literal"),
            "a setext title's literal hash is not an ATX delimiter"
        );
    }

    #[test]
    fn native_heading_delimiters_come_from_parsed_headings_and_rules_reveal_at_eof() {
        let text="## Not setext -\r\n\r\n> Quoted\r\n> ======\r\n\r\n- Listed\r\n  ------\r\n\r\n```md\r\nFake\r\n----\r\n```\r\n\r\n---";
        let doc = Document::parse(text.into(), 0, 0, 1);
        assert_eq!(
            doc.underlines.len(),
            2,
            "code and ATX headings have no setext delimiter"
        );
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let reading = doc
            .native_projection(&state, 0, text.len(), &[], &[], &[])
            .unwrap();
        assert!(reading.text.contains("Fake\r\n----\r\n"));
        assert!(!reading.text.ends_with("---"));
        let editing = doc
            .native_projection(&state, 0, text.len(), &[text.len()], &[], &[])
            .unwrap();
        assert!(
            editing.text.ends_with("---"),
            "an EOF rule can be entered at its collapsed source end"
        );
    }

    #[test]
    fn a_quoted_rule_keeps_its_own_line_even_with_duplicate_grid_joins() {
        let text = "> `─ ▌ > literal`\r\n> ***\r\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let join = text.find('\r').unwrap()..text.find("***").unwrap();
        for name in ["first", "duplicate"] {
            state.conceals.add(
                &mut state.marker_list,
                crate::view::overlay::OverlayNamespace::from_string(name.into()),
                join.clone(),
                Some(" ".into()),
            );
        }
        let native = doc
            .native_projection(&state, 0, text.len(), &[], &[], &[])
            .unwrap();
        assert!(
            native.text.contains("`─ ▌ > literal`\r\n\r\n"),
            "literal glyphs survive while the joined rule receives its own line"
        );
    }

    #[test]
    fn observed_cursor_ownership_maps_unchanged_ranges_and_rejects_moved_or_opaque_owners() {
        use crate::model::cursor::Cursor;
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"alpha café tail"}));
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        let primary = editor.active_cursors().primary_id();
        *editor.active_cursors_mut().primary_mut() = Cursor::new(0);
        let secondary = editor
            .active_cursors_mut()
            .add(Cursor::with_selection(11, 6));
        let scope = Scope {
            window: editor.active_window().id.0,
            leaf: editor.active_window().effective_active_split(),
            buffer: editor.active_window().active_buffer(),
            revision: editor.active_state().buffer.version(),
            projection: 0,
            selection: 0,
        };
        let mut input = Input::parse(&[]).unwrap();
        input.scope = Some(scope);
        input.buffer_epoch = editor.active_state().marker_list.instance_id();
        input.observed_cursors = vec![
            CursorAnchor {
                id: primary.0,
                position: 0,
                anchor: None,
            },
            CursorAnchor {
                id: secondary.0,
                position: 6,
                anchor: Some(11),
            },
        ];
        assert_eq!(
            input.preserved_cursors(&editor),
            vec![primary.0, secondary.0]
        );
        editor
            .replace_native_texts(vec![(0..0, "前".into(), primary)])
            .unwrap();
        assert_eq!(
            input.preserved_cursors(&editor),
            vec![primary.0, secondary.0],
            "known source edits preserve reversed selection ownership"
        );
        input.observed_cursors[1].position = usize::MAX;
        assert_eq!(
            input.preserved_cursors(&editor),
            vec![primary.0],
            "impossible offsets cannot overflow source rebasing"
        );
        input.observed_cursors[1].position = 6;
        editor
            .active_cursors_mut()
            .get_mut(secondary)
            .unwrap()
            .position += 1;
        assert_eq!(
            input.preserved_cursors(&editor),
            vec![primary.0],
            "a live ID at a different caret is not owned"
        );
        *editor.active_cursors_mut().get_mut(secondary).unwrap() = Cursor::with_selection(14, 9);
        input.source_revision = Some(editor.active_state().buffer.version() + 1);
        assert!(
            input.preserved_cursors(&editor).is_empty(),
            "future revisions cannot establish provenance"
        );
        input.source_revision = None;
        let state = editor.active_state_mut();
        state.coord_map.record_barrier(state.buffer.version());
        assert!(
            input.preserved_cursors(&editor).is_empty(),
            "opaque edits invalidate prior source anchors"
        );
        input.buffer_epoch += 1;
        assert!(
            input.preserved_cursors(&editor).is_empty(),
            "marker lifetime changes cannot transfer ownership"
        );
    }

    #[test]
    fn observed_cursor_input_requires_complete_unique_anchors_on_native_text() {
        use super::super::wire::field;
        let anchor = |fields: &[(u32, u64)]| {
            let mut bytes = Vec::new();
            for &(tag, value) in fields {
                field::opt_uint(&mut bytes, tag, Some(value));
            }
            bytes
        };
        for fields in [
            vec![],
            vec![(1, 0)],
            vec![(2, 0)],
            vec![(1, 0), (1, 1), (2, 0)],
            vec![(1, 0), (2, 0), (3, 1), (3, 2)],
        ] {
            assert!(CursorAnchor::parse(&anchor(&fields)).is_err());
        }
        let mut wrong_type = anchor(&[(2, 0)]);
        field::raw_bytes(&mut wrong_type, 1, b"0");
        assert!(CursorAnchor::parse(&wrong_type).is_err());
        let valid = anchor(&[(1, 0), (2, 0), (3, 3)]);
        assert_eq!(CursorAnchor::parse(&valid).unwrap().anchor, Some(3));
        let mut bytes = Vec::new();
        field::raw_bytes(&mut bytes, 21, &valid);
        assert!(
            Input::parse(&bytes).is_err(),
            "ordinary keys cannot carry native text ownership"
        );
        bytes.clear();
        field::raw_bytes(&mut bytes, 2, &[]);
        field::uint(&mut bytes, 3, 20);
        field::uint(&mut bytes, 14, 1);
        field::uint(&mut bytes, 18, 1);
        field::raw_bytes(&mut bytes, 21, &valid);
        assert_eq!(Input::parse(&bytes).unwrap().observed_cursors.len(), 1);
        field::raw_bytes(&mut bytes, 21, &valid);
        assert!(
            Input::parse(&bytes).is_err(),
            "duplicate IDs must not manufacture a cursor group"
        );
    }

    #[test]
    fn captured_pointer_refuses_opaque_edits_and_replaced_marker_owners() {
        let mut editor = crate::webui::build_editor(80, 24, &[], false).unwrap();
        crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"alpha café\n"}));
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        let scope = Scope {
            window: editor.active_window().id.0,
            leaf: editor.active_window().effective_active_split(),
            buffer: editor.active_window().active_buffer(),
            revision: editor.active_window().active_state().buffer.version(),
            projection: 0,
            selection: 0,
        };
        let mut input = Input::parse(&[]).unwrap();
        input.id = 1;
        input.scope = Some(scope);
        input.position = Some(6);
        input.source_revision = Some(scope.revision);
        input.buffer_epoch = editor
            .active_window()
            .active_state()
            .marker_list
            .instance_id();
        let mut cache = ComposeCache::default();
        assert!(input.rebase_pointer(&mut editor, &mut cache).is_ok());
        crate::webui::apply_step(&mut editor, &serde_json::json!({"type":"X"}));
        let state = editor.active_window_mut().active_state_mut();
        state.coord_map.record_barrier(state.buffer.version());
        assert!(input
            .rebase_pointer(&mut editor, &mut cache)
            .unwrap_err()
            .contains("file changed"));
        input.source_revision = Some(editor.active_window().active_state().buffer.version());
        editor.active_window_mut().active_state_mut().marker_list =
            crate::model::marker::MarkerList::new();
        assert!(input
            .rebase_pointer(&mut editor, &mut cache)
            .unwrap_err()
            .contains("closed or replaced"));
    }

    #[test]
    fn nested_image_alt_stays_one_attachment() {
        let doc = Document::parse("![outer ![inner](inner.png)](outer.png)\n".into(), 0, 0, 1);
        assert_eq!(doc.images.len(), 1);
        assert_eq!(doc.images[0].target, "outer.png");
        assert_eq!(doc.images[0].alt, "outer inner");
    }

    #[test]
    fn image_attachment_and_navigation_share_complete_parsed_alt_names() {
        let source = "![outer *word*\n`code` ![inner](inner.png)](outer.png)\r\n";
        let doc = Document::parse(source.into(), 0, 0, 1);
        assert_eq!(doc.images.len(), 1);
        assert_eq!(doc.semantics.entries.len(), 1);
        assert_eq!(doc.images[0].alt, "outer word code inner");
        assert_eq!(doc.images[0].alt, doc.semantics.entries[0].label);
        assert_eq!(doc.text.as_bytes(), source.as_bytes());
    }

    #[test]
    fn typography_ranges_preserve_nested_blocks_links_code_and_source_bytes() {
        let text = "# Héading\r\n\r\n> Quoted [a **link**](https://example.test) and `code`.\r\n\r\n- parent\r\n  - child with ~~strike~~\r\n\r\n```md\r\n[not a link](no)\r\n```\r\n";
        let doc = Document::parse(text.into(), 0, 0, 1);
        let ranges = |role| doc.spans.iter().filter(move |span| span.role == role);
        assert_eq!(ranges(1).count(), 1);
        assert_eq!(ranges(9).count(), 1);
        let lists: Vec<_> = ranges(10).collect();
        assert_eq!(lists.len(), 2);
        assert!(lists[0].start < lists[1].start && lists[0].end >= lists[1].end);
        assert_eq!(ranges(13).count(), 2);
        assert!(ranges(11).any(|span| text[span.start..span.end].starts_with("Quoted")));
        assert!(ranges(7).any(|span| &text[span.start..span.end] == "`code`"));
        assert!(ranges(12).any(|span| text[span.start..span.end].starts_with("```md")));
        let links: Vec<_> = doc
            .spans
            .iter()
            .filter(|span| span.flags & 16 != 0)
            .collect();
        assert_eq!(links.len(), 1, "fenced link-looking code is inert");
        assert_eq!(
            &text[links[0].start..links[0].end],
            "[a **link**](https://example.test)"
        );
        assert_eq!(links[0].flags & 4, 4);
        assert_eq!(
            links[0].link_target.as_deref(),
            Some("https://example.test")
        );
        assert!(doc
            .spans
            .iter()
            .any(|span| span.flags & 8 != 0 && &text[span.start..span.end] == "~~strike~~"));
        assert_eq!(doc.text.as_bytes(), text.as_bytes());
    }

    #[test]
    fn parsed_list_bodies_cover_tight_loose_tasks_nested_quotes_and_empty_items() {
        for (text, bodies) in [
            (
                "- alpha\r\n  - **café**\r\n\r\n  last\r\n",
                vec!["alpha", "**café**"],
            ),
            ("> 12. [ ] task **bold**\n>     more\n", vec!["task"]),
            ("1) first\n2) next\n", vec!["first", "next"]),
            ("- > quoted words\n  > more\n", vec!["quoted"]),
            ("-\n  - child\n", vec!["child"]),
            ("- \n\n```md\n- fake\n```\n", vec![]),
            ("- ![pic](x.png)\n", vec!["![pic]"]),
        ] {
            let doc = Document::parse(text.into(), 0, 0, 1);
            let positions: Vec<_> = doc.spans.iter().filter_map(|s| s.content_start).collect();
            assert_eq!(positions.len(), bodies.len(), "{text:?}");
            for (position, body) in positions.iter().zip(bodies) {
                assert!(
                    text[*position..].starts_with(body),
                    "{text:?}: {position} should start {body:?}"
                );
            }
            assert!(doc
                .spans
                .iter()
                .all(|s| s.content_start.is_none() || s.role == 13));
            assert_eq!(doc.text.as_str(), text);
        }
    }

    #[test]
    fn semantic_ranges_include_setext_headings_and_real_image_destinations() {
        let text = "Title\n=====\n\n![an *image*][pic]\n\n[pic]: <images/a b.png> \"caption\"\n\n```md\n![fake](x.png)\n```\n";
        let doc = Document::parse(text.to_string(), 0, 0, 1);
        assert!(doc.spans.iter().any(|s| s.role == 1 && s.start == 0));
        assert_eq!(doc.images.len(), 1);
        let image = &doc.images[0];
        assert_eq!(image.target, "images/a b.png");
        assert_eq!(image.alt, "an image");
        assert_eq!(image.title, "caption");
        assert!(image.block);
        assert_eq!(&text[image.start..image.end], "![an *image*][pic]");
    }

    #[test]
    fn parsed_link_destinations_preserve_escapes_references_unicode_and_empty_targets() {
        let source = concat!(
            "[**café 東京**](https://example.invalid/a_(b)?q=a&amp;b=c \"Title\")\r\n\r\n",
            "[angle](<docs/a b.md>) [escaped](docs/a\\(b\\).md)\r\n\r\n",
            "[reference][id] [collapsed][] [shortcut]\r\n\r\n",
            "[id]: <docs/reference.md> 'Title'\r\n",
            "[collapsed]: ../sibling.md\r\n",
            "[shortcut]: https://example.invalid/shortcut\r\n\r\n",
            "<https://example.invalid/auto> <user@example.invalid> [self]()\r\n\r\n",
            "`[inert](no.md)` ![image](image.png) [malformed](\r\n\r\n",
            "```md\r\n[also inert](no.md)\r\n```\r\n",
        );
        let doc = Document::parse(source.into(), 0, 0, 1);
        let links: Vec<_> = doc
            .spans
            .iter()
            .filter(|span| span.flags & 16 != 0)
            .collect();
        assert_eq!(
            links
                .iter()
                .map(|span| span.link_target.as_deref())
                .collect::<Vec<_>>(),
            [
                Some("https://example.invalid/a_(b)?q=a&b=c"),
                Some("docs/a b.md"),
                Some("docs/a(b).md"),
                Some("docs/reference.md"),
                Some("../sibling.md"),
                Some("https://example.invalid/shortcut"),
                Some("https://example.invalid/auto"),
                Some("mailto:user@example.invalid"),
                Some(""),
            ]
        );
        assert!(source[links[0].start..links[0].end].starts_with("[**café 東京**]"));
        assert!(doc
            .spans
            .iter()
            .filter(|span| span.flags & 16 == 0)
            .all(|span| span.link_target.is_none()));
        assert_eq!(doc.text.as_bytes(), source.as_bytes());
    }

    #[test]
    fn large_link_projection_keeps_exact_source_without_duplicating_the_destination() {
        let target = format!("https://example.invalid/{}", "a".repeat(5 << 20));
        let source = format!("[payload]({target})\r\n");
        let doc = Document::parse(source.clone(), 0, 0, 1);
        let pane = ComposePane::from_projection(
            Projection::new(&source, 0, &[]).unwrap(),
            &doc,
            0,
            Vec::new(),
            true,
            0,
            "",
            Vec::new(),
        );
        assert_eq!(pane.text.as_bytes(), source.as_bytes());
        assert_eq!(pane.document_len, source.len());
        assert!(
            crate::webui::wire::Message::encoded_len(&pane) < source.len() + (64 << 10),
            "link metadata must not double a previously valid five-megabyte scene payload"
        );
        let link = pane.spans.iter().find(|span| span.flags & 16 != 0).unwrap();
        assert!(link.link_target.is_none());
        assert_eq!(link.link_target_bytes, Some(target.len()));
        assert!(link.link_target_preview.as_ref().unwrap().len() <= 2048);
        assert_eq!(
            doc.spans
                .iter()
                .find(|span| span.flags & 16 != 0)
                .unwrap()
                .link_target
                .as_deref(),
            Some(target.as_str())
        );
        assert_eq!(doc.text.as_bytes(), source.as_bytes());
        let mut reply = InputReply {
            compose: Some(pane.clone()),
            edit: Some(EditReceipt {
                window: 1,
                leaf: 1,
                buffer: 1,
                epoch: 1,
                pane: pane.clone(),
                cursors: Vec::new(),
                preserved_cursors: Vec::new(),
            }),
            result: Some(EditReceipt {
                window: 1,
                leaf: 1,
                buffer: 1,
                epoch: 1,
                pane,
                cursors: Vec::new(),
                preserved_cursors: Vec::new(),
            }),
            ..Default::default()
        };
        let mut remaining = 3000;
        links::limit_reply(&mut reply, &mut remaining);
        let panes = [
            reply.compose.as_ref().unwrap(),
            &reply.edit.as_ref().unwrap().pane,
            &reply.result.as_ref().unwrap().pane,
        ];
        let mut total = 0;
        for pane in panes {
            assert_eq!(pane.text.as_bytes(), source.as_bytes());
            for span in &pane.spans {
                total += span.link_target.as_ref().map_or(0, |s| s.len())
                    + span.link_target_preview.as_ref().map_or(0, |s| s.len());
            }
        }
        assert!(total <= 3000);
        assert_eq!(total + remaining, 3000);
    }

    #[test]
    fn repeated_reference_destinations_share_one_full_parsed_allocation() {
        let target = format!("https://example.invalid/{}", "a".repeat(256 << 10));
        let source = "[label][id]\n\n".repeat(128) + &format!("[id]: <{target}>\n");
        let doc = Document::parse(source.clone(), 0, 0, 1);
        let links: Vec<_> = doc
            .spans
            .iter()
            .filter(|span| span.flags & 16 != 0)
            .map(|span| span.link_target.as_ref().unwrap())
            .collect();
        // The Markdown parser itself limits reference expansion relative to
        // the source size. Every admitted reference still shares its target.
        assert!(links.len() >= 2);
        for link in &links {
            assert!(Arc::ptr_eq(links[0], link));
            assert_eq!(link.as_ref(), target);
        }
        assert_eq!(doc.text.as_bytes(), source.as_bytes());
    }
}
