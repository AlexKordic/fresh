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

    fn utf16_at(&self, text: &str, byte: usize) -> Option<usize> {
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
        let mut start = caret.saturating_sub(CONTEXT_RADIUS);
        let mut end = caret.saturating_add(CONTEXT_RADIUS).min(text.len());
        while !text.is_char_boundary(start) {
            start += 1;
        }
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Some(Context {
            text: text[start..end].to_owned(),
            source_start: start,
            utf16_start: self.utf16_at(text, start)?,
            selection_start: position.min(anchor),
            selection_end: position.max(anchor),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
