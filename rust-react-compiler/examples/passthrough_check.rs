/// Analyze "passthrough" fixtures — those where the TS compiler chose NOT to
/// memoize (no `_c(` in expected output) — and check how our compiler handles them.
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

fn main() {
    // Use a large-stack thread to avoid overflow on complex fixtures.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run)
        .expect("spawn")
        .join()
        .expect("join");
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
        .filter(|p| !is_error_fixture(p))
        .collect();
    paths.sort();

    let mut pt_total = 0usize;       // passthrough fixtures
    let mut pt_correct = 0usize;     // passthrough & our output matches
    let mut pt_wrong = 0usize;       // passthrough but our output differs
    let mut pt_wrongly_memoized = 0usize; // passthrough but we emitted _c(
    let mut pt_wrong_other = 0usize; // passthrough, wrong, but no _c( in ours
    let mut pt_compile_fail = 0usize;
    let mut memo_total = 0usize;     // memoized fixtures
    let mut memo_correct = 0usize;
    let mut skipped = 0usize;        // no expected code section

    let mut wrong_memoized_examples: Vec<(String, String, String)> = Vec::new(); // (name, expected_snippet, actual_snippet)
    let mut wrong_other_examples: Vec<(String, String, String)> = Vec::new();

    for path in &paths {
        // Skip Flow files
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                skipped += 1;
                continue;
            }
        }

        let expect_path = expect_md_path(path);
        let expected = match std::fs::read_to_string(&expect_path) {
            Ok(md) => match parse_expected_code(&md) {
                Some(code) => code,
                None => { skipped += 1; continue; }
            },
            Err(_) => { skipped += 1; continue; }
        };

        let is_passthrough = !expected.contains("_c(");

        if is_passthrough {
            pt_total += 1;
            match run_fixture(path) {
                Ok(actual) => {
                    let norm_expected = normalize_js(&expected);
                    let norm_actual = normalize_js(&actual);
                    if norm_actual == norm_expected {
                        pt_correct += 1;
                    } else {
                        pt_wrong += 1;
                        if actual.contains("_c(") {
                            pt_wrongly_memoized += 1;
                            if wrong_memoized_examples.len() < 10 {
                                let fname = path.file_name().unwrap().to_str().unwrap().to_string();
                                let exp_snip = expected.lines().take(5).collect::<Vec<_>>().join("\n");
                                let act_snip = actual.lines().take(5).collect::<Vec<_>>().join("\n");
                                wrong_memoized_examples.push((fname, exp_snip, act_snip));
                            }
                        } else {
                            pt_wrong_other += 1;
                            if wrong_other_examples.len() < 5 {
                                let fname = path.file_name().unwrap().to_str().unwrap().to_string();
                                let exp_snip = expected.lines().take(3).collect::<Vec<_>>().join("\n");
                                let act_snip = actual.lines().take(3).collect::<Vec<_>>().join("\n");
                                wrong_other_examples.push((fname, exp_snip, act_snip));
                            }
                        }
                    }
                }
                Err(_) => { pt_compile_fail += 1; }
            }
        } else {
            memo_total += 1;
            match run_fixture(path) {
                Ok(actual) => {
                    if normalize_js(&actual) == normalize_js(&expected) {
                        memo_correct += 1;
                    }
                }
                Err(_) => {}
            }
        }
    }

    println!("=== Passthrough Fixture Analysis ===");
    println!();
    println!("Passthrough fixtures (no _c( in expected): {}", pt_total);
    println!("  Correct (output matches):                {}", pt_correct);
    println!("  Wrong (output differs):                  {}", pt_wrong);
    println!("    - Wrongly memoized (our output has _c():  {}", pt_wrongly_memoized);
    println!("    - Other mismatch (no _c( in ours):        {}", pt_wrong_other);
    println!("  Compile failures:                        {}", pt_compile_fail);
    println!("  Passthrough accuracy: {:.1}%", if pt_total > 0 { pt_correct as f64 / pt_total as f64 * 100.0 } else { 0.0 });
    println!();
    println!("Memoized fixtures (has _c( in expected):   {}", memo_total);
    println!("  Correct:                                 {}", memo_correct);
    println!("  Memoized accuracy: {:.1}%", if memo_total > 0 { memo_correct as f64 / memo_total as f64 * 100.0 } else { 0.0 });
    println!();
    println!("Skipped (Flow/@flow, no .expect.md, no ## Code): {}", skipped);

    if !wrong_memoized_examples.is_empty() {
        println!();
        println!("=== Examples: Wrongly Memoized Passthroughs (up to 10) ===");
        for (name, exp, act) in &wrong_memoized_examples {
            println!();
            println!("--- {} ---", name);
            println!("  Expected (first 5 lines):");
            for line in exp.lines() {
                println!("    {}", line);
            }
            println!("  Actual (first 5 lines):");
            for line in act.lines() {
                println!("    {}", line);
            }
        }
    }

    if !wrong_other_examples.is_empty() {
        println!();
        println!("=== Examples: Other Wrong Passthroughs (up to 5) ===");
        for (name, exp, act) in &wrong_other_examples {
            println!();
            println!("--- {} ---", name);
            println!("  Expected (first 3 lines):");
            for line in exp.lines() {
                println!("    {}", line);
            }
            println!("  Actual (first 3 lines):");
            for line in act.lines() {
                println!("    {}", line);
            }
        }
    }
}
