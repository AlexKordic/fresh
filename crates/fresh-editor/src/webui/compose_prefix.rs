//! Parser-owned list-prefix samples, independent of a consumer's source slice.
//! They are measurement inputs, never editable text or terminal columns.

use super::{ComposeSpan, Document};
use fresh_core::compose::{Affinity, Projection};
use unicode_segmentation::UnicodeSegmentation;

#[path = "compose_prefix_cache.rs"]
mod cache;
pub(super) use cache::Cache;

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

#[derive(Default)]
pub(super) struct Prepared {
    pub contexts: Vec<Context>,
    pub regions: Vec<std::ops::Range<usize>>,
    pub pending: bool,
    pub failed: bool,
}

/// Independently format the opening line instead of expanding the visible
/// window to every earlier paragraph of its list item. Whole initial physical
/// lines and atomic replacements remain deliberate exceptions, not hard bounds.
pub(super) fn prepare(
    doc: &mut Document,
    state: &crate::state::EditorState,
    readiness: &mut crate::app::compose_readiness::Registry,
    buffer: crate::model::event::BufferId,
    readiness_enabled: bool,
    measured_tables: bool,
    window: std::ops::Range<usize>,
    cursors: &[usize],
) -> Prepared {
    use crate::app::compose_readiness::Status;
    let mut result = Prepared::default();
    let mut cache = std::mem::take(&mut doc.prefix_cache);
    for item in doc.spans.iter().filter(|item| {
        item.role == 13
            && item.start < window.start
            && item.end > window.start
            && item.content_start.is_some_and(|body| body <= window.start)
    }) {
        let body = item.content_start.unwrap();
        let mut start = doc.text[..item.start]
            .rfind(['\r', '\n'])
            .map_or(0, |at| at + 1);
        let mut end = super::source_line_end(&doc.text, body);
        let content_end = doc.text[start..end].trim_end_matches(['\r', '\n']).len() + start;
        let active = state
            .conceals
            .query_viewport(start, end, &state.marker_list, cursors);
        // A trailing flow join is irrelevant to the marker sample. An atomic
        // replacement beginning in the opening line must remain complete.
        for (range, _) in &active {
            if range.start < content_end {
                start = start.min(range.start);
                end = end.max(range.end);
            }
        }
        for image in doc
            .images
            .iter()
            .filter(|image| image.start >= start && image.start < content_end)
        {
            end = end.max(image.end);
        }
        if doc.text.get(start..end).is_none() {
            result.failed = true;
            continue;
        }
        result.regions.push(start..end);
        let status = if readiness_enabled {
            readiness.demand(buffer, start..end)
        } else {
            Status::Ready
        };
        match status {
            Status::Pending => {
                result.pending = true;
                continue;
            }
            Status::Failed => {
                result.failed = true;
                continue;
            }
            Status::Ready => {}
        }
        let active = state
            .conceals
            .query_viewport(start, end, &state.marker_list, cursors)
            .into_iter()
            .filter(|(range, _)| range.start >= start && range.end <= end)
            .collect();
        let objects = doc
            .images
            .iter()
            .filter(|image| {
                image.start >= start
                    && image.end <= end
                    && !cursors
                        .iter()
                        .any(|position| image.start < *position && *position < image.end)
            })
            .map(|image| image.start..image.end)
            .collect::<Vec<_>>();
        let revealed = doc
            .images
            .iter()
            .filter(|image| {
                image.start >= start
                    && image.end <= end
                    && cursors
                        .iter()
                        .any(|position| image.start < *position && *position < image.end)
            })
            .map(|image| image.start..image.end)
            .collect::<Vec<_>>();
        let tables = if measured_tables {
            doc.tables.snapshot(&doc.text, start..end, cursors)
        } else {
            Vec::new()
        };
        let conceals =
            doc.native_conceals(start, end, cursors, &objects, &revealed, &tables, active);
        if let Some(mut context) = cache.get(&(start..end), item.start, body, &conceals) {
            context.source_line_start = physical_start(&doc.text, window.start);
            result.contexts.push(context);
            continue;
        }
        let Ok(projection) = Projection::new(&doc.text[start..end], start, &conceals) else {
            result.failed = true;
            continue;
        };
        let spans = doc
            .spans
            .iter()
            .filter(|span| span.start < end && span.end > start)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(mut context) = contexts(&projection, &spans, &doc.text)
            .into_iter()
            .find(|context| context.item_start == item.start)
        {
            cache.insert(start..end, &conceals, &context);
            context.source_line_start = physical_start(&doc.text, window.start);
            result.contexts.push(context);
        } else {
            result.failed = true;
        }
    }
    result.regions.sort_by_key(|range| (range.start, range.end));
    result.regions.dedup();
    doc.prefix_cache = cache;
    result
}

