//! Bounded image reads through the document's own filesystem authority.
//! Neither local nor SSH I/O runs on the editor thread. Only parsed image
//! annotations can be requested; the client never supplies a filesystem path.

use super::wire::{Field, Reader, WireError};
use crate::model::filesystem::FileSystem;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

pub(crate) struct Target {
    pub scope: super::compose::Scope,
    pub fs: Arc<dyn FileSystem + Send + Sync>,
    pub document: PathBuf,
    pub instance: usize,
}

impl Target {
    pub fn current(&self, editor: &crate::app::Editor) -> bool {
        editor
            .windows
            .get(&fresh_core::WindowId(self.scope.window))
            .is_some_and(|window| {
                window.pane_buffer(self.scope.leaf) == Some(self.scope.buffer)
                    && Arc::ptr_eq(&self.fs, &window.authority().filesystem)
                    && window.buffer_state(self.scope.buffer).is_some_and(|state| {
                        state.buffer.version() == self.scope.revision
                            && state.buffer.file_path() == Some(self.document.as_path())
                            && state.marker_list.instance_id() == self.instance
                    })
            })
    }
}

pub(crate) const MAX_BYTES: usize = 16 << 20;
pub(crate) const CHUNK_BYTES: usize = 128 << 10;
const MAX_REQUESTS: usize = 4;

#[derive(Default, Clone)]
pub(crate) struct Conditional {
    pub version: Vec<u8>,
    pub digest: Vec<u8>,
}

pub(crate) struct Request {
    pub id: u64,
    pub scope: Option<super::compose::Scope>,
    pub start: usize,
    pub end: usize,
    pub cancel: bool,
    pub authority: u64,
    pub document: u64,
    pub conditional: Conditional,
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
            document: 0,
            conditional: Conditional::default(),
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
                (7, v) => request.document = v.uint(),
                (tag @ (8 | 9), Field::Bytes(bytes)) => {
                    if !bytes.is_empty() && bytes.len() != 32 {
                        return Err(WireError::InvalidField("image validator length"));
                    }
                    if tag == 8 {
                        request.conditional.version = bytes.to_vec();
                    } else {
                        request.conditional.digest = bytes.to_vec();
                    }
                }
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
    pub version: Vec<u8>,
    pub digest: Vec<u8>,
    pub unchanged: bool,
    pub retryable: bool,
}

pub(crate) struct Transfer {
    pub id: u64,
    bytes: Vec<u8>,
    offset: usize,
    error: String,
    version: Vec<u8>,
    digest: Vec<u8>,
    unchanged: bool,
    retryable: bool,
}

