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

    let mut infer_mode_passthrough = 0;
    let mut has_pragma = 0;
    let mut no_pragma = 0;
    let mut no_pragma_names = Vec::new();

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

        // Check why it's passthrough
        if first.contains("@compilationMode") && first.contains("infer") {
            infer_mode_passthrough += 1;
        }
        if first.contains("@") {
            has_pragma += 1;
        } else {
            no_pragma += 1;
            no_pragma_names.push(name.to_string());
        }
    }

    println!("Passthrough mismatches with infer mode: {}", infer_mode_passthrough);
    println!("Passthrough mismatches with pragma: {}", has_pragma);
    println!("Passthrough mismatches without pragma: {}", no_pragma);
    println!("\nNo-pragma passthrough fixtures:");
    for n in no_pragma_names.iter().take(30) {
        println!("  {}", n);
    }
}
