//! Capacity weights for cache retention, not a process-memory measurement.
//! Shared Arc payloads are charged once per document. Allocator rounding and
//! unreported container bookkeeping are outside this soft retention target.

use std::{
    collections::HashSet,
    mem::{size_of, size_of_val},
    sync::Arc,
};

#[derive(Default)]
pub(in crate::webui) struct Heap {
    bytes: usize,
    shared: HashSet<usize>,
}

impl Heap {
    pub(in crate::webui) fn add(&mut self, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
    }

    pub(in crate::webui) fn vector<T>(&mut self, value: &Vec<T>) {
        self.add(value.capacity().saturating_mul(size_of::<T>()));
    }

    pub(in crate::webui) fn string(&mut self, value: &String) {
        self.add(value.capacity());
    }

    /// Charge the Arc header and inline value. The caller traverses nested
    /// allocations only on the first visit, including for shared URI strings.
    pub(in crate::webui) fn arc<T: ?Sized>(&mut self, value: &Arc<T>) -> bool {
        if !self.shared.insert(Arc::as_ptr(value) as *const () as usize) {
            return false;
        }
        self.add((2 * size_of::<usize>()).saturating_add(size_of_val(&**value)));
        true
    }

    pub(in crate::webui) fn bytes(&self) -> usize {
        self.bytes
    }
}

use super::Document;

impl Document {
    /// Immutable parser output is weighed once, on the parser worker for
    /// large documents. Frame gathering never rescans its samples or labels.
    pub(super) fn measure_metadata(&self) -> usize {
        let mut heap = Heap::default();
        heap.add(size_of::<Self>());
        // Their mutable capacities are read separately by retained_bytes().
        heap.arc(&self.text);
        heap.arc(&self.text_index);
        if heap.arc(&self.semantics) {
            heap.vector(&self.semantics.entries);
            for entry in &self.semantics.entries {
                heap.string(&entry.label);
            }
            if let Some(error) = &self.semantics.error {
                heap.string(error);
            }
        }
        heap.vector(&self.spans);
        for span in &self.spans {
            for target in [&span.link_target, &span.link_target_preview]
                .into_iter()
                .flatten()
            {
                heap.arc(target);
            }
        }
        heap.vector(&self.images);
        for image in &self.images {
            heap.string(&image.target);
            heap.string(&image.alt);
            heap.string(&image.title);
        }
        heap.vector(&self.paragraphs);
        heap.vector(&self.tight_paragraphs);
        heap.add(
            self.item_paragraphs
                .capacity()
                .saturating_mul(size_of::<(usize, usize)>()),
        );
        heap.vector(&self.underlines);
        heap.vector(&self.heading_openers);
        heap.vector(&self.code);
        for block in &self.code {
            heap.vector(&block.content);
        }
        self.tables.account_retention(&mut heap);
        heap.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_equal_link_targets_are_charged_by_allocation_identity() {
        let target = "target.md";
        let mut text = format!("[shared]: <{target}>\n\n");
        for row in 0..1000 {
            text.push_str(&format!("[entry {row}][shared] "));
        }
        let mut document = Document::parse(text, 1, 1, 1);
        let links: Vec<_> = document
            .spans
            .iter()
            .filter_map(|span| span.link_target.as_ref())
            .collect();
        assert_eq!(links.len(), 1000);
        assert!(links.windows(2).all(|pair| Arc::ptr_eq(pair[0], pair[1])));
        let count = links.len();
        let shared = document.metadata_bytes;
        for span in &mut document.spans {
            if let Some(target) = &span.link_target {
                span.link_target = Some(Arc::from(target.as_ref()));
            }
        }
        document.metadata_bytes = document.measure_metadata();
        assert_eq!(
            document.metadata_bytes - shared,
            (count - 1) * (target.len() + 2 * size_of::<usize>()),
            "equal bytes in separate allocations must count; clones of one allocation count once"
        );
    }

    #[test]
    fn pending_generation_counts_retained_formatter_records_and_releases_them() {
        use crate::view::conceal::ResolvedConceal;
        let mut document = Document::pending("text\n".into(), 2, 1, 1);
        let empty = document.retained_bytes();
        let mut snapshot = super::super::super::compose_decorations::Snapshot::new(1, Vec::new());
        let mut replacement = String::with_capacity(2 << 20);
        replacement.push('t');
        snapshot.capture(
            1,
            0..5,
            vec![0..5],
            5,
            vec![ResolvedConceal {
                source: 0..1,
                replacement: Some(replacement),
                activation: None,
            }],
        );
        document.previous_decorations = Some(snapshot);
        assert!(document.retained_bytes() >= empty + (2 << 20));
        document.previous_decorations = None;
        assert_eq!(document.retained_bytes(), empty);
    }
}
