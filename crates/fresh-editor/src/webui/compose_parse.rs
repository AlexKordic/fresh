//! One bounded semantic parser worker. Jobs own immutable source/index
//! snapshots; editor state, markers, filesystem access and receipts stay on
//! the editor thread. At most one job runs and one waits.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TrySendError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    window: u64,
    buffer: usize,
    epoch: usize,
    revision: u64,
    authority: u64,
    snapshot: usize,
}

struct Job {
    key: Key,
    text: Arc<String>,
    index: Arc<super::super::compose_text::Index>,
    cancel: Arc<AtomicBool>,
}

struct Completion {
    key: Key,
    text: Arc<String>,
    result: Result<Option<Document>, String>,
}

pub(super) struct Worker {
    jobs: mpsc::SyncSender<Job>,
    results: mpsc::Receiver<Completion>,
}

impl Worker {
    fn new() -> Result<Self, String> {
        let (jobs, input) = mpsc::sync_channel::<Job>(1);
        let (output, results) = mpsc::sync_channel(2);
        std::thread::Builder::new()
            .name("compose-parser".into())
            .spawn(move || {
                while let Ok(job) = input.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        Document::parse_shared(
                            job.text.clone(),
                            job.index,
                            job.key.revision,
                            0,
                            job.key.epoch,
                            Some(&job.cancel),
                        )
                    }))
                    .map_err(|_| {
                        "Rich parsing failed; the Markdown source remains editable.".to_owned()
                    });
                    if output
                        .send(Completion {
                            key: job.key,
                            text: job.text,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .map_err(|error| {
                format!("Rich parsing could not start: {error}. The source remains editable.")
            })?;
        Ok(Self { jobs, results })
    }
}

impl ComposeCache {
    pub(crate) fn parsing_pending(&self, window: u64, buffer: usize) -> bool {
        self.documents
            .get(&(window, buffer))
            .is_some_and(|doc| !doc.parsed && doc.parse_error.is_none())
    }
    fn key(&self, window: u64, buffer: usize, doc: &Document) -> Key {
        Key {
            window,
            buffer,
            epoch: doc.epoch,
            revision: doc.revision,
            authority: self.authorities[&window].1,
            snapshot: Arc::as_ptr(&doc.text) as usize,
        }
    }

    fn current(&self, editor: &Editor, key: Key) -> bool {
        self.documents
            .get(&(key.window, key.buffer))
            .is_some_and(|doc| {
                doc.revision == key.revision
                    && doc.epoch == key.epoch
                    && Arc::as_ptr(&doc.text) as usize == key.snapshot
            })
            && editor
                .windows
                .get(&fresh_core::WindowId(key.window))
                .is_some_and(|window| {
                    window
                        .buffer_state(BufferId(key.buffer))
                        .is_some_and(|state| {
                            state.marker_list.instance_id() == key.epoch
                                && state.buffer.version() == key.revision
                        })
                        && self
                            .authorities
                            .get(&key.window)
                            .is_some_and(|(fs, generation)| {
                                *generation == key.authority
                                    && fs.upgrade().is_some_and(|fs| {
                                        Arc::ptr_eq(&fs, &window.authority().filesystem)
                                    })
                            })
                })
    }

    pub(super) fn cancel_buffer_parse(&mut self, window: u64, buffer: usize) {
        self.parse_wanted
            .retain(|key| key.window != window || key.buffer != buffer);
        for (key, token) in &self.parsing {
            if key.window == window && key.buffer == buffer {
                token.store(true, Ordering::Release);
            }
        }
    }

    pub(super) fn request_parse(&mut self, window: u64, buffer: usize) {
        let doc = &self.documents[&(window, buffer)];
        if !doc.parsed {
            self.parse_wanted.insert(self.key(window, buffer, doc));
        }
    }

