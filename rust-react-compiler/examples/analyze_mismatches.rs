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
    let result = tokens.join(" ");
    let result = result.replace(",}", "}").replace(",]", "]");
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
    let handle = builder.spawn(run).expect("spawn thread");
    handle.join().expect("join thread");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js"|"jsx"|"ts"|"tsx")))
        .collect();
    entries.sort();

    // Categories
    let mut only_let_const = 0; // only diff is let vs const
    let mut only_slot_count = 0; // only diff is _c(N) number
    let mut dollar_t = 0; // has $tN in our output
    let mut missing_memo = 0; // expected has _c() but ours doesn't
    let mut extra_memo = 0; // ours has _c() but expected doesn't
    let mut passthrough_match = 0; // both are passthrough (no _c())
    let mut near_miss = Vec::new(); // edit distance <= 20 tokens

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

        let norm_actual = normalize_js(&actual);
        let norm_expected = normalize_js(&expected);
        if norm_actual == norm_expected { continue; }

        // Categorize
        let actual_has_c = actual.contains("_c(");
        let expected_has_c = expected.contains("_c(");
        let actual_has_dollar_t = actual.contains("$t");
        
        if !actual_has_c && !expected_has_c {
            passthrough_match += 1;
        }
        if actual_has_dollar_t { dollar_t += 1; }
        if !actual_has_c && expected_has_c { missing_memo += 1; }
        if actual_has_c && !expected_has_c { extra_memo += 1; }

        // Token-level diff
        let actual_tokens: Vec<&str> = norm_actual.split_whitespace().collect();
        let expected_tokens: Vec<&str> = norm_expected.split_whitespace().collect();
        
        // Check if only diff is slot count
        if actual_tokens.len() == expected_tokens.len() {
            let diffs: Vec<usize> = actual_tokens.iter().zip(expected_tokens.iter())
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .map(|(i, _)| i)
                .collect();
            if diffs.len() == 1 {
                let at = actual_tokens[diffs[0]];
                let et = expected_tokens[diffs[0]];
                if at.starts_with("_c(") && et.starts_with("_c(") {
                    only_slot_count += 1;
                    continue;
                }
                if (at == "let" && et == "const") || (at == "const" && et == "let") {
                    only_let_const += 1;
                    continue;
                }
            }
            if diffs.len() <= 3 {
                near_miss.push((name.to_string(), diffs.len(), 
                    diffs.iter().map(|&i| format!("  ours: {} | expected: {}", actual_tokens[i], expected_tokens[i])).collect::<Vec<_>>()));
            }
        } else {
            let len_diff = (actual_tokens.len() as i64 - expected_tokens.len() as i64).abs();
            if len_diff <= 5 {
                near_miss.push((name.to_string(), len_diff as usize + 100, vec![
                    format!("  token count: ours={} expected={}", actual_tokens.len(), expected_tokens.len())
                ]));
            }
        }
    }

    println!("=== Mismatch Categories ===");
    println!("Only slot count diff (_c(N)):  {}", only_slot_count);
    println!("Only let/const diff:           {}", only_let_const);
    println!("Has $tN in output:             {}", dollar_t);
    println!("Missing memo (no _c):          {}", missing_memo);
    println!("Extra memo (unexpected _c):    {}", extra_memo);
    println!("Both passthrough (no _c):      {}", passthrough_match);
    println!("\n=== Near Misses (1-3 token diffs, same length) ===");
    near_miss.sort_by_key(|(_, d, _)| *d);
    for (name, ndiff, details) in near_miss.iter().take(40) {
        println!("{} ({} diffs):", name, ndiff);
        for d in details { println!("{}", d); }
    }
}
