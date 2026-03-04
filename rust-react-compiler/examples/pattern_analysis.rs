use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_span::SourceType;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

fn parse_expected_code(md: &str) -> Option<String> {
    let start = md.find("## Code\n\n```javascript\n")?;
    let after_fence = start + "## Code\n\n```javascript\n".len();
    let end = md[after_fence..].find("\n```")?;
    Some(md[after_fence..after_fence + end].to_string())
}

/// Full normalize_js from tests/fixtures.rs
fn normalize_js(js: &str) -> String {
    let mut stripped = String::with_capacity(js.len());
    let bytes = js.as_bytes();
    let mut i = 0;
    let mut in_block_comment = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut prev = b' ';

    while i < bytes.len() {
        let c = bytes[i];
        if in_block_comment {
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_single_quote {
            if c == b'\'' && prev != b'\\' {
                in_single_quote = false;
            }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if in_double_quote {
            if c == b'"' && prev != b'\\' {
                in_double_quote = false;
            }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_single_quote = true;
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'"' {
            in_double_quote = true;
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        stripped.push(c as char);
        prev = c;
        i += 1;
    }

    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let mut result = String::new();
    for (i, &tok) in tokens.iter().enumerate() {
        let effective = if (tok.ends_with(',') || tok == ",")
            && i + 1 < tokens.len()
            && (tokens[i + 1] == "}"
                || tokens[i + 1] == "]"
                || tokens[i + 1].starts_with('}')
                || tokens[i + 1].starts_with(']'))
        {
            &tok[..tok.len() - 1]
        } else {
            tok
        };
        if effective.is_empty() {
            continue;
        }
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(effective);
    }
    let result = result.replace("[ ", "[").replace(" ]", "]");
    let result = result.replace("( ", "(").replace(" )", ")");
    let result = result.replace("{ }", "{}");
    let result = result.replace("return undefined;", "return;");
    let result = result.replace("} else {}", "}");
    result
}

fn source_type_for(path: &Path) -> SourceType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => SourceType::tsx(),
        Some("ts") => SourceType::ts(),
        Some("jsx") | Some("js") => SourceType::jsx(),
        _ => SourceType::mjs(),
    }
}

fn is_error_fixture(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("error.") || name.starts_with("todo.error.")
}

// ---- Category detection functions ----

/// EXTRA_BLANK_LINE: raw line-by-line comparison finds blank-line differences
fn check_extra_blank_line(expected_raw: &str, actual_raw: &str) -> bool {
    let exp_blanks: Vec<bool> = expected_raw.lines().map(|l| l.trim().is_empty()).collect();
    let act_blanks: Vec<bool> = actual_raw.lines().map(|l| l.trim().is_empty()).collect();
    // Count blank lines in each
    let exp_blank_count = exp_blanks.iter().filter(|&&b| b).count();
    let act_blank_count = act_blanks.iter().filter(|&&b| b).count();
    exp_blank_count != act_blank_count
}

/// CONST_VS_LET: expected uses `const` where we use `let` (or vice versa)
fn check_const_vs_let(expected: &str, actual: &str) -> bool {
    // Normalize by replacing const<->let and see if that helps
    let exp_norm = normalize_js(expected);
    let act_norm = normalize_js(actual);
    if exp_norm == act_norm {
        return false; // already matching, not relevant
    }
    // Replace all `const ` with `let ` in both, then compare
    let exp_unified = exp_norm.replace("const ", "let ");
    let act_unified = act_norm.replace("const ", "let ");
    // If they match after unification, then const-vs-let is the (or a) difference
    exp_unified != exp_norm || act_unified != act_norm // at least one has const
        // and unifying helps:
        && {
            // Check if unifying closes the gap at all
            // Count token diffs before and after
            let before = token_diff_count(&exp_norm, &act_norm);
            let after = token_diff_count(&exp_unified, &act_unified);
            after < before
        }
}

/// WRONG_ENTRYPOINT: only difference is in the FIXTURE_ENTRYPOINT export block
fn check_wrong_entrypoint(expected: &str, actual: &str) -> bool {
    let exp_norm = normalize_js(expected);
    let act_norm = normalize_js(actual);
    if exp_norm == act_norm {
        return false;
    }
    // Remove everything from "export const FIXTURE_ENTRYPOINT" to end
    let exp_stripped = strip_entrypoint(&exp_norm);
    let act_stripped = strip_entrypoint(&act_norm);
    // If they match after stripping entrypoint, then entrypoint is the only diff
    exp_stripped == act_stripped && exp_stripped != exp_norm
}

fn strip_entrypoint(s: &str) -> String {
    if let Some(pos) = s.find("export const FIXTURE_ENTRYPOINT") {
        s[..pos].trim().to_string()
    } else if let Some(pos) = s.find("export let FIXTURE_ENTRYPOINT") {
        s[..pos].trim().to_string()
    } else {
        s.to_string()
    }
}

/// EXTRA_IMPORT: we emit `import { c as _c }` but expected doesn't have it
fn check_extra_import(expected: &str, actual: &str) -> bool {
    let has_c_import = |s: &str| {
        s.contains("import { c as _c }") || s.contains("import {c as _c}")
    };
    !has_c_import(expected) && has_c_import(actual)
}

/// MISSING_IMPORT: expected has `import { c as _c }` but we don't
fn check_missing_import(expected: &str, actual: &str) -> bool {
    let has_c_import = |s: &str| {
        s.contains("import { c as _c }") || s.contains("import {c as _c}")
    };
    has_c_import(expected) && !has_c_import(actual)
}

/// MISSING_BB_LABEL: expected has `bb0:` labeled blocks but we don't
fn check_missing_bb_label(expected: &str, actual: &str) -> bool {
    expected.contains("bb0:") && !actual.contains("bb0:")
}

/// TEMP_NAME_DIFF: expected uses `t0`/`t1` but we use `$tN` style names
fn check_temp_name_diff(expected: &str, actual: &str) -> bool {
    // Check if expected has t0, t1 etc. and actual has $t0, $t1 etc. (or vice versa)
    let exp_has_bare_temps = has_bare_temps(expected);
    let act_has_dollar_temps = has_dollar_temps(actual);
    let act_has_bare_temps = has_bare_temps(actual);
    let exp_has_dollar_temps = has_dollar_temps(expected);
    (exp_has_bare_temps && act_has_dollar_temps) || (exp_has_dollar_temps && act_has_bare_temps)
}

fn has_bare_temps(s: &str) -> bool {
    // Look for standalone t0, t1 etc. that are not preceded by $
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b't' && bytes[i + 1].is_ascii_digit() {
            // Check it's not preceded by $ or alphanumeric
            if i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'$' && bytes[i - 1] != b'_') {
                return true;
            }
        }
    }
    false
}

