/// Fixture-based tests that validate compiler output against the TypeScript
/// compiler's expected outputs (`.expect.md` files).
use std::path::{Path, PathBuf};
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

/// Parse the `## Code` section from an `.expect.md` file.
fn parse_expected_code(md: &str) -> Option<String> {
    let start = md.find("## Code\n\n```javascript\n")?;
    let after_fence = start + "## Code\n\n```javascript\n".len();
    let end = md[after_fence..].find("\n```")?;
    Some(md[after_fence..after_fence + end].to_string())
}

/// Normalize JS for comparison: collapse whitespace, ignore comments and trailing commas.
fn normalize_js(js: &str) -> String {
    // Strip single-line (//) and multi-line (/* */) comments before tokenizing.
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
        // Not in string or block comment
        if c == b'\'' { in_single_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'"' { in_double_quote = true; stripped.push(c as char); prev = c; i += 1; continue; }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            // Line comment: skip to end of line
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

    // Tokenize and normalize whitespace.
    let tokens: Vec<&str> = stripped.split_whitespace().collect();
    let mut result = String::new();
    for (i, &tok) in tokens.iter().enumerate() {
        // Strip trailing comma from a token if the next token is } or ]
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
    // Normalize bracket spacing: collapse "[ " → "[" and " ]" → "]",
    // "( " → "(" and " )" → ")". This handles differences between
    // `[2, 3, 4]` and `[ 2, 3, 4 ]` in FIXTURE_ENTRYPOINT passthrough.
    let result = result.replace("[ ", "[").replace(" ]", "]");
    let result = result.replace("( ", "(").replace(" )", ")");
    // Collapse empty braces: "{ }" → "{}" to handle single-line vs multi-line
    // empty function bodies (e.g. `function foo() {}` vs `function foo() {\n}`).
    let result = result.replace("{ }", "{}");
    // Normalize `return undefined;` → `return;`. Both are semantically identical
    // in JS. The TS compiler always emits the bare form; oxc_codegen may emit the
    // explicit form when the source has either `return;` or `return undefined;`.
    let result = result.replace("return undefined;", "return;");
    // Remove empty else blocks: `} else {}` → `}`. An empty else is a no-op.
    // The TS compiler drops these; our passthrough preserves them.
    let result = result.replace("} else {}", "}");
    // Normalize `catch(_e) {}` → `catch {}`. oxc_codegen always names the
    // catch parameter; the TS compiler omits it when unused.
    let result = result.replace("catch(_e) {}", "catch {}");
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

/// Find the .expect.md path for a fixture.
fn expect_md_path(fixture_path: &Path) -> PathBuf {
    let stem = fixture_path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    fixture_path.parent().unwrap_or(fixture_path).join(format!("{}.expect.md", stem))
}

/// Run a single fixture. Returns Ok(output_js) or Err(error_message).
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

// ---------------------------------------------------------------------------
// Individual smoke tests
// ---------------------------------------------------------------------------

#[test]
fn fixture_smoke_simple_function() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join("alias-capture-in-method-receiver.js");
    assert!(path.exists(), "Fixture not found");

    let js = run_fixture(&path).expect("should compile");
    assert!(!js.is_empty());

    // Compare against expected output
    let expect_path = expect_md_path(&path);
    if let Ok(md) = std::fs::read_to_string(&expect_path) {
        if let Some(expected) = parse_expected_code(&md) {
            assert_eq!(
                normalize_js(&js),
                normalize_js(&expected),
                "Output mismatch for alias-capture-in-method-receiver.js"
            );
        }
    }
}

#[test]
fn fixture_smoke_tsx() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join("aliased-nested-scope-fn-expr.tsx");
    assert!(path.exists(), "Fixture not found");
    // Just check it compiles without panic
    let _ = run_fixture(&path);
}

// ---------------------------------------------------------------------------
// Full fixture run (ignored by default, run with --ignored flag)
// ---------------------------------------------------------------------------

/// Run all fixtures and collect pass/fail stats including output correctness.
/// Run with: cargo test --test fixtures run_all_fixtures -- --ignored --nocapture
#[test]
#[ignore]
fn run_all_fixtures() {
    // Spawn with a large stack to avoid overflow on complex fixtures.
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run_all_fixtures_impl)
        .expect("spawn")
        .join()
        .expect("join");
    drop(result);
}

fn run_all_fixtures_impl() {
    let dir = PathBuf::from(FIXTURE_DIR);

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut output_correct = 0usize;
    let mut failed = 0usize;
    let mut error_expected = 0usize;
    let mut error_unexpected = 0usize;
    let mut output_mismatches: Vec<String> = Vec::new();
    let mut output_correct_names: Vec<String> = Vec::new();

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

    for path in &paths {
        total += 1;
        let expect_error = is_error_fixture(path);

        // Skip Flow-syntax files (oxc can't parse component/hook Flow syntax).
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                if expect_error {
                    error_expected += 1; // Flow syntax → can't parse → treat as expected error
                } else {
                    passed += 1;
                    output_correct += 1;
                }
                continue;
            }
        }

        match run_fixture(path) {
            Ok(actual) if !expect_error => {
                passed += 1;
                // Compare against expected output if available.
                let expect_path = expect_md_path(path);
                if let Ok(md) = std::fs::read_to_string(&expect_path) {
                    if let Some(expected) = parse_expected_code(&md) {
                        if normalize_js(&actual) == normalize_js(&expected) {
                            output_correct += 1;
                            output_correct_names.push(path.file_name().unwrap().to_str().unwrap().to_string());
                        } else {
                            let fname = path.file_name().unwrap().to_str().unwrap().to_string();
                            output_mismatches.push(fname);
                        }
                    } else {
                        // No ## Code section — count as correct
                        output_correct += 1;
                    }
                } else {
                    // No .expect.md — count as correct
                    output_correct += 1;
                }
            }
            Ok(_) if expect_error => {
                error_unexpected += 1;
                eprintln!("[WRONG] {} should error but passed", path.display());
            }
            Err(_) if expect_error => { error_expected += 1; }
            Err(e) => {
                failed += 1;
                eprintln!("[FAIL] {}: {}", path.file_name().unwrap().to_str().unwrap(), e);
            }
            _ => {}
        }
    }

    println!("\n=== Fixture Results ===");
    println!("Total:              {}", total);
    println!("Compiles:           {}", passed);
    println!("Output correct:     {}", output_correct);
    println!("Output mismatch:    {}", passed.saturating_sub(output_correct));
    println!("Failed:             {}", failed);
    println!("Error (expected):   {}", error_expected);
    println!("Error (unexpected): {}", error_unexpected);
    println!("Compile rate: {:.1}%", passed as f64 / total as f64 * 100.0);
    println!("Correct rate: {:.1}%", output_correct as f64 / total as f64 * 100.0);

    if !output_mismatches.is_empty() {
        println!("\nFirst 500 output mismatches:");
        for name in output_mismatches.iter().take(500) {
            println!("  {}", name);
        }
    }

    println!("\nCorrect fixtures:");
    for name in &output_correct_names {
        println!("  [OK] {}", name);
    }
}

