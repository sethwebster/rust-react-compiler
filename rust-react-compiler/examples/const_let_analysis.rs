/// Analyzes fixtures where the ONLY difference between our output and the
/// expected output (after normalization) is `let` vs `const`.
use std::path::{Path, PathBuf};
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

// ---------------------------------------------------------------------------
// Helpers (duplicated from tests/fixtures.rs so the example is self-contained)
// ---------------------------------------------------------------------------

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
            && (tokens[i + 1] == "}" || tokens[i + 1] == "]"
                || tokens[i + 1].starts_with('}') || tokens[i + 1].starts_with(']'))
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
    let source = std::fs::read_to_string(path).map_err(|e| format!("read error: {}", e))?;
    let source_type = source_type_for(path);
    let opts = CompileOptions {
        source_type,
        filename: Some(path.display().to_string()),
        ..Default::default()
    };
    compile(&source, opts).map(|o| o.js).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Analysis types
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ConstLetDiff {
    /// Line from the expected output (normalized, before let→const replacement)
    expected_fragment: String,
    /// Line from our actual output (normalized, before let→const replacement)
    actual_fragment: String,
    /// true if this looks like a scope-output variable declaration (`let t0;`)
    is_scope_output_var: bool,
}

/// Classify a differing token window.
/// `expected_tok` will contain "const" where `actual_tok` contains "let" (or vice versa).
fn is_scope_output_var(line: &str) -> bool {
    // Scope output variables look like: `let t0;` or `let t1;` etc.
    // after normalization they are single tokens like `let t0;`
    let trimmed = line.trim();
    if trimmed.starts_with("let ") {
        let rest = &trimmed[4..];
        // Check pattern: t<digits>;
        if rest.starts_with('t') && rest.ends_with(';') {
            let middle = &rest[1..rest.len() - 1];
            return middle.chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // Run with a large stack
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handle = builder.spawn(run_analysis).expect("spawn thread");
    handle.join().expect("join thread");
}

fn run_analysis() {
    let dir = PathBuf::from(FIXTURE_DIR);
    assert!(dir.exists(), "Fixture directory not found: {}", dir.display());

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read fixture dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("js" | "jsx" | "ts" | "tsx")
        ))
        .collect();
    entries.sort();

    let mut const_let_only_fixtures: Vec<(String, Vec<ConstLetDiff>)> = Vec::new();
    let mut total_checked = 0usize;
    let mut total_mismatched = 0usize;

    for path in &entries {
        if is_error_fixture(path) { continue; }

        // Skip Flow files
        if let Ok(src) = std::fs::read_to_string(path) {
            if src.lines().next().unwrap_or("").contains("@flow") { continue; }
        }

        let actual = match run_fixture(path) {
            Ok(js) => js,
            Err(_) => continue,
        };

        let expect_path = expect_md_path(path);
        let md = match std::fs::read_to_string(&expect_path) {
            Ok(md) => md,
            Err(_) => continue,
        };
        let expected = match parse_expected_code(&md) {
            Some(code) => code,
            None => continue,
        };

        total_checked += 1;

        let norm_actual = normalize_js(&actual);
        let norm_expected = normalize_js(&expected);

        if norm_actual == norm_expected {
            continue; // Already matching
        }

        total_mismatched += 1;

        // Replace all "let " with "const " in both strings
        let patched_actual = norm_actual.replace("let ", "const ");
        let patched_expected = norm_expected.replace("let ", "const ");

        if patched_actual != patched_expected {
            continue; // Differences beyond let/const
        }

        // This fixture's ONLY issue is let vs const.
        // Find the specific differing lines.
        let actual_tokens: Vec<&str> = norm_actual.split_whitespace().collect();
        let expected_tokens: Vec<&str> = norm_expected.split_whitespace().collect();

        let mut diffs: Vec<ConstLetDiff> = Vec::new();

        // Walk through both token streams and find where they diverge.
        // Reconstruct "lines" around each difference for context.
        let max_len = actual_tokens.len().max(expected_tokens.len());
        let mut i = 0;
        while i < max_len {
            let at = actual_tokens.get(i).copied().unwrap_or("");
            let et = expected_tokens.get(i).copied().unwrap_or("");
            if at != et {
                // Collect context: a few tokens around the diff
                let start = i.saturating_sub(1);
                let end = (i + 4).min(max_len);
                let actual_ctx: String = actual_tokens[start..end.min(actual_tokens.len())].join(" ");
                let expected_ctx: String = expected_tokens[start..end.min(expected_tokens.len())].join(" ");

                let is_scope_var = is_scope_output_var(&actual_ctx);

                diffs.push(ConstLetDiff {
                    expected_fragment: expected_ctx,
                    actual_fragment: actual_ctx,
                    is_scope_output_var: is_scope_var,
                });
            }
            i += 1;
        }

        let fname = path.file_name().unwrap().to_str().unwrap().to_string();
        const_let_only_fixtures.push((fname, diffs));
    }

    // ---------------------------------------------------------------------------
    // Report
    // ---------------------------------------------------------------------------
    println!("========================================================");
    println!("  const/let ANALYSIS — Fixtures where the ONLY diff is");
    println!("  `let` vs `const` after normalization");
    println!("========================================================");
    println!();
    println!("Total fixtures checked (non-error, compilable, with expected output): {}", total_checked);
    println!("Total mismatched: {}", total_mismatched);
    println!("Fixtures where ONLY diff is let vs const: {}", const_let_only_fixtures.len());
    println!();

    for (name, diffs) in &const_let_only_fixtures {
        let scope_var_count = diffs.iter().filter(|d| d.is_scope_output_var).count();
        let regular_count = diffs.len() - scope_var_count;
        let category = if scope_var_count > 0 && regular_count > 0 {
            "BOTH (scope vars + regular declarations)"
        } else if scope_var_count > 0 {
            "scope output vars (let t0; -> const t0)"
        } else {
            "regular declarations"
        };

        println!("--------------------------------------------------------");
        println!("FIXTURE: {}", name);
        println!("  Category: {}", category);
        println!("  Differing lines ({}):", diffs.len());
        for (i, diff) in diffs.iter().enumerate() {
            println!("    [{}] expected: {}", i + 1, diff.expected_fragment);
            println!("        actual:   {}", diff.actual_fragment);
            if diff.is_scope_output_var {
                println!("        (scope output variable)");
            }
        }
        println!();
    }

    println!("========================================================");
    println!("SUMMARY");
    println!("========================================================");
    println!("Total const/let-only fixtures: {}", const_let_only_fixtures.len());
    println!();
    println!("All fixture names:");
    for (name, _) in &const_let_only_fixtures {
        println!("  - {}", name);
    }
}
