//! Graphical Compose data before terminal wrapping and cell flattening.
//!
//! The existing Compose conceal manager remains the text projection. The
//! Markdown parser already used by Fresh supplies semantic ranges (including
//! properly parsed image destinations), never replacement document contents.

use crate::app::Editor;
use crate::model::event::{BufferId, LeafId};
use crate::state::ViewMode;
use fresh_core::compose::{Projection, SegmentKind};
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

    pub fn snapshot(
        &self,
        editor: &mut Editor,
        cache: &mut ComposeCache,
        region: Option<usize>,
    ) -> Option<ComposePane> {
        if editor.active_window().id.0 != self.window
            || editor.active_window().pane_buffer(self.leaf) != Some(self.buffer)
        {
            return None;
        }
        cache
            .gather_at(editor, self.leaf, self.buffer, region)
            .filter(|pane| {
                pane.buffer_revision == self.revision
                    && pane.projection_revision == self.projection
                    && pane.selection_revision == self.selection
            })
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
        };
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
                _ => {}
            }
        }
        Ok(input)
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
    /// 0 body; 1..6 heading; 7 code; 8 table; 9 quote.
    pub role: u32,
    /// Bold 1, italic 2, underline 4, strikethrough 8.
    pub flags: u32,
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
}

#[derive(Clone, Debug, Hash)]
pub(crate) struct ComposeSelection {
    pub position: usize,
    pub anchor: Option<usize>,
    pub id: usize,
    pub primary: bool,
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
}

struct Document {
    revision: u64,
    epoch: usize,
    text: String,
    spans: Vec<ComposeSpan>,
    images: Vec<ComposeImage>,
    paragraphs: Vec<Range<usize>>,
    used: u64,
}

impl Document {
    fn parse(text: String, revision: u64, used: u64, epoch: usize) -> Self {
        let mut spans = Vec::new();
        let mut images = Vec::new();
        let mut image_stack: Vec<ComposeImage> = Vec::new();
        let mut paragraphs = Vec::new();
        for (event, range) in Parser::new_ext(&text, Options::all()).into_offset_iter() {
            let mut role = 0;
            let mut flags = 0;
            match event {
                Event::Start(Tag::Heading { level, .. }) => role = level as u32,
                Event::Start(Tag::CodeBlock(_)) => role = 7,
                Event::Code(text) => {
                    role = 7;
                    if let Some(image) = image_stack.last_mut() {
                        image.alt.push_str(&text);
                    }
                }
                Event::Start(Tag::Table(_)) => role = 8,
                Event::Start(Tag::BlockQuote(_)) => role = 9,
                Event::Start(Tag::Strong) => flags = 1,
                Event::Start(Tag::Emphasis) => flags = 2,
                Event::Start(Tag::Strikethrough) => flags = 8,
                Event::Start(Tag::Paragraph) => paragraphs.push(range.clone()),
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
                    end: range.end,
                    role,
                    flags,
                });
            }
        }
        Self {
            revision,
            epoch,
            text,
            spans,
            images,
            paragraphs,
            used,
        }
    }
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
        let state = editor.active_window_mut().buffers.get_mut(&buffer)?;
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
        for _ in 0..4 {
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
        let projection = state
            .conceals
            .project_text_with_objects(raw, start, &state.marker_list, &cursor_positions, &objects)
            .ok()?;
        let mut pane = ComposePane::from_projection(
            projection, doc, revision, cursors, focused, top_byte, &context,
        );
        pane.keyboard = owns_keyboard && focused;
        pane.asset_authority = asset_authority;
        pane.asset_document = asset_document;
        Some(pane)
    }
}

impl ComposePane {
    fn from_projection(
        p: Projection,
        doc: &Document,
        revision: u64,
        cursors: Vec<ComposeSelection>,
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
        cursors.hash(&mut hash);
        context.hash(&mut hash);
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
