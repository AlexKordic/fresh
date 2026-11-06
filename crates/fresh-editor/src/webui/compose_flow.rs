//! Tight list paragraphs have no Paragraph tags in pulldown-cmark's event
//! stream. Track their direct inline flows using the same parsed block tree;
//! do not infer paragraph boundaries from line lengths or Markdown spelling.

use pulldown_cmark::{Event, TagEnd};
use std::ops::Range;

#[derive(Default)]
pub(super) struct TightFlows {
    blocks: Vec<(TagEnd, usize)>,
    current: Option<(usize, Range<usize>)>,
    complete: Vec<(usize, Range<usize>)>,
}

fn inline(tag: TagEnd) -> bool {
    matches!(
        tag,
        TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Link
            | TagEnd::Image
    )
}

impl TightFlows {
    pub(super) fn event(&mut self, text: &str, event: &Event<'_>, range: &Range<usize>) {
        match event {
            Event::Start(tag) if inline(tag.to_end()) => self.extend(range),
            Event::End(tag) if inline(*tag) => self.extend(range),
            Event::Start(tag) => {
                self.flush(text);
                self.blocks.push((tag.to_end(), range.start));
            }
            Event::End(_) => {
                self.flush(text);
                self.blocks.pop();
            }
            Event::Rule | Event::DisplayMath(_) | Event::Html(_) => self.flush(text),
            Event::Text(_)
            | Event::Code(_)
            | Event::InlineMath(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::SoftBreak
            | Event::HardBreak
            | Event::TaskListMarker(_) => self.extend(range),
        }
    }

    fn extend(&mut self, range: &Range<usize>) {
        let Some(&(TagEnd::Item, owner)) = self.blocks.last() else {
            return;
        };
        if range.is_empty() {
            return;
        }
        let (_, current) = self.current.get_or_insert_with(|| (owner, range.clone()));
        current.start = current.start.min(range.start);
        current.end = current.end.max(range.end);
    }

    fn flush(&mut self, text: &str) {
        let Some((owner, mut range)) = self.current.take() else {
            return;
        };
        // Retain the last physical line's original EOL, including CRLF, without
        // swallowing a blank line or the next nested block after a HardBreak.
        let bytes = text.as_bytes();
        let previous = range.end.checked_sub(1).and_then(|at| bytes.get(at));
        if previous == Some(&b'\r') && bytes.get(range.end) == Some(&b'\n') {
            range.end += 1;
        } else if !matches!(previous, Some(b'\r' | b'\n')) {
            if bytes.get(range.end) == Some(&b'\r') {
                range.end += 1;
            }
            if bytes.get(range.end) == Some(&b'\n') {
                range.end += 1;
            }
        }
        self.complete.push((owner, range));
    }

    pub(super) fn finish(mut self, text: &str) -> Vec<(usize, Range<usize>)> {
        self.flush(text);
        self.complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulldown_cmark::{Options, Parser};

    fn collect(text: &str) -> Vec<(usize, Range<usize>)> {
        let mut flows = TightFlows::default();
        for (event, range) in Parser::new_ext(text, Options::all()).into_offset_iter() {
            flows.event(text, &event, &range);
        }
        flows.finish(text)
    }

    #[test]
    fn direct_inline_flows_keep_joined_lines_and_atomic_inline_syntax() {
        let text = "100) **Parent**\r\n     - [ ] **Child** with [a link](target.md), `code` and ![first\r\n       image](tiny.png)\r\n       continuation café 👩🏽‍💻  \r\n       hard break\r\n     - Next\r\n";
        let flows = collect(text);
        let slices: Vec<_> = flows
            .iter()
            .map(|(_, range)| &text[range.clone()])
            .collect();
        assert_eq!(slices, [
            "**Parent**\r\n",
            "[ ] **Child** with [a link](target.md), `code` and ![first\r\n       image](tiny.png)\r\n       continuation café 👩🏽‍💻  \r\n       hard break\r\n",
            "Next\r\n",
        ]);
        assert_eq!(flows[0].0, 0);
        assert_eq!(flows[1].0, text.find("- [ ]").unwrap());
        assert_eq!(flows[2].0, text.find("- Next").unwrap());
    }

    #[test]
    fn explicit_paragraphs_and_other_blocks_are_not_tight_inline_flows() {
        for text in [
            "- First paragraph.\n\n  Second paragraph.\n",
            "- # Heading\n",
            "- ```rust\n  let x = 1;\n  ```\n",
            "- > Quoted paragraph.\n",
            "- | A | B |\n  |---|---|\n  | one | two |\n",
            "- <pre>raw HTML\n  code</pre>\n",
        ] {
            assert!(
                collect(text).is_empty(),
                "only direct item inlines qualify: {text:?}"
            );
        }
        let text = "- Parent\n  - Nested\n- Sibling\n";
        let slices: Vec<_> = collect(text)
            .into_iter()
            .map(|(_, range)| &text[range])
            .collect();
        assert_eq!(slices, ["Parent\n", "Nested\n", "Sibling\n"]);
    }
}
