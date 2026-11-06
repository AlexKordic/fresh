//! Source-preserving text projection for graphical Compose presentations.
//!
//! Conceal activation is resolved by the caller using the presenting split's
//! cursors. This layer does no Markdown parsing and no layout. It retains the
//! two ends of every hidden or replaced source range so a graphical client's
//! caret can address the real buffer without reconstructing Markdown from its
//! appearance. All coordinates are UTF-8 byte boundaries, not cell columns.

use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Affinity {
    Upstream,
    Downstream,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    Copy,
    Hidden,
    Replacement,
    /// Presentation text inserted at a source boundary, such as cell padding.
    Insertion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub source: Range<usize>,
    pub display: Range<usize>,
    pub kind: SegmentKind,
}

/// A resolved conceal. Ranges are absolute positions in the source buffer.
#[derive(Clone, Debug)]
pub struct Conceal<'a> {
    pub source: Range<usize>,
    pub replacement: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    OutOfRange(Range<usize>),
    NotCharBoundary(Range<usize>),
    Overlap(Range<usize>),
    CoordinateOverflow,
}

/// A presentation of one source slice, including zero-width source segments.
///
/// The text is a projection, never a serialized replacement for the document.
/// Source edits must still go through the editor's ordinary event pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Projection {
    pub source: Range<usize>,
    pub text: String,
    pub segments: Vec<Segment>,
}

/// A validated borrowed view for repeated ordered coordinate queries.
/// No strings, segments or per-position tables are copied or retained.
pub struct ProjectionIndex<'a> {
    projection: &'a Projection,
}

impl ProjectionIndex<'_> {
    pub fn display_position(&self, source: usize, affinity: Affinity) -> Option<usize> {
        let segments = &self.projection.segments;
        let first = segments.partition_point(|s| s.source.end < source);
        let last = segments.partition_point(|s| s.source.start <= source);
        self.projection
            .display_position_in(&segments[first..last], source, affinity)
    }

    pub fn source_position(&self, display: usize, affinity: Affinity) -> Option<usize> {
        let segments = &self.projection.segments;
        let first = segments.partition_point(|s| s.display.end < display);
        let last = segments.partition_point(|s| s.display.start <= display);
        self.projection
            .source_position_in(&segments[first..last], display, affinity)
    }
}

