//! Explicit resource profiling, separate from correctness and plugin/socket
//! qualification. Exports real parser/projection/wire inputs for native layout.

use super::*;
use crate::webui::wire::Message;
use sha2::{Digest, Sha256};
use std::time::Instant;

fn peak_rss_bytes() -> Option<u64> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return None;
        }
        let value = u64::try_from(usage.ru_maxrss).ok()?;
        return value.checked_mul(if cfg!(target_os = "linux") { 1024 } else { 1 });
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}

fn fixtures() -> Vec<(&'static str, String)> {
    let mut table = "| Name | Detail | Link |\n|---|---|---|\n".to_owned();
    for i in 0..6000 {
        table.push_str(&format!("| row-{i:05} | **café 東京 {i}** with `code` | [entry {i}](https://example.invalid/{i}/detail) |\n"));
    }
    let mut images = "# Image paragraph\n\n".to_owned();
    for i in 0..4000 {
        images.push_str(&format!("text ![image {i}](image-{i}.png) "));
    }
    images.push('\n');
    let mut multiline_images = "# Image paragraph with soft breaks\n\n".to_owned();
    for i in 0..4000 {
        multiline_images.push_str(&format!("text ![image {i}](image-{i}.png) "));
        if i % 40 == 39 {
            multiline_images.push('\n');
        }
    }
    let mut nested = "# Nested\n\n".to_owned();
    for i in 0..1000 {
        nested.push_str(&format!("> > 1. **Item {i}** with [link](target.md), café and 👩🏽‍💻.\n> >\n> >    ```rust\n> >    let item = {i};\n> >    ```\n> >\n"));
    }
    vec![
        (
            "small",
            "# Small\n\nProportional **prose** and café.\n\n![picture](image.png)\n".repeat(20),
        ),
        (
            "large-prose",
            "## Section\n\nParagraph with **prose**, café, 東京 and [a link](target.md).\n\n"
                .repeat(30000),
        ),
        ("nested", nested),
        ("image-paragraph", images),
        ("unique-table", table),
        ("multiline-image-paragraph", multiline_images),
    ]
}

