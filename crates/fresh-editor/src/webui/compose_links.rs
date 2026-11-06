//! Destinations are optional tooltip metadata, not document contents. Keep
//! complete parsed destinations in Document; bound only the outgoing spans.

use super::ComposeSpan;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const SCENE_METADATA_BYTES: usize = 64 << 10;
const TARGET_BYTES: usize = 2 << 10;

pub(crate) fn limit_reply(reply: &mut super::InputReply, remaining: &mut usize) {
    if let Some(pane) = &mut reply.compose {
        limit(&mut pane.spans, remaining);
    }
    if let Some(edit) = &mut reply.edit {
        limit(&mut edit.pane.spans, remaining);
    }
    if let Some(result) = &mut reply.result {
        limit(&mut result.pane.spans, remaining);
    }
}

/// Also called across a scene's panes, so splits share one metadata budget.
/// Reapplying a smaller budget never upgrades a preview into a complete URI.
pub(crate) fn limit(spans: &mut [ComposeSpan], remaining: &mut usize) {
    for span in spans {
        let allowance = (*remaining).min(TARGET_BYTES);
        if let Some(target) = &span.link_target {
            if target.len() <= allowance {
                *remaining -= target.len();
                continue;
            }
            span.link_target_bytes = Some(target.len());
            span.link_target_preview = preview(target, allowance);
            span.link_target = None;
        } else if let Some(target) = &span.link_target_preview {
            if target.len() > allowance {
                span.link_target_preview = preview(target, allowance);
            }
        }
        if let Some(preview) = &span.link_target_preview {
            *remaining -= preview.len();
        }
    }
}

fn preview(target: &str, allowance: usize) -> Option<Arc<str>> {
    if target.len() <= allowance {
        return Some(target.into());
    }
    let mut end = allowance;
    while !target.is_char_boundary(end) {
        end -= 1;
    }
    // Segment only the bounded prefix: one pathological combining sequence
    // must not scan a multi-megabyte destination on the editor thread. Drop
    // the last cluster since its continuation lies outside that prefix.
    let (end, _) = target[..end].grapheme_indices(true).next_back()?;
    (end > 0).then(|| target[..end].into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(target: &str) -> ComposeSpan {
        ComposeSpan {
            start: 0,
            end: 10,
            role: 0,
            flags: 20,
            content_start: None,
            link_target: Some(target.into()),
            link_target_bytes: None,
            link_target_preview: None,
        }
    }

    #[test]
    fn metadata_budget_is_shared_and_preserves_empty_targets_and_complete_document_values() {
        let original = vec![link("first"), link("second"), link("")];
        let mut projected = original.clone();
        let mut budget = 7;
        limit(&mut projected, &mut budget);
        assert_eq!(projected[0].link_target.as_deref(), Some("first"));
        assert_eq!(projected[1].link_target, None);
        assert_eq!(projected[1].link_target_bytes, Some(6));
        assert_eq!(projected[1].link_target_preview.as_deref(), Some("s"));
        assert_eq!(projected[2].link_target.as_deref(), Some(""));
        assert_eq!(budget, 1);
        assert_eq!(original[1].link_target.as_deref(), Some("second"));
        limit(&mut projected, &mut 0);
        assert!(projected[0].link_target.is_none());
        assert!(projected[1].link_target.is_none());
        assert!(projected[1].link_target_preview.is_none());
        assert_eq!(projected[1].link_target_bytes, Some(6));
        assert_eq!(projected[2].link_target.as_deref(), Some(""));
    }

    #[test]
    fn long_and_repeated_destinations_have_a_bounded_explicit_preview() {
        let target = "a".repeat(5 << 20);
        let mut spans = vec![link(&target); 128];
        let mut budget = SCENE_METADATA_BYTES;
        limit(&mut spans, &mut budget);
        let bytes: usize = spans
            .iter()
            .map(|span| {
                assert!(span.link_target.is_none());
                assert_eq!(span.link_target_bytes, Some(target.len()));
                span.link_target_preview.as_ref().map_or(0, |text| {
                    assert!(text.len() <= TARGET_BYTES);
                    assert!(target.starts_with(text.as_ref()));
                    text.len()
                })
            })
            .sum();
        assert!(bytes <= SCENE_METADATA_BYTES);
        assert_eq!(budget + bytes, SCENE_METADATA_BYTES);
    }

    #[test]
    fn previews_never_split_emoji_or_an_unbounded_combining_cluster() {
        let emoji = "a👩🏽‍💻z";
        for bytes in 0..emoji.len() {
            let value = preview(emoji, bytes);
            assert!(value.as_deref().is_none_or(|p| p == "a" || p == "a👩🏽‍💻"));
        }
        let combining = format!("a{}z", "\u{301}".repeat(5000));
        assert!(preview(&combining, TARGET_BYTES).is_none());
        assert_eq!(preview("e\u{301}next", 4).as_deref(), Some("e\u{301}"));
    }
}
