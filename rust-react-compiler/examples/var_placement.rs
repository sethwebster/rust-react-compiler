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

fn extract_dep_checks(js: &str) -> Vec<String> {
    js.lines().map(|l| l.trim()).filter(|l| l.starts_with("if ($[") && l.contains("!==")).map(|s| s.to_string()).collect()
}

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    let mut actual_moves_inside = 0u32;  // actual puts const/let INSIDE scope that expected has OUTSIDE
    let mut expected_moves_inside = 0u32; // expected puts const/let INSIDE scope that actual has OUTSIDE
    let mut both_diff_placement = 0u32;
    let mut samples_in: Vec<String> = Vec::new();
    let mut samples_out: Vec<String> = Vec::new();

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
        if actual.contains("$t") && !expected.contains("$t") { continue; }
        let exp_slots = extract_c_count(&expected);
        let act_slots = extract_c_count(&actual);
        if exp_slots != act_slots { continue; }
        let exp_scopes = expected.matches("if ($[").count();
        let act_scopes = actual.matches("if ($[").count();
        if exp_scopes != act_scopes { continue; }
        let exp_deps = extract_dep_checks(&expected);
        let act_deps = extract_dep_checks(&actual);
        if exp_deps != act_deps { continue; }

        // Now compare: which declarations are inside/outside scope blocks
        let exp_outside = count_decls_outside_scope(&expected);
        let act_outside = count_decls_outside_scope(&actual);
        let exp_inside = count_decls_inside_scope(&expected);
        let act_inside = count_decls_inside_scope(&actual);

        if act_inside > exp_inside && act_outside < exp_outside {
            actual_moves_inside += 1;
            if samples_in.len() < 5 {
                samples_in.push(format!("{} (exp_out={}, act_out={}, exp_in={}, act_in={})",
                    name, exp_outside, act_outside, exp_inside, act_inside));
            }
        } else if act_inside < exp_inside && act_outside > exp_outside {
            expected_moves_inside += 1;
            if samples_out.len() < 5 {
                samples_out.push(format!("{} (exp_out={}, act_out={}, exp_in={}, act_in={})",
                    name, exp_outside, act_outside, exp_inside, act_inside));
            }
        } else if exp_inside != act_inside || exp_outside != act_outside {
            both_diff_placement += 1;
        }
    }

    println!("=== Variable Placement Analysis ===");
    println!("Actual moves decls INSIDE scope (expected has them outside): {}", actual_moves_inside);
    println!("Actual moves decls OUTSIDE scope (expected has them inside): {}", expected_moves_inside);
    println!("Other placement diff:                                        {}", both_diff_placement);
    println!();
    println!("--- Actual moves inside samples ---");
    for s in &samples_in { println!("  {}", s); }
    println!();
    println!("--- Actual moves outside samples ---");
    for s in &samples_out { println!("  {}", s); }
}

fn count_decls_outside_scope(js: &str) -> usize {
    // Count const/let declarations that are NOT within an if ($[]) block
    let mut count = 0;
    let mut depth = 0;
    let mut in_scope = false;
    for line in js.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("if ($[") { in_scope = true; depth = 0; }
        if in_scope {
            depth += trimmed.matches('{').count() as i32;
            depth -= trimmed.matches('}').count() as i32;
            if depth <= 0 { in_scope = false; }
        }
        if !in_scope && (trimmed.starts_with("const ") || trimmed.starts_with("let ")) {
            count += 1;
        }
    }
    count
}

fn count_decls_inside_scope(js: &str) -> usize {
    let mut count = 0;
    let mut depth = 0;
    let mut in_scope = false;
    for line in js.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("if ($[") { in_scope = true; depth = 0; }
        if in_scope {
            depth += trimmed.matches('{').count() as i32;
            depth -= trimmed.matches('}').count() as i32;
            if depth <= 0 { in_scope = false; }
        }
        if in_scope && (trimmed.starts_with("const ") || trimmed.starts_with("let ")) {
            count += 1;
        }
    }
    count
}
