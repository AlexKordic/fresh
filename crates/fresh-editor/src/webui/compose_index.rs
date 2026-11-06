//! A complete semantic inventory, independently paced from render frames.
//! One immutable snapshot and one unacknowledged chunk per client.

use super::compose::{semantics::Inventory, ComposeCache, Scope};
use super::wire::{Field, Reader, WireError};
use crate::app::Editor;
use crate::model::event::{BufferId, LeafId};
use std::sync::Arc;

pub(crate) const MAX_PAYLOAD: usize = 128 << 10;
const LABEL_BYTES: usize = 64 << 10;
const PART_BYTES: usize = 16 << 10;
const PARTS: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Owner {
    pub window: u64,
    pub leaf: usize,
    pub buffer: usize,
    pub epoch: usize,
    pub revision: u64,
    pub authority: u64,
    pub document: u64,
    pub length: usize,
}

impl Owner {
    fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let mut values: [Option<u64>; 8] = [None; 8];
        for field in Reader::new(bytes) {
            let (tag, value) = field?;
            if !(1..=8).contains(&tag) {
                continue;
            }
            let Field::Varint(value) = value else {
                return Err(WireError::InvalidField("semantic owner wire type"));
            };
            let slot = &mut values[tag as usize - 1];
            if slot.replace(value).is_some() {
                return Err(WireError::InvalidField("duplicate semantic owner field"));
            }
        }
        let value = |at: usize| values[at].unwrap_or(0);
        let index = |at: usize| usize::try_from(value(at)).map_err(|_| WireError::LongVarint);
        let owner = Self {
            window: value(0),
            leaf: index(1)?,
            buffer: index(2)?,
            epoch: index(3)?,
            revision: value(4),
            authority: value(5),
            document: value(6),
            length: index(7)?,
        };
        if owner.epoch == 0 {
            return Err(WireError::InvalidField("semantic owner lifetime"));
        }
        Ok(owner)
    }

    pub fn scope(&self) -> Scope {
        Scope {
            window: self.window,
            leaf: LeafId(fresh_core::SplitId(self.leaf)),
            buffer: BufferId(self.buffer),
            revision: self.revision,
            projection: 0,
            selection: 0,
        }
    }
}

pub(crate) struct Request {
    pub id: u64,
    pub owner: Option<Owner>,
    pub cancel: bool,
    pub applied: Option<u64>,
}

impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let mut id = None;
        let mut owner = None;
        let mut cancel = None;
        let mut applied = None;
        for field in Reader::new(bytes) {
            let (tag, value) = field?;
            if tag == 2 {
                if owner.is_some() {
                    return Err(WireError::InvalidField("duplicate semantic owner"));
                }
                let Field::Bytes(bytes) = value else {
                    return Err(WireError::InvalidField("semantic owner message wire type"));
                };
                owner = Some(Owner::parse(bytes)?);
                continue;
            }
            let slot = match tag {
                1 => &mut id,
                3 => &mut cancel,
                4 => &mut applied,
                _ => continue,
            };
            let Field::Varint(value) = value else {
                return Err(WireError::InvalidField("semantic request wire type"));
            };
            if slot.replace(value).is_some() {
                return Err(WireError::InvalidField("duplicate semantic request field"));
            }
        }
        let request = Self {
            id: id.unwrap_or(0),
            owner,
            cancel: cancel.unwrap_or(0) != 0,
            applied,
        };
        if request.id == 0
            || cancel.is_some_and(|v| v > 1)
            || usize::from(request.owner.is_some())
                + usize::from(request.cancel)
                + usize::from(request.applied.is_some())
                != 1
        {
            return Err(WireError::InvalidField("exclusive semantic request"));
        }
        Ok(request)
    }
}

pub(crate) struct Part {
    pub index: usize,
    pub start: usize,
    pub end: usize,
    pub kind: u32,
    pub label_offset: usize,
    pub label: String,
    pub label_done: bool,
    pub label_length: usize,
}

pub(crate) struct Chunk {
    pub id: u64,
    pub owner: Owner,
    pub total_entries: usize,
    pub label_bytes: usize,
    pub sequence: u64,
    pub parts: Vec<Part>,
    pub done: bool,
    pub error: String,
}

