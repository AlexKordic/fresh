//! Completion of the Compose decorations actually applied to source ranges.
//! Offers alone do not imply readiness: the plugin channel may be drained
//! across several frames. Unaffected ranges ride the same edits as seen lines.

use crate::model::event::BufferId;
use fresh_core::api::LinesBatch;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[path = "compose_offer.rs"]
mod offer;
pub(crate) use offer::OfferedLine;

static NEXT_BATCH: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Status {
    Pending,
    Ready,
    Failed,
}

struct Line {
    range: Range<usize>,
    batch: u64,
    revision: u64,
    status: Status,
    started: Instant,
}

fn coverage(lines: &[Line], range: Range<usize>) -> Status {
    let mut covered = range.start;
    let mut failed = false;
    for line in lines {
        if line.range.end <= covered {
            continue;
        }
        if line.range.start > covered || line.status == Status::Pending {
            return Status::Pending;
        }
        failed |= line.status == Status::Failed;
        covered = line.range.end;
        if covered >= range.end {
            return if failed {
                Status::Failed
            } else {
                Status::Ready
            };
        }
    }
    Status::Pending
}

struct Buffer {
    epoch: usize,
    revision: u64,
    lines: Vec<Line>,
    waiting: HashMap<(usize, usize), (Instant, bool)>,
    fallback: bool,
    offer: Option<offer::Offer>,
}

#[derive(Default)]
pub(crate) struct Registry {
    buffers: HashMap<BufferId, Buffer>,
}

impl Registry {
    /// Begin tracking only when a graphical client demands this buffer. A
    /// changed owner or an edit not accompanied by range adjustment forces a
    /// fresh offer, rather than treating old line acknowledgments as current.
    pub fn track(&mut self, id: BufferId, epoch: usize, revision: u64) -> bool {
        if self
            .buffers
            .get(&id)
            .is_some_and(|b| b.epoch == epoch && b.revision == revision)
        {
            return false;
        }
        self.buffers.insert(
            id,
            Buffer {
                epoch,
                revision,
                lines: Vec::new(),
                waiting: HashMap::new(),
                fallback: false,
                offer: None,
            },
        );
        true
    }

    pub fn is_tracked(&self, id: BufferId) -> bool {
        self.buffers.contains_key(&id)
    }
    pub fn remove(&mut self, id: BufferId) {
        self.buffers.remove(&id);
    }
    pub fn clear(&mut self) {
        self.buffers.clear();
    }
    pub fn invalidate(&mut self, id: BufferId) {
        if let Some(buffer) = self.buffers.get_mut(&id) {
            buffer.lines.clear();
            buffer.offer = None;
        }
    }
    pub fn invalidate_all(&mut self) {
        for buffer in self.buffers.values_mut() {
            buffer.lines.clear();
            buffer.offer = None;
        }
    }

    pub fn demand(&mut self, id: BufferId, range: Range<usize>) -> Status {
        let status = self.status(id, range.clone());
        let Some(buffer) = self.buffers.get_mut(&id) else {
            return status;
        };
        let key = (range.start, range.end);
        if status == Status::Failed {
            buffer.fallback = true;
        }
        // Recover only when an actual requested presentation is coherent.
        // A successful batch for a subset is not enough, especially after an
        // edit has cleared the previous demand's timeout entry.
        if status == Status::Ready
            && buffer.lines.iter().all(|line| line.status == Status::Ready)
            && buffer
                .waiting
                .keys()
                .all(|&(a, b)| coverage(&buffer.lines, a..b) == Status::Ready)
        {
            buffer.fallback = false;
        }
        // After a formatter failure, edits must remain immediately usable in
        // source form. Do not impose another formatting timeout per keystroke.
        if status == Status::Pending && buffer.fallback {
            return Status::Failed;
        }
        if status != Status::Pending {
            buffer.waiting.remove(&key);
            return status;
        }
        let (_, expired) = buffer.waiting.entry(key).or_insert((Instant::now(), false));
        if *expired {
            Status::Failed
        } else {
            Status::Pending
        }
    }

    pub fn retain_demands(&mut self, regions: &[crate::app::window::ComposeRegion]) {
        for (id, buffer) in &mut self.buffers {
            if buffer.offer.as_ref().is_some_and(|offer| {
                !regions
                    .iter()
                    .any(|r| r.buffer == *id && r.source == offer.request)
            }) {
                buffer.offer = None;
            }
            buffer.waiting.retain(|&(start, end), _| {
                regions
                    .iter()
                    .any(|r| r.buffer == *id && r.source.start == start && r.source.end == end)
            });
        }
    }