fn has_dollar_temps(s: &str) -> bool {
    s.contains("$t0") || s.contains("$t1") || s.contains("$t2")
}

/// SLOT_COUNT_DIFF: `_c(N)` differs between expected and actual
fn check_slot_count_diff(expected: &str, actual: &str) -> bool {
    let exp_slots = extract_all_c_counts(expected);
    let act_slots = extract_all_c_counts(actual);
    exp_slots != act_slots
}

fn extract_all_c_counts(js: &str) -> Vec<u32> {
    let mut counts = Vec::new();
    let mut search = js;
    while let Some(pos) = search.find("_c(") {
        let rest = &search[pos + 3..];
        if let Some(end) = rest.find(')') {
            if let Ok(n) = rest[..end].parse::<u32>() {
                counts.push(n);
            }
        }
        search = &search[pos + 3..];
    }
    counts
}

/// Count the number of token-level differences between two normalized strings
fn token_diff_count(a: &str, b: &str) -> usize {
    let a_tokens: Vec<&str> = a.split_whitespace().collect();
    let b_tokens: Vec<&str> = b.split_whitespace().collect();
    let mut diffs = 0;
    let max_len = a_tokens.len().max(b_tokens.len());
    for i in 0..max_len {
        match (a_tokens.get(i), b_tokens.get(i)) {
            (Some(at), Some(bt)) => {
                if at != bt {
                    diffs += 1;
                }
            }
            _ => diffs += 1,
        }
    }
    diffs
}

