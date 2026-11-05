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

impl Projection {
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
            let start = result.text.len();
            result.text.push_str(conceal.replacement.unwrap_or(""));
            let end = result.text.len();
            result.segments.push(Segment {
                source: conceal.source.clone(),
                display: start..end,
                kind: if start == end {
                    SegmentKind::Hidden
                } else {
                    SegmentKind::Replacement
                },
            });
            from = conceal.source.end;
        }
        result.copy(source, source_start, from..source_end);
        Ok(result)
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
    /// A replacement is atomic: its interior addresses a source range edge.
    pub fn source_position(&self, display: usize, affinity: Affinity) -> Option<usize> {
        if !self.text.is_char_boundary(display) {
            return None;
        }
        if self.segments.is_empty() {
            return (display == 0).then_some(self.source.start);
        }
        let candidates = self.segments.iter().filter_map(|segment| {
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
        if source < self.source.start || source > self.source.end {
            return None;
        }
        if self.segments.is_empty() {
            return Some(0);
        }
        let candidates = self.segments.iter().filter_map(|segment| {
            if source < segment.source.start || source > segment.source.end {
                return None;
            }
            let display = match segment.kind {
                SegmentKind::Copy => segment.display.start + source - segment.source.start,
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
        assert_eq!(p.source_position(3, Affinity::Downstream), Some(5));
        assert_eq!(p.source_position(4, Affinity::Downstream), Some(9));
        assert_eq!(p.display_position(7, Affinity::Upstream), Some(3));
        assert_eq!(p.display_position(7, Affinity::Downstream), Some(4));
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