    pub(super) fn schedule(&mut self) {
        if self.parsing.len() >= 2 {
            return;
        }
        let mut candidates: Vec<_> = self
            .documents
            .iter()
            .filter(|((window, _), doc)| {
                !doc.parsed && doc.parse_error.is_none() && self.authorities.contains_key(window)
            })
            .map(|(&(window, buffer), doc)| (doc.used, self.key(window, buffer, doc)))
            .filter(|(_, key)| self.parse_wanted.contains(key))
            .collect();
        candidates.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        for (_, key) in candidates {
            if self.parsing.len() >= 2 {
                break;
            }
            if self.parsing.contains_key(&key) {
                continue;
            }
            let worker = self.parser.get_or_insert_with(Worker::new);
            let worker = match worker {
                Ok(worker) => worker,
                Err(error) => {
                    self.documents
                        .get_mut(&(key.window, key.buffer))
                        .unwrap()
                        .parse_error = Some(error.clone());
                    continue;
                }
            };
            let doc = &self.documents[&(key.window, key.buffer)];
            let token = Arc::new(AtomicBool::new(false));
            let job = Job {
                key,
                text: doc.text.clone(),
                index: doc.text_index.clone(),
                cancel: token.clone(),
            };
            match worker.jobs.try_send(job) {
                Ok(()) => {
                    self.parsing.insert(key, token);
                }
                Err(TrySendError::Full(_)) => break,
                Err(TrySendError::Disconnected(_)) => {
                    self.documents
                        .get_mut(&(key.window, key.buffer))
                        .unwrap()
                        .parse_error =
                        Some("Rich parser stopped; the Markdown source remains editable.".into());
                }
            }
        }
    }

    /// A completed parse never borrows a newer mutable source. Polling also
    /// releases cancelled slots and starts the newest still-wanted snapshot.
    pub fn poll(&mut self, editor: &Editor) -> bool {
        let unwanted: Vec<_> = self
            .parse_wanted
            .iter()
            .copied()
            .filter(|key| !self.current(editor, *key))
            .collect();
        for key in unwanted {
            self.parse_wanted.remove(&key);
        }
        let obsolete: Vec<_> = self
            .parsing
            .keys()
            .copied()
            .filter(|key| !self.current(editor, *key))
            .collect();
        for key in obsolete {
            self.parsing[&key].store(true, Ordering::Release);
        }
        let completions: Vec<_> = match &self.parser {
            Some(Ok(worker)) => worker.results.try_iter().collect(),
            _ => Vec::new(),
        };
        let mut changed = false;
        for completion in completions {
            self.parsing.remove(&completion.key);
            self.parse_wanted.remove(&completion.key);
            if !self.current(editor, completion.key) {
                continue;
            }
            let key = (completion.key.window, completion.key.buffer);
            let Some(previous) = self.documents.get_mut(&key).filter(|doc| {
                doc.revision == completion.key.revision
                    && doc.epoch == completion.key.epoch
                    && Arc::ptr_eq(&doc.text, &completion.text)
            }) else {
                continue;
            };
            match completion.result {
                Ok(Some(mut parsed)) => {
                    parsed.used = previous.used;
                    if let Some(snapshot) = previous.previous_decorations.take() {
                        let coordinates = &editor.windows[&fresh_core::WindowId(key.0)]
                            .buffer_state(BufferId(key.1))
                            .unwrap()
                            .coord_map;
                        parsed.decorations = snapshot.rebase(
                            parsed.revision,
                            parsed.decorations.outline().to_vec(),
                            coordinates,
                        );
                    }
                    *previous = parsed;
                    changed = true;
                }
                Ok(None) => {}
                Err(error) => {
                    previous.parse_error = Some(error);
                    changed = true;
                }
            }
        }
        self.schedule();
        changed
    }

