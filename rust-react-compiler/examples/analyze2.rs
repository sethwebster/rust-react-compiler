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

    let mut dollar_t = 0;
    let mut missing_memo = 0;
    let mut extra_memo = 0;
    let mut both_passthrough = 0;
    let mut near_miss_1 = Vec::new();
    let mut near_miss_2_3 = Vec::new();
    let mut total_mismatch = 0;
    let mut slot_diff_only = 0;
    let mut len_diff_small = Vec::new();

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
        total_mismatch += 1;

        let actual_has_c = actual.contains("_c(");
        let expected_has_c = expected.contains("_c(");
        if actual.contains("$t") { dollar_t += 1; }
        if !actual_has_c && !expected_has_c { both_passthrough += 1; }
        if !actual_has_c && expected_has_c { missing_memo += 1; }
        if actual_has_c && !expected_has_c { extra_memo += 1; }

        let at: Vec<&str> = na.split_whitespace().collect();
        let et: Vec<&str> = ne.split_whitespace().collect();
        
        if at.len() == et.len() {
            let diffs: Vec<usize> = at.iter().zip(et.iter())
                .enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
            if diffs.len() == 1 {
                let a = at[diffs[0]];
                let e = et[diffs[0]];
                if a.starts_with("_c(") && e.starts_with("_c(") { slot_diff_only += 1; }
                near_miss_1.push((name.to_string(), a.to_string(), e.to_string()));
            } else if diffs.len() <= 3 {
                let details: Vec<String> = diffs.iter().map(|&i| format!("{}→{}", at[i], et[i])).collect();
                near_miss_2_3.push((name.to_string(), diffs.len(), details));
            }
        } else {
            let diff = (at.len() as i64 - et.len() as i64).abs();
            if diff <= 10 {
                len_diff_small.push((name.to_string(), at.len() as i64 - et.len() as i64));
            }
        }
    }

    println!("Total mismatches: {}", total_mismatch);
    println!("Has $tN: {}", dollar_t);
    println!("Missing memo: {}", missing_memo);
    println!("Extra memo: {}", extra_memo);
    println!("Both passthrough: {}", both_passthrough);
    println!("Slot count only: {}", slot_diff_only);
    
    println!("\n=== 1-token diffs ({}) ===", near_miss_1.len());
    // Group by diff pattern
    let mut patterns: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    for (name, a, e) in &near_miss_1 {
        let key = format!("{}→{}", a, e);
        patterns.entry(key).or_default().push(name.clone());
    }
    let mut sorted: Vec<_> = patterns.iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    for (pattern, names) in sorted.iter().take(20) {
        println!("  {} ({}): {}", pattern, names.len(), names.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
    }

    println!("\n=== 2-3 token diffs ({}) ===", near_miss_2_3.len());
    for (name, ndiff, details) in near_miss_2_3.iter().take(15) {
        println!("  {} ({} diffs): {}", name, ndiff, details.join(", "));
    }

    println!("\n=== Small length diffs ({}) ===", len_diff_small.len());
    // Group by diff amount
    let mut by_diff: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for (_, diff) in &len_diff_small { *by_diff.entry(*diff).or_default() += 1; }
    let mut sorted: Vec<_> = by_diff.iter().collect();
    sorted.sort_by_key(|(_, &count)| std::cmp::Reverse(count));
    for (diff, count) in &sorted {
        println!("  diff={}: {} fixtures", diff, count);
    }
    
    // Show some length-diff examples
    for (name, diff) in len_diff_small.iter().take(5) {
        println!("  {} (diff={})", name, diff);
    }
}
