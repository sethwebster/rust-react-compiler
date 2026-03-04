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

/// The normalization from fixtures.rs
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
            if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' { in_block_comment = false; i += 2; continue; }
            i += 1; continue;
        }
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
        let effective = if (tok.ends_with(',') || tok == ",")
            && i + 1 < tokens.len()
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

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    // Check the fixture that looks like it should match
    let name = "alias-nested-member-path.js";
    let path = dir.join(name);
    let source = std::fs::read_to_string(&path).unwrap();
    let opts = CompileOptions {
        source_type: SourceType::jsx(),
        filename: Some(path.display().to_string()),
        ..Default::default()
    };
    let actual = compile(&source, opts).unwrap().js;

    let stem = path.file_stem().unwrap().to_str().unwrap();
    let expect_path = path.parent().unwrap().join(format!("{}.expect.md", stem));
    let md = std::fs::read_to_string(&expect_path).unwrap();
    let expected = parse_expected_code(&md).unwrap();

    let norm_act = normalize_js(&actual);
    let norm_exp = normalize_js(&expected);

    if norm_act == norm_exp {
        println!("MATCH after normalization!");
    } else {
        println!("STILL DIFFERENT after normalization");
        // Find first difference
        let a_chars: Vec<char> = norm_act.chars().collect();
        let e_chars: Vec<char> = norm_exp.chars().collect();
        for i in 0..a_chars.len().min(e_chars.len()) {
            if a_chars[i] != e_chars[i] {
                let start = if i > 30 { i - 30 } else { 0 };
                let end_a = (i + 50).min(a_chars.len());
                let end_e = (i + 50).min(e_chars.len());
                println!("First diff at char {}:", i);
                println!("  ACT: ...{}...", a_chars[start..end_a].iter().collect::<String>());
                println!("  EXP: ...{}...", e_chars[start..end_e].iter().collect::<String>());
                break;
            }
        }
    }

    // Also count how many of the 969 mismatches become matches with proper normalization
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    let mut mismatched_simple = 0u32;
    let mut mismatched_proper = 0u32;
    let mut rescued = 0u32;

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
            Some("tsx") => SourceType::tsx(), Some("ts") => SourceType::ts(), _ => SourceType::jsx(),
        };
        let opts = CompileOptions { source_type, filename: Some(path.display().to_string()), ..Default::default() };
        let actual = match compile(&source, opts) { Ok(o) => o.js, Err(_) => continue };

        let simple_norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        if simple_norm(&actual) != simple_norm(&expected) {
            mismatched_simple += 1;
            if normalize_js(&actual) != normalize_js(&expected) {
                mismatched_proper += 1;
            } else {
                rescued += 1;
            }
        }
    }

    println!("\n=== Normalization Impact ===");
    println!("Simple whitespace-only mismatches: {}", mismatched_simple);
    println!("After proper normalization:         {}", mismatched_proper);
    println!("Rescued by normalization:           {}", rescued);
}
