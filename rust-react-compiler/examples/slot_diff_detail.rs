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

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    // For "fewer slots" cases (214): why does actual have fewer?
    let mut fewer_no_inner_scope = 0u32;  // expected has inner scopes, actual merges them
    let mut fewer_no_early_return = 0u32; // expected has early return, actual doesn't
    let mut fewer_samples: Vec<(String, u32, u32)> = Vec::new();

    // For "more slots" cases (90): why does actual have more?
    let mut more_split_scopes = 0u32;    // actual splits what expected combines
    let mut more_samples: Vec<(String, u32, u32)> = Vec::new();

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

        let exp_slots = extract_c_count(&expected).unwrap_or(0);
        let act_slots = extract_c_count(&actual).unwrap_or(0);
        if exp_slots == act_slots { continue; }

        if act_slots < exp_slots {
            // Fewer slots
            let exp_inner = expected.matches("if ($[").count();
            let act_inner = actual.matches("if ($[").count();
            if exp_inner > act_inner { fewer_no_inner_scope += 1; }
            if expected.contains("bb0:") && !actual.contains("bb0:") { fewer_no_early_return += 1; }
            if fewer_samples.len() < 10 { fewer_samples.push((name, exp_slots, act_slots)); }
        } else {
            // More slots
            let exp_inner = expected.matches("if ($[").count();
            let act_inner = actual.matches("if ($[").count();
            if act_inner > exp_inner { more_split_scopes += 1; }
            if more_samples.len() < 10 { more_samples.push((name, exp_slots, act_slots)); }
        }
    }

    println!("=== Slot Count Diff Detail ===");
    println!();
    println!("--- FEWER slots in actual (214 fixtures) ---");
    println!("Missing inner scopes:  {}", fewer_no_inner_scope);
    println!("Missing early return:  {}", fewer_no_early_return);
    println!("Samples:");
    for (name, exp, act) in &fewer_samples {
        println!("  {} (exp={}, act={})", name, exp, act);
    }
    println!();
    println!("--- MORE slots in actual (90 fixtures) ---");
    println!("Over-splitting scopes: {}", more_split_scopes);
    println!("Samples:");
    for (name, exp, act) in &more_samples {
        println!("  {} (exp={}, act={})", name, exp, act);
    }
}
