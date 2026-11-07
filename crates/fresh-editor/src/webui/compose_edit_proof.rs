//! Compact source anchors for an accepted explicit native replacement batch.
//! Preparation records rebased ranges; publication also verifies actual bytes
//! and actual genuine cursor positions after the editor applies the batch.

use crate::app::Editor;
use crate::model::event::CursorId;
use sha2::{Digest, Sha256};
use std::ops::Range;

#[derive(Clone, Debug)]
pub(crate) struct Anchor {
    pub cursor: usize,
    pub start: usize,
    pub end: usize,
    pub digest: Vec<u8>,
    pub before_start: usize,
    pub before_end: usize,
}

#[derive(Debug)]
pub(crate) struct Proof {
    pub revision: u64,
    pub length: usize,
    pub anchors: Vec<Anchor>,
    pub result_revision: u64,
    pub result_length: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{buffer::LineEnding, cursor::Cursor};

    fn fixture(source: &str) -> (Editor, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default();
        config.clipboard.use_osc52 = false;
        config.clipboard.use_system_clipboard = false;
        let mut editor = Editor::for_test(config, 80, 24, Some(root.path().into()),
            crate::config_io::DirectoryContext::for_testing(root.path()),
            crate::view::color_support::ColorCapability::TrueColor,
            std::sync::Arc::new(crate::model::filesystem::StdFileSystem), None, None, false, false, false).unwrap();
        editor.insert_native_text(source.into());
        (editor, root)
    }

    #[test]
    fn large_normalized_insertion_proves_actual_source_without_returning_inserted_text() {
        let (mut editor, _root) = fixture("tail");
        editor.active_state_mut().buffer.set_line_ending(LineEnding::CRLF);
        let id = editor.active_cursors().primary_id();
        let text = "café 👩🏽‍💻 日本\n".repeat(24_000);
        let normalized = text.replace('\n', "\r\n");
        let targets = vec![(0..0, text, id)];
        let proof = Proof::prepare(&editor, &targets).unwrap();
        let revision = proof.revision;
        editor.replace_native_texts(targets).unwrap();
        let proof = proof.verified(&editor).unwrap();
        assert_eq!(proof.revision, revision);
        assert_eq!(proof.length, 4);
        assert_eq!(proof.anchors.len(), 1);
        let anchor = &proof.anchors[0];
        assert_eq!((anchor.start, anchor.end), (0, normalized.len()));
        assert_eq!(anchor.digest, Sha256::digest(normalized.as_bytes()).to_vec());
        assert!(anchor.end > 64 << 10);
        assert_eq!(anchor.digest.len(), 32);
    }

    #[test]
    fn disjoint_unicode_replacements_and_deletion_prove_the_final_shifted_ranges() {
        let (mut editor, _root) = fixture("alpha café omega");
        let first = editor.active_cursors().primary_id();
        let second = editor.active_cursors_mut().add(Cursor::new(6));
        let third = editor.active_cursors_mut().add(Cursor::new(12));
        let targets = vec![(12..17, "".into(), third), (0..5, "日本".into(), first), (6..11, "é".into(), second)];
        let proof = Proof::prepare(&editor, &targets).unwrap();
        editor.replace_native_texts(targets).unwrap();
        let proof = proof.verified(&editor).unwrap();
        assert_eq!(proof.anchors.iter().map(|a| (a.before_start, a.before_end, a.start, a.end)).collect::<Vec<_>>(),
            vec![(0,5,0,6), (6,11,7,9), (12,17,10,10)]);
        assert_eq!(proof.anchors[2].digest, Sha256::digest(b"").to_vec());
        assert_eq!(editor.active_state().buffer.slice_bytes(0..10), "日本 é ".as_bytes());
    }

