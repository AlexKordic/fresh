//! Reuse validated source bytes only across a complete, lifetime-owned edit
//! history. Immutable snapshots held by parser/stream workers stay unchanged.

use crate::model::{buffer::Buffer, coord_map::CoordMap};
use anyhow::Result;
use std::sync::Arc;

pub(super) fn capture(
    buffer: &mut Buffer,
    changes: &CoordMap,
    previous: Option<(u64, Arc<String>)>,
) -> Result<String> {
    let length = buffer.len();
    if let Some((revision, previous)) = previous {
        if let Some((prefix, suffix)) = changes
            .unchanged_edges(revision, buffer.version(), previous.len(), length)
            .filter(|&(prefix, suffix)| {
                // A source-sized replacement should release obsolete owned
                // storage before the ordinary full read, rather than retain
                // it for a patch that saves little or no validation work.
                prefix + suffix >= length / 2
                    && previous.is_char_boundary(prefix)
                    && previous.is_char_boundary(previous.len() - suffix)
            })
        {
            let patch =
                String::from_utf8(buffer.get_text_range_mut(prefix, length - prefix - suffix)?)?;
            // A large deletion/undo should release an obsolete large capacity.
            // Otherwise uniquely owned storage can be updated without copying
            // it first. reserve_exact avoids amortized doubling on each growth.
            if previous.capacity() <= length.saturating_mul(2).max(64 << 10) {
                match Arc::try_unwrap(previous) {
                    Ok(mut text) => {
                        if length > text.len() {
                            text.reserve_exact(length - text.len());
                        }
                        text.replace_range(prefix..text.len() - suffix, &patch);
                        return Ok(text);
                    }
                    Err(previous) => return Ok(join(&previous, &patch, prefix, suffix, length)),
                }
            }
            return Ok(join(&previous, &patch, prefix, suffix, length));
        }
    }
    Ok(String::from_utf8(buffer.get_text_range_mut(0, length)?)?)
}

