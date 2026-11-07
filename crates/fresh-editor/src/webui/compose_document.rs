//! Complete projected text reads, paced independently from render frames.
//! Formatting is prepared in ordinary source windows; the visible viewport
//! never moves. No incomplete prefix is published as a document value.

use super::compose::{ComposeCache, ComposePane};
use super::compose_index::{Owner, Request as Envelope};
use super::wire::{Field, Message, Reader, WireError};
use crate::app::Editor;
use sha2::{Digest, Sha256};
use std::ops::Range;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub(crate) const MAX_SOURCE: usize = 16 << 20;
pub(crate) const MAX_ENCODED: usize = 32 << 20;
pub(crate) const MAX_PAYLOAD: usize = 128 << 10;
const CHUNK: usize = 64 << 10;
const PREPARE_INTERVAL: Duration = Duration::from_millis(20);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(30);
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
const BASE_TIMEOUT: Duration = Duration::from_secs(30);
mod delta;

/// Two readers per bridge. An updating reader retains at most one baseline
/// and one candidate body (64 MiB); delta literals borrow the candidate.
#[derive(Clone, Default)]
pub(crate) struct Budget(Arc<AtomicUsize>);
struct Lease(Arc<AtomicUsize>);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
impl Budget {
    fn acquire(&self) -> Option<Lease> {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < 2).then_some(count + 1)
            })
            .ok()
            .map(|_| Lease(self.0.clone()))
    }
}

pub(crate) struct Request {
    envelope: Envelope,
    selection: Option<u64>,
    base: Option<u64>,
}
impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let envelope = Envelope::parse(bytes)?;
        let mut selection = None;
        let mut base = None;
        for field in Reader::new(bytes) {
            let (tag, value) = field?;
            if tag != 5 && tag != 6 {
                continue;
            }
            let Field::Varint(value) = value else {
                return Err(WireError::InvalidField("document update request wire type"));
            };
            let slot = if tag == 5 { &mut selection } else { &mut base };
            if slot.replace(value).is_some() {
                return Err(WireError::InvalidField(
                    "duplicate document update request field",
                ));
            }
        }
        if envelope.owner.is_some() != selection.is_some() {
            return Err(WireError::InvalidField(
                "document source and selection ownership are required together",
            ));
        }
        if base.is_some() && envelope.owner.is_none() {
            return Err(WireError::InvalidField(
                "document baseline requires a source owner",
            ));
        }
        Ok(Self {
            envelope,
            selection,
            base,
        })
    }
}

pub(crate) struct Chunk {
    pub id: u64,
    pub owner: Owner,
    pub selection: u64,
    pub sequence: u64,
    pub length: usize,
    /// Binary encoded ComposePane. Chunk boundaries need not be text boundaries.
    pub data: Vec<u8>,
    pub done: bool,
    pub error: String,
    pub decoration_revision: u64,
    pub base_id: u64,
    pub result_length: usize,
    pub result_digest: Vec<u8>,
    pub base_digest: Vec<u8>,
    pub encoding: u32,
}

