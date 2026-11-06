use syntect::parsing::syntax_definition::{ContextReference, MatchOperation, Pattern};
use syntect::parsing::{Regex, SyntaxSetBuilder};

#[cfg(test)]
const REFERENCE_LABEL: &str = r"(\[)(.+?)(\])(:)";
// Keep the bundled definition exact: a dependency grammar change must be
// reviewed rather than applying a URL precondition to an unknown pattern.
const REFERENCE_DEFINITION: &str = r#"(?x:
    \s*                        # Leading whitespace
    (\[)(.+?)(\])(:)           # Reference name
    [ \t]*                     # Optional whitespace
    (?:
      (<)([^>]+)(>)            # The url
    | (\S+)                    # The url
    )
)"#;
const REFERENCE_GUARD: &str = r"\G(?=[\s\S]*?\]:[ \t]*\S)[\s\S]*?\K";

/// Avoid quadratic failed reference-definition searches in the bundled Markdown
/// grammar. A match requires `]:` followed by optional horizontal whitespace
/// and a non-whitespace URL character. Check that once from the search origin,
/// then retain the original leftmost search and every capture unchanged. `\K`
/// excludes the search prefix from match zero; no capture numbers are added.
/// Called before user grammars are loaded, so their definitions are untouched.
pub(crate) fn add_markdown_reference_prefilter(builder: &mut SyntaxSetBuilder) -> usize {
    let Some(mut markdown) = builder
        .syntaxes()
        .iter()
        .rev()
        .find(|syntax| syntax.name == "Markdown")
        .cloned()
    else {
        return 0;
    };
    let mut changed = 0;
    for context in markdown.contexts.values_mut() {
        for pattern in &mut context.patterns {
            let Pattern::Match(pattern) = pattern else {
                continue;
            };
            let source = pattern.regex.regex_str();
            if !pattern.has_captures && source == REFERENCE_DEFINITION {
                pattern.regex = Regex::new(format!("{REFERENCE_GUARD}(?:{source})"));
                changed += 1;
            }
        }
    }
    if changed > 0 {
        // Definitions recovered with into_builder retain resolved references
        // into the old set. Rebind references to equal local contexts so the
        // copied start/includes actually reach the optimized main context.
        let source = builder.clone().build();
        let original = builder
            .syntaxes()
            .iter()
            .rev()
            .find(|syntax| syntax.name == "Markdown")
            .unwrap();
        let mut names = std::collections::HashMap::new();
        let mut inspect = |reference: &ContextReference| {
            if let Ok(id) = reference.id() {
                if let Ok(target) = reference.resolve(&source) {
                    if let Some((name, _)) = original
                        .contexts
                        .iter()
                        .filter(|(_, context)| *context == target)
                        .min_by(|a, b| a.0.cmp(b.0))
                    {
                        names.insert(id, name.clone());
                    }
                }
            }
        };
        for context in original.contexts.values() {
            for pattern in &context.patterns {
                match pattern {
                    Pattern::Include(reference) => inspect(reference),
                    Pattern::Match(pattern) => {
                        if let MatchOperation::Push(references) | MatchOperation::Set(references) =
                            &pattern.operation
                        {
                            for reference in references {
                                inspect(reference);
                            }
                        }
                        if let Some(reference) = &pattern.with_prototype {
                            inspect(reference);
                        }
                    }
                }
            }
        }
        let rebind = |reference: &mut ContextReference| {
            if let Ok(id) = reference.id() {
                if let Some(name) = names.get(&id) {
                    // Syntect seals the variant constructor but exposes its
                    // public serializable grammar representation.
                    *reference = serde_json::from_value(serde_json::json!({"Named": name}))
                        .expect("a named context has a string name");
                }
            }
        };
        let has_prototype = markdown.contexts.contains_key("prototype");
        for context in markdown.contexts.values_mut() {
            if has_prototype {
                context.prototype = None;
            }
            for pattern in &mut context.patterns {
                match pattern {
                    Pattern::Include(reference) => rebind(reference),
                    Pattern::Match(pattern) => {
                        if let MatchOperation::Push(references) | MatchOperation::Set(references) =
                            &mut pattern.operation
                        {
                            for reference in references {
                                rebind(reference);
                            }
                        }
                        if let Some(reference) = &mut pattern.with_prototype {
                            rebind(reference);
                        }
                    }
                }
            }
        }
        builder.add(markdown);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use syntect::parsing::{ParseState, Region, SyntaxSet};

    fn reachable_prefilter(set: &SyntaxSet) -> bool {
        let builder = set.clone().into_builder();
        let markdown = builder
            .syntaxes()
            .iter()
            .rev()
            .find(|syntax| syntax.name == "Markdown")
            .unwrap();
        let mut pending = vec![&markdown.contexts["__start"]];
        let mut seen = std::collections::HashSet::new();
        while let Some(context) = pending.pop() {
            if !seen.insert(context as *const _) {
                continue;
            }
            for pattern in &context.patterns {
                let mut visit = |reference: &ContextReference| {
                    if let Ok(target) = reference.resolve(set) {
                        pending.push(target);
                    }
                };
                match pattern {
                    Pattern::Include(reference) => visit(reference),
                    Pattern::Match(pattern) => {
                        if pattern.regex.regex_str().starts_with(REFERENCE_GUARD) {
                            return true;
                        }
                        if let MatchOperation::Push(references) | MatchOperation::Set(references) =
                            &pattern.operation
                        {
                            for reference in references {
                                visit(reference);
                            }
                        }
                        if let Some(reference) = &pattern.with_prototype {
                            visit(reference);
                        }
                    }
                }
            }
        }
        false
    }

    #[test]
    fn copied_definition_without_rebinding_does_not_reach_the_prefilter() {
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        let mut markdown = builder
            .syntaxes()
            .iter()
            .find(|syntax| syntax.name == "Markdown")
            .unwrap()
            .clone();
        for context in markdown.contexts.values_mut() {
            for pattern in &mut context.patterns {
                if let Pattern::Match(pattern) = pattern {
                    if pattern.regex.regex_str() == REFERENCE_DEFINITION {
                        pattern.regex =
                            Regex::new(format!("{REFERENCE_GUARD}(?:{REFERENCE_DEFINITION})"));
                    }
                }
            }
        }
        builder.add(markdown);
        assert!(
            !reachable_prefilter(&builder.build()),
            "descriptor changes alone leave the parser following the original contexts"
        );
    }

    #[test]
    #[cfg(feature = "runtime")]
    fn rebuilt_markdown_grammar_is_deterministic() {
        let stock = SyntaxSet::load_defaults_newlines();
        let build = || {
            let mut builder = stock.clone().into_builder();
            assert_eq!(add_markdown_reference_prefilter(&mut builder), 1);
            syntect::dumps::dump_binary(&builder.build())
        };
        assert_eq!(build(), build());
    }

    fn original() -> String {
        let builder = SyntaxSet::load_defaults_newlines().into_builder();
        let markdown = builder
            .syntaxes()
            .iter()
            .find(|syntax| syntax.name == "Markdown")
            .unwrap();
        markdown
            .contexts
            .values()
            .flat_map(|context| &context.patterns)
            .find_map(|pattern| match pattern {
                Pattern::Match(value) if value.regex.regex_str().contains(REFERENCE_LABEL) => {
                    Some(value.regex.regex_str().to_owned())
                }
                _ => None,
            })
            .unwrap()
    }

    fn compare_searches(old: &Regex, new: &Regex, text: &str) {
        // Oniguruma's Region equality includes allocation identity. Compare
        // the documented positions instead, overestimating all group slots.
        let slots = old
            .regex_str()
            .bytes()
            .filter(|&byte| byte == b'(')
            .count()
            .max(new.regex_str().bytes().filter(|&byte| byte == b'(').count())
            + 1;
        for start in (0..=text.len()).filter(|&start| text.is_char_boundary(start)) {
            let (mut expected, mut actual) = (Region::new(), Region::new());
            let before = old.search(text, start, text.len(), Some(&mut expected));
            let after = new.search(text, start, text.len(), Some(&mut actual));
            assert_eq!(after, before, "at {start} in {text:?}");
            if before {
                let actual: Vec<_> = (0..slots).map(|index| actual.pos(index)).collect();
                let expected: Vec<_> = (0..slots).map(|index| expected.pos(index)).collect();
                assert_eq!(actual, expected, "capture positions at {start} in {text:?}");
            }
        }
    }

    #[test]
    fn reference_prefilter_preserves_every_match_and_capture_position() {
        let source = original();
        let old = Regex::new(source.clone());
        let new = Regex::new(format!("{REFERENCE_GUARD}(?:{source})"));
        for text in [
            "",
            "[label]: url\n",
            "   [label]: <url> \"title\"\r\n",
            "\t[label]:\turl",
            "prefix [a]:url [b]: <second>",
            "> - [café 東京]: target.md",
            "[a]b]: url",
            "[]:url []:last",
            "[a]:   \r\n[b]: target",
            "[a]:\u{a0}target",
            "[a]: <unclosed",
            "[a]: <> ",
            "\\[escaped\\]]: target",
            "![tiny](tiny.png) [other]: target",
            "[dangling] [still dangling]",
            "[nested [label]]: target",
            "[a]: \n[second]: target",
            "日本語\r\n [x]: target\n",
            "[a]: \0url",
        ] {
            compare_searches(&old, &new, text);
        }
        let tokens = [
            "[",
            "]",
            "]:",
            "] :",
            " ",
            "\t",
            "\r\n",
            "a",
            "👩🏽‍💻",
            "é",
            "\\]",
            "<",
            ">",
            "url",
            ":",
        ];
        let mut seed = 0x86c6_d2acu32;
        for _ in 0..256 {
            let mut text = String::new();
            for _ in 0..24 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                text.push_str(tokens[seed as usize % tokens.len()]);
            }
            compare_searches(&old, &new, &text);
        }
    }

    #[test]
    fn bundled_markdown_parse_operations_are_unchanged_and_prefilter_is_idempotent() {
        let stock = SyntaxSet::load_defaults_newlines();
        let mut builder = stock.clone().into_builder();
        assert_eq!(add_markdown_reference_prefilter(&mut builder), 1);
        assert_eq!(add_markdown_reference_prefilter(&mut builder), 0);
        let patched = builder.build();
        let mut old = ParseState::new(stock.find_syntax_by_name("Markdown").unwrap());
        let mut new = ParseState::new(patched.find_syntax_by_name("Markdown").unwrap());
        let document = "# Heading\r\n\r\n[ref]: target.md \"title\"\r\n> [quoted]: <other>\r\n\r\n| A | B |\r\n|---|---|\r\n| **café** | ![tiny](tiny.png) |\r\n\r\n- [nested]: target\r\n\r\n```rust\r\nlet x = \"[ref]: target\";\r\n```\r\n\r\n![tiny](tiny.png) [link][ref] 👩🏽‍💻\r\n";
        for line in document.split_inclusive('\n') {
            assert_eq!(
                new.parse_line(line, &patched).unwrap(),
                old.parse_line(line, &stock).unwrap(),
                "{line:?}"
            );
        }
    }

    #[test]
    fn runtime_defaults_and_precompiled_pack_use_the_prefilter() {
        use crate::primitives::grammar::GrammarRegistry;
        let defaults = GrammarRegistry::default();
        let precompiled = GrammarRegistry::defaults_only();
        for registry in [&defaults, precompiled.as_ref()] {
            assert!(
                reachable_prefilter(registry.syntax_set()),
                "the live parser must reach the prefilter from its start context"
            );
            let builder = registry.syntax_set().as_ref().clone().into_builder();
            let markdown = builder
                .syntaxes()
                .iter()
                .rev()
                .find(|syntax| syntax.name == "Markdown")
                .unwrap();
            assert!(markdown.contexts.values().flat_map(|context| &context.patterns).any(|pattern|
                matches!(pattern, Pattern::Match(value) if value.regex.regex_str().starts_with(REFERENCE_GUARD))));
        }
    }

    #[test]
    #[cfg(feature = "runtime")]
    #[ignore = "explicit complete Markdown parser resource experiment"]
    fn profile_bundled_markdown_reference_line() {
        let stock = SyntaxSet::load_defaults_newlines();
        let mut builder = stock.clone().into_builder();
        assert_eq!(add_markdown_reference_prefilter(&mut builder), 1);
        let patched = builder.build();
        let mut records = Vec::new();
        for count in [256, 1024, 4000] {
            let line: String = (0..count)
                .map(|i| format!("word{i} café ![tiny](tiny.png) more text. "))
                .collect::<String>()
                + "\r\n";
            let mut before = Vec::new();
            let mut after = Vec::new();
            for _ in 0..3 {
                let mut old = ParseState::new(stock.find_syntax_by_name("Markdown").unwrap());
                let mut new = ParseState::new(patched.find_syntax_by_name("Markdown").unwrap());
                let started = std::time::Instant::now();
                let expected = old.parse_line(&line, &stock).unwrap();
                before.push(started.elapsed().as_secs_f64() * 1000.0);
                let started = std::time::Instant::now();
                let actual = new.parse_line(&line, &patched).unwrap();
                after.push(started.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(
                    actual, expected,
                    "complete scope operations for {count} images"
                );
            }
            records.push(serde_json::json!({"images":count,"source_bytes":line.len(),"before_ms":before,"after_ms":after,"same_operations":true}));
        }
        let data = serde_json::to_vec_pretty(&records).unwrap();
        if let Some(path) = std::env::var_os("FC_MARKDOWN_REFERENCE_PROFILE") {
            std::fs::write(path, data).unwrap();
        } else {
            println!("{}", String::from_utf8(data).unwrap());
        }
    }
}
