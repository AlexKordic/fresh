//! Document-wide navigation names from the existing Markdown parser. Labels
//! retain all parsed text for filtering; transport, not source, is chunked.

use pulldown_cmark::{Event, Tag, TagEnd};
use std::borrow::Cow;
use std::ops::Range;

pub(crate) const MAX_ENTRIES: usize = 262_144;
pub(crate) const MAX_LABEL_BYTES: usize = 32 << 20;

/// The same parsed text names images and navigation entries. Markup, target
/// URLs and HTML are never reparsed or activated by a graphical consumer.
pub(super) fn plain_text<'a>(event: &'a Event<'_>) -> Option<Cow<'a, str>> {
    match event {
        Event::Text(value)
        | Event::Code(value)
        | Event::InlineMath(value)
        | Event::DisplayMath(value) => Some(Cow::Borrowed(value)),
        Event::SoftBreak | Event::HardBreak => Some(Cow::Borrowed(" ")),
        Event::FootnoteReference(value) => Some(Cow::Owned(format!("[{value}]"))),
        _ => None,
    }
}

#[derive(Debug)]
pub(crate) struct Entry {
    pub start: usize,
    pub end: usize,
    /// Heading levels 1–6, link 16, image 17.
    pub kind: u32,
    pub label: String,
}

#[derive(Default, Debug)]
pub(crate) struct Inventory {
    pub entries: Vec<Entry>,
    pub label_bytes: usize,
    /// No prefix of an over-budget inventory may be called complete.
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct Collector {
    inventory: Inventory,
    open: Vec<usize>,
    image_depth: usize,
}

impl Collector {
    fn fail(&mut self) {
        self.inventory.entries.clear();
        self.inventory.label_bytes = 0;
        self.inventory.error = Some("Document navigation exceeds its metadata budget; the complete Markdown remains editable.".into());
        self.open.clear();
    }

    fn start(&mut self, range: &Range<usize>, kind: u32) {
        if self.inventory.entries.len() == MAX_ENTRIES {
            self.fail();
            return;
        }
        self.open.push(self.inventory.entries.len());
        self.inventory.entries.push(Entry {
            start: range.start,
            end: range.end,
            kind,
            label: String::new(),
        });
    }

    fn end(&mut self, kind: u32) {
        if let Some(at) = self
            .open
            .iter()
            .rposition(|index| self.inventory.entries[*index].kind == kind)
        {
            self.open.remove(at);
        }
    }

    fn text(&mut self, value: &str) {
        let Some(bytes) = value.len().checked_mul(self.open.len()) else {
            self.fail();
            return;
        };
        if bytes > MAX_LABEL_BYTES - self.inventory.label_bytes {
            self.fail();
            return;
        }
        for &index in &self.open {
            self.inventory.entries[index].label.push_str(value);
        }
        self.inventory.label_bytes += bytes;
    }

    pub(super) fn event(&mut self, event: &Event<'_>, range: &Range<usize>) {
        if self.inventory.error.is_some() {
            return;
        }
        if !self.open.is_empty() {
            if let Some(value) = plain_text(event) {
                self.text(&value);
                return;
            }
        }
        match event {
            Event::Start(Tag::Heading { level, .. }) => self.start(range, *level as u32),
            Event::End(TagEnd::Heading(level)) => self.end(*level as u32),
            Event::Start(Tag::Link { .. }) if self.image_depth == 0 => self.start(range, 16),
            Event::End(TagEnd::Link) if self.image_depth == 0 => self.end(16),
            Event::Start(Tag::Image { .. }) => {
                if self.image_depth == 0 {
                    self.start(range, 17);
                }
                self.image_depth += 1;
            }
            Event::End(TagEnd::Image) => {
                self.image_depth = self.image_depth.saturating_sub(1);
                if self.image_depth == 0 {
                    self.end(17);
                }
            }
            _ => {}
        }
    }

