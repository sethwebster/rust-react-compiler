use std::path::{Path, PathBuf};
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
            if c == b'\'' && prev != b'\\' { in_single_quote = false; }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if in_double_quote {
            if c == b'"' && prev != b'\\' { in_double_quote = false; }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'\'' { in_single_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'"' { in_double_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
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
            && (tokens[i + 1] == "}" || tokens[i + 1] == "]" || tokens[i + 1].starts_with('}') || tokens[i + 1].starts_with(']'))
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

fn expect_md_path(fixture_path: &Path) -> PathBuf {
    let stem = fixture_path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    fixture_path.parent().unwrap_or(fixture_path).join(format!("{}.expect.md", stem))
}

fn run_fixture(path: &Path) -> Result<String, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("read error: {}", e))?;
    let source_type = source_type_for(path);
    let opts = CompileOptions {
        source_type,
        filename: Some(path.display().to_string()),
        ..Default::default()
    };
    compile(&source, opts).map(|o| o.js).map_err(|e| e.to_string())
}

/// Count differing lines between two normalized strings (split on single space as token separator,
/// but we actually want meaningful lines so we re-split the normalized output on semicolons and braces).
/// Actually, the simplest approach: split the normalized single-line string back into "lines" by
/// splitting on certain delimiters, or just compare raw normalized line-by-line from the original.
/// Let's normalize each line individually and compare line-by-line.
fn line_diff_count(expected: &str, actual: &str) -> usize {
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();
    let max_len = exp_lines.len().max(act_lines.len());
    let mut diff = 0;
    for i in 0..max_len {
        let e = exp_lines.get(i).map(|s| normalize_js(s)).unwrap_or_default();
        let a = act_lines.get(i).map(|s| normalize_js(s)).unwrap_or_default();
        if e != a {
            diff += 1;
        }
    }
    diff
}

fn line_diff_details(expected: &str, actual: &str) -> Vec<String> {
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();
    let max_len = exp_lines.len().max(act_lines.len());
    let mut diffs = Vec::new();
    for i in 0..max_len {
        let e_raw = exp_lines.get(i).copied().unwrap_or("");
        let a_raw = act_lines.get(i).copied().unwrap_or("");
        let e = normalize_js(e_raw);
        let a = normalize_js(a_raw);
        if e != a {
            diffs.push(format!("  line {}:\n    expected: {}\n    actual:   {}", i + 1, e_raw.trim(), a_raw.trim()));
        }
    }
    diffs
}

struct NearMiss {
    name: String,
    diff_count: usize,
    expected_raw: String,
    actual_raw: String,
}

fn main() {
    // Use a large stack for complex fixtures
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handle = builder.spawn(run).expect("spawn thread");
    handle.join().expect("join thread");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("js" | "jsx" | "ts" | "tsx")
        ))
        .collect();
    paths.sort();

    let mut near_misses: Vec<NearMiss> = Vec::new();
    let mut total = 0usize;
    let mut skipped = 0usize;
    let mut compile_fail = 0usize;
    let mut correct = 0usize;

    for path in &paths {
        if is_error_fixture(path) {
            skipped += 1;
            continue;
        }

        // Skip @flow files
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                skipped += 1;
                continue;
            }
        }

        total += 1;

        // Check we have expected output
        let expect_path = expect_md_path(path);
        let md = match std::fs::read_to_string(&expect_path) {
            Ok(m) => m,
            Err(_) => { skipped += 1; continue; }
        };
        let expected = match parse_expected_code(&md) {
            Some(e) => e,
            None => { skipped += 1; continue; }
        };

        // Try to compile
        let actual = match run_fixture(path) {
            Ok(js) => js,
            Err(_) => { compile_fail += 1; continue; }
        };

        let norm_expected = normalize_js(&expected);
        let norm_actual = normalize_js(&actual);

        if norm_expected == norm_actual {
            correct += 1;
            continue;
        }

        let diff_count = line_diff_count(&expected, &actual);
        let name = path.file_name().unwrap().to_str().unwrap().to_string();

        near_misses.push(NearMiss {
            name,
            diff_count,
            expected_raw: expected,
            actual_raw: actual,
        });
    }

    // Sort by fewest different lines
    near_misses.sort_by_key(|nm| nm.diff_count);

    println!("=== Near-Miss Analysis ===");
    println!("Total non-error fixtures examined: {}", total);
    println!("Skipped (no expected / @flow / error): {}", skipped);
    println!("Compile failures: {}", compile_fail);
    println!("Already correct: {}", correct);
    println!("Wrong (have diffs): {}", near_misses.len());
    println!();

    // Summary: all fixtures sorted by diff count
    println!("=== All wrong fixtures by diff count ===");
    for (i, nm) in near_misses.iter().enumerate() {
        println!("{:4}. [{}  diff lines] {}", i + 1, nm.diff_count, nm.name);
    }

    println!();
    println!("=== Top 30 near-misses with diffs ===");
    for (i, nm) in near_misses.iter().take(30).enumerate() {
        let diffs = line_diff_details(&nm.expected_raw, &nm.actual_raw);
        println!("--- #{} ({} diff lines): {} ---", i + 1, nm.diff_count, nm.name);
        for d in &diffs {
            println!("{}", d);
        }
        println!();
    }
}