    /// At most one native formatting offer is retained per tracked buffer.
    /// Competing live ranges wait; edits, invalidation and withdrawn demands
    /// discard unfinished source instead of mixing revisions or owners.
    pub fn offer_lines(
        &mut self,
        id: BufferId,
        request: Range<usize>,
        start: usize,
        line_number: usize,
        length: usize,
        close_run: bool,
        budget: usize,
        read: impl FnMut(usize, usize) -> Option<Vec<u8>>,
    ) -> Result<Option<Vec<OfferedLine>>, ()> {
        let buffer = self.buffers.get_mut(&id).ok_or(())?;
        if start > request.start || request.start > request.end || request.end > length {
            buffer.offer = None;
            buffer.fallback = true;
            return Err(());
        }
        if buffer.offer.as_ref().is_some_and(|o| o.request != request) {
            return Ok(None);
        }
        if buffer
            .offer
            .as_ref()
            .is_some_and(|o| !o.matches(&request, length))
        {
            buffer.offer = None;
            buffer.fallback = true;
            return Err(());
        }
        let offer = buffer.offer.get_or_insert_with(|| {
            offer::Offer::new(request, start, line_number, length, close_run)
        });
        let result = offer.poll(budget, read);
        if !matches!(result, Ok(None)) {
            buffer.offer = None;
        }
        if result.is_err() {
            buffer.fallback = true;
        }
        result
    }

    pub fn begin(
        &mut self,
        window_id: u64,
        id: BufferId,
        ranges: &[(usize, usize)],
    ) -> Option<LinesBatch> {
        let buffer = self.buffers.get_mut(&id)?;
        let token = NEXT_BATCH.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(
            batch = token,
            buffer = id.0,
            ranges = ranges.len(),
            "compose formatting batch offered"
        );
        // A newer offer owns every line it rebuilds. An older completion can
        // never complete the replacement batch, even at the same source range.
        buffer.lines.retain(|line| {
            !ranges.iter().any(|&(a, b)| {
                a < line.range.end && b > line.range.start || a == b && a == line.range.start
            })
        });
        buffer.lines.extend(ranges.iter().map(|&(start, end)| Line {
            range: start..end,
            batch: token,
            revision: buffer.revision,
            status: Status::Pending,
            started: Instant::now(),
        }));
        buffer
            .lines
            .sort_by_key(|line| (line.range.start, line.range.end));
        Some(LinesBatch {
            window_id,
            buffer_id: id,
            buffer_epoch: buffer.epoch,
            revision: buffer.revision,
            id: token,
        })
    }

    pub fn complete(&mut self, batch: &LinesBatch, success: bool) -> bool {
        let Some(buffer) = self
            .buffers
            .get_mut(&batch.buffer_id)
            .filter(|b| b.epoch == batch.buffer_epoch)
        else {
            return false;
        };
        let mut changed = false;
        for line in &mut buffer.lines {
            if line.batch == batch.id
                && line.revision == batch.revision
                && line.status == Status::Pending
            {
                line.status = if success {
                    Status::Ready
                } else {
                    Status::Failed
                };
                changed = true;
            }
        }
        if changed {
            tracing::trace!(
                batch = batch.id,
                success,
                "compose formatting batch completed"
            );
            if !success {
                buffer.fallback = true;
            }
            let lines = &buffer.lines;
            buffer
                .waiting
                .retain(|&(a, b), _| coverage(lines, a..b) == Status::Pending);
        }
        changed
    }

    pub fn has_batch(&self, batch: &LinesBatch) -> bool {
        self.buffers.get(&batch.buffer_id).is_some_and(|buffer| {
            buffer.epoch == batch.buffer_epoch
                && buffer.lines.iter().any(|line| {
                    line.batch == batch.id
                        && line.revision == batch.revision
                        && line.status == Status::Pending
                })
        })
    }