fn physical_start(source: &str, at: usize) -> bool {
    let previous = at.checked_sub(1).and_then(|at| source.as_bytes().get(at));
    at == 0
        || previous == Some(&b'\n')
        || previous == Some(&b'\r') && source.as_bytes().get(at) != Some(&b'\n')
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
    let source_line_start = physical_start(source, p.source.start);
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
    fn tight_nested_windows_omit_earlier_siblings_and_keep_the_whole_inline_flow() {
        let mut text = "100) **Parent** café\r\n".to_owned();
        for index in 0..2200 {
            text.push_str(&format!("     - [ ] Child {index} with **prose**, café 👩🏽‍💻 and `code`.\r\n       continuation 東京 and [link](target.md).\r\n"));
        }
        assert_eq!(
            pulldown_cmark::Parser::new_ext(&text, pulldown_cmark::Options::all())
                .filter(|event| matches!(
                    event,
                    pulldown_cmark::Event::Start(pulldown_cmark::Tag::Paragraph)
                ))
                .count(),
            0,
            "this fixture must exercise tight-list flow, not loose Paragraph tags"
        );
        let doc = Document::parse(text, 0, 0, 1);
        let anchor = doc.text.rfind("Child").unwrap();
        let physical = doc.text[..anchor].rfind('\n').unwrap() + 1;
        let continuation = doc.text[anchor..].find("continuation").unwrap() + anchor;
        let window = doc.source_window_policy(0, Some(continuation), Some(physical..anchor), true);
        assert_eq!(doc.source_window(0, Some(continuation)).start, 0);
        assert!(
            window.start > super::super::WINDOW_BYTES,
            "a nested tight list must not pull its earlier siblings into the window: {window:?}"
        );
        assert!(window.start <= physical && window.end == doc.text.len());
        assert!(window.len() < super::super::WINDOW_BYTES * 2);
        assert!(doc.text[window].contains("Child 2199"));
    }

    #[test]
    fn tight_joined_paragraphs_remain_whole_with_bidi_unicode_and_hard_breaks() {
        let text = "100) Parent\r\n     - Child **café**\r\n".to_owned()
            + &"       continuation עברית العربية 東京 👩🏽‍💻 with `code` and words.\r\n".repeat(2400)
            + "       final line  \r\n       after hard break\r\n";
        let doc = Document::parse(text, 0, 0, 1);
        let body = doc.text.find("Child").unwrap();
        let physical = doc.text[..body].rfind('\n').unwrap() + 1;
        let anchor = doc.text.rfind("continuation").unwrap();
        let window = doc.source_window_policy(0, Some(anchor), None, true);
        assert_eq!(window.start, physical);
        assert_eq!(window.end, doc.text.len());
        assert!(
            window.len() > super::super::WINDOW_BYTES * 2,
            "a logical paragraph must not be sliced into arbitrary soft-wrap contexts"
        );
        assert_eq!(doc.source_window(0, Some(anchor)).start, 0);
    }

    fn cache_fixture() -> (Document, crate::state::EditorState) {
        let text = "1) **Opening** café\r\n\r\n   Later paragraph.\r\n\r\n   Last paragraph.\r\n";
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            std::sync::Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let doc = Document::parse(text.into(), 0, 0, state.marker_list.instance_id());
        (doc, state)
    }

    fn sample(doc: &mut Document, state: &crate::state::EditorState, cursors: &[usize]) -> Context {
        let start = doc.text.find("Later").unwrap();
        let end = doc.text.len();
        let prepared = prepare(
            doc,
            state,
            &mut crate::app::compose_readiness::Registry::default(),
            crate::model::event::BufferId(3),
            false,
            false,
            start..end,
            cursors,
        );
        assert!(!prepared.failed && !prepared.pending);
        prepared.contexts.into_iter().next().unwrap()
    }

    #[test]
    fn prefix_cache_tracks_effective_reveal_and_header_changes_without_global_invalidation() {
        use crate::view::overlay::OverlayNamespace;
        use fresh_core::api::MarkerActivation;
        let (mut doc, mut state) = cache_fixture();
        let namespace = OverlayNamespace::from_string("prefix-cache-test".into());
        let open = doc.text.find("**").unwrap();
        let close = doc.text.rfind("**").unwrap();
        for at in [open, close] {
            state.conceals.add_with_activation(
                &mut state.marker_list,
                namespace.clone(),
                at..at + 2,
                None,
                Some(MarkerActivation {
                    if_cursor_in: false,
                    scope_start: open,
                    scope_end: close + 2,
                }),
            );
        }
        let first = sample(&mut doc, &state, &[40]);
        assert_eq!(first.text, "1) O");
        let moved = sample(&mut doc, &state, &[45]);
        assert_eq!(format!("{first:?}"), format!("{moved:?}"));
        assert_eq!(doc.prefix_cache.hits, 1);
        // Reveal changes a presentation input, even without marker mutation.
        let revealed = sample(&mut doc, &state, &[open + 2]);
        assert_eq!(revealed.text, "1) *");
        assert_eq!(doc.prefix_cache.hits, 1);
        assert_eq!(sample(&mut doc, &state, &[45]).text, first.text);
        assert_eq!(doc.prefix_cache.hits, 2);
        // Unrelated plugin metadata does not invalidate the opening line.
        let later = doc.text.find("Later").unwrap();
        state.conceals.add(
            &mut state.marker_list,
            namespace.clone(),
            later..later + 1,
            None,
        );
        assert_eq!(sample(&mut doc, &state, &[45]).text, first.text);
        assert_eq!(doc.prefix_cache.hits, 3);
        // A different replacement in that line must miss and use new content.
        state.conceals.add(
            &mut state.marker_list,
            namespace.clone(),
            open + 2..open + 3,
            Some("É".into()),
        );
        assert_eq!(sample(&mut doc, &state, &[45]).text, "1) É");
        assert_eq!(doc.prefix_cache.hits, 3);
        state
            .conceals
            .clear_namespace(&namespace, &mut state.marker_list);
        for at in [open, close] {
            state
                .conceals
                .add(&mut state.marker_list, namespace.clone(), at..at + 2, None);
        }
        let source = doc.text.replace("Opening", "Changed");
        let mut revised = Document::parse(source, 1, 0, doc.epoch);
        assert_eq!(sample(&mut revised, &state, &[45]).text, "1) C");
        assert_eq!(revised.prefix_cache.hits, 0);
        let mut replaced = Document::parse((*doc.text).clone(), 0, 0, doc.epoch + 1);
        assert_eq!(sample(&mut replaced, &state, &[45]).text, "1) O");
        assert_eq!(replaced.prefix_cache.hits, 0);
    }

    #[test]
    fn oversized_prefix_bypasses_cache_without_truncating_or_changing_presentation() {
        use crate::view::overlay::OverlayNamespace;
        let (mut doc, mut state) = cache_fixture();
        let replacement = "x".repeat(1024 * 1024 + 1);
        state.conceals.add(
            &mut state.marker_list,
            OverlayNamespace::from_string("prefix-cache-test".into()),
            0..2,
            Some(replacement.clone()),
        );
        let first = sample(&mut doc, &state, &[]);
        let again = sample(&mut doc, &state, &[]);
        assert_eq!(first.text, replacement + " *");
        assert_eq!(format!("{first:?}"), format!("{again:?}"));
        assert_eq!(doc.prefix_cache.hits, 0);
        assert_eq!(doc.prefix_cache.allocated_bytes(), 0);
    }

    #[test]
    #[ignore = "resource profile; set FC_COMPOSE_PREFIX_PROFILE_OUTPUT to export timings"]
    fn profile_offscreen_prefix_reuse() {
        use crate::view::overlay::OverlayNamespace;
        use std::time::Instant;
        let unit = "**Opening** café 👩🏽‍💻 東京 with complete source context. ";
        let opening = "100) ".to_owned() + &unit.repeat(4000) + "\r\n";
        let text = opening.clone() + "\r\n" + &"     Later paragraph.\r\n\r\n".repeat(1600);
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            std::sync::Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let namespace = OverlayNamespace::from_string("prefix-cache-profile".into());
        for (at, _) in opening.match_indices("**") {
            state
                .conceals
                .add(&mut state.marker_list, namespace.clone(), at..at + 2, None);
        }
        let mut doc = Document::parse(text, 1, 0, state.marker_list.instance_id());
        let start = doc.text.rfind("Later").unwrap();
        let end = doc.text.len();
        let mut readiness = crate::app::compose_readiness::Registry::default();
        let mut run = |doc: &mut Document, step: usize| {
            let cursor = start + step % 4;
            let prepared = prepare(
                doc,
                &state,
                &mut readiness,
                crate::model::event::BufferId(3),
                false,
                false,
                cursor..end,
                &[cursor],
            );
            assert!(!prepared.failed && !prepared.pending);
            assert_eq!(prepared.contexts[0].text, "100) O");
            prepared.contexts.into_iter().next().unwrap()
        };
        let expected = format!("{:?}", run(&mut doc, 0));
        let mut cold_ms = Vec::new();
        let mut warm_ms = Vec::new();
        // Interleave the two paths to avoid attributing a warm process's work
        // to a historical revision. Both use exactly this build and resolver.
        for iteration in 0..25 {
            doc.prefix_cache = Default::default();
            let started = Instant::now();
            let cold = run(&mut doc, iteration);
            cold_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(format!("{cold:?}"), expected);
            let started = Instant::now();
            let warm = run(&mut doc, iteration + 1);
            warm_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(format!("{warm:?}"), expected);
            assert_eq!(doc.prefix_cache.hits, 1);
        }
        let report = serde_json::json!({
            "profile": if cfg!(debug_assertions) { "debug" } else { "optimized" },
            "scope": "same-build offscreen prepare including active conceal query and native resolution; supplied completed decorations; excludes plugin, parser, wire, native shaping and event-loop latency",
            "comparison": "interleaved empty-cache and warm-cache calls with changing visible start/cursor and identical effective opening-line inputs",
            "source_bytes": doc.text.len(), "opening_line_bytes": opening.len(),
            "conceals": state.conceals.len(),
            "retained_cache_allocation_bytes": doc.prefix_cache.allocated_bytes(),
            "cold_ms": cold_ms, "warm_ms": warm_ms,
        });
        println!("COMPOSE_PREFIX_PROFILE {report}");
        if let Some(path) = std::env::var_os("FC_COMPOSE_PREFIX_PROFILE_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
    }

    #[test]
    fn paragraph_windows_keep_source_indentation_and_independently_require_a_certified_marker() {
        use crate::app::compose_readiness::{Registry, Status};
        use crate::model::event::BufferId;
        use crate::view::overlay::OverlayNamespace;
        use std::sync::Arc;
        let text = "1) **Opening** café\r\n\r\n".to_owned()
            + &"   Paragraph 東京 👩🏽‍💻 with words that retain their complete shaping context.\r\n\r\n".repeat(1400);
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(text.len());
        let body = text.find("**").unwrap();
        let close = text.find("** café").unwrap();
        let namespace = OverlayNamespace::from_string("prefix-test".into());
        state.conceals.add(
            &mut state.marker_list,
            namespace.clone(),
            body..body + 2,
            None,
        );
        state
            .conceals
            .add(&mut state.marker_list, namespace, close..close + 2, None);
        let mut doc = Document::parse(text, 0, 0, state.marker_list.instance_id());
        let anchor = doc.text.rfind("Paragraph").unwrap();
        let legacy = doc.source_window(0, Some(anchor));
        let window = doc.source_window_policy(0, Some(anchor), None, true);
        assert_eq!(legacy.start, 0);
        assert!(window.start > body);
        assert!(window.end - window.start < super::super::WINDOW_BYTES * 2);
        assert!(doc.text[window.clone()].starts_with("   Paragraph"));
        let id = BufferId(3);
        let mut readiness = Registry::default();
        readiness.track(id, state.marker_list.instance_id(), 0);
        let batch = readiness
            .begin(1, id, &[(window.start, window.end)])
            .unwrap();
        readiness.complete(&batch, true);
        assert_eq!(readiness.status(id, window.clone()), Status::Ready);
        let waiting = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            window.clone(),
            &[],
        );
        assert!(waiting.pending && !waiting.failed && waiting.contexts.is_empty());
        assert_eq!(waiting.regions.len(), 1);
        let head = waiting.regions[0].clone();
        assert_eq!(head.start, 0);
        assert!(head.end < window.start);
        let batch = readiness.begin(1, id, &[(head.start, head.end)]).unwrap();
        readiness.complete(&batch, true);
        let ready = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            window.clone(),
            &[],
        );
        assert!(!ready.pending && !ready.failed);
        assert_eq!(ready.contexts[0].text, "1) O");
        assert!(ready.contexts[0].source_line_start);
        assert!(ready.contexts[0].spans.iter().any(|span| span.flags == 1));
        assert_eq!(doc.prefix_cache.hits, 0);
        // Moving the visible slice cannot freeze its physical-line-start flag.
        let source_end = doc.text.len();
        let again = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            anchor..source_end,
            &[anchor],
        );
        assert!(!again.pending && !again.failed);
        assert_eq!(doc.prefix_cache.hits, 1);
        assert_eq!(again.contexts[0].text, ready.contexts[0].text);
        assert!(!again.contexts[0].source_line_start);
        // Even a warm cache cannot certify work in flight or a failed refresh.
        let batch = readiness.begin(1, id, &[(head.start, head.end)]).unwrap();
        let waiting = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            window.clone(),
            &[],
        );
        assert!(waiting.pending && waiting.contexts.is_empty());
        assert_eq!(doc.prefix_cache.hits, 1);
        readiness.complete(&batch, false);
        let failed = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            window.clone(),
            &[],
        );
        assert!(failed.failed && failed.contexts.is_empty());
        assert_eq!(doc.prefix_cache.hits, 1);
        let batch = readiness.begin(1, id, &[(head.start, head.end)]).unwrap();
        readiness.complete(&batch, true);
        let recovered = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            window,
            &[],
        );
        assert!(!recovered.pending && !recovered.failed);
        assert_eq!(doc.prefix_cache.hits, 2);
        // A replaced marker owner cannot borrow the earlier completion.
        readiness.track(id, state.marker_list.instance_id() + 1, 0);
        let source_end = doc.text.len();
        let replacement = prepare(
            &mut doc,
            &state,
            &mut readiness,
            id,
            true,
            false,
            anchor..source_end,
            &[],
        );
        assert!(replacement.pending && replacement.contexts.is_empty());
    }

    #[test]
    fn paragraph_window_ends_keep_the_entire_joined_paragraph() {
        let first = "First physical line\r\n   continued café 👩🏽‍💻 source\r\n".repeat(2200);
        let doc = Document::parse(
            "1) Heading\r\n\r\n   ".to_owned() + &first + "\r\n   Next paragraph\r\n",
            0,
            0,
            1,
        );
        let paragraph = doc
            .paragraphs
            .iter()
            .find(|range| range.len() > super::super::WINDOW_BYTES)
            .unwrap();
        let window =
            doc.source_window_policy(paragraph.start, Some(paragraph.start + 3), None, true);
        assert!(window.start <= paragraph.start);
        assert!(
            window.end >= paragraph.end,
            "complete paragraph context is not a hard byte cutoff"
        );
    }

    #[test]
    fn physical_indent_and_blank_line_pivots_do_not_expand_a_whole_loose_item() {
        let text = "100) Opening\r\n\r\n".to_owned()
            + &"     Later paragraph café 東京.\r\n\r\n".repeat(5000);
        let doc = Document::parse(text, 0, 0, 1);
        let anchor = doc.text.rfind("Later").unwrap();
        let physical = doc.text[..anchor].rfind('\n').unwrap() + 1;
        for start in [physical, physical - 2] {
            let window = doc.source_window_policy(0, Some(anchor), Some(start..start + 2), true);
            assert!(
                window.start > super::super::WINDOW_BYTES,
                "a physical flow pivot cannot pull in the opening marker: {window:?}"
            );
            assert!(window.end - window.start < super::super::WINDOW_BYTES * 2);
            assert!(window.start <= start && start < window.end);
        }
    }

    #[test]
    fn a_single_giant_item_keeps_its_existing_context_at_its_trailing_blank_line() {
        let text = "| A | B |\n|---|---|\n| one | two |\n\n# Long paragraph\n\n1) ".to_owned()
            + &"Measured **prose** café 👩🏽‍💻 東京 with words that continue in one physical paragraph. ".repeat(2000)
            + "\n\nTail\n";
        let doc = Document::parse(text, 0, 0, 1);
        let body_end = doc.text.find("\n\nTail").unwrap();
        for at in body_end..=doc.text.len() {
            for region in [None, Some(at)] {
                assert_eq!(
                    doc.source_window_policy(at, region, None, true).start,
                    doc.source_window_policy(at, region, None, false).start,
                    "a single-paragraph item must retain its qualified marker context at {at}"
                );
            }
        }
    }

    #[test]
    fn offscreen_prefixes_keep_a_multiline_first_image_atomic() {
        use crate::app::compose_readiness::Registry;
        use crate::model::event::BufferId;
        use std::sync::Arc;
        let text = "1) ![first\n   image](image.png)\n\n".to_owned()
            + &"   Later paragraph words.\n\n".repeat(4000);
        let mut doc = Document::parse(text, 0, 0, 1);
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(doc.text.len());
        let anchor = doc.text.rfind("Later").unwrap();
        let window = doc.source_window_policy(0, Some(anchor), None, true);
        let result = prepare(
            &mut doc,
            &state,
            &mut Registry::default(),
            BufferId(3),
            false,
            false,
            window,
            &[],
        );
        assert!(!result.pending && !result.failed);
        assert_eq!(result.contexts[0].text, "1) \u{fffc}");
        assert!(result.regions[0].end >= doc.images[0].end);
    }

    #[test]
    fn offscreen_prefixes_keep_a_replacement_crossing_the_opening_line_boundary_whole() {
        use crate::app::compose_readiness::Registry;
        use crate::model::event::BufferId;
        use crate::view::overlay::OverlayNamespace;
        use std::sync::Arc;
        let text = "Before\n1) Opening\n\n".to_owned() + &"   Later paragraph.\n\n".repeat(5000);
        let mut doc = Document::parse(text, 0, 0, 1);
        let item = doc.spans.iter().find(|span| span.role == 13).unwrap();
        let mut state = crate::state::EditorState::new(
            80,
            24,
            1 << 20,
            Arc::new(crate::model::filesystem::StdFileSystem),
        );
        state.marker_list.set_buffer_size(doc.text.len());
        let range = item.start - 1..item.content_start.unwrap();
        state.conceals.add(
            &mut state.marker_list,
            OverlayNamespace::from_string("prefix-test".into()),
            range.clone(),
            Some(" ".into()),
        );
        let anchor = doc.text.rfind("Later").unwrap();
        let window = doc.source_window_policy(0, Some(anchor), None, true);
        let result = prepare(
            &mut doc,
            &state,
            &mut Registry::default(),
            BufferId(3),
            false,
            false,
            window,
            &[],
        );
        assert!(!result.pending && !result.failed);
        assert_eq!(result.contexts[0].text, " O");
        assert!(result.regions[0].start <= range.start);
    }

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
