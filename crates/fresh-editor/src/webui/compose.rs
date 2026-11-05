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
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Weak};

const SOURCE_LIMIT: usize = 2 << 20;
const WINDOW_BYTES: usize = 64 << 10;
const CACHE_LIMIT: usize = 8;

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
    /// Terminal outcome: this request will never be retried or applied.
    pub cancelled: bool,
    pub error: String,
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
            && pane.ready != Some(false)
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
        Ok(input)
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
    ) -> Result<(), String> {
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
        Ok(())
    }

    pub fn pointer(&self, editor: &mut Editor) -> bool {
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
}

struct Document {
    revision: u64,
    epoch: usize,
    text: String,
    text_index: super::compose_text::Index,
    spans: Vec<ComposeSpan>,
    images: Vec<ComposeImage>,
    paragraphs: Vec<Range<usize>>,
    underlines: Vec<(Range<usize>, Range<usize>)>,
    used: u64,
}

impl Document {
    fn parse(text: String, revision: u64, used: u64, epoch: usize) -> Self {
        let mut spans = Vec::new();
        let mut images = Vec::new();
        let mut image_stack: Vec<ComposeImage> = Vec::new();
        let mut paragraphs = Vec::new();
        let mut underlines = Vec::new();
        let mut items: Vec<(usize, usize)> = Vec::new();
        for (event, range) in Parser::new_ext(&text, Options::all()).into_offset_iter() {
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
            match event {
                Event::Start(Tag::Heading { level, .. }) => {
                    role = level as u32;
                    if let Some(underline) = heading_underline(&text, &range) {
                        let line_start = text[..range.start].rfind('\n').map_or(0, |n| n + 1);
                        underlines.push((line_start..range.end, underline));
                    }
                }
                Event::Rule => role = 14,
                Event::Start(Tag::CodeBlock(_)) => role = 12,
                Event::Code(text) => {
                    role = 7;
                    if let Some(image) = image_stack.last_mut() {
                        image.alt.push_str(&text);
                    }
                }
                Event::Start(Tag::Table(_)) => role = 8,
                Event::Start(Tag::BlockQuote(_)) => role = 9,
                Event::Start(Tag::List(_)) => role = 10,
                Event::Start(Tag::Item) => role = 13,
                Event::Start(Tag::Link { .. }) => flags = 4 | 16,
                Event::Start(Tag::Strong) => flags = 1,
                Event::Start(Tag::Emphasis) => flags = 2,
                Event::Start(Tag::Strikethrough) => flags = 8,
                Event::Start(Tag::Paragraph) => {
                    role = 11;
                    paragraphs.push(range.clone());
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
                Event::Text(text) => {
                    if let Some(image) = image_stack.last_mut() {
                        image.alt.push_str(&text);
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
                });
                if new_item {
                    let line_end = text[range.start..]
                        .find('\n')
                        .map_or(text.len(), |n| range.start + n);
                    items.push((spans.len() - 1, line_end));
                }
            }
        }
        Self {
            revision,
            epoch,
            text_index: super::compose_text::Index::new(&text),
            text,
            spans,
            images,
            paragraphs,
            underlines,
            used,
        }
    }

    /// Native-only overrides are resolved from parsed block ownership and
    /// active grid conceals. Column-formatted blocks retain editable source
    /// until native cell/frame layout is available. Quote joins remain whole:
    /// removing a marker inside a join would accidentally unflow the prose.
    fn native_projection(
        &self,
        state: &crate::state::EditorState,
        start: usize,
        end: usize,
        cursors: &[usize],
        objects: &[Range<usize>],
        revealed: &[Range<usize>],
    ) -> Option<Projection> {
        let mut overrides: Vec<_> = objects
            .iter()
            .cloned()
            .map(|source| Conceal {
                source,
                replacement: Some("\u{fffc}"),
            })
            .collect();
        let mut revealed = revealed.to_vec();
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
        let active = state
            .conceals
            .query_viewport(start, end, &state.marker_list, cursors);
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
        for (range, replacement) in active {
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
                revealed.push(range);
                continue;
            }
            if replacement != Some("▌")
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
        state
            .conceals
            .project_text_with_overrides(
                &self.text[start..end],
                start,
                &state.marker_list,
                cursors,
                &overrides,
                &revealed,
            )
            .ok()
    }
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
}

impl ComposeCache {
    pub fn gather_at(
        &mut self,
        editor: &mut Editor,
        leaf: LeafId,
        buffer: BufferId,
        region: Option<usize>,
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
        let (epoch, current_revision, length) = {
            let state = editor.active_window().buffer_state(buffer)?;
            (
                state.marker_list.instance_id(),
                state.buffer.version(),
                state.buffer.len(),
            )
        };
        if length > SOURCE_LIMIT {
            editor.active_window_mut().compose_readiness.remove(buffer);
            return None;
        }
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
        // The initial scene is bounded. A document above this limit continues
        // through the existing grid Compose path until paged parsing is added.
        if state.buffer.len() > SOURCE_LIMIT {
            return None;
        }
        let revision = state.buffer.version();
        let mut document_hash = std::collections::hash_map::DefaultHasher::new();
        state.buffer.file_path().hash(&mut document_hash);
        buffer.0.hash(&mut document_hash);
        asset_authority.hash(&mut document_hash);
        state.marker_list.instance_id().hash(&mut document_hash);
        let asset_document = document_hash.finish();
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
            self.documents.insert(
                key,
                Document::parse(text, revision, self.clock, state.marker_list.instance_id()),
            );
        }
        let doc = self.documents.get_mut(&key)?;
        doc.used = self.clock;
        let mut start = region
            .map_or(top_byte, |anchor| anchor.saturating_sub(WINDOW_BYTES / 3))
            .min(doc.text.len());
        while !doc.text.is_char_boundary(start) {
            start -= 1;
        }
        // Start with the entire paragraph whose source contains the anchor.
        // This preserves Compose's join conceals across hard-wrapped prose.
        if let Some(paragraph) = doc.paragraphs.iter().find(|r| r.contains(&start)) {
            start = paragraph.start;
        } else {
            start = doc.text[..start].rfind('\n').map_or(0, |n| n + 1);
        }
        let mut end = (start + WINDOW_BYTES).min(doc.text.len());
        while !doc.text.is_char_boundary(end) {
            end -= 1;
        }
        if end < doc.text.len() {
            end += doc.text[end..].find('\n').map_or(0, |n| n + 1);
        }
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
            if status(start, end) != Status::Ready {
                break;
            }
            let ranges =
                state
                    .conceals
                    .query_viewport(start, end, &state.marker_list, &cursor_positions);
            let old = (start, end);
            for (range, _) in ranges {
                start = start.min(range.start);
                end = end.max(range.end);
            }
            if (start, end) == old {
                break;
            }
        }
        let raw = doc.text.get(start..end)?;
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
        let status = status(start, end);
        let projection = if status == Status::Ready {
            doc.native_projection(state, start, end, &cursor_positions, &objects, &revealed)?
        } else {
            // Never publish a partially applied replacement/conceal map.
            Projection::new(raw, start, &[]).ok()?
        };
        let mut pane = ComposePane::from_projection(
            projection, doc, revision, cursors, focused, top_byte, &context,
        );
        pane.keyboard = owns_keyboard && focused;
        pane.asset_authority = asset_authority;
        pane.asset_document = asset_document;
        pane.ready = Some(status != Status::Pending);
        if status != Status::Ready {
            for image in &mut pane.images {
                image.rendered = false;
                image.source_preview = false;
            }
        }
        if status == Status::Failed {
            pane.error = "Formatting is unavailable. The Markdown source remains editable.".into();
        }
        Some(pane)
    }
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
    ) -> Self {
        let spans: Vec<_> = doc
            .spans
            .iter()
            .filter(|s| s.start < p.source.end && s.end > p.source.start)
            .cloned()
            .collect();
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
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        p.source.hash(&mut hash);
        doc.epoch.hash(&mut hash);
        p.text.hash(&mut hash);
        spans.hash(&mut hash);
        images.hash(&mut hash);
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
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for cursor in &mut cursors {
            cursor.utf16_position = doc.text_index.utf16_at(&doc.text, cursor.position);
            cursor.utf16_anchor = cursor
                .anchor
                .and_then(|p| doc.text_index.utf16_at(&doc.text, p));
        }
        cursors.hash(&mut hash);
        context.hash(&mut hash);
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
            selection_revision: hash.finish(),
            source_start: p.source.start,
            source_end: p.source.end,
            text: p.text,
            segments,
            spans,
            images,
            cursors,
            focused,
            keyboard: false,
            top_byte,
            document_len: doc.text.len(),
            asset_authority: 0,
            asset_document: 0,
            viewport: None,
            ready: Some(true),
            error: String::new(),
            text_context,
            text_chunks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(
                projection.text, text,
                "all source survives at every terminal measure"
            );
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
            assert_eq!(
                clipped.text,
                &text[cell..code.end],
                "clipping cannot restore a crossing terminal replacement"
            );
            let object = cell..cell + "all café content".len();
            let images = doc
                .native_projection(&state, 0, text.len(), &[], &[object], &[])
                .unwrap();
            assert!(images.text.contains("| \u{fffc} | tail |"));
        }
        assert_eq!(doc.text, text);
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
        assert_eq!(doc.text, text);
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
            assert_eq!(doc.text, text);
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
}
