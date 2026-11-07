//! Binary splices over canonical protobuf field groups. Changes to a small
//! header cannot force the unchanged text field onto the wire. Text edits use
//! a common byte prefix/suffix; no Markdown or UTF-8 parsing occurs here.
//! Literal ranges refer to the new body, so a delta retains no third body.

use super::super::wire::{field, put_varint, read_varint};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

const MAX_FIELDS: usize = 64;

enum Part {
    Header(Vec<u8>),
    Literal(Range<usize>),
}
pub(super) struct Recipe {
    parts: Vec<Part>,
    pub length: usize,
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let (value, count) = read_varint(bytes.get(*at..)?).ok()??;
    *at = at.checked_add(count)?;
    Some(value)
}

fn groups(bytes: &[u8]) -> Option<BTreeMap<u32, Range<usize>>> {
    let mut result: BTreeMap<u32, Range<usize>> = BTreeMap::new();
    let mut at = 0;
    let mut last = 0;
    while at < bytes.len() {
        let start = at;
        let key = varint(bytes, &mut at)?;
        let tag = u32::try_from(key >> 3).ok()?;
        if tag == 0 || tag < last {
            return None;
        }
        last = tag;
        match key & 7 {
            0 => {
                varint(bytes, &mut at)?;
            }
            1 => {
                at = at.checked_add(8)?;
            }
            2 => {
                let length = usize::try_from(varint(bytes, &mut at)?).ok()?;
                at = at.checked_add(length)?;
            }
            5 => {
                at = at.checked_add(4)?;
            }
            _ => return None,
        }
        if at > bytes.len() {
            return None;
        }
        result
            .entry(tag)
            .and_modify(|range| range.end = at)
            .or_insert(start..at);
        if result.len() > MAX_FIELDS {
            return None;
        }
    }
    Some(result)
}

impl Recipe {
    /// Unsupported/noncanonical inputs or a delta no smaller than the complete
    /// body use an ordinary full read. All copies refer to exact binary bytes.
    pub fn new(old: &[u8], new: &[u8]) -> Option<Self> {
        let before = groups(old)?;
        let after = groups(new)?;
        let tags: BTreeSet<_> = before.keys().chain(after.keys()).copied().collect();
        if tags.len() > MAX_FIELDS {
            return None;
        }
        let mut parts = Vec::new();
        let mut length = 0;
        for tag in tags {
            let a = before.get(&tag).cloned().unwrap_or_else(|| {
                let at = before
                    .range(tag..)
                    .next()
                    .map_or(old.len(), |(_, r)| r.start);
                at..at
            });
            let b = after.get(&tag).cloned().unwrap_or(new.len()..new.len());
            let left = &old[a.clone()];
            let right = &new[b.clone()];
            if left == right {
                continue;
            }
            let prefix = left.iter().zip(right).take_while(|(a, b)| a == b).count();
            let suffix = left[prefix..]
                .iter()
                .rev()
                .zip(right[prefix..].iter().rev())
                .take_while(|(a, b)| a == b)
                .count();
            let insert = (b.start + prefix)..(b.end - suffix);
            let mut edit = Vec::new();
            field::uint(&mut edit, 1, (a.start + prefix) as u64);
            field::uint(&mut edit, 2, (a.len() - prefix - suffix) as u64);
            if !insert.is_empty() {
                put_varint(&mut edit, (3 << 3) | 2);
                put_varint(&mut edit, insert.len() as u64);
            }
            let mut header = Vec::new();
            put_varint(&mut header, (1 << 3) | 2);
            put_varint(&mut header, (edit.len() + insert.len()) as u64);
            header.extend(edit);
            length += header.len() + insert.len();
            parts.push(Part::Header(header));
            if !insert.is_empty() {
                parts.push(Part::Literal(insert));
            }
        }
        let mut tail = Vec::new();
        field::uint(&mut tail, 2, old.len() as u64);
        length += tail.len();
        parts.push(Part::Header(tail));
        (length < new.len()).then_some(Self { parts, length })
    }

