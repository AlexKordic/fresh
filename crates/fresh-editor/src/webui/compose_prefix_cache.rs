//! Document-owned, allocation-bounded semantic samples. This is not a source,
//! projection or shaping cache: the native consumer still uses current fonts.

use super::Context;
use fresh_core::compose::Conceal;
use std::{mem::size_of, ops::Range};

const MAX_ENTRIES: usize = 16;
const MAX_BYTES: usize = 1024 * 1024;
type OwnedConceal = (Range<usize>, Option<String>);

struct Entry {
    source: Range<usize>,
    conceals: Vec<OwnedConceal>,
    context: Context,
    heap_bytes: usize,
}

#[derive(Default)]
pub(in crate::webui::compose) struct Cache {
    entries: Vec<Entry>, // oldest first; never more than MAX_ENTRIES
    heap_bytes: usize,
    #[cfg(test)]
    pub(super) hits: usize,
}

impl Cache {
    pub(super) fn get(
        &mut self,
        source: &Range<usize>,
        item: usize,
        content: usize,
        conceals: &[Conceal<'_>],
    ) -> Option<Context> {
        let at = self.entries.iter().position(|entry| {
            entry.source == *source
                && entry.context.item_start == item
                && entry.context.content_start == content
                && entry.conceals.len() == conceals.len()
                && entry
                    .conceals
                    .iter()
                    .zip(conceals)
                    .all(|((range, text), c)| {
                        *range == c.source && text.as_deref() == c.replacement
                    })
        })?;
        let entry = self.entries.remove(at);
        let context = entry.context.clone();
        self.entries.push(entry);
        #[cfg(test)]
        {
            self.hits += 1;
        }
        Some(context)
    }

    pub(super) fn insert(
        &mut self,
        source: Range<usize>,
        conceals: &[Conceal<'_>],
        context: &Context,
    ) {
        // Preflight borrowed lengths before copying a potentially huge plugin
        // replacement. Over-budget inputs render normally, without retention.
        let estimate = conceals
            .iter()
            .fold(
                conceals.len().saturating_mul(size_of::<OwnedConceal>()),
                |bytes, c| bytes.saturating_add(c.replacement.map_or(0, str::len)),
            )
            .saturating_add(context.text.len())
            .saturating_add(
                context
                    .spans
                    .len()
                    .saturating_mul(size_of::<super::ComposeSpan>()),
            )
            .saturating_add(MAX_ENTRIES.saturating_mul(size_of::<Entry>()));
        if estimate > MAX_BYTES {
            return;
        }
        let conceals: Vec<_> = conceals
            .iter()
            .map(|c| (c.source.clone(), c.replacement.map(str::to_owned)))
            .collect();
        let context = context.clone();
        let heap_bytes = conceals
            .iter()
            .fold(
                conceals
                    .capacity()
                    .saturating_mul(size_of::<OwnedConceal>()),
                |bytes, (_, text)| bytes.saturating_add(text.as_ref().map_or(0, String::capacity)),
            )
            .saturating_add(context.text.capacity())
            .saturating_add(
                context
                    .spans
                    .capacity()
                    .saturating_mul(size_of::<super::ComposeSpan>()),
            );
        if self.entries.capacity() == 0 {
            let entries = Vec::with_capacity(MAX_ENTRIES);
            if heap_bytes.saturating_add(entries.capacity().saturating_mul(size_of::<Entry>()))
                > MAX_BYTES
            {
                return;
            }
            self.entries = entries;
        }
        let container_bytes = self.entries.capacity().saturating_mul(size_of::<Entry>());
        if heap_bytes.saturating_add(container_bytes) > MAX_BYTES {
            return;
        }
        while !self.entries.is_empty()
            && (self.entries.len() == MAX_ENTRIES
                || self
                    .heap_bytes
                    .saturating_add(heap_bytes)
                    .saturating_add(container_bytes)
                    > MAX_BYTES)
        {
            self.heap_bytes -= self.entries.remove(0).heap_bytes;
        }
        self.heap_bytes += heap_bytes;
        self.entries.push(Entry {
            source,
            conceals,
            context,
            heap_bytes,
        });
    }

    #[cfg(test)]
    pub(super) fn allocated_bytes(&self) -> usize {
        self.heap_bytes + self.entries.capacity() * size_of::<Entry>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(item: usize) -> Context {
        Context {
            item_start: item,
            content_start: item + 3,
            text: "1) O".into(),
            prefix_end: 3,
            spans: Vec::new(),
            source_line_start: true,
        }
    }

    #[test]
    fn lru_evicts_old_entries_and_compares_exact_replacements_and_anchors() {
        let mut cache = Cache::default();
        for item in 0..MAX_ENTRIES {
            cache.insert(0..100, &[], &context(item));
        }
        assert!(cache.get(&(0..100), 0, 3, &[]).is_some());
        cache.insert(0..100, &[], &context(MAX_ENTRIES));
        assert!(cache.get(&(0..100), 1, 4, &[]).is_none());
        assert!(cache.get(&(0..100), 0, 3, &[]).is_some());
        assert!(cache.get(&(0..99), 0, 3, &[]).is_none());
        assert!(cache.get(&(0..100), 0, 4, &[]).is_none());
        let input = [Conceal {
            source: 0..2,
            replacement: Some("é"),
        }];
        cache.insert(0..100, &input, &context(80));
        let changed = [Conceal {
            source: 0..2,
            replacement: Some("e"),
        }];
        assert!(cache.get(&(0..100), 80, 83, &changed).is_none());
        assert!(cache.get(&(0..100), 80, 83, &input).is_some());
        assert!(cache.allocated_bytes() <= MAX_BYTES);
    }

    #[test]
    fn byte_budget_evicts_and_oversized_inputs_bypass_retention() {
        let mut cache = Cache::default();
        let medium = "x".repeat(MAX_BYTES / 3);
        let input = [Conceal {
            source: 0..1,
            replacement: Some(&medium),
        }];
        for item in 0..MAX_ENTRIES * 2 {
            cache.insert(0..100, &input, &context(item));
            assert!(cache.allocated_bytes() <= MAX_BYTES);
        }
        assert!(cache.entries.len() < MAX_ENTRIES);
        assert!(cache.get(&(0..100), 0, 3, &input).is_none());
        let before = cache.allocated_bytes();
        let huge = "x".repeat(MAX_BYTES);
        let input = [Conceal {
            source: 0..1,
            replacement: Some(&huge),
        }];
        cache.insert(0..100, &input, &context(100));
        let mut sample = context(101);
        sample.text = huge;
        cache.insert(0..100, &[], &sample);
        assert_eq!(cache.allocated_bytes(), before);
        assert!(cache.get(&(0..100), 100, 103, &[]).is_none());
        assert!(cache.get(&(0..100), 101, 104, &[]).is_none());
    }
}
