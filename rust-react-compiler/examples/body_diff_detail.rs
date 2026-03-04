use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;
use std::collections::HashMap;

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

    let mut categories: HashMap<String, u32> = HashMap::new();
    let mut total = 0u32;

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

        // This is a "code body differs" case
        total += 1;

        // Classify the specific difference
        let exp_norm = normalize_js(&expected);
        let act_norm = normalize_js(&actual);

        // Find first differing token
        let exp_tokens: Vec<&str> = exp_norm.split_whitespace().collect();
        let act_tokens: Vec<&str> = act_norm.split_whitespace().collect();

        let mut diff_context = String::new();
        for i in 0..exp_tokens.len().min(act_tokens.len()) {
            if exp_tokens[i] != act_tokens[i] {
                let start = if i > 3 { i - 3 } else { 0 };
                let end = (i + 5).min(exp_tokens.len()).min(act_tokens.len());
                diff_context = format!(
                    "exp='{}' act='{}'",
                    exp_tokens[start..end].join(" "),
                    act_tokens[start..end].join(" ")
                );
                break;
            }
        }
        if diff_context.is_empty() && exp_tokens.len() != act_tokens.len() {
            diff_context = format!("length diff: exp={} act={}", exp_tokens.len(), act_tokens.len());
        }

        // Categorize
        let cat = if expected.contains("const t0 =") && !actual.contains("const t0 =") && actual.contains("let t0") {
            "const-vs-let-t0"
        } else if diff_context.contains("const") || diff_context.contains("let") {
            "var-decl-diff"
        } else if diff_context.contains("<") || diff_context.contains("/>") {
            "jsx-diff"
        } else if diff_context.contains("return") {
            "return-diff"
        } else if diff_context.contains("$[") {
            "store-order-diff"
        } else if diff_context.contains("function") || diff_context.contains("=>") {
            "function-body-diff"
        } else {
            "other"
        };

        *categories.entry(cat.to_string()).or_default() += 1;

        // Print first few samples for each category
        if *categories.get(cat).unwrap() <= 2 {
            println!("[{}] {}", cat, name);
            println!("  {}", diff_context);
        }
    }

    println!("\n=== Code Body Diff Categories (same slots, scopes, deps) ===");
    println!("Total: {}", total);
    let mut cats: Vec<_> = categories.into_iter().collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1));
    for (cat, count) in &cats {
        println!("  {:<25} {:>4}  ({:.1}%)", cat, count, *count as f64 / total as f64 * 100.0);
    }
}