    pub fn copy(&self, body: &[u8], range: Range<usize>) -> Vec<u8> {
        let mut out = Vec::with_capacity(range.len());
        let mut at = 0;
        for part in &self.parts {
            let bytes = match part {
                Part::Header(bytes) => bytes,
                Part::Literal(range) => &body[range.clone()],
            };
            let start = range.start.saturating_sub(at).min(bytes.len());
            let end = range.end.saturating_sub(at).min(bytes.len());
            if start < end {
                out.extend_from_slice(&bytes[start..end]);
            }
            at += bytes.len();
            if at >= range.end {
                break;
            }
        }
        debug_assert_eq!(out.len(), range.len());
        out
    }
}

#[cfg(test)]
#[path = "../../../tests/scene_proto/fresh.scene.v1.rs"]
#[allow(dead_code, clippy::all)]
mod pb;

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn apply(old: &[u8], delta: &[u8]) -> Vec<u8> {
        let delta = pb::ComposeDocumentDelta::decode(delta).unwrap();
        assert_eq!(delta.base_length, old.len() as u64);
        let mut result = Vec::new();
        let mut at = 0;
        for edit in delta.edits {
            let start = edit.offset as usize;
            assert!(start >= at && start + edit.remove as usize <= old.len());
            result.extend_from_slice(&old[at..start]);
            result.extend(edit.insert);
            at = start + edit.remove as usize;
        }
        result.extend_from_slice(&old[at..]);
        result
    }

    #[test]
    fn headers_and_local_unicode_edits_do_not_resend_the_unchanged_text_field() {
        let mut old = pb::ComposePane {
            text: "café 👩🏽‍💻\r\n".repeat(80_000),
            buffer_revision: 127,
            selection_revision: 1,
            cursors: vec![pb::ComposeSelection {
                primary: true,
                id: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        for mode in 0..4 {
            let before = old.encode_to_vec();
            old.selection_revision += 1;
            old.cursors[0].position += 1;
            if mode == 1 {
                old.text.insert_str(5, "東京");
                old.buffer_revision += 1;
            }
            if mode == 2 {
                old.editable = Some(false);
            }
            if mode == 3 {
                old.cursors.push(pb::ComposeSelection {
                    id: 2,
                    position: 10,
                    ..Default::default()
                });
            }
            let after = old.encode_to_vec();
            let recipe = Recipe::new(&before, &after).unwrap();
            let bytes = recipe.copy(&after, 0..recipe.length);
            assert_eq!(apply(&before, &bytes), after);
            assert!(bytes.len() < 256, "{}", bytes.len());
            for step in [1, 7, 65536] {
                let joined: Vec<_> = (0..recipe.length)
                    .step_by(step)
                    .flat_map(|start| recipe.copy(&after, start..(start + step).min(recipe.length)))
                    .collect();
                assert_eq!(joined, bytes);
            }
        }
    }

    #[test]
    fn removed_and_inserted_field_groups_keep_canonical_order_and_identity_is_small() {
        let old = pb::ComposePane {
            text: "long constant prose ".repeat(100),
            buffer_revision: 1,
            focused: true,
            editable: Some(true),
            ..Default::default()
        }
        .encode_to_vec();
        let new = pb::ComposePane {
            text: "long constant prose ".repeat(100),
            selection_revision: 5,
            keyboard: true,
            decoration_revision: Some(0),
            ..Default::default()
        }
        .encode_to_vec();
        for (a, b) in [(&old, &new), (&new, &old), (&new, &new)] {
            let recipe = Recipe::new(a, b).unwrap();
            let bytes = recipe.copy(b, 0..recipe.length);
            assert_eq!(apply(a, &bytes), *b);
            assert!(bytes.len() < 100);
        }
        assert!(Recipe::new(&[10, 200], &new).is_none());
        assert!(Recipe::new(&[16, 1, 8, 1], &new).is_none());
        assert!(
            Recipe::new(&[], &[8, 1]).is_none(),
            "a larger recipe uses full delivery"
        );
    }
}