#[derive(Debug, Clone)]
struct FixtureResult {
    name: String,
    categories: Vec<String>,
}

fn main() {
    // Use a large stack for complex fixtures
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handler = builder
        .spawn(run_analysis)
        .expect("Failed to spawn analysis thread");
    handler.join().expect("Analysis thread panicked");
}

fn run_analysis() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("js" | "jsx" | "ts" | "tsx")
            )
        })
        .collect();
    paths.sort();

    let category_names = [
        "EXTRA_BLANK_LINE",
        "CONST_VS_LET",
        "WRONG_ENTRYPOINT",
        "EXTRA_IMPORT",
        "MISSING_IMPORT",
        "MISSING_BB_LABEL",
        "TEMP_NAME_DIFF",
        "SLOT_COUNT_DIFF",
    ];

    let mut total_non_error = 0usize;
    let mut total_compiled = 0usize;
    let mut total_correct = 0usize;
    let mut wrong_fixtures: Vec<FixtureResult> = Vec::new();
    let mut compile_failures = 0usize;

    for path in &paths {
        if is_error_fixture(path) {
            continue;
        }
        // Skip Flow files
        if let Ok(src) = std::fs::read_to_string(path) {
            if src.lines().next().unwrap_or("").contains("@flow") {
                continue;
            }
        }

        total_non_error += 1;

        let stem = path.file_stem().unwrap().to_str().unwrap();
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", stem));
        let expected_raw = match std::fs::read_to_string(&expect_path) {
            Ok(md) => match parse_expected_code(&md) {
                Some(e) => e,
                None => {
                    total_correct += 1;
                    continue;
                }
            },
            Err(_) => {
                total_correct += 1;
                continue;
            }
        };

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let source_type = source_type_for(path);
        let opts = CompileOptions {
            source_type,
            filename: Some(path.display().to_string()),
            ..Default::default()
        };

        let actual_raw = match compile(&source, opts) {
            Ok(o) => {
                total_compiled += 1;
                o.js
            }
            Err(_) => {
                compile_failures += 1;
                continue;
            }
        };

        // Check with full normalize_js
        if normalize_js(&actual_raw) == normalize_js(&expected_raw) {
            total_correct += 1;
            continue;
        }

        // This is a WRONG fixture -- classify it
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let mut categories = Vec::new();

        if check_extra_blank_line(&expected_raw, &actual_raw) {
            categories.push("EXTRA_BLANK_LINE".to_string());
        }
        if check_const_vs_let(&expected_raw, &actual_raw) {
            categories.push("CONST_VS_LET".to_string());
        }
        if check_wrong_entrypoint(&expected_raw, &actual_raw) {
            categories.push("WRONG_ENTRYPOINT".to_string());
        }
        if check_extra_import(&expected_raw, &actual_raw) {
            categories.push("EXTRA_IMPORT".to_string());
        }
        if check_missing_import(&expected_raw, &actual_raw) {
            categories.push("MISSING_IMPORT".to_string());
        }
        if check_missing_bb_label(&expected_raw, &actual_raw) {
            categories.push("MISSING_BB_LABEL".to_string());
        }
        if check_temp_name_diff(&expected_raw, &actual_raw) {
            categories.push("TEMP_NAME_DIFF".to_string());
        }
        if check_slot_count_diff(&expected_raw, &actual_raw) {
            categories.push("SLOT_COUNT_DIFF".to_string());
        }

        wrong_fixtures.push(FixtureResult { name, categories });
    }

    // --- Build category stats ---
    let mut cat_counts: HashMap<&str, Vec<String>> = HashMap::new();
    for cat in &category_names {
        cat_counts.insert(cat, Vec::new());
    }
    for fixture in &wrong_fixtures {
        for cat in &fixture.categories {
            if let Some(list) = cat_counts.get_mut(cat.as_str()) {
                list.push(fixture.name.clone());
            }
        }
    }

    // Exclusive counts: fixtures that have ONLY this one category
    let mut cat_exclusive: HashMap<&str, Vec<String>> = HashMap::new();
    for cat in &category_names {
        cat_exclusive.insert(cat, Vec::new());
    }
    for fixture in &wrong_fixtures {
        if fixture.categories.len() == 1 {
            if let Some(list) = cat_exclusive.get_mut(fixture.categories[0].as_str()) {
                list.push(fixture.name.clone());
            }
        }
    }

    // Uncategorized count
    let uncategorized: Vec<&FixtureResult> = wrong_fixtures
        .iter()
        .filter(|f| f.categories.is_empty())
        .collect();

    // --- Print results ---
    println!("=== Pattern Analysis of Wrong Fixtures ===");
    println!();
    println!("Total non-error fixtures:  {}", total_non_error);
    println!("Compiled successfully:     {}", total_compiled);
    println!("Compile failures:          {}", compile_failures);
    println!("Output correct:            {}", total_correct);
    println!(
        "Output WRONG:              {}",
        wrong_fixtures.len()
    );
    println!(
        "Correct rate:              {:.1}%",
        total_correct as f64 / total_non_error as f64 * 100.0
    );
    println!();
    println!("=== Category Breakdown (not mutually exclusive) ===");
    println!(
        "{:<22} {:>8} {:>12}  {}",
        "Category", "Affected", "Exclusive", "Examples"
    );
    println!("{}", "-".repeat(90));

    for cat in &category_names {
        let affected = cat_counts.get(cat).map(|v| v.len()).unwrap_or(0);
        let exclusive = cat_exclusive.get(cat).map(|v| v.len()).unwrap_or(0);
        let examples: Vec<&str> = cat_counts
            .get(cat)
            .map(|v| {
                v.iter()
                    .take(3)
                    .map(|s| s.as_str())
                    .collect::<Vec<&str>>()
            })
            .unwrap_or_default();
        println!(
            "{:<22} {:>8} {:>12}  {}",
            cat,
            affected,
            exclusive,
            examples.join(", ")
        );
    }

    println!("{}", "-".repeat(90));
    println!(
        "Uncategorized (none of the above): {}",
        uncategorized.len()
    );
    if !uncategorized.is_empty() {
        println!("  First 10 uncategorized fixtures:");
        for f in uncategorized.iter().take(10) {
            println!("    {}", f.name);
        }
    }

    // Multi-category stats
    let multi: Vec<&FixtureResult> = wrong_fixtures
        .iter()
        .filter(|f| f.categories.len() > 1)
        .collect();
    println!();
    println!("Fixtures with multiple categories: {}", multi.len());
    if !multi.is_empty() {
        println!("  First 5 multi-category fixtures:");
        for f in multi.iter().take(5) {
            println!("    {} => [{}]", f.name, f.categories.join(", "));
        }
    }

    // Summary: how many wrong fixtures would be fixed by each category alone
    println!();
    println!("=== Potential Impact (exclusive fixes) ===");
    println!("If we fixed ONLY fixtures where a single category is the sole issue:");
    let mut total_exclusive = 0;
    for cat in &category_names {
        let exclusive = cat_exclusive.get(cat).map(|v| v.len()).unwrap_or(0);
        if exclusive > 0 {
            println!(
                "  {:<22} => {} more fixtures correct",
                cat, exclusive
            );
            total_exclusive += exclusive;
        }
    }
    println!(
        "  Total exclusive fixes:    {} (from {} wrong)",
        total_exclusive,
        wrong_fixtures.len()
    );
    println!(
        "  New correct rate:         {:.1}%",
        (total_correct + total_exclusive) as f64 / total_non_error as f64 * 100.0
    );
}
