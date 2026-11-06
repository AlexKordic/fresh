//! Parser-owned list-prefix samples, independent of a consumer's source slice.
//! They are measurement inputs, never editable text or terminal columns.

use super::ComposeSpan;
use fresh_core::compose::{Affinity, Projection};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Hash)]
pub(crate) struct Context {
    pub item_start: usize,
    pub content_start: usize,
    pub text: String,
    pub prefix_end: usize,
    /// Relative display-byte ranges, with the same role/flag font semantics.
    pub spans: Vec<ComposeSpan>,
    /// Whether the enclosing pane's source_start is a physical line start.
    pub source_line_start: bool,
}

pub(super) fn contexts(p: &Projection, spans: &[ComposeSpan], source: &str) -> Vec<Context> {
    let Some(positions) = p.index() else {
        return Vec::new();
    };
    let mut styles = Vec::new();
    for span in spans
        .iter()
        .filter(|s| (1..=7).contains(&s.role) || s.role == 12 || s.flags != 0)
    {
        let from = span.start.max(p.source.start);
        let to = span.end.min(p.source.end);
        if from >= to {
            continue;
        }
        if let (Some(start), Some(end)) = (
            positions.display_position(from, Affinity::Downstream),
            positions.display_position(to, Affinity::Upstream),
        ) {
            if start < end {
                styles.push((start..end, span.role, span.flags));
            }
        }
    }
    styles.sort_by_key(|(range, _, _)| range.start);
    let mut samples = Vec::new();
    for item in spans.iter().filter(|s| s.role == 13) {
        let Some(content) = item.content_start else {
            continue;
        };
        if item.start < p.source.start
            || item.start > content
            || content >= item.end
            || content >= p.source.end
        {
            continue;
        }
        let (Some(marker), Some(body)) = (
            positions.display_position(item.start, Affinity::Downstream),
            positions.display_position(content, Affinity::Downstream),
        ) else {
            continue;
        };
        let start = p.text[..marker].rfind(['\r', '\n']).map_or(0, |n| n + 1);
        if body < start || p.text[start..body].contains(['\r', '\n']) {
            continue;
        }
        let Some(first) = p.text[body..].graphemes(true).next() else {
            continue;
        };
        if first.contains(['\r', '\n']) {
            continue;
        }
        samples.push((start..body + first.len(), body, item));
    }
    samples.sort_by_key(|(range, _, _)| (range.start, range.end));
    let mut next_style = 0;
    let mut active: Vec<usize> = Vec::new();
    let previous = p
        .source
        .start
        .checked_sub(1)
        .and_then(|at| source.as_bytes().get(at));
    let source_line_start = p.source.start == 0
        || previous == Some(&b'\n')
        || previous == Some(&b'\r') && source.as_bytes().get(p.source.start) != Some(&b'\n');
    samples
        .into_iter()
        .map(|(range, body, item)| {
            active.retain(|&index| {
                let (r, _, _) = &styles[index];
                r.end > range.start
            });
            while next_style < styles.len() && styles[next_style].0.start < range.end {
                active.push(next_style);
                next_style += 1;
            }
            let mut sample_spans = Vec::new();
            for &index in &active {
                let (style, role, flags) = &styles[index];
                let from = style.start.max(range.start);
                let to = style.end.min(range.end);
                if from < to {
                    sample_spans.push(ComposeSpan {
                        start: from - range.start,
                        end: to - range.start,
                        role: *role,
                        flags: *flags,
                        content_start: None,
                    });
                }
            }
            Context {
                item_start: item.start,
                content_start: item.content_start.unwrap(),
                text: p.text[range.clone()].to_owned(),
                prefix_end: body - range.start,
                spans: sample_spans,
                source_line_start,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresh_core::compose::Conceal;

    #[test]
    fn samples_use_projected_prefixes_one_complete_body_grapheme_and_relative_styles() {
        let text = "Before\n> 12) **👩🏽‍💻 café** followed by a long body.\n";
        let marker = text.find("12)").unwrap();
        let content = text.find("**").unwrap();
        let close = text.rfind("**").unwrap();
        let spans = vec![
            ComposeSpan {
                start: marker,
                end: text.len(),
                role: 13,
                flags: 0,
                content_start: Some(content),
            },
            ComposeSpan {
                start: content,
                end: close + 2,
                role: 0,
                flags: 1,
                content_start: None,
            },
        ];
        let p = Projection::new(
            text,
            0,
            &[
                Conceal {
                    source: text.find('>').unwrap()..marker,
                    replacement: None,
                },
                Conceal {
                    source: content..content + 2,
                    replacement: None,
                },
                Conceal {
                    source: close..close + 2,
                    replacement: None,
                },
            ],
        )
        .unwrap();
        let values = contexts(&p, &spans, text);
        assert_eq!(values.len(), 1);
        let sample = &values[0];
        assert_eq!(sample.text, "12) 👩🏽‍💻");
        assert_eq!(sample.prefix_end, 4);
        assert_eq!(sample.item_start, marker);
        assert_eq!(sample.content_start, content);
        assert_eq!(sample.spans.len(), 1);
        assert_eq!(
            (
                sample.spans[0].start,
                sample.spans[0].end,
                sample.spans[0].flags
            ),
            (4, sample.text.len(), 1)
        );
    }

    #[test]
    fn source_offsets_are_global_and_empty_or_multiline_prefixes_are_not_invented() {
        let text = "- An item\n\n";
        let p = Projection::new(text, 300, &[]).unwrap();
        let spans = vec![ComposeSpan {
            start: 300,
            end: 311,
            role: 13,
            flags: 0,
            content_start: Some(302),
        }];
        let source = "x".repeat(300) + text;
        let values = contexts(&p, &spans, &source);
        assert_eq!(values[0].text, "- A");
        assert!(!values[0].source_line_start);
        assert_eq!(values[0].content_start, 302);
        let mut empty = spans.clone();
        empty[0].content_start = None;
        assert!(contexts(&p, &empty, &source).is_empty());
        let mut multiline = spans;
        multiline[0].content_start = Some(310);
        assert!(contexts(&p, &multiline, &source).is_empty());
    }

    #[test]
    fn overlapping_styles_stay_with_their_own_samples_during_the_sweep() {
        let text = "- café words\n".repeat(300);
        let mut spans = vec![ComposeSpan {
            start: 0,
            end: text.len(),
            role: 0,
            flags: 1,
            content_start: None,
        }];
        let mut at = 0;
        for line in text.split_inclusive('\n') {
            spans.push(ComposeSpan {
                start: at,
                end: at + line.len(),
                role: 13,
                flags: 0,
                content_start: Some(at + 2),
            });
            spans.push(ComposeSpan {
                start: at + 2,
                end: at + "- café".len(),
                role: 0,
                flags: 2,
                content_start: None,
            });
            at += line.len();
        }
        let projection = Projection::new(&text, 0, &[]).unwrap();
        let samples = contexts(&projection, &spans, &text);
        assert_eq!(samples.len(), 300);
        for sample in samples {
            assert_eq!(sample.text, "- c");
            assert_eq!(sample.spans.len(), 2);
            assert!(sample
                .spans
                .iter()
                .any(|s| (s.start, s.end, s.flags) == (0, 3, 1)));
            assert!(sample
                .spans
                .iter()
                .any(|s| (s.start, s.end, s.flags) == (2, 3, 2)));
        }
    }

    #[test]
    fn physical_line_flag_does_not_split_a_crlf_separator() {
        for (source, start, expected) in [
            ("\r\n- An item", 2, true),
            ("\r- An item", 1, true),
            ("x- An item", 1, false),
        ] {
            let projection = Projection::new(&source[start..], start, &[]).unwrap();
            let spans = vec![ComposeSpan {
                start,
                end: source.len(),
                role: 13,
                flags: 0,
                content_start: Some(start + 2),
            }];
            assert_eq!(
                contexts(&projection, &spans, source)[0].source_line_start,
                expected
            );
        }
        let source = "\r\n- An item";
        let projection = Projection::new(&source[1..], 1, &[]).unwrap();
        let spans = vec![ComposeSpan {
            start: 2,
            end: source.len(),
            role: 13,
            flags: 0,
            content_start: Some(4),
        }];
        assert!(!contexts(&projection, &spans, source)[0].source_line_start);
    }
}
