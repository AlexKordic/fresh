//! Complete parser-owned font inputs, shared by every source window of a table.
//! No pixel widths or terminal column estimates are produced by the backend.

use super::super::compose::{ComposeImage, ComposeSpan};
use super::*;
use std::collections::HashMap;
use std::hash::BuildHasher;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Measurement {
    pub columns: Vec<Column>,
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Column {
    pub samples: Vec<Sample>,
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Sample {
    pub text: String,
    pub spans: Vec<Style>,
    pub minimum_only: bool,
    pub image: bool,
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct Style {
    pub start: usize,
    pub end: usize,
    pub flags: u32,
    pub code: bool,
}

/// Samples own their text and styles exactly once. The randomized fingerprint
/// index contains only offsets, with full equality checks for hash collisions.
#[derive(Default)]
struct Samples {
    values: Vec<Sample>,
    heads: HashMap<u64, usize>,
    previous: Vec<usize>,
}

impl Samples {
    fn insert(&mut self, sample: Sample) {
        let fingerprint = self.heads.hasher().hash_one(&sample);
        self.insert_hashed(fingerprint, sample);
    }

    fn insert_hashed(&mut self, fingerprint: u64, sample: Sample) {
        const NONE: usize = usize::MAX;
        let head = self.heads.entry(fingerprint).or_insert(NONE);
        let mut index = *head;
        while index != NONE {
            if self.values[index] == sample {
                return;
            }
            index = self.previous[index];
        }
        self.previous.push(*head);
        *head = self.values.len();
        self.values.push(sample);
    }
}

#[cfg(test)]
impl Measurement {
    /// Compare reconstructed deduplication scratch capacity for the complete
    /// parser samples. Excludes final samples, allocator/hash-table bookkeeping,
    /// construction temporaries and RSS; this is not a measured live heap peak.
    pub(crate) fn deduplication_scratch_capacity(&self) -> (usize, usize) {
        use std::collections::HashSet;
        let (mut cloned, mut indexed) = (0, 0);
        for column in &self.columns {
            let mut old = HashSet::new();
            let mut new = Samples::default();
            for sample in &column.samples {
                old.insert(sample.clone());
                new.insert(sample.clone());
            }
            assert_eq!(new.values, column.samples);
            cloned += old.capacity() * std::mem::size_of::<Sample>()
                + old
                    .iter()
                    .map(|s| s.text.capacity() + s.spans.capacity() * std::mem::size_of::<Style>())
                    .sum::<usize>();
            indexed += new.heads.capacity() * std::mem::size_of::<(u64, usize)>()
                + new.previous.capacity() * std::mem::size_of::<usize>();
        }
        (cloned, indexed)
    }
}

fn style(at: usize, header: bool, spans: &[&ComposeSpan], images: &[&ComposeImage]) -> (u32, bool) {
    let active: Vec<_> = spans
        .iter()
        .filter(|s| s.start <= at && at < s.end)
        .collect();
    let flags = active
        .iter()
        .fold(u32::from(header), |flags, s| flags | (s.flags & 3));
    let code = active.iter().any(|s| s.role == 7)
        || images
            .iter()
            .any(|image| image.start <= at && at < image.end);
    (flags, code)
}

impl Sample {
    fn new(minimum_only: bool) -> Self {
        Self {
            text: String::new(),
            spans: Vec::new(),
            minimum_only,
            image: false,
        }
    }
    fn append(&mut self, text: &str, (flags, code): (u32, bool)) {
        let start = self.text.len();
        self.text.push_str(text);
        if start == self.text.len() || flags == 0 && !code {
            return;
        }
        if let Some(previous) = self
            .spans
            .last_mut()
            .filter(|s| s.end == start && s.flags == flags && s.code == code)
        {
            previous.end = self.text.len();
        } else {
            self.spans.push(Style {
                start,
                end: self.text.len(),
                flags,
                code,
            });
        }
    }
}

impl Tables {
    /// False retires the whole parse, including any earlier completed tables.
    /// The caller must not publish a partially finalized document.
    pub fn finish(
        &mut self,
        text: &str,
        spans: &[ComposeSpan],
        images: &[ComposeImage],
        mut cancelled: impl FnMut() -> bool,
    ) -> bool {
        if cancelled() {
            return false;
        }
        let mut styles = Vec::new();
        for chunk in spans.chunks(128) {
            if cancelled() {
                return false;
            }
            styles.extend(chunk.iter().filter(|s| s.role == 7 || s.flags & 3 != 0));
        }
        styles.sort_by_key(|s| s.start);
        if cancelled() {
            return false;
        }
        let mut assets: Vec<_> = images.iter().collect();
        assets.sort_by_key(|i| i.start);
        let (mut style_index, mut image_index) = (0, 0);
        let mut relevant: Vec<&ComposeSpan> = Vec::new();
        let mut images: Vec<&ComposeImage> = Vec::new();
        for table in &mut self.parsed {
            if cancelled() {
                return false;
            }
            let mut columns: Vec<_> = (0..table.alignments.len())
                .map(|_| Samples::default())
                .collect();
            for row in &table.rows {
                if cancelled() {
                    return false;
                }
                for (column, cell) in row.cells.iter().enumerate().take(columns.len()) {
                    if cancelled() {
                        return false;
                    }
                    if cell.source.start >= row.source.end {
                        continue;
                    }
                    relevant.retain(|s| s.end > cell.source.start);
                    images.retain(|i| i.end > cell.source.start);
                    while style_index < styles.len() && styles[style_index].start < cell.source.end
                    {
                        if cancelled() {
                            return false;
                        }
                        let item = styles[style_index];
                        style_index += 1;
                        if item.end > cell.source.start {
                            relevant.push(item);
                        }
                    }
                    while image_index < assets.len() && assets[image_index].start < cell.source.end
                    {
                        if cancelled() {
                            return false;
                        }
                        let image = assets[image_index];
                        image_index += 1;
                        if image.end > cell.source.start {
                            images.push(image);
                        }
                    }
                    let mut reading = Sample::new(false);
                    for part in &cell.parts {
                        if cancelled() {
                            return false;
                        }
                        if images
                            .iter()
                            .any(|i| i.start <= part.source.start && part.source.end <= i.end)
                        {
                            reading.image = true;
                            continue;
                        }
                        reading.append(
                            part.text.as_deref().unwrap_or(&text[part.source.clone()]),
                            style(part.source.start, row.header, &relevant, &[]),
                        );
                    }
                    // Revealing syntax can introduce a wider cluster, but its
                    // URI/markup length must not dictate preferred column width.
                    let mut raw = Sample::new(true);
                    let mut boundaries = vec![cell.source.start, cell.source.end];
                    for (start, end) in relevant
                        .iter()
                        .map(|s| (s.start, s.end))
                        .chain(images.iter().map(|i| (i.start, i.end)))
                    {
                        if cancelled() {
                            return false;
                        }
                        if cell.source.start < start && start < cell.source.end {
                            boundaries.push(start);
                        }
                        if cell.source.start < end && end < cell.source.end {
                            boundaries.push(end);
                        }
                    }
                    boundaries.sort_unstable();
                    boundaries.dedup();
                    for pair in boundaries.windows(2) {
                        if cancelled() {
                            return false;
                        }
                        raw.append(
                            &text[pair[0]..pair[1]],
                            style(pair[0], row.header, &relevant, &images),
                        );
                    }
                    if raw.text == reading.text && raw.spans == reading.spans && !reading.image {
                        columns[column].insert(reading);
                    } else {
                        for sample in [reading, raw] {
                            columns[column].insert(sample);
                        }
                    }
                }
            }
            if cancelled() {
                return false;
            }
            // Avoid Vec's in-place collect reusing the larger Samples slots
            // as excess retained capacity for the much smaller Column type.
            let mut measurement = Measurement {
                columns: Vec::with_capacity(columns.len()),
            };
            for column in columns {
                measurement.columns.push(Column {
                    samples: column.values,
                });
            }
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            measurement.hash(&mut hash);
            if cancelled() {
                return false;
            }
            table.measurement_id = hash.finish().max(1);
            table.measurement = Some(Arc::new(measurement));
        }
        !cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn parsed_tables(text: &str) -> Tables {
        let mut tables = Tables::default();
        for (event, range) in
            pulldown_cmark::Parser::new_ext(text, pulldown_cmark::Options::all()).into_offset_iter()
        {
            tables.event(text, &event, &range);
        }
        tables
    }

    #[test]
    fn superseded_table_work_stops_before_publishing_its_measurement_and_can_retry() {
        let mut text = "| First | Value |\n|---|---|\n| small | table |\n\n".to_owned();
        text.push_str("| Second | Value |\n|---|---|\n");
        for row in 0..1000 {
            text.push_str(&format!(
                "| café {row} | **東京** [entry {row}](target.md) |\n"
            ));
        }
        let mut tables = parsed_tables(&text);
        assert_eq!(tables.parsed.len(), 2);
        let mut polls = 0;
        assert!(!tables.finish(&text, &[], &[], || {
            polls += 1;
            polls >= 128
        }));
        assert!(
            tables.parsed[0].measurement.is_some(),
            "finish earlier work"
        );
        assert!(
            tables.parsed[1].measurement.is_none(),
            "do not finish the obsolete large table before observing cancellation"
        );
        assert!(tables.finish(&text, &[], &[], || false));
        let mut uninterrupted = parsed_tables(&text);
        assert!(uninterrupted.finish(&text, &[], &[], || false));
        for (actual, expected) in tables.parsed.iter().zip(&uninterrupted.parsed) {
            assert_eq!(actual.measurement, expected.measurement);
            assert_eq!(actual.measurement_id, expected.measurement_id);
        }
    }

    #[test]
    fn already_cancelled_table_work_does_not_build_measurements() {
        let text = "| Header | Value |\n|---|---|\n| café | 東京 |\n";
        let mut tables = parsed_tables(text);
        assert!(!tables.finish(text, &[], &[], || true));
        assert!(tables.parsed[0].measurement.is_none());
    }

    fn variants() -> Vec<Sample> {
        let plain = Sample {
            text: "café 東京".into(),
            spans: vec![],
            minimum_only: false,
            image: false,
        };
        vec![
            plain.clone(),
            Sample {
                // Canonically equal text still has different byte/style offsets.
                text: "cafe\u{301} 東京".into(),
                ..plain.clone()
            },
            Sample {
                spans: vec![Style {
                    start: 0,
                    end: 5,
                    flags: 1,
                    code: false,
                }],
                ..plain.clone()
            },
            Sample {
                spans: vec![Style {
                    start: 0,
                    end: 5,
                    flags: 1,
                    code: true,
                }],
                ..plain.clone()
            },
            Sample {
                minimum_only: true,
                ..plain.clone()
            },
            Sample {
                image: true,
                ..plain
            },
        ]
    }

    #[test]
    fn fingerprint_collisions_preserve_complete_sample_identity_and_order() {
        let values = variants();
        let mut samples = Samples::default();
        for sample in values
            .iter()
            .chain(values.iter().rev())
            .chain(values.iter())
        {
            samples.insert_hashed(7, sample.clone());
        }
        assert_eq!(samples.values, values);
        assert_eq!(samples.heads.len(), 1, "exercise an actual collision chain");
    }

    #[test]
    fn randomized_index_matches_hashset_deduplication_and_keeps_first_storage() {
        let mut samples = Samples::default();
        let mut seen = HashSet::new();
        let mut expected = Vec::new();
        let first = variants().remove(0);
        let first_storage = first.text.as_ptr();
        samples.insert(first);
        for sample in variants().into_iter().cycle().take(60) {
            if seen.insert(sample.clone()) {
                expected.push(sample.clone());
            }
            samples.insert(sample);
        }
        assert_eq!(samples.values, expected);
        assert_eq!(samples.values[0].text.as_ptr(), first_storage);
    }
}
