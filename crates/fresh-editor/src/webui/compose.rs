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

const SOURCE_LIMIT: usize = 2 << 20;
const WINDOW_BYTES: usize = 64 << 10;
const CACHE_LIMIT: usize = 8;

#[derive(Clone, Copy)]
pub(crate) struct InputReply {
    pub id: u64,
    pub accepted: bool,
}

pub(crate) struct Input<'a> {
    pub id: u64,
    pub scope: Option<Scope>,
    pub kind: u64,
    pub payload: &'a [u8],
    pub position: Option<usize>,
    pub extend: bool,
    pub count: u32,
}

pub(crate) struct Scope {
    pub window: u64,
    pub leaf: LeafId,
    pub buffer: BufferId,
    pub revision: u64,
    pub projection: u64,
    pub selection: u64,
}

impl Scope {
    fn parse(bytes: &[u8]) -> Result<Self, super::wire::WireError> {
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

    pub fn matches(&self, editor: &mut Editor, cache: &mut ComposeCache) -> bool {
        if editor.active_window().id.0 != self.window
            || editor.active_window().pane_buffer(self.leaf) != Some(self.buffer)
        {
            return false;
        }
        cache
            .gather(editor, self.leaf, self.buffer)
            .is_some_and(|pane| {
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
                _ => {}
            }
        }
        Ok(input)
    }

    pub fn pointer(&self, editor: &mut Editor) -> bool {
        let (Some(scope), Some(position)) = (&self.scope, self.position) else {
            return false;
        };
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
        editor
            .active_window_mut()
            .set_buffer_cursor_in_splits_selecting(
                scope.buffer,
                position,
                &[scope.leaf],
                self.extend,
            );
        editor.mark_plugin_snapshot_dirty();
        editor.shell_description_stale = true;
        use crate::input::keybindings::Action;
        match self.count {
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
}

struct Document {
    revision: u64,
    text: String,
    spans: Vec<ComposeSpan>,
    images: Vec<ComposeImage>,
    paragraphs: Vec<Range<usize>>,
    used: u64,
}

impl Document {
    fn parse(text: String, revision: u64, used: u64) -> Self {
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
                    });
                }
                Event::Text(text) => {
                    if let Some(image) = image_stack.last_mut() {
                        image.alt.push_str(&text);
                    }
                }
                Event::End(TagEnd::Image) => {
                    if let Some(image) = image_stack.pop() {
                        images.push(image);
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
    clock: u64,
}

impl ComposeCache {
    pub fn gather(
        &mut self,
        editor: &mut Editor,
        leaf: LeafId,
        buffer: BufferId,
    ) -> Option<ComposePane> {
        use crate::input::keybindings::KeyContext;
        let context = editor.get_key_context();
        let owns_keyboard = matches!(
            context,
            KeyContext::Normal | KeyContext::Mode(_) | KeyContext::CompositeBuffer
        );
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
        let state = editor.active_window_mut().buffers.get_mut(&buffer)?;
        // The initial scene is bounded. A document above this limit continues
        // through the existing grid Compose path until paged parsing is added.
        if state.buffer.len() > SOURCE_LIMIT {
            return None;
        }
        let revision = state.buffer.version();
        let key = (window_id, buffer.0);
        self.clock += 1;
        if !self
            .documents
            .get(&key)
            .is_some_and(|d| d.revision == revision)
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
            self.documents
                .insert(key, Document::parse(text, revision, self.clock));
        }
        let doc = self.documents.get_mut(&key)?;
        doc.used = self.clock;
        let mut start = top_byte.min(doc.text.len());
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
        let projection = state
            .conceals
            .project_text(raw, start, &state.marker_list, &cursor_positions)
            .ok()?;
        let mut pane = ComposePane::from_projection(
            projection, doc, revision, cursors, focused, top_byte, &context,
        );
        pane.keyboard = owns_keyboard && focused;
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
            .collect();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        p.source.hash(&mut hash);
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_ranges_include_setext_headings_and_real_image_destinations() {
        let text = "Title\n=====\n\n![an *image*][pic]\n\n[pic]: <images/a b.png> \"caption\"\n\n```md\n![fake](x.png)\n```\n";
        let doc = Document::parse(text.to_string(), 0, 0);
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