struct Transfer {
    id: u64,
    owner: Owner,
    inventory: Option<Arc<Inventory>>,
    entry: usize,
    offset: usize,
    next_sequence: u64,
    waiting: Option<u64>,
    terminal: bool,
    error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Session {
    last_id: u64,
    active: Option<Transfer>,
}

impl Session {
    pub fn handle(&mut self, request: Request) {
        if request.cancel {
            if self
                .active
                .as_ref()
                .is_some_and(|state| state.id == request.id)
            {
                self.active = None;
            }
        } else if let Some(sequence) = request.applied {
            if let Some(state) = self.active.as_mut().filter(|state| state.id == request.id) {
                match state.waiting {
                    Some(expected) if expected == sequence => {
                        state.waiting = None;
                        if state.terminal {
                            self.active = None;
                        }
                    }
                    Some(expected) if sequence > expected => {
                        state.error = Some(
                            "Document navigation acknowledgment is ahead of its stream.".into(),
                        )
                    }
                    _ => {}
                }
            }
        } else if let Some(owner) = request.owner {
            if request.id <= self.last_id {
                return;
            }
            self.last_id = request.id;
            self.active = Some(Transfer {
                id: request.id,
                owner,
                inventory: None,
                entry: 0,
                offset: 0,
                next_sequence: 0,
                waiting: None,
                terminal: false,
                error: None,
            });
        }
    }

    pub fn next(&mut self, editor: &Editor, cache: &ComposeCache) -> Option<Chunk> {
        let state = self.active.as_mut()?;
        if state.terminal {
            return None;
        }
        match cache.semantic_inventory(editor, &state.owner) {
            Err(error) => state.error = Some(error),
            Ok(Some(inventory)) if state.inventory.is_none() => state.inventory = Some(inventory),
            _ => {}
        }
        Self::next_chunk(state)
    }

    fn next_chunk(state: &mut Transfer) -> Option<Chunk> {
        if state.terminal {
            return None;
        }
        if let Some(error) = state.error.take() {
            let sequence = state.next_sequence;
            state.next_sequence += 1;
            state.waiting = Some(sequence);
            state.terminal = true;
            state.inventory = None;
            return Some(Chunk {
                id: state.id,
                owner: state.owner,
                total_entries: 0,
                label_bytes: 0,
                sequence,
                parts: Vec::new(),
                done: true,
                error,
            });
        }
        if state.waiting.is_some() {
            return None;
        }
        let inventory = state.inventory.as_ref()?;
        let mut parts = Vec::new();
        let mut remaining = LABEL_BYTES;
        while let Some(entry) = inventory.entries.get(state.entry) {
            if parts.len() == PARTS || remaining == 0 {
                break;
            }
            let mut end = state.offset
                + PART_BYTES
                    .min(remaining)
                    .min(entry.label.len() - state.offset);
            while !entry.label.is_char_boundary(end) {
                end -= 1;
            }
            if end == state.offset && end < entry.label.len() {
                break;
            }
            let label = entry.label[state.offset..end].to_owned();
            let label_done = end == entry.label.len();
            remaining -= label.len();
            parts.push(Part {
                index: state.entry,
                start: entry.start,
                end: entry.end,
                kind: entry.kind,
                label_offset: state.offset,
                label,
                label_done,
                label_length: entry.label.len(),
            });
            if label_done {
                state.entry += 1;
                state.offset = 0;
            } else {
                state.offset = end;
            }
        }
        let done = state.entry == inventory.entries.len();
        let sequence = state.next_sequence;
        state.next_sequence += 1;
        state.waiting = Some(sequence);
        state.terminal = done;
        let chunk = Chunk {
            id: state.id,
            owner: state.owner,
            total_entries: inventory.entries.len(),
            label_bytes: inventory.label_bytes,
            sequence,
            parts,
            done,
            error: String::new(),
        };
        if done {
            state.inventory = None;
        }
        Some(chunk)
    }
}

#[cfg(test)]
#[path = "compose_index_tests.rs"]
mod tests;
