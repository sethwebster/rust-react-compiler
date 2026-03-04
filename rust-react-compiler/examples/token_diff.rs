/// Token-level diff analysis of fixture outputs.
///
/// For each fixture that compiles but doesn't match expected output,
/// normalizes both sides with the EXACT same `normalize_js` function
/// used in the test harness, then does a word-level diff to find the
/// first differing token pair. Groups fixtures by that pair and prints
/// a summary sorted by frequency.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use oxc_span::SourceType;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

// ---------------------------------------------------------------------------
// normalize_js  — EXACT copy from tests/fixtures.rs
// ---------------------------------------------------------------------------
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
    // Normalize bracket spacing
    let result = result.replace("[ ", "[").replace(" ]", "]");
    let result = result.replace("( ", "(").replace(" )", ")");
    // Collapse empty braces
    let result = result.replace("{ }", "{}");
    // Normalize return undefined; → return;
    let result = result.replace("return undefined;", "return;");
    // Remove empty else blocks
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
// Main
// ---------------------------------------------------------------------------
fn main() {
    // Use a large-stack thread just like the test harness.
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

    // Map from (expected_token, actual_token) → list of fixture names
    let mut diff_groups: HashMap<(String, String), Vec<String>> = HashMap::new();
    let mut total = 0usize;
    let mut compiled = 0usize;
    let mut matched = 0usize;
    let mut mismatched = 0usize;

    for path in &entries {
        total += 1;
        if is_error_fixture(path) {
            continue;
        }

        // Skip Flow files
        if let Ok(src) = std::fs::read_to_string(path) {
            let first = src.lines().next().unwrap_or("");
            if first.contains("@flow") {
                continue;
            }
        }

        let actual = match run_fixture(path) {
            Ok(js) => js,
            Err(_) => continue,
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

        mismatched += 1;
        let fname = path.file_name().unwrap().to_str().unwrap().to_string();

        // Word-level diff: find the FIRST differing token pair
        let exp_tokens: Vec<&str> = norm_expected.split(' ').collect();
        let act_tokens: Vec<&str> = norm_actual.split(' ').collect();

        let mut first_diff: Option<(String, String)> = None;
        let max_len = exp_tokens.len().max(act_tokens.len());
        for idx in 0..max_len {
            let et = if idx < exp_tokens.len() { exp_tokens[idx] } else { "<END>" };
            let at = if idx < act_tokens.len() { act_tokens[idx] } else { "<END>" };
            if et != at {
                first_diff = Some((et.to_string(), at.to_string()));
                break;
            }
        }

        if let Some(diff) = first_diff {
            diff_groups.entry(diff).or_default().push(fname);
        }
    }

    // Sort groups by count descending
    let mut groups: Vec<_> = diff_groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    println!("=== Token Diff Summary ===");
    println!("Total fixtures:    {}", total);
    println!("Compiled (non-err): {}", compiled);
    println!("Output matched:    {}", matched);
    println!("Output mismatched: {}", mismatched);
    println!();

    println!("=== First-Differing-Token Groups (by frequency) ===");
    println!("{:<6} {:<40} {:<40}", "Count", "Expected Token", "Actual Token");
    println!("{}", "-".repeat(90));

    for ((exp_tok, act_tok), fixtures) in &groups {
        let exp_display = truncate(exp_tok, 38);
        let act_display = truncate(act_tok, 38);
        println!("{:<6} {:<40} {:<40}", fixtures.len(), exp_display, act_display);
    }

    // Print detailed list for top groups
    println!("\n=== Details for Top 15 Groups ===");
    for (i, ((exp_tok, act_tok), fixtures)) in groups.iter().enumerate().take(15) {
        println!(
            "\n--- Group {} ({} fixtures): expected '{}' vs actual '{}' ---",
            i + 1,
            fixtures.len(),
            truncate(exp_tok, 60),
            truncate(act_tok, 60),
        );
        for f in fixtures.iter().take(5) {
            println!("  {}", f);
        }
        if fixtures.len() > 5 {
            println!("  ... and {} more", fixtures.len() - 5);
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.min(s.len())])
    }
}
