/// Closest-to-correct fixtures analysis.
///
/// Compiles all non-error fixtures, normalizes both expected and actual output
/// using the EXACT normalize_js function from tests/fixtures.rs, splits into
/// tokens, counts positional differences, and prints the top 50 fixtures
/// sorted by fewest differing tokens.
use std::path::{Path, PathBuf};

use oxc_span::SourceType;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

// ---------------------------------------------------------------------------
// normalize_js — EXACT copy from tests/fixtures.rs
// ---------------------------------------------------------------------------
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
            if c == b'\'' && prev != b'\\' {
                in_single_quote = false;
            }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if in_double_quote {
            if c == b'"' && prev != b'\\' {
                in_double_quote = false;
            }
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_single_quote = true;
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'"' {
            in_double_quote = true;
            stripped.push(c as char);
            prev = c;
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
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
            && (tokens[i + 1] == "}"
                || tokens[i + 1] == "]"
                || tokens[i + 1].starts_with('}')
                || tokens[i + 1].starts_with(']'))
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

// ---------------------------------------------------------------------------
// Helpers copied from tests/fixtures.rs
// ---------------------------------------------------------------------------
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
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("error.") || name.starts_with("todo.error.")
}

fn expect_md_path(fixture_path: &Path) -> PathBuf {
    let stem = fixture_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    fixture_path
        .parent()
        .unwrap_or(fixture_path)
        .join(format!("{}.expect.md", stem))
}

fn run_fixture(path: &Path) -> Result<String, String> {
    let source = std::fs::read_to_string(path).map_err(|e| format!("read error: {}", e))?;
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

// ---------------------------------------------------------------------------
// Token diff counting (positional comparison — fast)
// ---------------------------------------------------------------------------

struct FixtureDiff {
    name: String,
    diff_count: usize,
    /// First N differing token pairs: (expected, actual)
    sample_diffs: Vec<(String, String)>,
}

/// Count positional token differences between two normalized strings.
/// Returns (total_diff_count, first 5 differing pairs).
fn count_token_diffs(norm_expected: &str, norm_actual: &str) -> (usize, Vec<(String, String)>) {
    let exp_tokens: Vec<&str> = norm_expected.split(' ').collect();
    let act_tokens: Vec<&str> = norm_actual.split(' ').collect();
    let max_len = exp_tokens.len().max(act_tokens.len());

    let mut diff_count = 0usize;
    let mut samples: Vec<(String, String)> = Vec::new();

    for idx in 0..max_len {
        let et = if idx < exp_tokens.len() {
            exp_tokens[idx]
        } else {
            "<END>"
        };
        let at = if idx < act_tokens.len() {
            act_tokens[idx]
        } else {
            "<END>"
        };
        if et != at {
            diff_count += 1;
            if samples.len() < 5 {
                samples.push((et.to_string(), at.to_string()));
            }
        }
    }

    (diff_count, samples)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
fn main() {
    let handle = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run)
        .expect("spawn thread");
    handle.join().expect("join thread");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("fixture dir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("js" | "jsx" | "ts" | "tsx")
            )
        })
        .collect();
    entries.sort();

    let mut diffs: Vec<FixtureDiff> = Vec::new();
    let mut total = 0usize;
    let mut compiled = 0usize;
    let mut matched = 0usize;
    let mut skipped_error = 0usize;
    let mut skipped_flow = 0usize;
    let mut compile_fail = 0usize;

    for path in &entries {
        total += 1;

        if is_error_fixture(path) {
            skipped_error += 1;
            continue;
        }

        // Skip Flow files
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                skipped_flow += 1;
                continue;
            }
        }

        let actual = match run_fixture(path) {
            Ok(js) => js,
            Err(_) => {
                compile_fail += 1;
                continue;
            }
        };
        compiled += 1;

        // Load expected
        let expect_path = expect_md_path(path);
        let md = match std::fs::read_to_string(&expect_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let expected = match parse_expected_code(&md) {
            Some(e) => e,
            None => continue,
        };

        let norm_expected = normalize_js(&expected);
        let norm_actual = normalize_js(&actual);

        if norm_expected == norm_actual {
            matched += 1;
            continue;
        }

        let fname = path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let (diff_count, sample_diffs) = count_token_diffs(&norm_expected, &norm_actual);

        diffs.push(FixtureDiff {
            name: fname,
            diff_count,
            sample_diffs,
        });
    }

    // Sort by fewest different tokens (closest to correct first)
    diffs.sort_by_key(|d| d.diff_count);

    println!("=== Closest-to-Correct Fixtures ===");
    println!("Total fixtures:      {}", total);
    println!("Skipped (error):     {}", skipped_error);
    println!("Skipped (flow):      {}", skipped_flow);
    println!("Compile failures:    {}", compile_fail);
    println!("Compiled OK:         {}", compiled);
    println!("Output matched:      {}", matched);
    println!("Output mismatched:   {}", diffs.len());
    println!();

    println!(
        "=== Top 50 Closest Fixtures (fewest token diffs) ===\n"
    );
    println!(
        "{:<4} {:<6} {}",
        "#", "Diffs", "Fixture"
    );
    println!("{}", "-".repeat(80));

    for (rank, d) in diffs.iter().enumerate().take(50) {
        println!(
            "{:<4} {:<6} {}",
            rank + 1,
            d.diff_count,
            d.name
        );

        for (j, (exp, act)) in d.sample_diffs.iter().enumerate() {
            println!(
                "       diff {}: expected {:>40} vs actual {:<40}",
                j + 1,
                truncate(exp, 40),
                truncate(act, 40),
            );
        }
        println!();
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}