struct Read {
    id: u64,
    owner: Owner,
    selection: u64,
    decoration_revision: Option<u64>,
    progress: usize,
    regions: Vec<Range<usize>>,
    started: Instant,
    prepared: Instant,
    blob: Option<Vec<u8>>,
    length: usize,
    offset: usize,
    sequence: u64,
    waiting: Option<(u64, Instant)>,
    terminal: bool,
    error: Option<String>,
    persistent: bool,
    delta: Option<delta::Recipe>,
    base_id: u64,
    result_digest: Vec<u8>,
    base_digest: Vec<u8>,
}
impl Read {
    fn stage(&mut self, bytes: Vec<u8>, base: Option<&Baseline>) {
        if self.persistent {
            self.result_digest = Sha256::digest(&bytes).to_vec();
            if let Some(base) = base {
                self.delta = delta::Recipe::new(&base.bytes, &bytes);
                if self.delta.is_some() {
                    self.base_id = base.id;
                    self.base_digest = base.digest.clone();
                }
            }
        }
        self.length = self
            .delta
            .as_ref()
            .map_or(bytes.len(), |delta| delta.length);
        self.blob = Some(bytes);
        self.regions.clear();
    }
    fn expire(&mut self, now: Instant) {
        if self.error.is_none() && !self.terminal {
            if self.blob.is_none()
                && self.waiting.is_none()
                && now.saturating_duration_since(self.started) >= PREPARE_TIMEOUT
            {
                self.fail("Complete document formatting timed out; retry the read.".into());
            } else if self
                .waiting
                .is_some_and(|(_, at)| now.saturating_duration_since(at) >= ACK_TIMEOUT)
            {
                self.fail(
                    "Complete document reading timed out waiting for its acknowledgment.".into(),
                );
            }
        }
    }
    fn fail(&mut self, error: String) {
        self.error = Some(error);
        self.blob = None;
        self.delta = None;
        self.regions.clear();
    }
    fn validate(&self, editor: &mut Editor, cache: &ComposeCache) -> Result<(), String> {
        cache.validate_document_source(editor, &self.owner)?;
        if self.decoration_revision.is_some_and(|version| {
            editor
                .active_window()
                .buffer_state(self.owner.scope().buffer)
                .is_none_or(|state| u64::from(state.conceals.version()) != version)
        }) {
            return Err("Document formatting changed; request its current projection.".into());
        }
        if cache.document_selection(editor, &self.owner) != Some(self.selection) {
            return Err(
                "Document selection or source reveal changed; request its current projection."
                    .into(),
            );
        }
        Ok(())
    }
}

