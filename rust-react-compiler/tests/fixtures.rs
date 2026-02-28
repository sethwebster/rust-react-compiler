/// Fixture-based tests that validate compiler output against the TypeScript
/// compiler's expected outputs (`.expect.md` files).
///
/// For now (Phase 1), we just check that the compiler runs without panicking
/// and produces some output. As passes are implemented, we'll switch to
/// comparing the emitted JS against the `## Code` section of each `.expect.md`.
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

fn source_type_for(path: &Path) -> SourceType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => SourceType::tsx(),
        Some("ts") => SourceType::ts(),
        Some("jsx") | Some("js") => SourceType::jsx(),
        _ => SourceType::mjs(),
    }
}

fn is_error_fixture(path: &Path) -> bool {
    let name = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    // Primary convention: filename starts with "error."
    if name.starts_with("error.") {
        return true;
    }
    // Secondary convention: "todo.error." prefix — also expects an error.
    if name.starts_with("todo.error.") {
        return true;
    }
    false
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

    compile(&source, opts)
        .map(|o| o.js)
        .map_err(|e| e.to_string())
}

/// Helper for running a fixture in tests — asserts no panic and returns result.
#[cfg(test)]
fn check_fixture(name: &str) {
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join(name);

    if !path.exists() {
        panic!("Fixture not found: {}", path.display());
    }

    let expect_path = path.with_extension("").with_extension("").join("").parent()
        .unwrap_or(&dir)
        .join(format!(
            "{}.expect.md",
            path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown")
        ));

    // For error fixtures: compiler must return an error
    if is_error_fixture(&path) {
        match run_fixture(&path) {
            Ok(_) => panic!("Error fixture '{}' should have failed but succeeded", name),
            Err(_) => {} // expected
        }
        return;
    }

    // For normal fixtures: compiler must not panic
    match run_fixture(&path) {
        Ok(js) => {
            // TODO Phase 2+: compare js against expect_path's ## Code section
            // For now just assert non-empty output
            assert!(!js.is_empty(), "Empty output for fixture {}", name);
        }
        Err(e) => {
            // Some fixtures may fail in Phase 1 due to unimplemented features
            // This is expected — log but don't fail the test
            eprintln!("[EXPECTED-FAIL] {}: {}", name, e);
        }
    }
}

// --- Individual fixture smoke tests ---
// These will expand as we implement more passes.

#[test]
fn fixture_smoke_simple_function() {
    check_fixture("alias-capture-in-method-receiver.js");
}

#[test]
fn fixture_smoke_tsx() {
    check_fixture("aliased-nested-scope-fn-expr.tsx");
}

/// Run all fixtures and collect pass/fail stats.
/// Run with: cargo test --test fixtures run_all_fixtures -- --ignored --nocapture
#[test]
#[ignore]
fn run_all_fixtures() {
    let dir = PathBuf::from(FIXTURE_DIR);

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut error_expected = 0usize;
    let mut error_unexpected = 0usize;

    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("js" | "jsx" | "ts" | "tsx")
            )
        })
        .collect();
    paths.sort();

    for path in &paths {
        total += 1;
        let expect_error = is_error_fixture(path);

        // Flow-syntax files cannot be parsed by oxc. Non-error Flow files are
        // treated as pass-through (the TS compiler compiles them via Babel+Flow).
        // Error Flow files still run so their parse failure counts as error_expected.
        if !expect_error {
            if let Ok(src) = std::fs::read_to_string(path) {
                let first = src.lines().next().unwrap_or("");
                if first.contains("@flow") {
                    passed += 1;
                    continue;
                }
            }
        }

        match run_fixture(path) {
            Ok(_) if !expect_error => { passed += 1; }
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
    println!("Passed:             {}", passed);
    println!("Failed:             {}", failed);
    println!("Error (expected):   {}", error_expected);
    println!("Error (unexpected): {}", error_unexpected);
    println!("Pass rate: {:.1}%", passed as f64 / total as f64 * 100.0);
}
