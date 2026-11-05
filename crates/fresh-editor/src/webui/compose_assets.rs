//! Bounded image reads through the document's own filesystem authority.
//! Neither local nor SSH I/O runs on the editor thread. Only parsed image
//! annotations can be requested; the client never supplies a filesystem path.

use super::wire::{Field, Reader, WireError};
use crate::model::filesystem::FileSystem;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

pub(crate) struct Target {
    pub scope: super::compose::Scope,
    pub fs: Arc<dyn FileSystem + Send + Sync>,
}

impl Target {
    pub fn current(&self, editor: &crate::app::Editor) -> bool {
        editor
            .windows
            .get(&fresh_core::WindowId(self.scope.window))
            .is_some_and(|window| {
                window.pane_buffer(self.scope.leaf) == Some(self.scope.buffer)
                    && Arc::ptr_eq(&self.fs, &window.authority().filesystem)
                    && window
                        .buffer_state(self.scope.buffer)
                        .is_some_and(|state| state.buffer.version() == self.scope.revision)
            })
    }
}

pub(crate) const MAX_BYTES: usize = 16 << 20;
pub(crate) const CHUNK_BYTES: usize = 128 << 10;
const MAX_REQUESTS: usize = 4;

pub(crate) struct Request {
    pub id: u64,
    pub scope: Option<super::compose::Scope>,
    pub start: usize,
    pub end: usize,
    pub cancel: bool,
    pub authority: u64,
}

impl Request {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireError> {
        let mut request = Self {
            id: 0,
            scope: None,
            start: 0,
            end: 0,
            cancel: false,
            authority: 0,
        };
        for field in Reader::new(bytes) {
            match field? {
                (1, v) => request.id = v.uint(),
                (2, Field::Bytes(bytes)) => {
                    request.scope = Some(super::compose::Scope::parse(bytes)?)
                }
                (3, v) => request.start = v.uint() as usize,
                (4, v) => request.end = v.uint() as usize,
                (5, v) => request.cancel = v.flag(),
                (6, v) => request.authority = v.uint(),
                _ => {}
            }
        }
        Ok(request)
    }
}

/// One frame carries at most one bounded chunk. `done` also terminates errors
/// and empty results, so an unsuccessful load cannot strand an assembler.
pub(crate) struct Chunk {
    pub id: u64,
    pub offset: usize,
    pub total: usize,
    pub data: Vec<u8>,
    pub error: String,
    pub done: bool,
}

pub(crate) struct Transfer {
    pub id: u64,
    bytes: Vec<u8>,
    offset: usize,
    error: String,
}

impl Transfer {
    pub fn failure(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            bytes: Vec::new(),
            offset: 0,
            error: error.into(),
        }
    }

    pub fn next(&mut self) -> Chunk {
        let end = self.bytes.len().min(self.offset + CHUNK_BYTES);
        let chunk = Chunk {
            id: self.id,
            offset: self.offset,
            total: self.bytes.len(),
            data: self.bytes[self.offset..end].to_vec(),
            error: std::mem::take(&mut self.error),
            done: end == self.bytes.len(),
        };
        self.offset = end;
        chunk
    }
}

struct Job {
    key: (u64, u64),
    fs: Arc<dyn FileSystem + Send + Sync>,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
}

struct Workers {
    jobs: mpsc::SyncSender<Job>,
    results: mpsc::Receiver<((u64, u64), Transfer)>,
}

