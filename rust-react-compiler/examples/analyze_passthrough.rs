use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

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
        if in_single_quote {
            if c == b'\'' && prev != b'\\' { in_single_quote = false; }
            stripped.push(c as char); prev = c; i += 1; continue;
        }
        if in_double_quote {
            if c == b'"' && prev != b'\\' { in_double_quote = false; }
            stripped.push(c as char); prev = c; i += 1; continue;
        }
        if c == b'/' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'/' { while i < bytes.len() && bytes[i] != b'\n' { i += 1; } continue; }
            if bytes[i + 1] == b'*' { in_block_comment = true; i += 2; continue; }
        }
        if c == b'\'' { in_single_quote = true; }
        if c == b'"' { in_double_quote = true; }
        stripped.push(c as char);
        prev = c; i += 1;
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

fn parse_expected_code(md: &str) -> Option<String> {
    let start = md.find("## Code\n\n```javascript\n")?;
    let after_fence = start + "## Code\n\n```javascript\n".len();
    let end = md[after_fence..].find("\n```")?;
    Some(md[after_fence..after_fence + end].to_string())
}

fn main() {
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handle = builder.spawn(run).expect("spawn");
    handle.join().expect("join");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js"|"jsx"|"ts"|"tsx")))
        .collect();
    entries.sort();

    let mut categories: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();

    for path in &entries {
        let name = path.file_name().unwrap().to_str().unwrap();
        if name.starts_with("error.") || name.starts_with("todo.error.") { continue; }
        let source = match std::fs::read_to_string(path) { Ok(s) => s, Err(_) => continue };
        let first = source.lines().next().unwrap_or("");
        if first.contains("@flow") { continue; }

        let source_type = match path.extension().and_then(|e| e.to_str()) {
            Some("tsx") => SourceType::tsx(), Some("ts") => SourceType::ts(),
            Some("jsx") | Some("js") => SourceType::jsx(), _ => SourceType::mjs(),
        };
        let opts = CompileOptions { source_type, filename: Some(path.display().to_string()), ..Default::default() };
        let actual = match compile(&source, opts) { Ok(o) => o.js, Err(_) => continue };
        
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", path.file_stem().unwrap().to_str().unwrap()));
        let md = match std::fs::read_to_string(&expect_path) { Ok(s) => s, Err(_) => continue };
        let expected = match parse_expected_code(&md) { Some(s) => s, None => continue };

        let na = normalize_js(&actual);
        let ne = normalize_js(&expected);
        if na == ne { continue; }

        if actual.contains("_c(") || expected.contains("_c(") { continue; }

        // Both passthrough — categorize the diff
        let at: Vec<&str> = na.split_whitespace().collect();
        let et: Vec<&str> = ne.split_whitespace().collect();
        
        // Find first diff
        let mut first_diff = String::new();
        if at.len() == et.len() {
            for (i, (a, e)) in at.iter().zip(et.iter()).enumerate() {
                if a != e {
                    let ctx_s = if i > 2 { i - 2 } else { 0 };
                    let ctx_e = (i + 3).min(at.len());
                    first_diff = format!("same-len: ours[{}]='{}' exp[{}]='{}' ctx: '...{} | ...{}'", 
                        i, a, i, e,
                        at[ctx_s..ctx_e].join(" "),
                        et[ctx_s..ctx_e].join(" "));
                    break;
                }
            }
        } else {
            first_diff = format!("len-diff: ours={} exp={}", at.len(), et.len());
        }
        
        // Check for common patterns
        let cat = if na.contains("export default") != ne.contains("export default") {
            "export_default_diff"
        } else if actual.contains("import") && expected.contains("import") && na.find("function") != ne.find("function") {
            "import_diff"
        } else if na.len().abs_diff(ne.len()) < 20 {
            "small_text_diff"
        } else {
            "other"
        };
        
        categories.entry(cat.to_string()).or_default().push(format!("{}: {}", name, first_diff));
    }

    for (cat, items) in &categories {
        println!("=== {} ({}) ===", cat, items.len());
        for item in items.iter().take(10) {
            println!("  {}", item);
        }
        if items.len() > 10 { println!("  ... and {} more", items.len() - 10); }
        println!();
    }
}
