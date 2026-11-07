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
    /// Cache pressure must not make visible panes or workspaces continually
    /// evict and reparse each other. Hidden tabs remain ordinary LRU victims.
    pub(super) fn visible_documents(editor: &Editor) -> HashSet<(u64, usize)> {
        let mut visible = HashSet::new();
        for (id, window) in &editor.windows {
            if let Some((manager, views)) = window.buffers.splits() {
                for (leaf, buffer) in manager.visible_leaves() {
                    if views.get(&leaf).and_then(|view| view.buffer_state(buffer))
                        .is_some_and(|view| view.view_mode == ViewMode::PageView)
                    {
                        visible.insert((id.0, buffer.0));
                    }
                }
            }
        }
        visible
    }

    /// Bound cached snapshot retention by capacity as well as entry count.
    /// Visible Compose working sources and the requested document are never
    /// discarded solely to fit the budget. Parser ownership is separate.
    pub(super) fn trim_source_cache(&mut self, current: Option<(u64, usize)>, incoming: usize,
                                   visible: &HashSet<(u64, usize)>) {
        loop {
            let bytes = self.documents.values().fold(incoming, |bytes, doc| {
                bytes.saturating_add(doc.snapshot_bytes())
            });
            let entries =
                self.documents.len() + usize::from(current.is_some_and(|key| !self.documents.contains_key(&key)));
            if entries <= CACHE_LIMIT && bytes <= SOURCE_CACHE_BYTES {
                return;
            }
            let Some(oldest) = self
                .documents
                .iter()
                .filter(|(key, _)| Some(**key) != current && !visible.contains(*key))
                .min_by_key(|(_, doc)| doc.used)
                .map(|(key, _)| *key)
            else {
                return;
            };
            self.documents.remove(&oldest);
            self.cancel_buffer_parse(oldest.0, oldest.1);
        }
    }

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

    /// Cached source and parsed metadata belong to a live buffer lifetime and
    /// filesystem authority. Entry-count eviction alone must not keep a closed
    /// workspace alive until another composing document happens to open.
    pub(super) fn prune(&mut self, editor: &Editor) {
        self.authorities.retain(|id, (previous, _)| {
            editor
                .windows
                .get(&fresh_core::WindowId(*id))
                .is_some_and(|window| {
                    previous
                        .upgrade()
                        .is_some_and(|fs| Arc::ptr_eq(&fs, &window.authority().filesystem))
                })
        });
        let authorities = &self.authorities;
        self.documents.retain(|(window, buffer), doc| {
            authorities.contains_key(window)
                && editor
                    .windows
                    .get(&fresh_core::WindowId(*window))
                    .and_then(|window| window.buffer_state(BufferId(*buffer)))
                    .is_some_and(|state| state.marker_list.instance_id() == doc.epoch)
        });
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
        let visible = Self::visible_documents(editor);
        self.trim_source_cache(None, 0, &visible);
    }

    /// A completed parse never borrows a newer mutable source. Polling also
    /// releases cancelled slots and starts the newest still-wanted snapshot.
    pub fn poll(&mut self, editor: &Editor) -> bool {
        self.prune(editor);
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
        self.parse_wanted.clear();
        // Cancellation is cooperative. Keep the worker and occupied slots
        // until their completions are consumed; reconnects share that same
        // bounded queue. A quiescent worker (including a failed startup) can
        // be released and started again on demand.
        if self.parsing.is_empty() {
            self.parser = None;
        }
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
            .gather_tables_at(editor, leaf, buffer, Some(0), true, false)
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

    fn retain_capacity(cache: &mut ComposeCache, editor: &Editor, capacity: usize) -> Weak<String> {
        let key = (editor.active_window().id.0, editor.active_buffer().0);
        let text = &mut cache.documents.get_mut(&key).unwrap().text;
        let value = Arc::get_mut(text).expect("a small parsed snapshot has no parser owner");
        value.reserve_exact(capacity.saturating_sub(value.len()));
        Arc::downgrade(text)
    }

    #[test]
    fn snapshot_byte_pressure_evicts_the_oldest_entry_below_the_count_limit() {
        let (mut editor, _root) = editor("# Current café\n");
        let mut cache = ComposeCache::default();
        let window = editor.active_window().id.0;
        let mut buffers = Vec::new();
        let mut sources = Vec::new();
        for index in 0..3 {
            if index != 0 {
                editor.new_buffer();
                editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
            }
            gather(&mut cache, &mut editor);
            buffers.push(editor.active_buffer());
            sources.push(retain_capacity(&mut cache, &editor, 20 << 20));
        }
        editor.switch_buffer(buffers[0]);
        gather(&mut cache, &mut editor); // Touch the first entry: the second is now oldest.
        let current = editor.new_buffer();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        gather(&mut cache, &mut editor);
        let live = retain_capacity(&mut cache, &editor, 20 << 20);
        gather(&mut cache, &mut editor);
        assert!(cache.documents.len() < 4, "entry-count-only retention exceeds the byte budget");
        assert!(!cache.documents.contains_key(&(window, buffers[1].0)));
        assert!(sources[1].upgrade().is_none());
        assert!(sources[0].upgrade().is_some());
        assert!(sources[2].upgrade().is_some());
        assert!(cache.documents.contains_key(&(window, current.0)));
        assert!(live.upgrade().is_some());
        assert!(cache.documents.values().map(Document::snapshot_bytes).sum::<usize>() <= SOURCE_CACHE_BYTES);
    }

    #[test]
    fn one_oversized_current_snapshot_keeps_exact_source_and_evicts_other_retention() {
        let (mut editor, _root) = editor("# Earlier\n");
        let mut cache = ComposeCache::default();
        gather(&mut cache, &mut editor);
        let old = retain_capacity(&mut cache, &editor, 20 << 20);
        let current = editor.new_buffer();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        editor.active_state_mut().buffer.set_line_ending(crate::model::buffer::LineEnding::CRLF);
        let cursor = editor.active_cursors().primary_id();
        let source = "# Current café e\u{301} 👩🏽‍💻\r\n";
        editor.replace_native_texts(vec![(0..0, source.to_owned(), cursor)]).unwrap();
        editor.active_cursors_mut().primary_mut().position = 0;
        gather(&mut cache, &mut editor);
        let live = retain_capacity(&mut cache, &editor, SOURCE_CACHE_BYTES + (1 << 20));
        let pane = gather(&mut cache, &mut editor);
        let key = (editor.active_window().id.0, current.0);
        assert_eq!(cache.documents.len(), 1);
        assert!(cache.documents[&key].snapshot_bytes() > SOURCE_CACHE_BYTES);
        assert_eq!(cache.documents[&key].text.as_str(), source);
        assert_eq!(pane.document_len, source.len());
        assert!(pane.text.contains("Current café"));
        assert!(old.upgrade().is_none());
        assert!(live.upgrade().is_some());
    }

    #[test]
    fn snapshot_budget_includes_sparse_indices_at_the_source_capacity_boundary() {
        let (mut editor, _root) = editor("# First\n");
        let mut cache = ComposeCache::default();
        gather(&mut cache, &mut editor);
        let old = retain_capacity(&mut cache, &editor, SOURCE_CACHE_BYTES / 2);
        editor.new_buffer();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        gather(&mut cache, &mut editor);
        retain_capacity(&mut cache, &editor, SOURCE_CACHE_BYTES / 2);
        assert_eq!(cache.documents.values().map(|doc| doc.text.capacity()).sum::<usize>(), SOURCE_CACHE_BYTES);
        gather(&mut cache, &mut editor);
        assert_eq!(cache.documents.len(), 1, "the indices also consume retained heap capacity");
        assert!(old.upgrade().is_none());
    }

    #[test]
    fn visible_workspaces_keep_their_working_snapshots_under_cache_pressure() {
        let (mut editor, root) = editor("# First workspace\n");
        let mut cache = ComposeCache::default();
        let first_window = editor.active_window().id;
        let first_buffer = editor.active_buffer();
        gather(&mut cache, &mut editor);
        let first_source = retain_capacity(&mut cache, &editor, 40 << 20);
        let other = root.path().join("second");
        std::fs::create_dir(&other).unwrap();
        let second_window = editor.create_window_at(other, "Second workspace".into());
        editor.set_active_window(second_window);
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        gather(&mut cache, &mut editor);
        let second_source = retain_capacity(&mut cache, &editor, 40 << 20);
        for _ in 0..3 {
            gather(&mut cache, &mut editor);
            assert!(first_source.upgrade().is_some(), "visible documents must not evict each other");
            editor.set_active_window(first_window);
            gather(&mut cache, &mut editor);
            assert!(second_source.upgrade().is_some());
            assert_eq!(Arc::as_ptr(&cache.documents[&(first_window.0, first_buffer.0)].text), first_source.as_ptr());
            editor.set_active_window(second_window);
        }
        // After leaving Compose, that old snapshot is an ordinary LRU victim.
        editor.windows.get_mut(&first_window).unwrap().focused_view_mut().view_mode = ViewMode::Source;
        assert!(!cache.poll(&editor), "retire over-budget hidden retention without another gather");
        assert!(first_source.upgrade().is_none());
        assert!(second_source.upgrade().is_some());
        assert_eq!(cache.documents.len(), 1);
    }

    #[test]
    fn byte_eviction_cancels_parser_but_keeps_its_real_slot_until_completion() {
        let (mut editor, _root) = editor("# Source\n");
        let (mut cache, input, output) = controlled();
        let window = editor.active_window().id.0;
        let mut keys = Vec::new();
        for index in 0..3 {
            if index != 0 {
                editor.new_buffer();
                editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
            }
            gather(&mut cache, &mut editor);
            retain_capacity(&mut cache, &editor, 20 << 20);
            keys.push((window, editor.active_buffer().0));
        }
        // Submit the immutable middle snapshot to the controlled real queue.
        cache.documents.get_mut(&keys[1]).unwrap().parsed = false;
        cache.request_parse(keys[1].0, keys[1].1);
        cache.schedule();
        let job = input.recv().unwrap();
        let source = Arc::downgrade(&job.text);
        let cancel = job.cancel.clone();
        editor.switch_buffer(BufferId(keys[0].1));
        gather(&mut cache, &mut editor); // Middle snapshot becomes oldest.
        editor.new_buffer();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        gather(&mut cache, &mut editor);
        retain_capacity(&mut cache, &editor, 20 << 20);
        gather(&mut cache, &mut editor);
        assert!(!cache.documents.contains_key(&keys[1]));
        assert!(cancel.load(Ordering::Acquire));
        assert!(cache.parse_wanted.is_empty());
        assert_eq!(cache.parsing.len(), 1, "eviction cannot create an extra parser slot");
        assert!(source.upgrade().is_some(), "the running job still owns its snapshot");
        complete(job, &output);
        assert!(!cache.poll(&editor), "an evicted late result cannot restore the source");
        assert!(cache.parsing.is_empty());
        assert!(source.upgrade().is_none());
        assert!(input.try_recv().is_err());
    }

    #[test]
    fn closed_buffers_release_cached_source_and_keep_live_tabs_cached() {
        let (mut editor, _root) = editor("# Old heading\n\n![old](missing.png)\n");
        let mut cache = ComposeCache::default();
        gather(&mut cache, &mut editor);
        let old = editor.active_buffer();
        let window = editor.active_window().id.0;
        let document = &cache.documents[&(window, old.0)];
        let source = Arc::downgrade(&document.text);
        let semantics = Arc::downgrade(&document.semantics);

        let live = editor.new_buffer();
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        gather(&mut cache, &mut editor);
        let retained = Arc::downgrade(&cache.documents[&(window, live.0)].text);
        editor.force_close_buffer(old).unwrap();
        assert!(editor.active_window().buffer_state(old).is_none());
        assert!(!cache.poll(&editor));
        assert!(!cache.documents.contains_key(&(window, old.0)));
        assert!(source.upgrade().is_none());
        assert!(semantics.upgrade().is_none());
        assert!(retained.upgrade().is_some());
        let before = retained.as_ptr();
        gather(&mut cache, &mut editor);
        assert_eq!(
            Arc::as_ptr(&cache.documents[&(window, live.0)].text),
            before
        );
    }

    #[test]
    fn closed_windows_release_their_semantic_cache_and_authority() {
        let (mut editor, root) = editor("# Old workspace\n");
        let mut cache = ComposeCache::default();
        gather(&mut cache, &mut editor);
        let old = editor.active_window().id;
        let buffer = editor.active_buffer();
        let source = Arc::downgrade(&cache.documents[&(old.0, buffer.0)].text);
        editor.active_state_mut().buffer.set_modified(false);
        let directory = root.path().join("other-workspace");
        std::fs::create_dir(&directory).unwrap();
        let live = editor.create_window_at(directory, "Other workspace".into());
        editor.set_active_window(live);
        editor.active_window_mut().focused_view_mut().view_mode = ViewMode::PageView;
        gather(&mut cache, &mut editor);
        assert!(editor.close_window(old));
        assert!(!cache.poll(&editor));
        assert!(source.upgrade().is_none());
        assert!(!cache.authorities.contains_key(&old.0));
        assert!(cache.authorities.contains_key(&live.0));
        assert!(cache.documents.keys().all(|(window, _)| *window == live.0));
    }

    #[test]
    fn closed_pending_parse_is_cancelled_and_cannot_restore_its_snapshot() {
        let (mut editor, _root) = editor(&"Paragraph café.\n\n".repeat(6000));
        let (mut cache, input, output) = controlled();
        gather(&mut cache, &mut editor);
        let job = input
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let key = job.key;
        let source = Arc::downgrade(&job.text);
        let cancellation = job.cancel.clone();
        let old = editor.active_buffer();
        editor.new_buffer();
        editor.force_close_buffer(old).unwrap();
        assert!(!cache.poll(&editor));
        assert!(cancellation.load(Ordering::Acquire));
        assert!(!cache.documents.contains_key(&(key.window, key.buffer)));
        assert!(cache.parse_wanted.is_empty());
        assert_eq!(
            cache.parsing.len(),
            1,
            "a running slot remains owned until its completion"
        );
        complete(job, &output);
        assert!(!cache.poll(&editor));
        assert!(cache.parsing.is_empty());
        assert!(cache.documents.is_empty());
        assert!(source.upgrade().is_none());
        assert!(input.try_recv().is_err());
    }

    #[test]
    fn clear_reconnect_keeps_running_and_waiting_work_owned_until_completion() {
        let (mut editor, _root) = editor(&"# Anchor\n\nParagraph café.\n\n".repeat(6000));
        let (mut cache, input, output) = controlled();
        gather(&mut cache, &mut editor);
        let running = input.recv().unwrap();
        let running_source = Arc::downgrade(&running.text);
        let running_cancel = running.cancel.clone();
        cache.clear();
        gather(&mut cache, &mut editor);
        for turn in 0..12 {
            let cursor = editor.active_cursors().primary_id();
            editor
                .replace_native_texts(vec![(0..0, format!("{turn} "), cursor)])
                .unwrap();
            cache.clear();
            let raw = gather(&mut cache, &mut editor);
            assert_eq!(raw.ready, Some(false));
            assert_eq!(
                cache.parsing.len(),
                2,
                "resets must keep both occupied slots"
            );
            assert_eq!(
                cache.parse_wanted.len(),
                1,
                "only the newest request is wanted"
            );
        }
        let waiting = input
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let waiting_source = Arc::downgrade(&waiting.text);
        assert!(running_cancel.load(Ordering::Acquire));
        assert!(waiting.cancel.load(Ordering::Acquire));
        assert!(
            input.try_recv().is_err(),
            "no additional job can bypass occupied slots"
        );
        complete(running, &output);
        assert!(
            !cache.poll(&editor),
            "a late cancelled result cannot publish"
        );
        assert!(running_source.upgrade().is_none());
        let newest = input
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(newest.key.revision, editor.active_state().buffer.version());
        assert!(newest.text.starts_with("11 10 9 "));
        assert!(input.try_recv().is_err());
        complete(waiting, &output);
        assert!(!cache.poll(&editor));
        assert!(waiting_source.upgrade().is_none());
        assert_eq!(cache.parsing.len(), 1);
        complete(newest, &output);
        assert!(cache.poll(&editor));
        assert!(cache.parsing.is_empty());
        let rich = gather(&mut cache, &mut editor);
        assert_eq!(rich.ready, Some(true));
        assert!(rich.text.starts_with("11 10 9 "));
    }

    #[test]
    fn clear_without_reconnect_drains_results_before_releasing_the_worker() {
        let (mut editor, _root) = editor(&"Paragraph café.\n\n".repeat(6000));
        let (mut cache, input, output) = controlled();
        gather(&mut cache, &mut editor);
        let job = input.recv().unwrap();
        let source = Arc::downgrade(&job.text);
        let cancel = job.cancel.clone();
        for _ in 0..12 {
            cache.clear();
            assert!(!cache.poll(&editor));
        }
        assert!(cancel.load(Ordering::Acquire));
        complete(job, &output);
        assert!(!cache.poll(&editor));
        assert!(source.upgrade().is_none());
        assert!(cache.documents.is_empty());
        assert!(cache.parsing.is_empty());
        cache.clear();
        assert!(matches!(
            input.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn dropping_cache_cancels_work_and_closes_the_parser_channels() {
        let (mut editor, _root) = editor(&"Paragraph café.\n\n".repeat(6000));
        let (mut cache, input, output) = controlled();
        gather(&mut cache, &mut editor);
        let job = input.recv().unwrap();
        let source = Arc::downgrade(&job.text);
        let cancel = job.cancel.clone();
        drop(cache);
        assert!(cancel.load(Ordering::Acquire));
        assert!(matches!(
            input.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        let completion = Completion {
            key: job.key,
            text: job.text.clone(),
            result: Ok(None),
        };
        drop(job);
        assert!(output.send(completion).is_err());
        assert!(source.upgrade().is_none());
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
        cache.clear();
        assert_eq!(gather(&mut cache, &mut editor).ready, Some(false));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !cache.poll(&editor) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(gather(&mut cache, &mut editor).ready, Some(true));
        assert!(cache.documents.values().all(|document| document.parsed));
    }
}
