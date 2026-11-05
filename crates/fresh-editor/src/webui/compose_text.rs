//! Bounded raw-source context for native text services. Its UTF-16 positions
//! are document-wide and do not change when Compose conceals or scrolls text.

const STRIDE: usize = 1024;
const CONTEXT_RADIUS: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct Context {
    pub text: String,
    pub source_start: usize,
    pub utf16_start: usize,
    pub selection_start: usize,
    pub selection_end: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Chunk {
    pub text: String,
    pub source_start: usize,
    pub utf16_start: usize,
}

/// Built once per parsed source revision. A lookup scans at most one sparse
/// checkpoint interval rather than counting from the start on every caret move.
pub(super) struct Index(Vec<(usize, usize)>);

impl Index {
    pub fn new(text: &str) -> Self {
        let mut entries = vec![(0, 0)];
        let mut units = 0;
        for (byte, scalar) in text.char_indices() {
            if byte - entries.last().unwrap().0 >= STRIDE {
                entries.push((byte, units));
            }
            units += scalar.len_utf16();
        }
        Self(entries)
    }

    pub fn utf16_at(&self, text: &str, byte: usize) -> Option<usize> {
        if !text.is_char_boundary(byte) {
            return None;
        }
        let index = self.0.partition_point(|&(position, _)| position <= byte) - 1;
        let (start, units) = self.0[index];
        Some(units + text[start..byte].encode_utf16().count())
    }

    pub fn context(&self, text: &str, caret: usize, anchor: Option<usize>) -> Option<Context> {
        let position = self.utf16_at(text, caret)?;
        let anchor = self.utf16_at(text, anchor.unwrap_or(caret))?;
        let range = Self::range(text, caret)?;
        Some(Context {
            text: text[range.clone()].to_owned(),
            source_start: range.start,
            utf16_start: self.utf16_at(text, range.start)?,
            selection_start: position.min(anchor),
            selection_end: position.max(anchor),
        })
    }

    fn range(text: &str, caret: usize) -> Option<std::ops::Range<usize>> {
        if !text.is_char_boundary(caret) {
            return None;
        }
        let mut start = caret.saturating_sub(CONTEXT_RADIUS);
        let mut end = caret.saturating_add(CONTEXT_RADIUS).min(text.len());
        while !text.is_char_boundary(start) {
            start += 1;
        }
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Some(start..end)
    }

    /// Coalesce nearby caret windows. Even a caret at every character sends
    /// each source byte at most once here, rather than 8 KiB per cursor.
    pub fn chunks(&self, text: &str, carets: &[usize], primary: Option<&Context>) -> Vec<Chunk> {
        if carets.len() < 2 {
            return Vec::new();
        }
        let mut ranges: Vec<_> = carets
            .iter()
            .filter_map(|&p| Self::range(text, p))
            .collect();
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for range in ranges {
            if let Some(previous) = merged.last_mut().filter(|p| p.end >= range.start) {
                previous.end = previous.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        merged
            .into_iter()
            .filter(|range| {
                !primary.is_some_and(|p| {
                    range.start >= p.source_start && range.end <= p.source_start + p.text.len()
                })
            })
            .filter_map(|range| {
                Some(Chunk {
                    utf16_start: self.utf16_at(text, range.start)?,
                    source_start: range.start,
                    text: text[range].to_owned(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_carets_share_coalesced_context_instead_of_multiplying_source_payload() {
        let text = "café 文 👩🏽‍💻\r\n".repeat(4000);
        let index = Index::new(&text);
        let carets: Vec<_> = text
            .char_indices()
            .step_by(10)
            .map(|(byte, _)| byte)
            .collect();
        let primary = index.context(&text, carets[0], None).unwrap();
        let chunks = index.chunks(&text, &carets, Some(&primary));
        assert_eq!(chunks.len(), 1);
        assert!(chunks.iter().map(|c| c.text.len()).sum::<usize>() <= text.len());
        assert!(chunks[0].text.len() > primary.text.len());
        assert_eq!(
            chunks[0].utf16_start,
            text[..chunks[0].source_start].encode_utf16().count()
        );
        assert!(index.chunks(&text, &[0, 1], Some(&primary)).is_empty());
    }

    #[test]
    fn source_context_retains_markup_and_document_utf16_positions_far_from_the_start() {
        let prefix = "👩🏽‍💻 café e\u{301}\r\n".repeat(1200);
        let text = format!("{prefix}**日本語**\r\nTail");
        let caret = prefix.len() + "**日本".len();
        let anchor = prefix.len() + 2;
        let index = Index::new(&text);
        let context = index.context(&text, caret, Some(anchor)).unwrap();
        assert!(context.source_start > 0);
        assert!(context.text.contains("**日本語**\r\n"));
        assert_eq!(
            context.utf16_start,
            text[..context.source_start].encode_utf16().count()
        );
        assert_eq!(context.selection_start, prefix.encode_utf16().count() + 2);
        assert_eq!(context.selection_end, context.selection_start + 2);
        assert!(context.text.len() <= CONTEXT_RADIUS * 2);
        assert!(index.context(&text, prefix.len() + 3, None).is_none());
        assert!(index.context(&text, text.len() + 1, None).is_none());
        assert!(index.context(&text, caret, Some(text.len() + 1)).is_none());
    }

    #[test]
    fn a_large_selection_does_not_expand_the_bounded_context() {
        let text = "文".repeat(30_000);
        let context = Index::new(&text)
            .context(&text, text.len(), Some(0))
            .unwrap();
        assert_eq!(context.selection_start, 0);
        assert_eq!(context.selection_end, 30_000);
        assert!(context.text.len() <= CONTEXT_RADIUS);
        let empty = Index::new("").context("", 0, None).unwrap();
        assert!(empty.text.is_empty());
        assert_eq!(
            (empty.source_start, empty.utf16_start, empty.selection_end),
            (0, 0, 0)
        );
    }
}