fn join(previous: &str, patch: &str, prefix: usize, suffix: usize, length: usize) -> String {
    let mut text = String::with_capacity(length);
    text.push_str(&previous[..prefix]);
    text.push_str(patch);
    text.push_str(&previous[previous.len() - suffix..]);
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::filesystem::StdFileSystem;

    #[test]
    fn captured_sources_match_complete_reads_and_preserve_worker_snapshots() {
        let mut expected = "# Title\r\n\ncafé e\u{301} 👩🏽‍💻 東京 עברית\r\n".repeat(300);
        let mut buffer = Buffer::from_bytes(expected.as_bytes().to_vec(), Arc::new(StdFileSystem));
        let mut changes = CoordMap::default();
        let mut old = (buffer.version(), Arc::new(expected.clone()));
        for step in 0..80 {
            let retained = (step % 2 == 0).then(|| Arc::clone(&old.1));
            let before = expected.clone();
            let mut start = (step * 7919) % (expected.len() + 1);
            while !expected.is_char_boundary(start) {
                start -= 1;
            }
            let end = expected[start..]
                .char_indices()
                .nth(step % 5)
                .map_or(expected.len(), |(byte, _)| start + byte);
            if end > start {
                buffer.delete(start..end);
                changes.record_delete(buffer.version(), start, end - start);
                expected.replace_range(start..end, "");
            }
            let text = ["", "🦀", "\r\n## Heading\r\n", "![alt](image.png)"][step % 4];
            if !text.is_empty() {
                buffer.insert(start, text);
                changes.record_insert(buffer.version(), start, text.len());
                expected.insert_str(start, text);
            }
            let captured = capture(&mut buffer, &changes, Some(old)).unwrap();
            assert_eq!(captured, expected);
            assert_eq!(
                captured.as_bytes(),
                buffer.get_text_range_mut(0, buffer.len()).unwrap()
            );
            if let Some(retained) = retained {
                assert_eq!(*retained, before);
            }
            old = (buffer.version(), Arc::new(captured));
        }
    }

    #[test]
    fn absent_evicted_and_barriered_history_falls_back_to_actual_source() {
        for kind in 0..3 {
            let mut buffer = Buffer::from_bytes(b"original".to_vec(), Arc::new(StdFileSystem));
            let old = (buffer.version(), Arc::new("original".to_owned()));
            let mut changes = CoordMap::new(1);
            buffer.insert(0, "new ");
            if kind != 0 {
                changes.record_insert(buffer.version(), 0, 4);
            }
            buffer.insert(0, "second ");
            if kind == 1 {
                changes.record_insert(buffer.version(), 0, 7);
            }
            if kind == 2 {
                changes.record_barrier(buffer.version());
            }
            assert_eq!(
                capture(&mut buffer, &changes, Some(old)).unwrap(),
                "second new original"
            );
        }
    }

    #[test]
    fn invalid_utf8_and_edits_inside_old_scalars_cannot_bypass_validation() {
        let original = "café";
        let mut buffer = Buffer::from_bytes(original.as_bytes().to_vec(), Arc::new(StdFileSystem));
        let old = (buffer.version(), Arc::new(original.to_owned()));
        let mut changes = CoordMap::default();
        buffer.insert_bytes(4, vec![0xff]);
        changes.record_insert(buffer.version(), 4, 1);
        assert!(capture(&mut buffer, &changes, Some(old)).is_err());

        let mut buffer = Buffer::from_bytes(b"plain".to_vec(), Arc::new(StdFileSystem));
        let old = (buffer.version(), Arc::new("plain".to_owned()));
        buffer.insert_bytes(2, vec![0xff]);
        let mut changes = CoordMap::default();
        changes.record_insert(buffer.version(), 2, 1);
        assert!(capture(&mut buffer, &changes, Some(old)).is_err());

        // A raw edit inside a former scalar can also produce valid UTF-8.
        // Its unproven old edges must take the complete-validation fallback.
        let mut buffer = Buffer::from_bytes(original.as_bytes().to_vec(), Arc::new(StdFileSystem));
        let old = (buffer.version(), Arc::new(original.to_owned()));
        let mut changes = CoordMap::default();
        buffer.delete_bytes(4, 1);
        changes.record_delete(buffer.version(), 4, 1);
        buffer.insert_bytes(4, vec![0xb1]);
        changes.record_insert(buffer.version(), 4, 1);
        assert_eq!(capture(&mut buffer, &changes, Some(old)).unwrap(), "cafñ");
    }

    #[test]
    fn deleting_a_large_source_releases_its_excess_capacity() {
        let original = "a".repeat(1 << 20);
        let mut buffer = Buffer::from_bytes(original.as_bytes().to_vec(), Arc::new(StdFileSystem));
        let old = (buffer.version(), Arc::new(original));
        buffer.delete(10..buffer.len());
        let mut changes = CoordMap::default();
        changes.record_delete(buffer.version(), 10, (1 << 20) - 10);
        let captured = capture(&mut buffer, &changes, Some(old)).unwrap();
        assert_eq!(captured, "a".repeat(10));
        assert!(captured.capacity() <= 64 << 10);
    }

    #[test]
    fn opaque_snapshot_restore_reads_the_restored_source() {
        let original = "# Original\r\n\ncafé 👩🏽‍💻\r\n";
        let mut buffer = Buffer::from_bytes(original.as_bytes().to_vec(), Arc::new(StdFileSystem));
        let snapshot = buffer.snapshot_buffer_state();
        buffer.insert(0, "changed ");
        let previous = (buffer.version(), Arc::new(format!("changed {original}")));
        buffer.restore_buffer_state(&snapshot);
        let mut changes = CoordMap::default();
        changes.record_barrier(buffer.version());
        assert_eq!(
            capture(&mut buffer, &changes, Some(previous)).unwrap(),
            original
        );
    }

    #[test]
    fn a_complete_replacement_uses_the_actual_new_source() {
        let original = "x".repeat(1 << 20);
        let replacement = "# Replacement\r\n\ncafé 👩🏽‍💻 東京\r\n".repeat(20_000);
        let mut buffer = Buffer::from_bytes(original.as_bytes().to_vec(), Arc::new(StdFileSystem));
        let previous = (buffer.version(), Arc::new(original.clone()));
        buffer.apply_bulk_edits(&[(0, original.len(), &replacement)]);
        let mut changes = CoordMap::default();
        changes.record_replace(buffer.version(), 0, original.len(), replacement.len());
        let captured = capture(&mut buffer, &changes, Some(previous)).unwrap();
        assert!(captured == replacement);
    }
}