impl Workers {
    fn new() -> Result<Self, String> {
        let (jobs, receive) = mpsc::sync_channel::<Job>(8);
        let receive = Arc::new(Mutex::new(receive));
        let (complete, results) = mpsc::sync_channel(4);
        for index in 0..2 {
            let receive = receive.clone();
            let complete = complete.clone();
            std::thread::Builder::new()
                .name(format!("compose-image-{index}"))
                .spawn(move || loop {
                    let job = { receive.lock().unwrap().recv() };
                    let Ok(job) = job else {
                        break;
                    };
                    if job.cancel.load(Ordering::Acquire) {
                        continue;
                    }
                    let transfer = match load(job.fs.as_ref(), &job.path, &job.cancel) {
                        Ok(bytes) => Transfer {
                            id: job.key.1,
                            bytes,
                            offset: 0,
                            error: String::new(),
                        },
                        Err(error) => Transfer::failure(job.key.1, error),
                    };
                    if !job.cancel.load(Ordering::Acquire)
                        && complete.send((job.key, transfer)).is_err()
                    {
                        break;
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        Ok(Self { jobs, results })
    }
}

/// Four outstanding results globally, including transfers awaiting a slow
/// client's frame acknowledgments. Cancellation frees their buffers; the
/// fixed worker/queue counts still bound remote reads that cannot be aborted.
#[derive(Default)]
pub(crate) struct Service {
    workers: Option<Workers>,
    requests: HashMap<(u64, u64), Arc<AtomicBool>>,
}

impl Service {
    pub fn submit(
        &mut self,
        client: u64,
        id: u64,
        fs: Arc<dyn FileSystem + Send + Sync>,
        path: PathBuf,
    ) -> Result<(), String> {
        let key = (client, id);
        if self.requests.contains_key(&key) {
            return Err("image request id is already active".into());
        }
        if self.requests.len() >= MAX_REQUESTS {
            return Err("image loading is busy; retry shortly".into());
        }
        if self.workers.is_none() {
            self.workers = Some(Workers::new()?);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.workers
            .as_ref()
            .unwrap()
            .jobs
            .try_send(Job {
                key,
                fs,
                path,
                cancel: cancel.clone(),
            })
            .map_err(|_| "image loading queue is busy; retry shortly".to_string())?;
        self.requests.insert(key, cancel);
        Ok(())
    }

    pub fn cancel(&mut self, client: u64, id: u64) {
        if let Some(cancel) = self.requests.remove(&(client, id)) {
            cancel.store(true, Ordering::Release);
        }
    }

    pub fn retain_clients(&mut self, clients: &HashSet<u64>) {
        self.requests.retain(|(client, _), cancel| {
            let live = clients.contains(client);
            if !live {
                cancel.store(true, Ordering::Release);
            }
            live
        });
    }

    pub fn completed(&mut self) -> Vec<(u64, Transfer)> {
        let mut completed = Vec::new();
        if let Some(workers) = &self.workers {
            while let Ok((key, transfer)) = workers.results.try_recv() {
                if self.requests.contains_key(&key) {
                    completed.push((key.0, transfer));
                }
            }
        }
        completed
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        for cancel in self.requests.values() {
            cancel.store(true, Ordering::Release);
        }
    }
}

fn load(
    fs: &(dyn FileSystem + Send + Sync),
    path: &Path,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, String> {
    let before = fs
        .metadata(path)
        .map_err(|e| format!("Image unavailable: {e}"))?;
    if before.size > MAX_BYTES as u64 {
        return Err("Image exceeds the 16 MiB compressed limit".into());
    }
    if cancel.load(Ordering::Acquire) {
        return Err("Image request cancelled".into());
    }
    // Cap the READ itself at the size already checked above: read_range is an
    // exact read, not a read-until-EOF. A replacement cannot increase this
    // allocation; the metadata/length check below detects a changed resource.
    let bytes = fs
        .read_range(path, 0, before.size as usize)
        .map_err(|e| format!("Image unavailable: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err("Image exceeds the 16 MiB compressed limit".into());
    }
    if cancel.load(Ordering::Acquire) {
        return Err("Image request cancelled".into());
    }
    let after = fs
        .metadata(path)
        .map_err(|e| format!("Image unavailable: {e}"))?;
    if before.size != after.size
        || before.modified != after.modified
        || bytes.len() as u64 != after.size
    {
        return Err("Image changed while reading; retry the load".into());
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") && !bytes.starts_with(b"\xff\xd8\xff") {
        return Err("Image format is unsupported; use PNG or JPEG".into());
    }
    Ok(bytes)
}

/// Markdown destinations are URIs; decode their path without treating `+` as
/// a space. Reads stay on the document authority, including absolute paths.
pub(crate) fn resolve(document: &Path, target: &str) -> Result<PathBuf, String> {
    let path = target.split('#').next().unwrap_or("");
    if path.is_empty()
        || path.starts_with("//")
        || path.split('/').next().is_some_and(|s| s.contains(':'))
    {
        return Err("Only filesystem images on the document's authority load automatically".into());
    }
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let a = bytes.next().and_then(|c| (c as char).to_digit(16));
            let b = bytes.next().and_then(|c| (c as char).to_digit(16));
            match (a, b) {
                (Some(a), Some(b)) => decoded.push((a * 16 + b) as u8),
                _ => return Err("Image destination has an invalid percent escape".into()),
            }
        } else {
            decoded.push(byte);
        }
    }
    if decoded.contains(&0) {
        return Err("Image destination contains a NUL".into());
    }
    let path =
        PathBuf::from(String::from_utf8(decoded).map_err(|_| "Image destination is not UTF-8")?);
    Ok(if path.is_absolute() {
        path
    } else {
        document.parent().unwrap_or(Path::new(".")).join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destinations_resolve_on_the_document_authority_without_network_fetching() {
        let document = Path::new("/remote/docs/readme.md");
        assert_eq!(
            resolve(document, "../images/a%20b+c.png#x").unwrap(),
            Path::new("/remote/docs/../images/a b+c.png")
        );
        assert_eq!(
            resolve(document, "/tmp/picture.jpg").unwrap(),
            Path::new("/tmp/picture.jpg")
        );
        for target in [
            "https://example.test/x.png",
            "//example.test/x.png",
            "file:///tmp/x.png",
            "data:image/png,x",
            "a%00.png",
            "a%2.png",
        ] {
            assert!(resolve(document, target).is_err(), "{target}");
        }
    }

    #[test]
    fn binary_chunks_are_bounded_and_reconstruct_exactly() {
        let bytes: Vec<_> = (0..CHUNK_BYTES * 3 + 17).map(|i| i as u8).collect();
        let mut transfer = Transfer {
            id: 7,
            bytes: bytes.clone(),
            offset: 0,
            error: String::new(),
        };
        let mut received = Vec::new();
        loop {
            let chunk = transfer.next();
            assert_eq!(chunk.offset, received.len());
            assert!(chunk.data.len() <= CHUNK_BYTES);
            received.extend(chunk.data);
            if chunk.done {
                break;
            }
        }
        assert_eq!(received, bytes);
        let failed = Transfer::failure(8, "missing").next();
        assert!(failed.done && failed.data.is_empty() && failed.error == "missing");
    }

    #[test]
    fn filesystem_reads_and_outstanding_results_are_bounded() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset.png");
        let bytes = [b"\x89PNG\r\n\x1a\n".as_slice(), &[0, 128, 255, 0, 10]].concat();
        std::fs::write(&path, &bytes).unwrap();
        let fs = Arc::new(crate::model::filesystem::StdFileSystem);
        let mut service = Service::default();
        for id in 1..=4 {
            service.submit(1, id, fs.clone(), path.clone()).unwrap();
        }
        assert!(service.submit(1, 5, fs.clone(), path.clone()).is_err());
        let mut received = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while received.len() < 4 && std::time::Instant::now() < deadline {
            received.extend(service.completed());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(received.len(), 4);
        for (_, mut transfer) in received {
            let chunk = transfer.next();
            assert!(chunk.error.is_empty(), "{}", chunk.error);
            assert_eq!(chunk.data, bytes);
        }
        assert!(
            service.submit(1, 5, fs.clone(), path.clone()).is_err(),
            "delivery still holds the memory slot"
        );
        service.cancel(1, 1);
        service.submit(1, 5, fs.clone(), path.clone()).unwrap();
        service.retain_clients(&HashSet::new());
        assert!(service.requests.is_empty());
        let oversized = root.path().join("oversized.png");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
        assert!(load(fs.as_ref(), &oversized, &AtomicBool::new(false))
            .unwrap_err()
            .contains("16 MiB"));
        assert!(load(fs.as_ref(), &path, &AtomicBool::new(true))
            .unwrap_err()
            .contains("cancelled"));
        std::fs::write(&path, b"not an image").unwrap();
        assert!(load(fs.as_ref(), &path, &AtomicBool::new(false))
            .unwrap_err()
            .contains("unsupported"));
    }
}