pub(crate) struct Session {
    budget: Budget,
    last_id: u64,
    read: Option<Read>,
    baseline: Option<Baseline>,
    lease: Option<Lease>,
}
struct Baseline {
    id: u64,
    owner: Owner,
    bytes: Vec<u8>,
    digest: Vec<u8>,
    used: Instant,
}
fn same_document(a: &Owner, b: &Owner) -> bool {
    (a.window, a.leaf, a.buffer, a.epoch, a.authority, a.document)
        == (b.window, b.leaf, b.buffer, b.epoch, b.authority, b.document)
}
impl Default for Session {
    fn default() -> Self {
        Self::new(Budget::default())
    }
}
impl Session {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            last_id: 0,
            read: None,
            baseline: None,
            lease: None,
        }
    }

    pub fn handle(&mut self, request: Request) {
        let request_selection = request.selection;
        let requested_base = request.base;
        let request = request.envelope;
        if request.cancel {
            if self.read.as_ref().is_some_and(|r| r.id == request.id) {
                self.read = None;
            }
            if self
                .baseline
                .as_ref()
                .is_some_and(|base| base.id == request.id)
            {
                self.baseline = None;
                self.read = None;
            }
        } else if let Some(sequence) = request.applied {
            if let Some(read) = self.read.as_mut().filter(|r| r.id == request.id) {
                match read.waiting {
                    Some((expected, _)) if expected == sequence => {
                        read.waiting = None;
                        if read.terminal {
                            let mut read = self.read.take().unwrap();
                            if read.persistent {
                                if let Some(bytes) = read.blob.take() {
                                    self.baseline = Some(Baseline {
                                        id: read.id,
                                        owner: read.owner,
                                        bytes,
                                        digest: read.result_digest,
                                        used: Instant::now(),
                                    });
                                }
                            }
                        }
                    }
                    Some((expected, _)) if sequence > expected && !read.terminal => {
                        read.fail("Document acknowledgment is ahead of its stream.".into());
                    }
                    _ => {}
                }
            }
        } else if let Some(owner) = request.owner {
            if request.id <= self.last_id {
                return;
            }
            self.last_id = request.id;
            self.read = None;
            if !self.baseline.as_ref().is_some_and(|base| {
                requested_base == Some(base.id) && same_document(&base.owner, &owner)
            }) {
                self.baseline = None;
            }
            if self.lease.is_none() {
                self.lease = self.budget.acquire();
            }
            let error = if owner.length > MAX_SOURCE {
                Some("Complete document reading exceeds its source budget; the complete source remains editable.".into())
            } else if self.lease.is_none() {
                Some("Complete document readers are busy; retry the read.".into())
            } else {
                None
            };
            self.read = Some(Read {
                id: request.id,
                owner,
                selection: request_selection.unwrap(),
                decoration_revision: None,
                progress: 0,
                regions: Vec::new(),
                started: Instant::now(),
                prepared: Instant::now() - PREPARE_INTERVAL,
                blob: None,
                length: 0,
                offset: 0,
                sequence: 0,
                waiting: None,
                terminal: false,
                error,
                persistent: requested_base.is_some(),
                delta: None,
                base_id: 0,
                result_digest: Vec::new(),
                base_digest: Vec::new(),
            });
        }
        self.release_unused();
    }

    fn release_unused(&mut self) {
        let active = self
            .read
            .as_ref()
            .is_some_and(|r| r.error.is_none() && (!r.terminal || r.blob.is_some()));
        if self.baseline.is_none() && !active {
            self.lease = None;
        }
    }

    pub fn wants_prepare(&self, now: Instant) -> bool {
        self.read.as_ref().is_some_and(|r| {
            !r.terminal
                && r.error.is_none()
                && r.blob.is_none()
                && now.saturating_duration_since(r.prepared) >= PREPARE_INTERVAL
        })
    }

    /// Release storage on time even when a client's output queue is full and
    /// the terminal error cannot yet be sent.
    pub fn expire(&mut self, now: Instant) {
        if self.read.as_ref().is_some_and(|read| {
            read.terminal
                && read
                    .waiting
                    .is_some_and(|(_, at)| now.saturating_duration_since(at) >= ACK_TIMEOUT)
        }) {
            self.read = None;
        }
        if let Some(read) = self.read.as_mut() {
            read.expire(now);
        }
        let active = self
            .read
            .as_ref()
            .is_some_and(|r| !r.terminal && r.error.is_none());
        if !active
            && self
                .baseline
                .as_ref()
                .is_some_and(|base| now.saturating_duration_since(base.used) >= BASE_TIMEOUT)
        {
            self.baseline = None;
        }
        self.release_unused();
    }

    /// Called after the client's visible panes were gathered. Return only the
    /// current preparatory ranges so normal formatter offers retain ownership.
    pub fn prepare(
        &mut self,
        editor: &mut Editor,
        cache: &mut ComposeCache,
        client: u64,
        measured_tables: bool,
        paragraph_windows: bool,
    ) -> Vec<crate::app::window::ComposeRegion> {
        let regions = self.prepare_read(editor, cache, client, measured_tables, paragraph_windows);
        self.release_unused();
        regions
    }

    fn prepare_read(
        &mut self,
        editor: &mut Editor,
        cache: &mut ComposeCache,
        client: u64,
        measured_tables: bool,
        paragraph_windows: bool,
    ) -> Vec<crate::app::window::ComposeRegion> {
        let base = self.baseline.as_ref();
        let Some(read) = self.read.as_mut() else {
            return Vec::new();
        };
        read.expire(Instant::now());
        if read.terminal || read.error.is_some() || read.blob.is_some() {
            return Vec::new();
        }
        read.prepared = Instant::now();
        if let Err(error) = read.validate(editor, cache) {
            read.fail(error);
            return Vec::new();
        }
        let scope = read.owner.scope();
        // Prior reads may already have certified every source interval. Reuse
        // that authoritative coverage instead of walking all windows again
        // for each cursor. A gap still follows ordinary bounded preparation.
        if !editor.compose_plugins_active()
            || editor
                .active_window()
                .compose_readiness
                .status(scope.buffer, 0..read.owner.length)
                == crate::app::compose_readiness::Status::Ready
        {
            read.progress = read.owner.length;
        }
        let full = read.progress >= read.owner.length;
        let keep = full.then_some(0..read.owner.length);
        let pane = cache.gather_tables_with_context(
            editor,
            scope.leaf,
            scope.buffer,
            Some(if full { 0 } else { read.progress }),
            measured_tables,
            paragraph_windows,
            keep,
        );
        let Some(mut pane) = pane else {
            read.fail("The complete document view was replaced.".into());
            return Vec::new();
        };
        read.regions = pane.format_regions.clone();
        if pane.selection_revision != read.selection {
            read.fail("Document selection or source reveal changed; retry the read.".into());
        } else if !pane.error.is_empty() {
            read.fail(pane.error.clone());
        } else if pane.ready != Some(false) || read.owner.length == 0 {
            if full {
                if pane.source_start != 0 || pane.source_end != read.owner.length {
                    read.fail(
                        "The document projection is incomplete; no text was published.".into(),
                    );
                } else {
                    // The value snapshot retains global cursor coordinates,
                    // text/map and presentation semantics. Raw IME context is
                    // already delivered by ordinary input frames; duplicating
                    // its moving source window would bloat every caret delta.
                    pane.text_context = None;
                    pane.text_chunks.clear();
                    pane.viewport =
                        super::compose_viewport::snapshot(editor, client, scope.leaf, scope.buffer)
                            .or(Some(super::compose_viewport::Value {
                                epoch: read.owner.epoch,
                                position: pane.top_byte,
                                ..Default::default()
                            }));
                    if read.owner.length == 0 {
                        pane.ready = Some(true);
                        pane.current_projection = true;
                    }
                    read.decoration_revision = pane.decoration_revision;
                    match encode(pane) {
                        Ok(bytes) => {
                            read.stage(bytes, base);
                        }
                        Err(error) => read.fail(error),
                    }
                }
            } else if pane.source_start <= read.progress && pane.source_end > read.progress {
                read.progress = pane.source_end;
            } else {
                read.fail("Document preparation made no source progress.".into());
            }
        }
        read.regions
            .iter()
            .map(|source| crate::app::window::ComposeRegion {
                leaf: scope.leaf,
                buffer: scope.buffer,
                revision: read.owner.revision,
                source: source.clone(),
            })
            .collect()
    }

    pub fn next(&mut self, editor: &mut Editor, cache: &ComposeCache) -> Option<Chunk> {
        let read = self.read.as_mut()?;
        if read.terminal {
            return None;
        }
        read.expire(Instant::now());
        if read.error.is_none() {
            if let Err(error) = read.validate(editor, cache) {
                read.fail(error);
            }
        }
        let chunk = Self::next_chunk(read);
        self.release_unused();
        chunk
    }

    fn next_chunk(read: &mut Read) -> Option<Chunk> {
        if read.terminal {
            return None;
        }
        let error = read.error.take();
        if error.is_none() && (read.waiting.is_some() || read.blob.is_none()) {
            return None;
        }
        let data = if error.is_none() {
            let bytes = read.blob.as_ref()?;
            let end = (read.offset + CHUNK).min(read.length);
            let data = read.delta.as_ref().map_or_else(
                || bytes[read.offset..end].to_vec(),
                |delta| delta.copy(bytes, read.offset..end),
            );
            read.offset = end;
            data
        } else {
            Vec::new()
        };
        let done = error.is_some() || read.offset == read.length;
        let sequence = read.sequence;
        read.sequence += 1;
        read.waiting = Some((sequence, Instant::now()));
        read.terminal = done;
        let result_length = if read.persistent && error.is_none() {
            read.blob.as_ref().map_or(0, Vec::len)
        } else {
            0
        };
        let encoding = u32::from(read.delta.is_some());
        if done {
            if !read.persistent || error.is_some() {
                read.blob = None;
            }
            read.delta = None;
            read.regions.clear();
        }
        Some(Chunk {
            id: read.id,
            owner: read.owner,
            selection: read.selection,
            sequence,
            length: if error.is_some() { 0 } else { read.length },
            data,
            done,
            error: error.unwrap_or_default(),
            decoration_revision: read.decoration_revision.unwrap_or(0),
            base_id: read.base_id,
            result_length,
            result_digest: read.result_digest.clone(),
            base_digest: read.base_digest.clone(),
            encoding,
        })
    }
}

fn encode(pane: ComposePane) -> Result<Vec<u8>, String> {
    let size = pane.encoded_len();
    if size > MAX_ENCODED {
        return Err("Complete document reading exceeds its metadata budget; source editing remains available.".into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| "Complete document storage is unavailable.".to_owned())?;
    pane.encode(&mut bytes);
    if bytes.len() > MAX_ENCODED {
        return Err("Complete document payload exceeded its declared budget.".into());
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "compose_document_tests.rs"]
mod tests;