#[test]
#[ignore = "resource profile; set FC_COMPOSE_PROFILE_OUTPUT to export native inputs"]
fn profile_rich_compose_document_workloads() {
    let paragraph_windows = match std::env::var("FC_COMPOSE_PROFILE_PARAGRAPH_WINDOWS")
        .as_deref()
        .unwrap_or("1")
    {
        "1" => true,
        "0" => false,
        _ => panic!("FC_COMPOSE_PROFILE_PARAGRAPH_WINDOWS must be 0 or 1"),
    };
    let window_policy = if paragraph_windows {
        "paragraph"
    } else {
        "legacy"
    };
    let output = std::env::var_os("FC_COMPOSE_PROFILE_OUTPUT").map(std::path::PathBuf::from);
    if let Some(path) = &output {
        std::fs::create_dir_all(path).unwrap();
    }
    let selected = std::env::var("FC_COMPOSE_PROFILE_CASE").ok();
    let mut reports = Vec::new();
    for (name, text) in fixtures() {
        if selected.as_deref().is_some_and(|selected| selected != name) {
            continue;
        }
        let mut parse_ms = Vec::new();
        let mut source_context_ms = Vec::new();
        let mut document = None;
        for revision in 1..=3 {
            let started = Instant::now();
            let pending = Document::pending(text.clone(), revision, 0, 1);
            source_context_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let started = Instant::now();
            // Mirror the worker's prepared immutable input. Document::parse
            // also builds the UTF-16 index, which would count that work twice
            // and mislabel it as semantic parsing in these separate stages.
            let parsed =
                Document::parse_shared(pending.text, pending.text_index, revision, 0, 1, None)
                    .unwrap();
            parse_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            document = Some(parsed);
        }
        let doc = document.unwrap();
        let retention_started = Instant::now();
        for _ in 0..10_000 {
            std::hint::black_box(std::hint::black_box(&doc).retained_bytes());
        }
        let retention_lookup_ms = retention_started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let window = doc.source_window_policy(0, None, None, paragraph_windows);
        let tables = doc.tables.snapshot(&doc.text, window.clone(), &[]);
        let objects: Vec<_> = doc
            .images
            .iter()
            .filter(|i| window.start <= i.start && i.end <= window.end)
            .map(|i| i.start..i.end)
            .collect();
        let projection = doc
            .project_native(
                window.start,
                window.end,
                &[],
                &objects,
                &[],
                &tables,
                Vec::new(),
            )
            .unwrap();
        let mut pane = ComposePane::from_projection(
            projection,
            &doc,
            3,
            vec![ComposeSelection {
                id: 1,
                primary: true,
                position: 0,
                anchor: None,
                utf16_position: None,
                utf16_anchor: None,
            }],
            true,
            0,
            "Normal",
            tables,
        );
        pane.keyboard = true;
        pane.native_newline = 1;
        pane.asset_authority = 1;
        pane.asset_document = 1;
        if paragraph_windows && name.ends_with("image-paragraph") {
            assert!(window.end >= doc.images.last().unwrap().end);
            assert_eq!(
                pane.images.len(),
                4000,
                "profile the complete image paragraph"
            );
        }
        let projection_ms = started.elapsed().as_secs_f64() * 1000.0;
        let samples: usize = pane
            .tables
            .iter()
            .filter_map(|t| t.measurement.as_ref())
            .flat_map(|m| &m.columns)
            .map(|c| c.samples.len())
            .sum();
        let mut bytes = Vec::new();
        let encoding_started = Instant::now();
        pane.encode(&mut bytes);
        let encoding_ms = encoding_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(bytes.len(), pane.encoded_len());
        if let Some(path) = &output {
            std::fs::write(path.join(format!("{name}.pb")), &bytes).unwrap();
        }
        let wire_bytes = bytes.len();
        for table in &mut pane.tables {
            table.measurement = None;
        }
        let report = serde_json::json!({
            "case": name, "source_bytes": text.len(), "source_sha256": format!("{:x}", Sha256::digest(text.as_bytes())),
            "window_policy": window_policy, "background_parse_threshold": BACKGROUND_PARSE_BYTES,
            "semantic_parse_uses_worker": text.len() > BACKGROUND_PARSE_BYTES,
            "parse_ms": parse_ms, "source_context_ms": source_context_ms, "projection_ms": projection_ms,
            "encoding_ms": encoding_ms,
            "source_window_bytes": window.len(), "pane_bytes": wire_bytes,
            "pane_without_measurements_bytes": pane.encoded_len(),
            "semantic_spans": doc.spans.len(), "images": doc.images.len(), "window_images": pane.images.len(),
            "window_table_rows": pane.tables.iter().map(|table| table.rows.len()).sum::<usize>(), "table_samples": samples,
            "process_peak_rss_bytes": peak_rss_bytes(),
            "retained_capacity_weight_bytes": doc.retained_bytes(),
            "retained_source_capacity_bytes": doc.text.capacity(),
            "retained_index_capacity_bytes": doc.text_index.allocated_bytes(),
            "retained_static_metadata_bytes": doc.metadata_bytes,
            "retained_formatter_bytes": doc.decorations.retained_bytes(),
            "retained_prefix_bytes": doc.prefix_cache.allocated_bytes(),
            "retention_lookup_10k_ms": retention_lookup_ms,
        });
        println!("COMPOSE_PROFILE {report}");
        reports.push(report);
    }
    assert!(!reports.is_empty(), "unknown FC_COMPOSE_PROFILE_CASE");
    if let Some(path) = output {
        std::fs::write(path.join("backend.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "window_policy": window_policy,
            "scope": "native parser/projection/encoding only; excludes plugin formatting and event-loop latency",
            "parse_scope": "semantic parse of a prepared immutable source and UTF-16 index; preparation is reported separately",
            "rss_scope": "process high-water mark since start, including input fixtures and prior cases",
            "retention_scope": "unit-profile cache capacity weights, not resident/allocator-live bytes; excludes allocator rounding, unreported container bookkeeping, editor buffers and external/parser/native ownership",
            "cases": reports,
        })).unwrap()).unwrap();
    }
}
