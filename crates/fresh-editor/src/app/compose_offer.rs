//! Stage complete native formatter lines with bounded reads between frames.
//! Complete batches still retain their logical source context; this reader's
//! budget does not bound later grammar/formatting work or total payload size.

use std::ops::Range;

const READ_BYTES: usize = 64 * 1024;

pub(crate) struct OfferedLine {
    pub line_number: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub content: String,
}

pub(super) struct Offer {
    pub request: Range<usize>,
    length: usize,
    next: usize,
    line_start: usize,
    line_number: usize,
    text: String,
    lines: Vec<OfferedLine>,
    at_line_start: bool,
    close_run: bool,
    tail_lines: usize,
}

impl Offer {
    pub fn new(
        request: Range<usize>,
        start: usize,
        line_number: usize,
        length: usize,
        close_run: bool,
    ) -> Self {
        Self {
            request,
            length,
            next: start,
            line_start: start,
            line_number,
            text: String::new(),
            lines: Vec::new(),
            at_line_start: true,
            close_run,
            tail_lines: 0,
        }
    }

    pub fn matches(&self, request: &Range<usize>, length: usize) -> bool {
        self.request == *request && self.length == length
    }

    fn line(&mut self, newline: bool) -> bool {
        let content = std::mem::take(&mut self.text);
        let blank = content.trim().is_empty();
        self.lines.push(OfferedLine {
            line_number: self.line_number,
            byte_start: self.line_start,
            byte_end: self.next,
            content,
        });
        self.line_start = self.next;
        self.line_number += 1;
        self.at_line_start = newline;
        // The old line-number walk includes the line containing the exclusive
        // end byte. Preserve that context when the end follows a newline.
        if self.next > self.request.end {
            let done = !self.close_run
                || blank
                || self.tail_lines >= super::super::render::FLOW_RUN_MAX_LINES;
            self.tail_lines += 1;
            done
        } else {
            false
        }
    }

    fn finish(&mut self) -> Vec<OfferedLine> {
        if self.next == self.length && self.at_line_start {
            self.lines.push(OfferedLine {
                line_number: self.line_number,
                byte_start: self.next,
                byte_end: self.next,
                content: String::new(),
            });
        }
        std::mem::take(&mut self.lines)
    }

    /// The read callback sees at most READ_BYTES at once and at most `budget`
    /// bytes in total. A split UTF-8 suffix is retried, never decoded lossily.
    pub fn poll(
        &mut self,
        budget: usize,
        mut read: impl FnMut(usize, usize) -> Option<Vec<u8>>,
    ) -> Result<Option<Vec<OfferedLine>>, ()> {
        tracing::trace!(
            start = self.request.start,
            end = self.request.end,
            frontier = self.next,
            lines = self.lines.len(),
            budget,
            "native compose offer progress"
        );
        if budget < 4 || self.next > self.length || self.request.end > self.length {
            return Err(());
        }
        if self.next == self.length {
            return Ok(Some(self.finish()));
        }
        let mut remaining = budget;
        while remaining >= 4 {
            let count = READ_BYTES.min(remaining).min(self.length - self.next);
            let bytes = read(self.next, count).ok_or(())?;
            if bytes.len() != count {
                return Err(());
            }
            remaining -= count;
            let valid = match std::str::from_utf8(&bytes) {
                Ok(text) => text,
                Err(error) if error.error_len().is_none() && error.valid_up_to() > 0 => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()]).map_err(|_| ())?
                }
                Err(_) => return Err(()),
            };
            for part in valid.split_inclusive('\n') {
                self.text.push_str(part);
                self.at_line_start = false;
                self.next += part.len();
                if part.ends_with('\n') && self.line(true) {
                    return Ok(Some(self.finish()));
                }
            }
            if self.next == self.length {
                if !self.text.is_empty() {
                    self.line(false);
                }
                return Ok(Some(self.finish()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_unicode_crlf_lines_publish_only_whole_after_bounded_reads() {
        let source = format!(
            "# Header\r\n\r\n1) {}\r\n\r\nTail\r\n",
            "**café 👩🏽‍💻 東京** and exact source. ".repeat(7000)
        );
        let end = source.find("Tail").unwrap();
        let mut offer = Offer::new(0..end, 0, 0, source.len(), true);
        let mut turns = 0;
        let lines = loop {
            let mut bytes = 0;
            let result = offer
                .poll(128 * 1024, |start, count| {
                    assert!(count <= READ_BYTES);
                    bytes += count;
                    Some(source.as_bytes()[start..start + count].to_vec())
                })
                .unwrap();
            assert!(bytes <= 128 * 1024);
            turns += 1;
            if let Some(lines) = result {
                break lines;
            }
            assert!(turns < 20);
        };
        assert!(turns > 1);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.content.as_str())
                .collect::<String>(),
            source
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line.line_number)
                .collect::<Vec<_>>(),
            (0..6).collect::<Vec<_>>()
        );
        let mut at = 0;
        for line in lines {
            assert_eq!(line.byte_start, at);
            at += line.content.len();
            assert_eq!(line.byte_end, at);
            assert!(line.content.ends_with('\n') || at == source.len());
        }
    }

    #[test]
    fn tiny_budgets_retry_utf8_suffixes_and_preserve_nonzero_source_scope() {
        let source = "ignored\r\nabc👩🏽‍💻é東京\r\nnext\r\n\r\nnot requested";
        let start = source.find("abc").unwrap();
        let end = source.find("next").unwrap();
        let mut offer = Offer::new(start..end, start, 12, source.len(), true);
        let lines = loop {
            let result = offer
                .poll(7, |at, count| {
                    Some(source.as_bytes()[at..at + count].to_vec())
                })
                .unwrap();
            if let Some(lines) = result {
                break lines;
            }
        };
        assert_eq!(lines[0].line_number, 12);
        assert_eq!(lines[0].byte_start, start);
        assert_eq!(
            lines.last().unwrap().byte_end,
            source.find("not requested").unwrap()
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line.content.as_str())
                .collect::<String>(),
            &source[start..source.find("not requested").unwrap()]
        );
    }

    #[test]
    fn eof_and_invalid_reads_do_not_manufacture_source_or_certification() {
        for source in ["", "text", "text\n"] {
            let mut offer = Offer::new(0..source.len(), 0, 0, source.len(), true);
            let lines = offer
                .poll(1024, |at, n| Some(source.as_bytes()[at..at + n].to_vec()))
                .unwrap()
                .unwrap();
            assert_eq!(lines.len(), if source.ends_with('\n') { 2 } else { 1 });
            assert_eq!(
                lines
                    .iter()
                    .map(|line| line.content.as_str())
                    .collect::<String>(),
                source
            );
        }
        let mut offer = Offer::new(0..8, 0, 0, 8, true);
        assert!(offer.poll(8, |_, _| None).is_err());
        let mut offer = Offer::new(0..8, 0, 0, 8, true);
        assert!(offer.poll(8, |_, _| Some(vec![b'x'; 7])).is_err());
        let mut offer = Offer::new(0..8, 0, 0, 8, true);
        assert!(offer.poll(8, |_, _| Some(vec![0xff; 8])).is_err());
    }
}