impl Projection {
    /// Validate continuity once before repeated binary coordinate lookups.
    /// Publicly constructed/mutated projections keep their existing linear
    /// query behavior; an unordered or incomplete map cannot use this index.
    pub fn index(&self) -> Option<ProjectionIndex<'_>> {
        let (mut source, mut display) = (self.source.start, 0);
        for segment in &self.segments {
            if segment.source.start != source
                || segment.display.start != display
                || segment.source.end < source
                || segment.display.end < display
                || !self.text.is_char_boundary(segment.display.start)
                || !self.text.is_char_boundary(segment.display.end)
            {
                return None;
            }
            if segment.kind == SegmentKind::Copy
                && segment.source.end - source != segment.display.end - display
            {
                return None;
            }
            source = segment.source.end;
            display = segment.display.end;
        }
        (source == self.source.end && display == self.text.len())
            .then_some(ProjectionIndex { projection: self })
    }
    /// Project an exact source slice using non-overlapping, active conceals.
    ///
    /// Refuse malformed ranges instead of issuing an ambiguous source map.
    /// The caller must resolve overlapping decoration precedence before this
    /// step, and must supply complete ranges rather than clipping replacements.
    pub fn new(
        source: &str,
        source_start: usize,
        conceals: &[Conceal<'_>],
    ) -> Result<Self, ProjectionError> {
        let source_end = source_start
            .checked_add(source.len())
            .ok_or(ProjectionError::CoordinateOverflow)?;
        let mut ordered: Vec<_> = conceals.iter().collect();
        ordered.sort_by_key(|c| c.source.start);
        let mut previous = source_start;
        for conceal in &ordered {
            let r = &conceal.source;
            if r.start < source_start || r.end > source_end || r.start >= r.end {
                return Err(ProjectionError::OutOfRange(r.clone()));
            }
            if !source.is_char_boundary(r.start - source_start)
                || !source.is_char_boundary(r.end - source_start)
            {
                return Err(ProjectionError::NotCharBoundary(r.clone()));
            }
            if r.start < previous {
                return Err(ProjectionError::Overlap(r.clone()));
            }
            previous = r.end;
        }
        let mut result = Self {
            source: source_start..source_end,
            text: String::with_capacity(source.len()),
            segments: Vec::with_capacity(ordered.len() * 2 + 1),
        };
        let mut from = source_start;
        for conceal in ordered {
            result.copy(source, source_start, from..conceal.source.start);
            result.replace(source, source_start, conceal);
            from = conceal.source.end;
        }
        result.copy(source, source_start, from..source_end);
        Ok(result)
    }

    fn replace(&mut self, source: &str, source_start: usize, conceal: &Conceal<'_>) {
        let old = &source[conceal.source.start - source_start..conceal.source.end - source_start];
        let new = conceal.replacement.unwrap_or("");
        let display_start = self.text.len();
        self.text.push_str(new);
        // Padding/trimming is common, including very large table cells. Map
        // those directly before considering a character diff.
        if let Some(at) = new.find(old) {
            self.segment(
                conceal.source.start..conceal.source.start,
                display_start..display_start + at,
                false,
            );
            self.segment(
                conceal.source.clone(),
                display_start + at..display_start + at + old.len(),
                true,
            );
            self.segment(
                conceal.source.end..conceal.source.end,
                display_start + at + old.len()..display_start + new.len(),
                false,
            );
            return;
        }
        if !new.is_empty() {
            if let Some(at) = old.find(new) {
                self.segment(
                    conceal.source.start..conceal.source.start + at,
                    display_start..display_start,
                    false,
                );
                self.segment(
                    conceal.source.start + at..conceal.source.start + at + new.len(),
                    display_start..display_start + new.len(),
                    true,
                );
                self.segment(
                    conceal.source.start + at + new.len()..conceal.source.end,
                    display_start + new.len()..display_start + new.len(),
                    false,
                );
                return;
            }
        }
        let old_bytes: Vec<_> = old
            .char_indices()
            .map(|(i, _)| i)
            .chain([old.len()])
            .collect();
        let new_bytes: Vec<_> = new
            .char_indices()
            .map(|(i, _)| i)
            .chain([new.len()])
            .collect();
        // Reuse the editor's deterministic, bounded patience/LCS engine, with
        // Unicode scalars as the token ids. A clock deadline would make the
        // projection revision vary between rendering and checking an input.
        let old_tokens: Vec<_> = old.chars().map(u32::from).collect();
        let new_tokens: Vec<_> = new.chars().map(u32::from).collect();
        let changes = crate::diff::diff_interned_lines(&old_tokens, &new_tokens);
        let mut from = (0, 0);
        for change in changes {
            let to = (
                old_bytes[change.old_start as usize],
                new_bytes[change.new_start as usize],
            );
            self.segment(
                conceal.source.start + from.0..conceal.source.start + to.0,
                display_start + from.1..display_start + to.1,
                true,
            );
            from = (
                old_bytes[(change.old_start + change.old_count) as usize],
                new_bytes[(change.new_start + change.new_count) as usize],
            );
            self.segment(
                conceal.source.start + to.0..conceal.source.start + from.0,
                display_start + to.1..display_start + from.1,
                false,
            );
        }
        self.segment(
            conceal.source.start + from.0..conceal.source.end,
            display_start + from.1..display_start + new.len(),
            true,
        );
    }

    fn segment(&mut self, source: Range<usize>, display: Range<usize>, copied: bool) {
        if source.is_empty() && display.is_empty() {
            return;
        }
        let kind = if copied {
            SegmentKind::Copy
        } else if source.is_empty() {
            SegmentKind::Insertion
        } else if display.is_empty() {
            SegmentKind::Hidden
        } else {
            SegmentKind::Replacement
        };
        self.segments.push(Segment {
            source,
            display,
            kind,
        });
    }

    fn copy(&mut self, source: &str, source_start: usize, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text
            .push_str(&source[range.start - source_start..range.end - source_start]);
        self.segments.push(Segment {
            source: range,
            display: start..self.text.len(),
            kind: SegmentKind::Copy,
        });
    }

    /// Resolve a displayed caret, including the two sides of concealed markup.
    /// Unmatched replacement content addresses a source edge; identical text
    /// within a replacement remains individually addressable.
    pub fn source_position(&self, display: usize, affinity: Affinity) -> Option<usize> {
        self.source_position_in(&self.segments, display, affinity)
    }

    fn source_position_in(
        &self,
        segments: &[Segment],
        display: usize,
        affinity: Affinity,
    ) -> Option<usize> {
        if !self.text.is_char_boundary(display) {
            return None;
        }
        if self.segments.is_empty() {
            return (display == 0).then_some(self.source.start);
        }
        let candidates = segments.iter().filter_map(|segment| {
            if display < segment.display.start || display > segment.display.end {
                return None;
            }
            Some(match segment.kind {
                SegmentKind::Copy => segment.source.start + display - segment.display.start,
                _ if display == segment.display.start && !segment.display.is_empty() => {
                    segment.source.start
                }
                _ if display == segment.display.end && !segment.display.is_empty() => {
                    segment.source.end
                }
                _ => match affinity {
                    Affinity::Upstream => segment.source.start,
                    Affinity::Downstream => segment.source.end,
                },
            })
        });
        match affinity {
            Affinity::Upstream => candidates.min(),
            Affinity::Downstream => candidates.max(),
        }
    }

    /// Find the displayed caret for a source position. Interior positions in
    /// concealed/replaced text collapse to its appropriate visual edge.
    pub fn display_position(&self, source: usize, affinity: Affinity) -> Option<usize> {
        self.display_position_in(&self.segments, source, affinity)
    }

    fn display_position_in(
        &self,
        segments: &[Segment],
        source: usize,
        affinity: Affinity,
    ) -> Option<usize> {
        if source < self.source.start || source > self.source.end {
            return None;
        }
        if self.segments.is_empty() {
            return Some(0);
        }
        let candidates = segments.iter().filter_map(|segment| {
            if source < segment.source.start || source > segment.source.end {
                return None;
            }
            let display = match segment.kind {
                SegmentKind::Copy => segment.display.start + source - segment.source.start,
                SegmentKind::Insertion => match affinity {
                    Affinity::Upstream => segment.display.start,
                    Affinity::Downstream => segment.display.end,
                },
                _ if source == segment.source.start => segment.display.start,
                _ if source == segment.source.end => segment.display.end,
                _ => match affinity {
                    Affinity::Upstream => segment.display.start,
                    Affinity::Downstream => segment.display.end,
                },
            };
            self.text.is_char_boundary(display).then_some(display)
        });
        match affinity {
            Affinity::Upstream => candidates.min(),
            Affinity::Downstream => candidates.max(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_queries_match_linear_affinity_at_every_unicode_and_atomic_boundary() {
        let source = "café 👩🏽‍💻 **bold** and 東京\r\n".repeat(80);
        for replacement in [
            None,
            Some(" "),
            Some("**padded bold**"),
            Some("• 東京"),
            Some("👩🏽‍💻"),
        ] {
            let conceals = source
                .match_indices("**bold**")
                .map(|(at, text)| Conceal {
                    source: 173 + at..173 + at + text.len(),
                    replacement,
                })
                .collect::<Vec<_>>();
            let projection = Projection::new(&source, 173, &conceals).unwrap();
            let index = projection.index().unwrap();
            for affinity in [Affinity::Upstream, Affinity::Downstream] {
                for position in 172..175 + source.len() {
                    assert_eq!(
                        index.display_position(position, affinity),
                        projection.display_position(position, affinity)
                    );
                }
                for position in 0..projection.text.len() + 2 {
                    assert_eq!(
                        index.source_position(position, affinity),
                        projection.source_position(position, affinity)
                    );
                }
            }
        }
        let mut malformed = Projection::new("abc", 10, &[]).unwrap();
        malformed.segments[0].source.start = 9;
        assert!(malformed.index().is_none());
        let mut unequal_copy = Projection::new("abc", 10, &[]).unwrap();
        unequal_copy.source.end += 1;
        unequal_copy.segments[0].source.end += 1;
        assert!(unequal_copy.index().is_none());
        let empty = Projection::new("", 173, &[]).unwrap();
        assert_eq!(
            empty
                .index()
                .unwrap()
                .source_position(0, Affinity::Downstream),
            Some(173)
        );
    }

    #[test]
    fn a_rendered_heading_keeps_both_sides_of_its_hidden_markup() {
        let source = "## Héllo **world**\r\n";
        let bold = source.find("**").unwrap();
        let close = source.rfind("**").unwrap();
        let p = Projection::new(
            source,
            100,
            &[
                Conceal {
                    source: 100..103,
                    replacement: None,
                },
                Conceal {
                    source: 100 + bold..102 + bold,
                    replacement: None,
                },
                Conceal {
                    source: 100 + close..102 + close,
                    replacement: None,
                },
            ],
        )
        .unwrap();
        assert_eq!(p.text, "Héllo world\r\n");
        assert_eq!(p.source_position(0, Affinity::Upstream), Some(100));
        assert_eq!(p.source_position(0, Affinity::Downstream), Some(103));
        let word = p.text.find("world").unwrap();
        assert_eq!(
            p.source_position(word, Affinity::Upstream),
            Some(100 + bold)
        );
        assert_eq!(
            p.source_position(word, Affinity::Downstream),
            Some(102 + bold)
        );
        // Projection does not mutate or serialize the source, including CRLF.
        assert_eq!(source, "## Héllo **world**\r\n");
        assert_eq!(p.source.end, 100 + source.len());
    }

    #[test]
    fn substituted_bullets_are_atomic_and_following_unicode_keeps_its_bytes() {
        let source = "- [x] café 👩🏽‍💻";
        let p = Projection::new(
            source,
            20,
            &[Conceal {
                source: 20..25,
                replacement: Some("☑"),
            }],
        )
        .unwrap();
        assert_eq!(p.text, "☑ café 👩🏽‍💻");
        assert_eq!(p.source_position(0, Affinity::Downstream), Some(20));
        assert_eq!(p.source_position("☑".len(), Affinity::Upstream), Some(25));
        for (byte, _) in source.char_indices().filter(|(byte, _)| *byte > 5) {
            let display = p.display_position(20 + byte, Affinity::Downstream).unwrap();
            assert_eq!(
                p.source_position(display, Affinity::Downstream),
                Some(20 + byte)
            );
        }
        assert_eq!(p.source_position(1, Affinity::Downstream), None);
    }

    #[test]
    fn collapsed_newlines_keep_the_source_interval_a_caret_crosses() {
        let source = "> one\r\n> two";
        let p = Projection::new(
            source,
            0,
            &[
                Conceal {
                    source: 0..2,
                    replacement: None,
                },
                Conceal {
                    source: 5..9,
                    replacement: Some(" "),
                },
            ],
        )
        .unwrap();
        assert_eq!(p.text, "one two");
        assert_eq!(p.source_position(3, Affinity::Upstream), Some(5));
        assert_eq!(p.source_position(3, Affinity::Downstream), Some(8));
        assert_eq!(p.source_position(4, Affinity::Downstream), Some(9));
        assert_eq!(p.display_position(7, Affinity::Upstream), Some(3));
        assert_eq!(p.display_position(7, Affinity::Downstream), Some(3));
    }

    #[test]
    fn padded_replacement_retains_the_source_characters_and_maps_insertions() {
        let p = Projection::new(
            "abc",
            40,
            &[Conceal {
                source: 40..43,
                replacement: Some("    abc   "),
            }],
        )
        .unwrap();
        assert_eq!(p.display_position(40, Affinity::Upstream), Some(0));
        assert_eq!(p.display_position(40, Affinity::Downstream), Some(4));
        assert_eq!(p.display_position(41, Affinity::Downstream), Some(5));
        assert_eq!(p.source_position(5, Affinity::Downstream), Some(41));
        assert_eq!(p.source_position(2, Affinity::Downstream), Some(40));
        assert_eq!(p.source_position(9, Affinity::Downstream), Some(43));
        assert_eq!(p.display_position(43, Affinity::Upstream), Some(7));
        assert_eq!(p.display_position(43, Affinity::Downstream), Some(10));

        let source = "  **café 👩🏽‍💻**  ";
        let p = Projection::new(
            source,
            0,
            &[Conceal {
                source: 0..source.len(),
                replacement: Some(" café 👩🏽‍💻     "),
            }],
        )
        .unwrap();
        for (byte, _) in "afé 👩🏽‍💻".char_indices() {
            let source_byte = "  **c".len() + byte;
            let display_byte = p
                .display_position(source_byte, Affinity::Downstream)
                .unwrap();
            assert_eq!(
                p.source_position(display_byte, Affinity::Downstream),
                Some(source_byte)
            );
        }
    }

    #[test]
    fn large_replacements_have_deterministic_bounded_maps_and_padding_stays_editable() {
        let source = "a".repeat(4096);
        let replacement = "b".repeat(4096);
        let conceal = Conceal {
            source: 0..source.len(),
            replacement: Some(&replacement),
        };
        let first = Projection::new(&source, 0, &[conceal.clone()]).unwrap();
        assert_eq!(first, Projection::new(&source, 0, &[conceal]).unwrap());
        assert_eq!(first.segments.len(), 1);
        let padded = format!("  {source}    ");
        let p = Projection::new(
            &source,
            0,
            &[Conceal {
                source: 0..source.len(),
                replacement: Some(&padded),
            }],
        )
        .unwrap();
        assert_eq!(p.source_position(2048, Affinity::Downstream), Some(2046));
    }

    #[test]
    fn empty_and_fully_hidden_blocks_keep_a_valid_source_anchor() {
        let empty = Projection::new("", 51, &[]).unwrap();
        assert_eq!(empty.source_position(0, Affinity::Downstream), Some(51));
        assert_eq!(empty.display_position(51, Affinity::Upstream), Some(0));
        let p = Projection::new(
            "![alt](image.png)",
            9,
            &[Conceal {
                source: 9..26,
                replacement: None,
            }],
        )
        .unwrap();
        assert_eq!(p.text, "");
        assert_eq!(p.source_position(0, Affinity::Upstream), Some(9));
        assert_eq!(p.source_position(0, Affinity::Downstream), Some(26));
    }

    #[test]
    fn invalid_source_maps_are_refused_before_a_client_can_edit_with_them() {
        assert!(matches!(
            Projection::new(
                "é",
                10,
                &[Conceal {
                    source: 10..11,
                    replacement: None
                },]
            ),
            Err(ProjectionError::NotCharBoundary(_))
        ));
        assert!(matches!(
            Projection::new(
                "abcdef",
                10,
                &[
                    Conceal {
                        source: 10..14,
                        replacement: None
                    },
                    Conceal {
                        source: 12..15,
                        replacement: None
                    },
                ]
            ),
            Err(ProjectionError::Overlap(_))
        ));
        assert!(matches!(
            Projection::new(
                "abc",
                10,
                &[Conceal {
                    source: 9..11,
                    replacement: None
                },]
            ),
            Err(ProjectionError::OutOfRange(_))
        ));
    }
}
