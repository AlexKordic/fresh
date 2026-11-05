//! Per-client graphical navigation. Geometry is supplied by the renderer;
//! keybindings and cursor events remain Fresh's. A context is installed only
//! while handling one scene input and is removed before any terminal input.

use super::wire::{Field, Reader};
use crate::app::{window::Window, Editor};
use crate::input::keybindings::Action;
use crate::model::event::{BufferId, CursorId, Event, LeafId};
use std::collections::HashMap;
use std::sync::Arc;

const MAX_LINES: usize = 16_384;
const MAX_STOPS: usize = 100_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Affinity {
    Upstream,
    #[default]
    Downstream,
}

impl Affinity {
    pub fn value(self) -> u32 {
        if self == Self::Upstream {
            1
        } else {
            0
        }
    }
    fn parse(value: u64) -> Result<Self, String> {
        match value {
            0 => Ok(Self::Downstream),
            1 => Ok(Self::Upstream),
            _ => Err("unknown caret affinity".into()),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Stop {
    pub source: usize,
    pub x: f64,
    pub affinity: Affinity,
}

#[derive(Clone, Debug)]
pub(crate) struct Line {
    pub number: usize,
    pub y: f64,
    pub height: f64,
    pub source_start: usize,
    pub source_end: usize,
    pub start: Stop,
    pub end: Stop,
    pub stops: Vec<Stop>,
}

#[derive(Debug)]
pub(crate) struct Layout {
    pub id: u64,
    pub page_height: f64,
    pub source_start: usize,
    pub source_end: usize,
    pub document_len: usize,
    pub lines: Vec<Line>,
    pub total_lines: usize,
    pub height: f64,
    pub style_revision: u64,
}

fn fields(bytes: &[u8]) -> impl Iterator<Item = Result<(u32, Field<'_>), String>> {
    Reader::new(bytes).map(|f| f.map_err(|e| e.to_string()))
}

fn number(field: Field<'_>) -> Result<f64, String> {
    let Field::Fixed64(bits) = field else {
        return Err("expected a graphical coordinate".into());
    };
    let value = f64::from_bits(bits);
    if !value.is_finite() || value.abs() > 100_000_000.0 {
        return Err("invalid graphical coordinate".into());
    }
    Ok(value)
}

fn stop(bytes: &[u8]) -> Result<Stop, String> {
    let mut result = Stop {
        source: 0,
        x: 0.0,
        affinity: Affinity::Downstream,
    };
    for f in fields(bytes) {
        match f? {
            (1, v) => result.source = v.uint() as usize,
            (2, v) => result.x = number(v)?,
            (3, v) => result.affinity = Affinity::parse(v.uint())?,
            _ => {}
        }
    }
    Ok(result)
}

impl Layout {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut result = Self {
            id: 0,
            page_height: 0.0,
            source_start: 0,
            source_end: 0,
            document_len: 0,
            lines: Vec::new(),
            total_lines: 0,
            height: 0.0,
            style_revision: 0,
        };
        let mut total_stops = 0;
        for f in fields(bytes) {
            match f? {
                (1, v) => result.id = v.uint(),
                (2, v) => result.page_height = number(v)?,
                (3, v) => result.source_start = v.uint() as usize,
                (4, v) => result.source_end = v.uint() as usize,
                (5, v) => result.document_len = v.uint() as usize,
                (6, Field::Bytes(bytes)) => {
                    if result.lines.len() >= MAX_LINES {
                        return Err("too many graphical lines".into());
                    }
                    let mut line = Line {
                        number: 0,
                        y: 0.0,
                        height: 0.0,
                        source_start: 0,
                        source_end: 0,
                        start: Stop {
                            source: 0,
                            x: 0.0,
                            affinity: Affinity::Downstream,
                        },
                        end: Stop {
                            source: 0,
                            x: 0.0,
                            affinity: Affinity::Upstream,
                        },
                        stops: Vec::new(),
                    };
                    let (mut has_start, mut has_end) = (false, false);
                    for f in fields(bytes) {
                        match f? {
                            (1, v) => line.y = number(v)?,
                            (2, v) => line.height = number(v)?,
                            (3, v) => line.source_start = v.uint() as usize,
                            (4, v) => line.source_end = v.uint() as usize,
                            (5, Field::Bytes(bytes)) => {
                                total_stops += 1;
                                if total_stops > MAX_STOPS {
                                    return Err("too many graphical carets".into());
                                }
                                line.stops.push(stop(bytes)?);
                            }
                            (6, Field::Bytes(bytes)) => {
                                line.start = stop(bytes)?;
                                has_start = true;
                            }
                            (7, Field::Bytes(bytes)) => {
                                line.end = stop(bytes)?;
                                has_end = true;
                            }
                            (8, value) => line.number = value.uint() as usize,
                            _ => {}
                        }
                    }
                    if !has_start
                        || !has_end
                        || line.stops.is_empty()
                        || line.height <= 0.0
                        || line.source_start > line.source_end
                    {
                        return Err("incomplete graphical line".into());
                    }
                    if result
                        .lines
                        .last()
                        .is_some_and(|previous| line.y < previous.y + previous.height - 0.001)
                    {
                        return Err("graphical lines overlap".into());
                    }
                    if result
                        .lines
                        .last()
                        .is_some_and(|previous| line.number <= previous.number)
                    {
                        return Err("graphical line numbers must increase".into());
                    }
                    result.lines.push(line);
                }
                (7, v) => result.total_lines = v.uint() as usize,
                (8, v) => result.height = number(v)?,
                (9, v) => result.style_revision = v.uint(),
                _ => {}
            }
        }
        if result.id == 0
            || result.page_height <= 0.0
            || result.lines.is_empty()
            || result.source_start > result.source_end
            || result.source_end > result.document_len
            || result.total_lines == 0
            || result.height <= 0.0
            || result.lines.iter().any(|line| {
                line.number >= result.total_lines
                    || line.y < 0.0
                    || line.y + line.height > result.height + 0.001
            })
        {
            return Err("incomplete graphical layout".into());
        }
        Ok(result)
    }

    pub fn validate(
        &self,
        window: &Window,
        buffer: BufferId,
        source_start: usize,
        source_end: usize,
    ) -> bool {
        let Some(state) = window.buffer_state(buffer) else {
            return false;
        };
        if self.source_start < source_start
            || self.source_end > source_end
            || self.document_len != state.buffer.len()
        {
            return false;
        }
        let raw = state.buffer.slice_bytes(self.source_start..self.source_end);
        let Ok(source) = std::str::from_utf8(&raw) else {
            return false;
        };
        let valid = |at: usize| {
            at >= self.source_start
                && at <= self.source_end
                && source.is_char_boundary(at - self.source_start)
        };
        self.lines.iter().all(|line| {
            line.source_start >= self.source_start
                && line.source_end <= self.source_end
                && valid(line.source_start)
                && valid(line.source_end)
                && [line.start, line.end]
                    .iter()
                    .chain(line.stops.iter())
                    .all(|stop| {
                        stop.source >= line.source_start
                            && stop.source <= line.source_end
                            && valid(stop.source)
                    })
        })
    }

    fn line_for(&self, position: usize, affinity: Affinity) -> Option<(usize, f64)> {
        // Exact affinity matters at a soft wrap, where the same source byte
        // can be the end of one line and the beginning of the next.
        let exact = |strict: bool| {
            self.lines.iter().enumerate().find_map(|(i, line)| {
                line.stops
                    .iter()
                    .find(|s| s.source == position && (!strict || s.affinity == affinity))
                    .map(|s| (i, s.x))
            })
        };
        if let Some(found) = exact(true).or_else(|| exact(false)) {
            return Some(found);
        }
        let matches: Vec<_> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| position >= l.source_start && position <= l.source_end)
            .collect();
        let (i, line) = if affinity == Affinity::Upstream {
            matches.first().copied()
        } else {
            matches.last().copied()
        }?;
        let nearest = line
            .stops
            .iter()
            .min_by_key(|s| s.source.abs_diff(position))?;
        Some((i, nearest.x))
    }

    fn nearest(&self, row: usize, x: f64) -> Stop {
        let line = &self.lines[row];
        *line
            .stops
            .iter()
            .min_by(|a, b| {
                (a.x - x)
                    .abs()
                    .total_cmp(&(b.x - x).abs())
                    .then_with(|| (a.affinity.value()).cmp(&b.affinity.value()))
            })
            .unwrap_or(&line.start)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Memory {
    pub position: usize,
    pub revision: u64,
    pub affinity: Affinity,
    pub goal_x: Option<f64>,
}

#[derive(Clone)]
pub(crate) struct CaretState {
    pub id: usize,
    pub position: usize,
    pub affinity: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Identity(u64, LeafId, BufferId);

struct CachedLayout {
    revision: u64,
    projection: u64,
    layout: Arc<Layout>,
}

/// One client's navigation state. No geometry or pixel goal is shared with
/// another graphical client or a terminal attached to this backend.
#[derive(Default)]
pub(crate) struct Session {
    identity: Option<Identity>,
    cached: Option<CachedLayout>,
    memory: HashMap<CursorId, Memory>,
    pending: Option<(u64, Identity, Action)>,
    regions: HashMap<Identity, (u64, usize)>,
}

impl Session {
    pub fn region(
        &self,
        window: u64,
        leaf: LeafId,
        buffer: BufferId,
        revision: u64,
    ) -> Option<usize> {
        self.regions
            .get(&Identity(window, leaf, buffer))
            .filter(|(version, _)| *version == revision)
            .map(|(_, anchor)| *anchor)
    }

    pub fn request_region(&mut self, scope: &super::compose::Scope, anchor: usize) {
        let identity = Identity(scope.window, scope.leaf, scope.buffer);
        if self.identity != Some(identity) {
            self.cached = None;
            self.memory.clear();
        }
        self.identity = Some(identity);
        self.regions.insert(identity, (scope.revision, anchor));
        self.cached = None;
    }

    pub fn begin(
        &mut self,
        editor: &mut Editor,
        input: &super::compose::Input<'_>,
        snapshot: &super::compose::ComposePane,
    ) -> Result<(), String> {
        let scope = input
            .scope
            .as_ref()
            .ok_or("graphical navigation requires a pane")?;
        let identity = Identity(scope.window, scope.leaf, scope.buffer);
        if self
            .pending
            .as_ref()
            .is_some_and(|(id, target, _)| *id != input.id || *target != identity)
        {
            return Err("deferred graphical input changed its target".into());
        }
        if self.identity != Some(identity) {
            self.identity = Some(identity);
            self.cached = None;
            self.memory.clear();
        }
        if let Some(bytes) = input.layout {
            let layout = Layout::parse(bytes)?;
            if layout.id != input.layout_id
                || !layout.validate(
                    editor.active_window(),
                    scope.buffer,
                    snapshot.source_start,
                    snapshot.source_end,
                )
            {
                return Err("graphical layout does not match the source window".into());
            }
            if self
                .cached
                .as_ref()
                .is_some_and(|previous| previous.layout.style_revision != layout.style_revision)
            {
                for memory in self.memory.values_mut() {
                    memory.goal_x = None;
                }
            }
            self.cached = Some(CachedLayout {
                revision: scope.revision,
                projection: scope.projection,
                layout: Arc::new(layout),
            });
        }
        let layout = self
            .cached
            .as_ref()
            .filter(|c| {
                c.revision == scope.revision
                    && c.projection == scope.projection
                    && c.layout.id == input.layout_id
            })
            .map(|c| c.layout.clone());
        let memory = std::mem::take(&mut self.memory);
        editor.active_window_mut().compose_input =
            Some(InputContext::new(scope.leaf, scope.buffer, layout, memory));
        Ok(())
    }

    pub fn pending_action(&self, id: u64) -> Option<Action> {
        self.pending
            .as_ref()
            .filter(|(request, _, _)| *request == id)
            .map(|(_, _, action)| action.clone())
    }

    pub fn finish(
        &mut self,
        editor: &mut Editor,
        input: &super::compose::Input<'_>,
    ) -> super::compose::InputReply {
        let mut reply = super::compose::InputReply {
            id: input.id,
            accepted: true,
            ..Default::default()
        };
        let Some(scope) = input.scope.as_ref() else {
            self.clear();
            return reply;
        };
        let Some(window) = editor.windows.get_mut(&fresh_core::WindowId(scope.window)) else {
            self.clear();
            return reply;
        };
        let Some(context) = window.compose_input.take() else {
            return reply;
        };
        self.memory = context.memory;
        if let Some(action) = context.deferred {
            self.pending = Some((
                input.id,
                Identity(scope.window, scope.leaf, scope.buffer),
                action,
            ));
            reply.accepted = false;
            reply.needs_layout = true;
            reply.need_region = context.need_region;
            reply.need_y = context.need_y;
            return reply;
        }
        self.pending = None;
        if let Some(state) = window.buffer_state(scope.buffer) {
            let revision = state.buffer.version();
            let cursors = window
                .buffers
                .splits()
                .and_then(|(_, views)| views.get(&scope.leaf))
                .and_then(|v| v.buffer_state(scope.buffer));
            self.memory.retain(|id, m| {
                m.revision == revision
                    && cursors.is_some_and(|v| {
                        v.cursors.get(*id).is_some_and(|c| c.position == m.position)
                    })
            });
            reply.carets = self
                .memory
                .iter()
                .map(|(id, m)| CaretState {
                    id: id.0,
                    position: m.position,
                    affinity: m.affinity.value(),
                })
                .collect();
            reply.carets.sort_by_key(|c| c.id);
        }
        reply
    }

    pub fn pointer(
        &mut self,
        editor: &Editor,
        input: &super::compose::Input<'_>,
    ) -> Vec<CaretState> {
        if let Some(scope) = &input.scope {
            let identity = Identity(scope.window, scope.leaf, scope.buffer);
            if self.identity != Some(identity) {
                self.cached = None;
            }
            self.identity = Some(identity);
        }
        self.memory.clear();
        self.pending = None;
        let window = editor.active_window();
        let id = window.active_cursors().primary_id();
        let position = window.active_cursors().primary().position;
        let revision = window.active_state().buffer.version();
        self.memory.insert(
            id,
            Memory {
                position,
                revision,
                affinity: input.affinity,
                goal_x: None,
            },
        );
        vec![CaretState {
            id: id.0,
            position,
            affinity: input.affinity.value(),
        }]
    }

    pub fn clear(&mut self) {
        self.identity = None;
        self.cached = None;
        self.memory.clear();
        self.pending = None;
    }
}

#[derive(Clone, Copy)]
enum Motion {
    Up,
    Down,
    Start,
    End,
    PageUp,
    PageDown,
}

fn classify(action: &Action) -> Option<(Motion, bool)> {
    use Action::*;
    Some(match action {
        MoveUp => (Motion::Up, false),
        MoveDown => (Motion::Down, false),
        SelectUp => (Motion::Up, true),
        SelectDown => (Motion::Down, true),
        MoveLineStart => (Motion::Start, false),
        MoveLineEnd => (Motion::End, false),
        SelectLineStart => (Motion::Start, true),
        SelectLineEnd => (Motion::End, true),
        MovePageUp => (Motion::PageUp, false),
        MovePageDown => (Motion::PageDown, false),
        SelectPageUp => (Motion::PageUp, true),
        SelectPageDown => (Motion::PageDown, true),
        _ => return std::option::Option::None,
    })
}

#[derive(Default)]
struct Need {
    region: Option<usize>,
    y: Option<f64>,
}

impl Need {
    fn region(position: usize) -> Self {
        Self {
            region: Some(position),
            y: None,
        }
    }
    fn y(y: f64) -> Self {
        Self {
            region: None,
            y: Some(y),
        }
    }
}

pub(crate) struct InputContext {
    pub leaf: LeafId,
    pub buffer: BufferId,
    pub layout: Option<Arc<Layout>>,
    pub memory: HashMap<CursorId, Memory>,
    pub deferred: Option<Action>,
    pub need_region: Option<usize>,
    pub need_y: Option<f64>,
    prepared: Option<(Action, Vec<Event>, HashMap<CursorId, Memory>)>,
}

impl InputContext {
    pub fn new(
        leaf: LeafId,
        buffer: BufferId,
        layout: Option<Arc<Layout>>,
        memory: HashMap<CursorId, Memory>,
    ) -> Self {
        Self {
            leaf,
            buffer,
            layout,
            memory,
            deferred: None,
            need_region: None,
            need_y: None,
            prepared: None,
        }
    }

    fn plan(
        &self,
        window: &Window,
        motion: Motion,
        select: bool,
    ) -> Result<(Vec<Event>, HashMap<CursorId, Memory>), Need> {
        let Some(layout) = self.layout.as_ref() else {
            return Err(Need::default());
        };
        let state = window.buffer_state(self.buffer).ok_or_else(Need::default)?;
        let revision = state.buffer.version();
        let mut events = Vec::new();
        let mut memory = self.memory.clone();
        for (id, cursor) in window.active_cursors().iter() {
            let old = self
                .memory
                .get(&id)
                .filter(|m| m.position == cursor.position && m.revision == revision);
            let affinity = old.map_or(Affinity::Downstream, |m| m.affinity);
            let backwards = matches!(motion, Motion::Up | Motion::PageUp);
            let vertical = matches!(
                motion,
                Motion::Up | Motion::Down | Motion::PageUp | Motion::PageDown
            );
            let from = if vertical && !select && cursor.deselect_on_move {
                cursor.anchor.map_or(cursor.position, |a| {
                    if backwards {
                        a.min(cursor.position)
                    } else {
                        a.max(cursor.position)
                    }
                })
            } else {
                cursor.position
            };
            let (row, x) = layout
                .line_for(from, affinity)
                .ok_or_else(|| Need::region(from))?;
            let goal = old.and_then(|m| m.goal_x).unwrap_or(x);
            let target_row = match motion {
                Motion::Up if layout.lines[row].number == 0 => {
                    if layout.source_start > 0 {
                        return Err(Need::region(layout.source_start.saturating_sub(1)));
                    }
                    row
                }
                Motion::Up => layout
                    .lines
                    .iter()
                    .position(|line| line.number + 1 == layout.lines[row].number)
                    .ok_or_else(|| Need::y(layout.lines[row].y - 0.1))?,
                Motion::Down if layout.lines[row].number + 1 == layout.total_lines => {
                    if layout.source_end < layout.document_len {
                        return Err(Need::region(layout.source_end));
                    }
                    row
                }
                Motion::Down => layout
                    .lines
                    .iter()
                    .position(|line| line.number == layout.lines[row].number + 1)
                    .ok_or_else(|| Need::y(layout.lines[row].y + layout.lines[row].height + 0.1))?,
                Motion::PageUp | Motion::PageDown => {
                    let y = layout.lines[row].y
                        + if backwards {
                            -layout.page_height
                        } else {
                            layout.page_height
                        };
                    if y < 0.0 && layout.source_start > 0 {
                        return Err(Need::region(layout.source_start.saturating_sub(1)));
                    }
                    if y >= layout.height && layout.source_end < layout.document_len {
                        return Err(Need::region(layout.source_end));
                    }
                    let y = y.clamp(0.0, (layout.height - 0.001).max(0.0));
                    layout
                        .lines
                        .iter()
                        .position(|line| y >= line.y && y < line.y + line.height)
                        .ok_or_else(|| Need::y(y))?
                }
                _ => row,
            };
            let target = match motion {
                Motion::Start => layout.lines[row].start,
                Motion::End => layout.lines[row].end,
                _ if target_row == row && backwards => layout.lines[row].start,
                _ if target_row == row
                    && !backwards
                    && layout.lines[row].number + 1 == layout.total_lines =>
                {
                    layout.lines[row].end
                }
                _ => layout.nearest(target_row, goal),
            };
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
                new_position: target.source,
                old_anchor: cursor.anchor,
                new_anchor,
                old_sticky_column: cursor.sticky_column,
                new_sticky_column: None,
            });
            memory.insert(
                id,
                Memory {
                    position: target.source,
                    revision,
                    affinity: target.affinity,
                    goal_x: vertical.then_some(goal),
                },
            );
        }
        Ok((events, memory))
    }
}

/// Run before macro recording or other action side effects. Deferral retains
/// the resolved Action, so a key chord is not resolved a second time on retry.
pub(crate) fn prepare_action(editor: &mut Editor, action: &Action) -> bool {
    let Some((motion, select)) = classify(action) else {
        return false;
    };
    let window = editor.active_window_mut();
    let Some(mut context) = window.compose_input.take() else {
        return false;
    };
    if context.leaf != window.effective_active_split() || context.buffer != window.active_buffer() {
        window.compose_input = Some(context);
        return false;
    }
    let deferred = match context.plan(window, motion, select) {
        Ok((events, memory)) => {
            context.prepared = Some((action.clone(), events, memory));
            false
        }
        Err(need) => {
            context.deferred = Some(action.clone());
            context.need_region = need.region;
            context.need_y = need.y;
            true
        }
    };
    window.compose_input = Some(context);
    deferred
}

pub(crate) fn action_events(window: &mut Window, action: &Action) -> Option<Vec<Event>> {
    let context = window.compose_input.as_mut()?;
    let (prepared, events, memory) = context.prepared.take()?;
    if &prepared != action {
        return None;
    }
    context.memory = memory;
    Some(events)
}