    pub fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for buffer in self.buffers.values_mut() {
            let lines = &buffer.lines;
            buffer
                .waiting
                .retain(|&(a, b), _| coverage(lines, a..b) == Status::Pending);
            for (started, expired) in buffer.waiting.values_mut() {
                if !*expired && now.saturating_duration_since(*started) >= Duration::from_secs(5) {
                    tracing::trace!("compose formatting demand expired");
                    *expired = true;
                    buffer.fallback = true;
                    changed = true;
                }
            }
            for line in &mut buffer.lines {
                if line.status == Status::Pending
                    && now.saturating_duration_since(line.started) >= Duration::from_secs(5)
                {
                    tracing::trace!(
                        batch = line.batch,
                        start = line.range.start,
                        end = line.range.end,
                        "compose formatting batch expired"
                    );
                    line.status = Status::Failed;
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn status(&self, id: BufferId, range: Range<usize>) -> Status {
        if range.is_empty() {
            return Status::Ready;
        }
        let Some(buffer) = self.buffers.get(&id) else {
            return Status::Pending;
        };
        coverage(&buffer.lines, range)
    }

    /// Disjoint completed source intervals; pending/failed batches never
    /// certify a partially applied clear/rebuild command stream.
    pub fn completed_ranges(&self, id: BufferId) -> Vec<Range<usize>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        if let Some(buffer) = self.buffers.get(&id) {
            for line in buffer
                .lines
                .iter()
                .filter(|line| line.status == Status::Ready)
            {
                if let Some(last) = ranges
                    .last_mut()
                    .filter(|last| last.end >= line.range.start)
                {
                    last.end = last.end.max(line.range.end);
                } else {
                    ranges.push(line.range.clone());
                }
            }
        }
        ranges
    }

    pub fn adjust(
        &mut self,
        id: BufferId,
        epoch: usize,
        revision: u64,
        position: usize,
        removed: usize,
        inserted: usize,
    ) {
        let Some(buffer) = self.buffers.get_mut(&id) else {
            return;
        };
        if buffer.epoch != epoch {
            self.buffers.remove(&id);
            return;
        }
        let end = position + removed;
        buffer.lines.retain_mut(|line| {
            if line.range.end <= position {
                true
            } else if line.range.start >= end && line.range.start > position {
                line.range.start = (line.range.start + inserted) - removed;
                line.range.end = (line.range.end + inserted) - removed;
                true
            } else {
                false
            }
        });
        buffer.revision = revision;
        buffer.offer = None;
        buffer.waiting.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unfinished_offers_do_not_certify_and_edits_restart_from_current_source() {
        let id = BufferId(3);
        let old = "a".repeat(200);
        let new = "b".repeat(200);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        assert!(registry
            .offer_lines(id, 0..200, 0, 0, 200, true, 64, |at, n| Some(
                old.as_bytes()[at..at + n].to_vec()
            ))
            .unwrap()
            .is_none());
        assert_eq!(registry.demand(id, 0..200), Status::Pending);
        registry.adjust(id, 7, 5, 0, 1, 1);
        let mut starts = Vec::new();
        let lines = loop {
            let result = registry
                .offer_lines(id, 0..200, 0, 0, 200, true, 64, |at, n| {
                    starts.push(at);
                    Some(new.as_bytes()[at..at + n].to_vec())
                })
                .unwrap();
            if let Some(lines) = result {
                break lines;
            }
            assert_eq!(registry.status(id, 0..200), Status::Pending);
        };
        assert_eq!(starts[0], 0);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.content.as_str())
                .collect::<String>(),
            new
        );
        let ranges = lines
            .iter()
            .map(|line| (line.byte_start, line.byte_end))
            .collect::<Vec<_>>();
        let batch = registry.begin(1, id, &ranges).unwrap();
        assert_eq!(registry.demand(id, 0..200), Status::Pending);
        registry.complete(&batch, true);
        assert_eq!(registry.demand(id, 0..200), Status::Ready);
    }

    #[test]
    fn competing_offers_wait_and_withdrawn_or_replaced_owners_release_source() {
        let id = BufferId(3);
        let source = "a".repeat(200);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        assert!(registry
            .offer_lines(id, 0..200, 0, 0, 200, true, 64, |at, n| Some(
                source.as_bytes()[at..at + n].to_vec()
            ))
            .unwrap()
            .is_none());
        assert!(registry
            .offer_lines(id, 0..100, 0, 0, 200, true, 64, |_, _| panic!(
                "a competing offer must not allocate another source batch"
            ))
            .unwrap()
            .is_none());
        registry.retain_demands(&[]);
        assert!(registry
            .offer_lines(id, 0..100, 0, 0, 200, true, 64, |at, n| {
                assert_eq!(at, 0);
                Some(source.as_bytes()[at..at + n].to_vec())
            })
            .unwrap()
            .is_none());
        assert!(registry.track(id, 8, 0));
        let fresh = "fresh\n";
        let lines = registry
            .offer_lines(id, 0..fresh.len(), 0, 0, fresh.len(), true, 64, |at, n| {
                Some(fresh.as_bytes()[at..at + n].to_vec())
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            lines
                .iter()
                .map(|line| line.content.as_str())
                .collect::<String>(),
            fresh
        );
        assert_eq!(registry.status(id, 0..fresh.len()), Status::Pending);
    }

    #[test]
    fn failed_offer_reads_release_work_and_enter_editable_source_fallback() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        assert!(registry
            .offer_lines(id, 0..200, 0, 0, 200, true, 64, |_, _| None)
            .is_err());
        assert!(registry.buffers[&id].offer.is_none());
        assert_eq!(registry.demand(id, 0..200), Status::Failed);
    }

    #[test]
    fn completed_requests_do_not_expire_after_the_reader_leaves_the_view() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let batch = registry.begin(1, id, &[(0, 10)]).unwrap();
        assert_eq!(registry.demand(id, 0..10), Status::Pending);
        registry.complete(&batch, true);
        assert!(!registry.expire(Instant::now() + Duration::from_secs(6)));
        registry.adjust(id, 7, 5, 0, 0, 1);
        assert_eq!(
            registry.demand(id, 0..11),
            Status::Pending,
            "completed work cannot turn into a phantom fallback while its window is inactive"
        );
    }