    #[test]
    fn moved_caret_or_different_accepted_bytes_cannot_certify_an_expected_insertion() {
        for wrong_bytes in [false, true] {
            let (mut editor, _root) = fixture("tail");
            let id = editor.active_cursors().primary_id();
            let targets = vec![(0..0, "abc".into(), id)];
            let proof = Proof::prepare(&editor, &targets).unwrap();
            if wrong_bytes { editor.replace_native_texts(vec![(0..0, "xyz".into(), id)]).unwrap(); }
            else {
                editor.replace_native_texts(targets).unwrap();
                *editor.active_cursors_mut().primary_mut() = Cursor::new(2);
            }
            assert!(proof.verified(&editor).is_none());
        }
    }

    #[test]
    fn empty_replacement_and_cr_disk_policy_have_verified_in_memory_coordinates() {
        for ending in [LineEnding::LF, LineEnding::CR] {
            let (mut editor, _root) = fixture("tail");
            editor.active_state_mut().buffer.set_line_ending(ending);
            let id = editor.active_cursors().primary_id();
            for text in ["", "A\r\nB\rC\n"] {
                *editor.active_cursors_mut().primary_mut() = Cursor::new(0);
                let targets = vec![(0..0, text.into(), id)];
                let proof = Proof::prepare(&editor, &targets).unwrap();
                editor.replace_native_texts(targets).unwrap();
                let proof = proof.verified(&editor).unwrap();
                let expected = text.replace("\r\n", "\n").replace('\r', "\n");
                assert_eq!(proof.anchors[0].end, expected.len());
                assert_eq!(proof.anchors[0].digest, Sha256::digest(expected.as_bytes()).to_vec());
            }
        }
    }
}

impl Proof {
    pub(super) fn prepare(editor: &Editor, targets: &[(Range<usize>, String, CursorId)]) -> Option<Self> {
        if targets.is_empty() || targets.len() > 1024 { return None; }
        let state = editor.active_state();
        let mut sorted: Vec<_> = targets.iter().collect();
        sorted.sort_by_key(|(range, _, _)| (range.start, range.end));
        let mut anchors = Vec::with_capacity(sorted.len());
        let mut before = 0usize;
        let mut after = 0usize;
        for (range, text, id) in sorted {
            // Match the native text policy, then verify the actual accepted
            // bytes below. A future policy change withholds proof, not text.
            let text = text.replace("\r\n", "\n").replace('\r', "\n")
                .replace('\n', state.buffer.line_ending().insertion_str());
            let start = after.checked_add(range.start.checked_sub(before)?)?;
            let end = start.checked_add(text.len())?;
            anchors.push(Anchor { cursor: id.0, start, end, digest: Sha256::digest(text.as_bytes()).to_vec(),
                before_start: range.start, before_end: range.end });
            before = range.end;
            after = end;
        }
        Some(Self { revision: state.buffer.version(), length: state.buffer.len(), anchors,
            result_revision: state.buffer.version(), result_length: state.buffer.len() })
    }

    pub(super) fn verified(mut self, editor: &Editor) -> Option<Self> {
        let state = editor.active_state();
        if state.buffer.version() < self.revision { return None; }
        let last = self.anchors.last()?;
        let length = last.end.checked_add(self.length.checked_sub(last.before_end)?)?;
        if state.buffer.len() != length { return None; }
        for anchor in &self.anchors {
            let cursor = editor.active_cursors().get(CursorId(anchor.cursor))?;
            if cursor.position != anchor.end || cursor.anchor.is_some_and(|a| a != cursor.position) { return None; }
            let mut digest = Sha256::new();
            // Hash the accepted buffer with bounded temporary reads. Neither
            // the receipt nor the verifier copies a complete large insertion.
            for start in (anchor.start..anchor.end).step_by(64 << 10) {
                let end = anchor.end.min(start.saturating_add(64 << 10));
                let bytes = state.buffer.slice_bytes(start..end);
                if bytes.len() != end-start { return None; }
                digest.update(&bytes);
            }
            if digest.finalize().as_slice() != anchor.digest { return None; }
        }
        self.result_revision = state.buffer.version();
        self.result_length = state.buffer.len();
        Some(self)
    }
}
