use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

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
        if in_block_comment { if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' { in_block_comment = false; i += 2; continue; } i += 1; continue; }
        if in_single_quote { if c == b'\'' && prev != b'\\' { in_single_quote = false; } stripped.push(c as char); prev = c; i += 1; continue; }
        if in_double_quote { if c == b'"' && prev != b'\\' { in_double_quote = false; } stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'\'' { in_single_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'"' { in_double_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' { while i < bytes.len() && bytes[i] != b'\n' { i += 1; } continue; }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' { in_block_comment = true; i += 2; continue; }
        stripped.push(c as char); prev = c; i += 1;
    }
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let mut result = String::new();
    for (i, &tok) in tokens.iter().enumerate() {
        let effective = if (tok.ends_with(',') || tok == ",") && i + 1 < tokens.len()
            && (tokens[i + 1] == "}" || tokens[i + 1] == "]" || tokens[i + 1].starts_with('}') || tokens[i + 1].starts_with(']'))
        { &tok[..tok.len() - 1] } else { tok };
        if effective.is_empty() { continue; }
        if !result.is_empty() { result.push(' '); }
        result.push_str(effective);
    }
    let result = result.replace("[ ", "[").replace(" ]", "]");
    let result = result.replace("( ", "(").replace(" )", ")");
    let result = result.replace("{ }", "{}");
    let result = result.replace("return undefined;", "return;");
    let result = result.replace("} else {}", "}");
    result
}

fn extract_c_count(js: &str) -> Option<u32> {
    if let Some(pos) = js.find("_c(") {
        let rest = &js[pos + 3..];
        if let Some(end) = rest.find(')') { return rest[..end].parse().ok(); }
    }
    None
}

fn count_if_dollar(js: &str) -> usize { js.matches("if ($[").count() }

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    let mut total_mismatch = 0u32;

    // Primary categories (mutually exclusive priority order)
    let mut raw_tvar = 0u32;           // Has $tN raw variable names in output
    let mut no_memo_actual = 0u32;     // Expected has _c() but actual doesn't
    let mut no_memo_expected = 0u32;   // Actual has _c() but expected doesn't (passthrough)
    let mut slot_diff = 0u32;          // Different _c(N) count
    let mut scope_count_diff = 0u32;   // Same slots but different # of if($[]) blocks
    let mut dep_check_diff = 0u32;     // Same slots+scopes but different dependency expressions
    let mut code_body_diff = 0u32;     // Same slots+scopes+deps but different code inside scopes
    let mut other = 0u32;

    // Sub-categories
    let mut slot_more = 0u32;
    let mut slot_fewer = 0u32;
    let mut early_return_missing = 0u32;
    let mut sentinel_overuse = 0u32;   // Uses sentinel where expected uses !== dep check

    // Specific issue tracking
    let mut raw_tvar_samples: Vec<String> = Vec::new();

    for path in &paths {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if name.starts_with("error.") || name.starts_with("todo.error.") { continue; }
        if let Ok(src) = std::fs::read_to_string(path) {
            if src.lines().next().unwrap_or("").contains("@flow") { continue; }
        }

        let stem = path.file_stem().unwrap().to_str().unwrap();
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", stem));
        let expected = match std::fs::read_to_string(&expect_path) {
            Ok(md) => match parse_expected_code(&md) { Some(e) => e, None => continue },
            Err(_) => continue,
        };
        let source = match std::fs::read_to_string(path) { Ok(s) => s, Err(_) => continue };
        let source_type = match path.extension().and_then(|e| e.to_str()) {
            Some("tsx") => SourceType::tsx(), Some("ts") => SourceType::ts(), _ => SourceType::jsx(),
        };
        let opts = CompileOptions { source_type, filename: Some(path.display().to_string()), ..Default::default() };
        let actual = match compile(&source, opts) { Ok(o) => o.js, Err(_) => continue };

        if normalize_js(&actual) == normalize_js(&expected) { continue; }
        total_mismatch += 1;

        // Priority classification
        if actual.contains("$t") && !expected.contains("$t") {
            raw_tvar += 1;
            if raw_tvar_samples.len() < 5 { raw_tvar_samples.push(name); }
            continue;
        }

        let exp_slots = extract_c_count(&expected);
        let act_slots = extract_c_count(&actual);

        match (exp_slots, act_slots) {
            (Some(_), None) => { no_memo_actual += 1; continue; }
            (None, Some(_)) => { no_memo_expected += 1; continue; }
            (Some(e), Some(a)) if e != a => {
                slot_diff += 1;
                if a > e { slot_more += 1; } else { slot_fewer += 1; }
                // Check sub-issues
                if expected.contains("bb0:") && !actual.contains("bb0:") { early_return_missing += 1; }
                continue;
            }
            _ => {}
        }

        // Same slot count - check scope count
        let exp_scopes = count_if_dollar(&expected);
        let act_scopes = count_if_dollar(&actual);
        if exp_scopes != act_scopes {
            scope_count_diff += 1;
            // Check sentinel overuse
            let exp_sent = expected.matches("Symbol.for(\"react.memo_cache_sentinel\")").count();
            let act_sent = actual.matches("Symbol.for(\"react.memo_cache_sentinel\")").count();
            if act_sent > exp_sent { sentinel_overuse += 1; }
            continue;
        }

        // Same slots and scopes - check dependency expressions
        let exp_deps = extract_dep_checks(&expected);
        let act_deps = extract_dep_checks(&actual);
        if exp_deps != act_deps {
            dep_check_diff += 1;
            continue;
        }

        // Everything structural is the same - code body differs
        code_body_diff += 1;
    }

    println!("=== Final Analysis of 744 True Mismatches ===");
    println!("Total true mismatches: {}", total_mismatch);
    println!();
    println!("=== PRIORITY CLASSIFICATION (mutually exclusive) ===");
    println!("1. Raw $tN variables leaked:     {:>4}  ({:.1}%)", raw_tvar, raw_tvar as f64 / total_mismatch as f64 * 100.0);
    println!("2. Missing _c() in actual:       {:>4}  ({:.1}%)", no_memo_actual, no_memo_actual as f64 / total_mismatch as f64 * 100.0);
    println!("3. Unexpected _c() in actual:    {:>4}  ({:.1}%)", no_memo_expected, no_memo_expected as f64 / total_mismatch as f64 * 100.0);
    println!("4. Different slot count:         {:>4}  ({:.1}%)", slot_diff, slot_diff as f64 / total_mismatch as f64 * 100.0);
    println!("   - actual MORE slots:          {:>4}", slot_more);
    println!("   - actual FEWER slots:         {:>4}", slot_fewer);
    println!("   - early return (bb0:) miss:   {:>4}", early_return_missing);
    println!("5. Different scope count:        {:>4}  ({:.1}%)", scope_count_diff, scope_count_diff as f64 / total_mismatch as f64 * 100.0);
    println!("   - sentinel overuse:           {:>4}", sentinel_overuse);
    println!("6. Different dep expressions:    {:>4}  ({:.1}%)", dep_check_diff, dep_check_diff as f64 / total_mismatch as f64 * 100.0);
    println!("7. Code body differs:            {:>4}  ({:.1}%)", code_body_diff, code_body_diff as f64 / total_mismatch as f64 * 100.0);
    println!();
    println!("Raw $tN samples: {:?}", raw_tvar_samples);
}

fn extract_dep_checks(js: &str) -> Vec<String> {
    js.lines().map(|l| l.trim()).filter(|l| l.starts_with("if ($[") && l.contains("!==")).map(|s| s.to_string()).collect()
}