    pub fn clear(&mut self) {
        for token in self.parsing.values() {
            token.store(true, Ordering::Release);
        }
        self.documents.clear();
        self.parsing.clear();
        self.parse_wanted.clear();
        self.parser = None;
    }
}

impl Drop for ComposeCache {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(text: &str) -> (Editor, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let mut editor = Editor::for_test(
            crate::config::Config::default(),
            80,
            24,
            Some(root.path().to_path_buf()),
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
        let cursor = editor.active_cursors().primary_id();
        editor
            .replace_native_texts(vec![(0..0, text.to_owned(), cursor)])
            .unwrap();
        editor.active_cursors_mut().primary_mut().position = 0;
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        (editor, root)
    }

    fn gather(cache: &mut ComposeCache, editor: &mut Editor) -> ComposePane {
        let leaf = editor.active_window().effective_active_split();
        let buffer = editor.active_window().active_buffer();
        cache
            .gather_tables_at(editor, leaf, buffer, Some(0), true)
            .unwrap()
    }

    fn controlled() -> (
        ComposeCache,
        mpsc::Receiver<Job>,
        mpsc::SyncSender<Completion>,
    ) {
        let (jobs, input) = mpsc::sync_channel(1);
        let (output, results) = mpsc::sync_channel(2);
        let mut cache = ComposeCache::default();
        cache.parser = Some(Ok(Worker { jobs, results }));
        (cache, input, output)
    }

    fn complete(job: Job, output: &mpsc::SyncSender<Completion>) {
        // A parse can finish just before its cancellation token is stored.
        // Inject that already-completed immutable result after the edit.
        let parsed = Document::parse_shared(
            job.text.clone(),
            job.index,
            job.key.revision,
            0,
            job.key.epoch,
            None,
        );
        output
            .send(Completion {
                key: job.key,
                text: job.text,
                result: Ok(parsed),
            })
            .unwrap();
    }

    #[test]
    fn large_source_keeps_current_context_and_receipts_while_semantics_are_pending() {
        let source = "# Heading\n\n![picture](image.png)\n\n".to_owned()
            + &"café 東京 👩🏽‍💻 paragraph.\n\n".repeat(70000);
        assert!(source.len() > 2 << 20);
        let (mut editor, _root) = editor(&source);
        let (mut cache, input, output) = controlled();
        let raw = gather(&mut cache, &mut editor);
        assert_eq!(raw.document_len, source.len());
        assert_eq!(raw.ready, Some(false));
        assert!(!raw.current_projection);
        assert_eq!(raw.cursors[0].utf16_position, Some(0));
        assert!(raw.text.starts_with("# Heading"));
        assert!(raw.images.is_empty());
        let job = input.recv().unwrap();
        let cursor = editor.active_cursors().primary_id();
        editor
            .replace_native_texts(vec![(0..0, "前".into(), cursor)])
            .unwrap();
        let receipt = gather(&mut cache, &mut editor);
        assert_eq!(
            receipt.buffer_revision,
            editor.active_state().buffer.version()
        );
        assert_eq!(receipt.text_context.as_ref().unwrap().selection_end, 1);
        assert_eq!(receipt.cursors[0].utf16_position, Some(1));
        assert!(receipt.text.starts_with("前# Heading"));
        editor.active_cursors_mut().primary_mut().position = 5;
        assert!(job.cancel.load(Ordering::Acquire));
        complete(job, &output);
        assert!(
            !cache.poll(&editor),
            "a completed old source cannot replace a newer receipt snapshot"
        );
        let next = input.recv().unwrap();
        complete(next, &output);
        assert!(cache.poll(&editor));
        let rich = gather(&mut cache, &mut editor);
        assert_eq!(rich.document_len, source.len() + 3);
        assert_eq!(rich.ready, Some(true));
        assert_eq!(rich.images[0].target, "image.png");
        assert_eq!(rich.cursors[0].utf16_position, Some(3));
        assert_eq!(
            receipt.cursors[0].utf16_position,
            Some(1),
            "later cursor movement cannot rewrite an immutable receipt"
        );
        assert!(
            !rich.spans.iter().any(|span| span.role == 1),
            "the new leading text changed heading ownership"
        );
    }

    #[test]
    fn parser_queue_coalesces_edits_and_rejects_retired_lifetimes() {
        let (mut editor, _root) = editor(&"paragraph café\n\n".repeat(5000));
        let (mut cache, input, output) = controlled();
        gather(&mut cache, &mut editor);
        let first = input.recv().unwrap();
        let cursor = editor.active_cursors().primary_id();
        for _ in 0..12 {
            editor
                .replace_native_texts(vec![(0..0, "x".into(), cursor)])
                .unwrap();
            gather(&mut cache, &mut editor);
            assert!(cache.parsing.len() <= 2);
            assert!(cache.parse_wanted.len() <= 1);
        }
        let queued = input.recv().unwrap();
        assert!(first.cancel.load(Ordering::Acquire));
        assert!(queued.cancel.load(Ordering::Acquire));
        complete(first, &output);
        complete(queued, &output);
        assert!(!cache.poll(&editor));
        let newest = input.recv().unwrap();
        assert!(newest.text.starts_with("xxxxxxxxxxxx"));
        editor.active_window_mut().active_state_mut().marker_list =
            crate::model::marker::MarkerList::new();
        complete(newest, &output);
        assert!(
            !cache.poll(&editor),
            "numeric buffer identity cannot revive a retired lifetime"
        );
        assert!(cache.parse_wanted.is_empty());
        assert!(cache.parsing.is_empty());
    }

    #[test]
    fn actual_worker_completes_a_document_larger_than_the_old_cutoff() {
        let source = "# Heading\n\n![picture](image.png)\n\n".to_owned()
            + &"Paragraph café 東京.\n\n".repeat(110000);
        let (mut editor, _root) = editor(&source);
        let mut cache = ComposeCache::default();
        editor.active_cursors_mut().primary_mut().position = source.len();
        assert_eq!(gather(&mut cache, &mut editor).ready, Some(false));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !cache.poll(&editor) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let rich = gather(&mut cache, &mut editor);
        assert_eq!(rich.ready, Some(true));
        assert_eq!(rich.document_len, source.len());
        assert!(rich.spans.iter().any(|span| span.role == 1));
        assert_eq!(rich.images[0].target, "image.png");
        assert!(rich.text.starts_with("Heading"));
    }

    #[test]
    fn evicted_snapshot_with_repeated_source_numbers_cannot_replace_its_new_instance() {
        let (mut editor, _root) = editor(&"paragraph café\n\n".repeat(5000));
        let (mut cache, input, output) = controlled();
        gather(&mut cache, &mut editor);
        let old = input.recv().unwrap();
        cache.documents.remove(&(old.key.window, old.key.buffer));
        gather(&mut cache, &mut editor);
        let current = input.recv().unwrap();
        assert_eq!(old.key.revision, current.key.revision);
        assert_eq!(old.key.epoch, current.key.epoch);
        assert_ne!(old.key.snapshot, current.key.snapshot);
        complete(old, &output);
        assert!(!cache.poll(&editor));
        complete(current, &output);
        assert!(cache.poll(&editor));
        assert_eq!(gather(&mut cache, &mut editor).ready, Some(true));
    }

    #[test]
    fn stopped_parser_reports_editable_source_without_retrying_a_dead_worker() {
        let (mut editor, _root) = editor(&"paragraph café\n\n".repeat(5000));
        let (mut cache, input, _output) = controlled();
        drop(input);
        let raw = gather(&mut cache, &mut editor);
        assert_eq!(raw.ready, Some(true));
        assert!(raw.current_projection);
        assert!(raw.error.contains("parser stopped"));
        assert!(raw.text.starts_with("paragraph café"));
        assert!(!cache.parsing_pending(editor.active_window().id.0, editor.active_buffer().0));
        assert!(!cache.poll(&editor));
        assert!(cache.parsing.is_empty());
    }
}