    #[test]
    fn an_offer_is_not_ready_and_older_completions_cannot_complete_newer_lines() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        assert!(registry.track(id, 7, 4));
        let first = registry.begin(1, id, &[(0, 10), (10, 20)]).unwrap();
        assert_eq!(registry.status(id, 0..20), Status::Pending);
        let second = registry.begin(1, id, &[(10, 20)]).unwrap();
        assert!(registry.complete(&first, true));
        assert_eq!(registry.status(id, 0..10), Status::Ready);
        assert_eq!(registry.status(id, 0..20), Status::Pending);
        registry.complete(&second, true);
        assert_eq!(registry.status(id, 0..20), Status::Ready);
        assert!(!registry.complete(&first, true));
    }

    #[test]
    fn edits_preserve_unaffected_completion_and_invalidate_the_edited_lines() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let first = registry
            .begin(1, id, &[(0, 10), (10, 20), (20, 30)])
            .unwrap();
        registry.complete(&first, true);
        // The insertion changes only the middle line. The earlier line stays
        // ready; the later line shifts without another plugin round trip.
        registry.adjust(id, 7, 5, 10, 0, 2);
        assert!(!registry.track(id, 7, 5));
        assert_eq!(registry.status(id, 0..10), Status::Ready);
        assert_eq!(registry.status(id, 22..32), Status::Ready);
        assert_eq!(registry.status(id, 0..32), Status::Pending);
        assert_eq!(registry.completed_ranges(id), vec![0..10, 22..32]);
        let next = registry.begin(1, id, &[(10, 22)]).unwrap();
        assert!(!registry.complete(&first, true));
        registry.complete(&next, true);
        assert_eq!(registry.status(id, 0..32), Status::Ready);
        assert_eq!(registry.completed_ranges(id), vec![0..32]);
        registry.adjust(id, 7, 6, 0, 0, 2);
        assert_eq!(registry.status(id, 12..24), Status::Ready);
        assert_eq!(registry.status(id, 0..12), Status::Pending);
    }

    #[test]
    fn an_inflight_unaffected_line_can_complete_after_its_coordinates_shift() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let batch = registry.begin(1, id, &[(20, 30)]).unwrap();
        registry.adjust(id, 7, 5, 0, 0, 2);
        assert!(registry.complete(&batch, true));
        assert_eq!(registry.status(id, 22..32), Status::Ready);
        assert_eq!(registry.status(id, 20..32), Status::Pending);
    }

    #[test]
    fn refresh_replacement_and_opaque_revision_changes_reject_old_completion_tokens() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let old = registry.begin(1, id, &[(0, 10)]).unwrap();
        registry.remove(id);
        registry.track(id, 7, 4);
        let refreshed = registry.begin(1, id, &[(0, 10)]).unwrap();
        assert!(!registry.complete(&old, true));
        registry.complete(&refreshed, true);
        assert_eq!(registry.status(id, 0..10), Status::Ready);
        assert!(registry.track(id, 8, 4));
        assert!(!registry.complete(&refreshed, true));
        let replacement = registry.begin(1, id, &[(0, 10)]).unwrap();
        registry.complete(&replacement, true);
        assert!(
            registry.track(id, 8, 5),
            "a version change without range adjustment invalidates opaque content"
        );
        assert_eq!(registry.status(id, 0..10), Status::Pending);
    }

    #[test]
    fn failed_formatting_has_a_terminal_fallback_and_a_new_offer_can_recover() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let failed = registry.begin(1, id, &[(0, 10)]).unwrap();
        registry.complete(&failed, false);
        assert_eq!(registry.status(id, 0..10), Status::Failed);
        assert!(registry.completed_ranges(id).is_empty());
        let retry = registry.begin(1, id, &[(0, 10)]).unwrap();
        registry.complete(&retry, true);
        assert_eq!(registry.status(id, 0..10), Status::Ready);
    }

    #[test]
    fn stalled_batches_fall_back_without_blocking_input_and_late_success_cannot_relabel_them() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let batch = registry.begin(1, id, &[(0, 10)]).unwrap();
        assert!(!registry.expire(Instant::now()));
        assert!(registry.expire(Instant::now() + Duration::from_secs(6)));
        assert_eq!(registry.status(id, 0..10), Status::Failed);
        assert!(!registry.complete(&batch, true));
        assert_eq!(registry.status(id, 0..10), Status::Failed);
    }

    #[test]
    fn missing_coverage_has_a_deadline_even_when_no_handler_offered_a_line() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        assert_eq!(registry.demand(id, 0..10), Status::Pending);
        registry.invalidate(id);
        assert!(registry.expire(Instant::now() + Duration::from_secs(6)));
        assert_eq!(registry.demand(id, 0..10), Status::Failed);
        // A newly completed current batch can recover the presentation, while
        // repeated refreshes alone cannot continually postpone the fallback.
        let retry = registry.begin(1, id, &[(0, 10)]).unwrap();
        registry.complete(&retry, true);
        assert_eq!(registry.demand(id, 0..10), Status::Ready);
    }

    #[test]
    fn failed_formatting_does_not_reimpose_a_wait_for_every_source_edit() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        let batch = registry.begin(1, id, &[(0, 10)]).unwrap();
        registry.complete(&batch, false);
        registry.adjust(id, 7, 5, 0, 0, 2);
        assert_eq!(
            registry.demand(id, 0..12),
            Status::Failed,
            "the next character stays editable without a second timeout"
        );
        let recovered = registry.begin(1, id, &[(0, 12)]).unwrap();
        registry.complete(&recovered, true);
        assert_eq!(registry.demand(id, 0..12), Status::Ready);
        registry.adjust(id, 7, 6, 0, 0, 1);
        assert_eq!(
            registry.demand(id, 0..13),
            Status::Pending,
            "successful current formatting restores the ordinary readiness contract"
        );
    }

    #[test]
    fn partial_recovery_cannot_restart_the_wait_while_requested_coverage_is_missing() {
        let id = BufferId(3);
        let mut registry = Registry::default();
        registry.track(id, 7, 4);
        assert_eq!(registry.demand(id, 0..10), Status::Pending);
        registry.expire(Instant::now() + Duration::from_secs(6));
        let partial = registry.begin(1, id, &[(0, 5)]).unwrap();
        registry.complete(&partial, true);
        assert_eq!(registry.demand(id, 0..10), Status::Failed);
        registry.adjust(id, 7, 5, 0, 0, 1);
        assert_eq!(
            registry.demand(id, 0..11),
            Status::Failed,
            "partial formatting cannot delay the next source edit"
        );
        let partial = registry.begin(1, id, &[(0, 5)]).unwrap();
        registry.complete(&partial, true);
        assert_eq!(registry.demand(id, 0..11), Status::Failed);
        registry.adjust(id, 7, 6, 0, 0, 1);
        assert_eq!(registry.demand(id, 0..12), Status::Failed);
        let recovered = registry.begin(1, id, &[(0, 12)]).unwrap();
        registry.complete(&recovered, true);
        assert_eq!(registry.demand(id, 0..12), Status::Ready);
        registry.adjust(id, 7, 7, 0, 0, 1);
        assert_eq!(registry.demand(id, 0..13), Status::Pending);
    }
}