impl Transfer {
    pub fn failure(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            bytes: Vec::new(),
            offset: 0,
            error: error.into(),
            version: Vec::new(),
            digest: Vec::new(),
            unchanged: false,
            retryable: false,
        }
    }

    pub fn retryable_failure(id: u64, error: impl Into<String>) -> Self {
        Self {
            retryable: true,
            ..Self::failure(id, error)
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
            version: if end == self.bytes.len() {
                self.version.clone()
            } else {
                Vec::new()
            },
            digest: if end == self.bytes.len() {
                self.digest.clone()
            } else {
                Vec::new()
            },
            unchanged: self.unchanged,
            retryable: self.retryable,
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
    conditional: Conditional,
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
                    let transfer =
                        match load(job.fs.as_ref(), &job.path, &job.cancel, &job.conditional) {
                            Ok(loaded) => Transfer {
                                id: job.key.1,
                                bytes: loaded.bytes,
                                offset: 0,
                                error: String::new(),
                                version: loaded.version,
                                digest: loaded.digest,
                                unchanged: loaded.unchanged,
                                retryable: false,
                            },
                            Err(error) => {
                                if error.retryable {
                                    Transfer::retryable_failure(job.key.1, error.message)
                                } else {
                                    Transfer::failure(job.key.1, error.message)
                                }
                            }
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
        conditional: Conditional,
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
                conditional,
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

#[derive(Debug)]
struct Loaded {
    bytes: Vec<u8>,
    version: Vec<u8>,
    digest: Vec<u8>,
    unchanged: bool,
}

#[derive(Debug)]
struct Failure {
    message: String,
    retryable: bool,
}

impl Failure {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
    fn io(error: std::io::Error) -> Self {
        let retryable = !matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::InvalidInput
                | std::io::ErrorKind::InvalidData
                | std::io::ErrorKind::IsADirectory
                | std::io::ErrorKind::NotADirectory
        );
        Self {
            message: format!("Image unavailable: {error}"),
            retryable,
        }
    }
}

fn version(
    fs: &(dyn FileSystem + Send + Sync),
    path: &Path,
    size: u64,
    stamp: &Option<Vec<u8>>,
) -> Vec<u8> {
    let Some(stamp) = stamp else {
        return Vec::new();
    };
    let mut hash = Sha256::new();
    hash.update(b"Fresh Compose image version\0");
    hash.update((fs as *const _ as *const () as usize).to_le_bytes());
    let path = path.as_os_str().as_encoded_bytes();
    hash.update(path.len().to_le_bytes());
    hash.update(path);
    hash.update(size.to_le_bytes());
    hash.update(stamp);
    hash.finalize().to_vec()
}

fn load(
    fs: &(dyn FileSystem + Send + Sync),
    path: &Path,
    cancel: &AtomicBool,
    conditional: &Conditional,
) -> Result<Loaded, Failure> {
    if cancel.load(Ordering::Acquire) {
        return Err(Failure::retryable("Image request cancelled"));
    }
    let (before, before_stamp) = fs.metadata_with_version(path).map_err(Failure::io)?;
    if before.size > MAX_BYTES as u64 {
        return Err(Failure::permanent(
            "Image exceeds the 16 MiB compressed limit",
        ));
    }
    let version = version(fs, path, before.size, &before_stamp);
    if !version.is_empty() && version == conditional.version && conditional.digest.len() == 32 {
        return Ok(Loaded {
            bytes: Vec::new(),
            version,
            digest: conditional.digest.clone(),
            unchanged: true,
        });
    }
    if cancel.load(Ordering::Acquire) {
        return Err(Failure::retryable("Image request cancelled"));
    }
    // Hash on the authority when a stat shortcut cannot establish freshness.
    // RemoteFileSystem does this on the host, so an unchanged image never
    // needs to cross SSH merely to refresh its validator.
    if conditional.digest.len() == 32 {
        let digest = fs
            .content_digest_bounded(path, MAX_BYTES as u64)
            .map_err(Failure::io)?;
        if cancel.load(Ordering::Acquire) {
            return Err(Failure::retryable("Image request cancelled"));
        }
        let (checked, checked_stamp) = fs.metadata_with_version(path).map_err(Failure::io)?;
        if checked.size != before.size
            || checked.modified != before.modified
            || checked_stamp != before_stamp
            || digest.size != before.size
        {
            return Err(Failure::retryable(
                "Image changed while hashing; retry the load",
            ));
        }
        if digest.sha256.as_slice() == conditional.digest {
            return Ok(Loaded {
                bytes: Vec::new(),
                version,
                digest: conditional.digest.clone(),
                unchanged: true,
            });
        }
    }
    // Cap the READ itself at the size already checked above: read_range is an
    // exact read, not a read-until-EOF. A replacement cannot increase this
    // allocation; the metadata/length check below detects a changed resource.
    let bytes = fs
        .read_range(path, 0, before.size as usize)
        .map_err(Failure::io)?;
    if bytes.len() > MAX_BYTES {
        return Err(Failure::permanent(
            "Image exceeds the 16 MiB compressed limit",
        ));
    }
    if cancel.load(Ordering::Acquire) {
        return Err(Failure::retryable("Image request cancelled"));
    }
    let (after, after_stamp) = fs.metadata_with_version(path).map_err(Failure::io)?;
    if before.size != after.size
        || before.modified != after.modified
        || before_stamp != after_stamp
        || bytes.len() as u64 != after.size
    {
        return Err(Failure::retryable(
            "Image changed while reading; retry the load",
        ));
    }
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") && !bytes.starts_with(b"\xff\xd8\xff") {
        return Err(Failure::permanent(
            "Image format is unsupported; use PNG or JPEG",
        ));
    }
    let digest = Sha256::digest(&bytes).to_vec();
    let unchanged = digest == conditional.digest;
    Ok(Loaded {
        bytes: if unchanged { Vec::new() } else { bytes },
        version,
        digest,
        unchanged,
    })
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
            version: vec![1; 32],
            digest: vec![2; 32],
            unchanged: false,
            retryable: false,
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
            service
                .submit(1, id, fs.clone(), path.clone(), Conditional::default())
                .unwrap();
        }
        assert!(service
            .submit(1, 5, fs.clone(), path.clone(), Conditional::default())
            .is_err());
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
            service
                .submit(1, 5, fs.clone(), path.clone(), Conditional::default())
                .is_err(),
            "delivery still holds the memory slot"
        );
        service.cancel(1, 1);
        service
            .submit(1, 5, fs.clone(), path.clone(), Conditional::default())
            .unwrap();
        service.retain_clients(&HashSet::new());
        assert!(service.requests.is_empty());
        let oversized = root.path().join("oversized.png");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_BYTES as u64 + 1)
            .unwrap();
        assert!(load(
            fs.as_ref(),
            &oversized,
            &AtomicBool::new(false),
            &Conditional::default()
        )
        .unwrap_err()
        .message
        .contains("16 MiB"));
        assert!(load(
            fs.as_ref(),
            &path,
            &AtomicBool::new(true),
            &Conditional::default()
        )
        .unwrap_err()
        .message
        .contains("cancelled"));
        std::fs::write(&path, b"not an image").unwrap();
        assert!(load(
            fs.as_ref(),
            &path,
            &AtomicBool::new(false),
            &Conditional::default()
        )
        .unwrap_err()
        .message
        .contains("unsupported"));
    }

    #[test]
    fn conditional_images_skip_unchanged_payloads_and_detect_replacement_with_preserved_mtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("image.png");
        let original = b"\x89PNG\r\n\x1a\nold";
        std::fs::write(&path, original).unwrap();
        let fs = crate::model::filesystem::StdFileSystem;
        let cancel = AtomicBool::new(false);
        let initial = load(&fs, &path, &cancel, &Conditional::default()).unwrap();
        assert_eq!(initial.bytes, original);
        assert!(fs.content_digest_bounded(&path, 2).is_err());
        assert_eq!(initial.digest.len(), 32);
        let conditional = Conditional {
            version: initial.version.clone(),
            digest: initial.digest.clone(),
        };
        let unchanged = load(&fs, &path, &cancel, &conditional).unwrap();
        assert!(unchanged.unchanged && unchanged.bytes.is_empty());
        // Explicit revalidation and filesystems without a reliable stamp use
        // the digest, rather than sending the same compressed payload again.
        let hashed = load(
            &fs,
            &path,
            &cancel,
            &Conditional {
                version: Vec::new(),
                ..conditional.clone()
            },
        )
        .unwrap();
        assert!(hashed.unchanged && hashed.bytes.is_empty());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = root.path().join("replacement.png");
        std::fs::write(&replacement, b"\x89PNG\r\n\x1a\nnew").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let updated = load(&fs, &path, &cancel, &conditional).unwrap();
        assert!(!updated.unchanged);
        assert_ne!(updated.digest, initial.digest);
        assert_eq!(updated.bytes, b"\x89PNG\r\n\x1a\nnew");
        #[cfg(unix)]
        assert_ne!(updated.version, initial.version);
        std::fs::remove_file(&path).unwrap();
        let missing = load(&fs, &path, &cancel, &conditional).unwrap_err();
        assert!(!missing.retryable && missing.message.contains("unavailable"));
    }

    #[test]
    fn remote_agent_supplies_change_stamps_and_hashes_conditional_images_on_its_authority() {
        use crate::services::remote::{spawn_local_agent, RemoteFileSystem};
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let channel = runtime
            .block_on(spawn_local_agent())
            .expect("start the production Python agent");
        let fs = RemoteFileSystem::new(channel, "compose-local-agent-fixture".into());
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("asset.png");
        let bytes = b"\x89PNG\r\n\x1a\nold";
        std::fs::write(&path, bytes).unwrap();
        let (_, stamp) = fs.metadata_with_version(&path).unwrap();
        #[cfg(unix)]
        assert_eq!(stamp.as_ref().unwrap().len(), 48);
        let cancel = AtomicBool::new(false);
        let initial = load(&fs, &path, &cancel, &Conditional::default()).unwrap();
        let conditional = Conditional {
            version: initial.version.clone(),
            digest: initial.digest.clone(),
        };
        let unchanged = load(&fs, &path, &cancel, &conditional).unwrap();
        assert!(unchanged.unchanged && unchanged.bytes.is_empty());
        let verified = load(
            &fs,
            &path,
            &cancel,
            &Conditional {
                version: Vec::new(),
                ..conditional.clone()
            },
        )
        .unwrap();
        assert!(verified.unchanged && verified.bytes.is_empty());
        assert_eq!(
            fs.content_digest_bounded(&path, bytes.len() as u64)
                .unwrap()
                .sha256
                .as_slice(),
            initial.digest
        );
        assert!(
            fs.content_digest_bounded(&path, 1).is_err(),
            "the agent must bound even a growing digest read"
        );
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let replacement = root.path().join("new.png");
        std::fs::write(&replacement, b"\x89PNG\r\n\x1a\nnew").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        std::fs::rename(replacement, &path).unwrap();
        let replaced = load(&fs, &path, &cancel, &conditional).unwrap();
        assert!(!replaced.unchanged);
        assert_eq!(replaced.bytes, b"\x89PNG\r\n\x1a\nnew");
        assert_ne!(replaced.digest, initial.digest);
        // A stat after the rejected bounded hash proves the protocol remains
        // usable: the error cannot poison the shared remote channel.
        assert_eq!(fs.metadata(&path).unwrap().size, bytes.len() as u64);
    }
}
