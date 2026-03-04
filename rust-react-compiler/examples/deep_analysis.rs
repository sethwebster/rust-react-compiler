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
    let tokens: Vec<&str> = js.split_whitespace().collect();
    tokens.join(" ")
}

fn extract_c_count(js: &str) -> Option<u32> {
    if let Some(pos) = js.find("_c(") {
        let rest = &js[pos + 3..];
        if let Some(end) = rest.find(')') {
            return rest[..end].parse().ok();
        }
    }
    None
}

fn count_if_dollar(js: &str) -> usize {
    js.matches("if ($[").count()
}

fn count_sentinel(js: &str) -> usize {
    js.matches("Symbol.for(\"react.memo_cache_sentinel\")").count()
}

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    let mut slots_more = 0u32;
    let mut slots_fewer = 0u32;
    let mut slots_same = 0u32;
    let mut scopes_more = 0u32;
    let mut scopes_fewer = 0u32;
    let mut sentinel_more = 0u32; // actual uses more sentinel checks
    let mut sentinel_fewer = 0u32;

    // Track how many slots difference
    let mut slot_diff_hist: std::collections::HashMap<i32, u32> = std::collections::HashMap::new();
    let mut scope_diff_hist: std::collections::HashMap<i32, u32> = std::collections::HashMap::new();

    // Specific pattern checks
    let mut no_memo_in_actual = 0u32; // actual has no _c() at all
    let mut no_memo_in_expected = 0u32;
    let mut actual_has_dollar_var = 0u32; // uses $tN or $t0 raw variable names in output

    let mut mismatched = 0u32;

    for path in &paths {
        let name = path.file_name().unwrap().to_str().unwrap();
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
            Some("tsx") => SourceType::tsx(),
            Some("ts") => SourceType::ts(),
            _ => SourceType::jsx(),
        };
        let opts = CompileOptions {
            source_type,
            filename: Some(path.display().to_string()),
            ..Default::default()
        };

        let actual = match compile(&source, opts) {
            Ok(o) => o.js,
            Err(_) => continue,
        };

        if normalize_js(&actual) == normalize_js(&expected) { continue; }
        mismatched += 1;

        let exp_slots = extract_c_count(&expected);
        let act_slots = extract_c_count(&actual);

        match (exp_slots, act_slots) {
            (Some(e), Some(a)) => {
                let diff = a as i32 - e as i32;
                *slot_diff_hist.entry(diff).or_default() += 1;
                if a > e { slots_more += 1; }
                else if a < e { slots_fewer += 1; }
                else { slots_same += 1; }
            }
            (Some(_), None) => { no_memo_in_actual += 1; }
            (None, Some(_)) => { no_memo_in_expected += 1; }
            _ => {}
        }

        let exp_scopes = count_if_dollar(&expected) as i32;
        let act_scopes = count_if_dollar(&actual) as i32;
        let scope_diff = act_scopes - exp_scopes;
        *scope_diff_hist.entry(scope_diff).or_default() += 1;
        if act_scopes > exp_scopes { scopes_more += 1; }
        else if act_scopes < exp_scopes { scopes_fewer += 1; }

        let exp_sent = count_sentinel(&expected);
        let act_sent = count_sentinel(&actual);
        if act_sent > exp_sent { sentinel_more += 1; }
        if act_sent < exp_sent { sentinel_fewer += 1; }

        // Check for raw $tN variables in actual output
        if actual.contains("$t") {
            actual_has_dollar_var += 1;
        }
    }

    println!("=== Deep Mismatch Analysis ===");
    println!("Total mismatched: {}", mismatched);
    println!();
    println!("--- Cache Slot Count (_c(N)) Direction ---");
    println!("Actual has MORE slots:    {}", slots_more);
    println!("Actual has FEWER slots:   {}", slots_fewer);
    println!("Same slots (other diff):  {}", slots_same);
    println!("No _c() in actual:        {}", no_memo_in_actual);
    println!("No _c() in expected:      {}", no_memo_in_expected);
    println!();
    println!("--- Scope Count Direction ---");
    println!("Actual has MORE scopes:   {}", scopes_more);
    println!("Actual has FEWER scopes:  {}", scopes_fewer);
    println!();
    println!("--- Sentinel Usage ---");
    println!("Actual uses MORE sentinel checks:  {}", sentinel_more);
    println!("Actual uses FEWER sentinel checks: {}", sentinel_fewer);
    println!();
    println!("--- Other Patterns ---");
    println!("Actual has raw $tN vars:  {}", actual_has_dollar_var);
    println!();

    println!("--- Slot diff histogram (actual - expected) ---");
    let mut diffs: Vec<_> = slot_diff_hist.into_iter().collect();
    diffs.sort_by_key(|(k, _)| *k);
    for (diff, count) in &diffs {
        if *count >= 5 {
            println!("  {:+3} slots: {} fixtures", diff, count);
        }
    }

    println!();
    println!("--- Scope diff histogram (actual - expected) ---");
    let mut sdiffs: Vec<_> = scope_diff_hist.into_iter().collect();
    sdiffs.sort_by_key(|(k, _)| *k);
    for (diff, count) in &sdiffs {
        if *count >= 5 {
            println!("  {:+3} scopes: {} fixtures", diff, count);
        }
    }
}
