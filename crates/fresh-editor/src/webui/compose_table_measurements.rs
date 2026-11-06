//! Complete parser-owned font inputs, shared by every source window of a table.
//! No pixel widths or terminal column estimates are produced by the backend.

use super::super::compose::{ComposeImage, ComposeSpan};
use super::*;
use std::collections::HashSet;

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
    pub fn finish(&mut self, text: &str, spans: &[ComposeSpan], images: &[ComposeImage]) {
        let mut styles: Vec<_> = spans
            .iter()
            .filter(|s| s.role == 7 || s.flags & 3 != 0)
            .collect();
        styles.sort_by_key(|s| s.start);
        let mut assets: Vec<_> = images.iter().collect();
        assets.sort_by_key(|i| i.start);
        let (mut style_index, mut image_index) = (0, 0);
        let mut relevant: Vec<&ComposeSpan> = Vec::new();
        let mut images: Vec<&ComposeImage> = Vec::new();
        for table in &mut self.parsed {
            let mut columns = vec![
                Column {
                    samples: Vec::new()
                };
                table.alignments.len()
            ];
            let mut seen = vec![HashSet::new(); columns.len()];
            for row in &table.rows {
                for (column, cell) in row.cells.iter().enumerate().take(columns.len()) {
                    if cell.source.start >= row.source.end {
                        continue;
                    }
                    relevant.retain(|s| s.end > cell.source.start);
                    images.retain(|i| i.end > cell.source.start);
                    while style_index < styles.len() && styles[style_index].start < cell.source.end
                    {
                        let item = styles[style_index];
                        style_index += 1;
                        if item.end > cell.source.start {
                            relevant.push(item);
                        }
                    }
                    while image_index < assets.len() && assets[image_index].start < cell.source.end
                    {
                        let image = assets[image_index];
                        image_index += 1;
                        if image.end > cell.source.start {
                            images.push(image);
                        }
                    }
                    let mut reading = Sample::new(false);
                    for part in &cell.parts {
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
                        raw.append(
                            &text[pair[0]..pair[1]],
                            style(pair[0], row.header, &relevant, &images),
                        );
                    }
                    if raw.text == reading.text && raw.spans == reading.spans && !reading.image {
                        if seen[column].insert(reading.clone()) {
                            columns[column].samples.push(reading);
                        }
                    } else {
                        for sample in [reading, raw] {
                            if seen[column].insert(sample.clone()) {
                                columns[column].samples.push(sample);
                            }
                        }
                    }
                }
            }
            let measurement = Measurement { columns };
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            measurement.hash(&mut hash);
            table.measurement_id = hash.finish().max(1);
            table.measurement = Some(Arc::new(measurement));
        }
    }
}