    pub(super) fn finish(mut self) -> Inventory {
        if self.inventory.error.is_none() {
            for entry in &mut self.inventory.entries {
                if entry.label.is_empty() {
                    let fallback = match entry.kind {
                        16 => "Link",
                        17 => "Image",
                        _ => "Heading",
                    };
                    entry.label.push_str(fallback);
                    self.inventory.label_bytes += fallback.len();
                }
            }
            if self.inventory.label_bytes > MAX_LABEL_BYTES {
                self.fail();
            }
        }
        self.inventory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    fn collect(text: &str) -> Inventory {
        let mut collector = Collector::default();
        for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
            collector.event(&event, &range);
        }
        collector.finish()
    }

    #[test]
    fn names_cover_the_complete_document_and_use_parsed_text_without_destinations() {
        let source = "# **Café** [東京][ref] ![an *image*](pic.png)\r\n\r\n".to_owned()
            + &"plain text without navigation\r\n\r\n".repeat(8000)
            + "Last `code` heading\r\n===================\r\n\r\n[final *link*](escaped/a\\(b\\).md)\r\n\r\n[ref]: <a b.md>\r\n";
        let inventory = collect(&source);
        assert!(inventory.error.is_none());
        assert_eq!(
            inventory
                .entries
                .iter()
                .map(|e| (e.kind, e.label.as_str()))
                .collect::<Vec<_>>(),
            [
                (1, "Café 東京 an image"),
                (16, "東京"),
                (17, "an image"),
                (1, "Last code heading"),
                (16, "final link")
            ]
        );
        assert!(inventory.entries.last().unwrap().start > 64 << 10);
        assert_eq!(
            inventory.label_bytes,
            inventory
                .entries
                .iter()
                .map(|e| e.label.len())
                .sum::<usize>()
        );
        for entry in &inventory.entries {
            assert!(source.is_char_boundary(entry.start) && source.is_char_boundary(entry.end));
            assert!(entry.start < entry.end && entry.end <= source.len());
        }
    }

    #[test]
    fn nested_image_labels_are_one_image_and_code_or_image_alt_links_are_inert() {
        // A link inside an image's alt text prevents an enclosing link from
        // parsing in CommonMark. Keep the outer-link and alt-link cases
        // separate, and follow the parser rather than inventing navigation.
        let inventory = collect("# [![outer ![inner](inner.png)](outer.png)](visible.md)\n\n![with [alt link](hidden.md)](other.png)\n\n`[code](no.md)`\n\n```md\n# fake\n[also fake](no.md)\n```\n");
        assert!(inventory.error.is_none());
        assert_eq!(
            inventory.entries.iter().map(|e| e.kind).collect::<Vec<_>>(),
            [1, 16, 17, 17]
        );
        assert_eq!(
            inventory
                .entries
                .iter()
                .map(|e| e.label.as_str())
                .collect::<Vec<_>>(),
            ["outer inner", "outer inner", "outer inner", "with alt link"]
        );
    }

    #[test]
    fn label_tails_are_preserved_and_a_large_reference_target_is_not_duplicated() {
        let large = "x".repeat(5 << 20);
        let inventory = collect(&format!(
            "# {large} café 👩🏽‍💻 tail\n\n[short][id]\n\n[id]: https://example.invalid/{large}\n"
        ));
        assert!(inventory.error.is_none());
        assert!(inventory.entries[0].label.ends_with("café 👩🏽‍💻 tail"));
        assert_eq!(inventory.entries[1].label, "short");
        assert_eq!(
            inventory.label_bytes,
            large.len() + " café 👩🏽‍💻 tailshort".len()
        );
    }

    #[test]
    fn metadata_limits_fail_the_whole_inventory_without_a_misleading_prefix() {
        let mut collector = Collector::default();
        collector.start(&(0..2), 1);
        collector.text("first");
        collector.inventory.label_bytes = MAX_LABEL_BYTES;
        collector.text("extra");
        let result = collector.finish();
        assert!(result.error.is_some());
        assert!(result.entries.is_empty());
        assert_eq!(result.label_bytes, 0);
        let mut collector = Collector::default();
        collector.inventory.entries = (0..MAX_ENTRIES)
            .map(|n| Entry {
                start: n,
                end: n + 1,
                kind: 1,
                label: String::new(),
            })
            .collect();
        collector.start(&(MAX_ENTRIES..MAX_ENTRIES + 1), 16);
        let result = collector.finish();
        assert!(result.error.is_some());
        assert!(result.entries.is_empty());
    }
}
