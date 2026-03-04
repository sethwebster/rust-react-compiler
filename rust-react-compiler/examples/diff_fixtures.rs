use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

fn parse_expected_code(md: &str) -> Option<String> {
    let start = md.find("## Code\n\n```javascript\n")?;
    let after_fence = start + "## Code\n\n```javascript\n".len();
    let end = md[after_fence..].find("\n```")?;
    Some(md[after_fence..after_fence + end].to_string())
}

fn classify_mismatch(expected: &str, actual: &str) -> Vec<String> {
    let mut issues = Vec::new();
    
    // Count memo cache slots
    let exp_slots = extract_cache_size(expected);
    let act_slots = extract_cache_size(actual);
    if exp_slots != act_slots {
        if act_slots > exp_slots {
            issues.push(format!("EXTRA_SCOPES(exp={},act={})", exp_slots, act_slots));
        } else {
            issues.push(format!("MISSING_SCOPES(exp={},act={})", exp_slots, act_slots));
        }
    }
    
    // Count if/else memo blocks
    let exp_memo = expected.matches("Symbol.for(\"react.memo_cache_sentinel\")").count()
        + expected.matches("$[").count() / 3; // rough heuristic
    let act_memo = actual.matches("Symbol.for(\"react.memo_cache_sentinel\")").count()
        + actual.matches("$[").count() / 3;
    
    // Check for scope boundary differences - variables inside vs outside memo blocks
    let exp_if_blocks = count_memo_if_blocks(expected);
    let act_if_blocks = count_memo_if_blocks(actual);
    if act_if_blocks > exp_if_blocks {
        issues.push(format!("EXTRA_MEMO_BLOCKS(exp={},act={})", exp_if_blocks, act_if_blocks));
    } else if act_if_blocks < exp_if_blocks {
        issues.push(format!("MISSING_MEMO_BLOCKS(exp={},act={})", exp_if_blocks, act_if_blocks));
    }
    
    // Check for missing blank line after import (cosmetic)
    let exp_has_blank_after_import = expected.contains("\";\n\n");
    let act_has_blank_after_import = actual.contains("\";\n\n");
    if exp_has_blank_after_import && !act_has_blank_after_import {
        issues.push("MISSING_BLANK_LINES".to_string());
    }
    
    // Check if hook arguments got memoized when they shouldn't have been
    // (hook args extracted into separate memo blocks)
    let exp_lines: Vec<&str> = expected.lines().collect();
    let act_lines: Vec<&str> = actual.lines().collect();
    
    // Check for variable declarations that differ (const moved into scope)
    let exp_consts: Vec<&str> = exp_lines.iter()
        .filter(|l| l.trim().starts_with("const ") && !l.contains("_c("))
        .map(|l| l.trim())
        .collect();
    let act_consts: Vec<&str> = act_lines.iter()
        .filter(|l| l.trim().starts_with("const ") && !l.contains("_c("))
        .map(|l| l.trim())
        .collect();
    
    // Check for let declarations that differ  
    let exp_lets: Vec<&str> = exp_lines.iter()
        .filter(|l| l.trim().starts_with("let ") && l.trim().ends_with(";"))
        .map(|l| l.trim())
        .collect();
    let act_lets: Vec<&str> = act_lines.iter()
        .filter(|l| l.trim().starts_with("let ") && l.trim().ends_with(";"))
        .map(|l| l.trim())
        .collect();
    if act_lets.len() > exp_lets.len() {
        issues.push(format!("EXTRA_LET_DECLS(exp={},act={})", exp_lets.len(), act_lets.len()));
    }
    
    // Check for code that's inside a memo block in actual but not in expected
    // This indicates wrong scope boundaries
    let exp_unscoped = count_unscoped_stmts(expected);
    let act_unscoped = count_unscoped_stmts(actual);
    if exp_unscoped != act_unscoped {
        issues.push(format!("SCOPE_BOUNDARY_DIFF(exp_unscoped={},act_unscoped={})", exp_unscoped, act_unscoped));
    }
    
    // Check for fire() transform
    if expected.contains("useFire") || expected.contains("_fireFn") {
        if !actual.contains("useFire") && !actual.contains("_fireFn") {
            issues.push("MISSING_FIRE_TRANSFORM".to_string());
        }
    }
    
    // Check for hoisted functions
    if expected.contains("function _temp") && !actual.contains("function _temp") {
        issues.push("MISSING_HOISTED_FN".to_string());
    }

    // Check for noAlias hooks (args shouldn't be memoized)
    if expected.contains("useNoAlias") || expected.contains("noAlias") {
        issues.push("NOALIAS_HOOK".to_string());
    }

    if issues.is_empty() {
        issues.push("UNKNOWN".to_string());
    }
    
    issues
}

fn extract_cache_size(js: &str) -> i32 {
    for line in js.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("const $ = _c(") {
            let start = trimmed.find('(').unwrap() + 1;
            let end = trimmed.find(')').unwrap();
            return trimmed[start..end].parse().unwrap_or(-1);
        }
    }
    -1
}

fn count_memo_if_blocks(js: &str) -> usize {
    let mut count = 0;
    for line in js.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("if ($[") {
            count += 1;
        }
    }
    count
}

fn count_unscoped_stmts(js: &str) -> usize {
    // Count statements at function body level (2-space indent) that are not
    // if/else/let/const$/return
    let mut count = 0;
    for line in js.lines() {
        if line.starts_with("  ") && !line.starts_with("    ") {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && !trimmed.starts_with("if ")
                && !trimmed.starts_with("} else")
                && !trimmed.starts_with("}")
                && !trimmed.starts_with("let ")
                && !trimmed.starts_with("const $ =")
                && !trimmed.starts_with("return ")
                && !trimmed.starts_with("//")
            {
                count += 1;
            }
        }
    }
    count
}

fn main() {
    let dir = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
    ));

    let mut entries: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            !name.starts_with("error.") && !name.starts_with("todo.error.")
        })
        .collect();
    entries.sort();
    
    let mut issue_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut total_wrong = 0;
    let mut total_match = 0;
    let mut total_error = 0;
    let mut examples: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    
    // Also collect full diffs for 15 representative WRONG cases
    let mut full_diffs: Vec<(String, String, String)> = Vec::new();
    
    for path in &entries {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if source.lines().next().unwrap_or("").contains("@flow") {
            continue;
        }
        let source_type = match path.extension().and_then(|e| e.to_str()) {
            Some("tsx") => SourceType::tsx(),
            Some("ts") => SourceType::ts(),
            _ => SourceType::jsx(),
        };
        let opts = CompileOptions {
            source_type,
            filename: Some(path.display().to_string()),
            ..Default::default()
        };
        
        match compile(&source, opts) {
            Ok(output) => {
                let expect_path = path.parent().unwrap().join(
                    format!("{}.expect.md", path.file_stem().unwrap().to_str().unwrap())
                );
                if let Ok(md) = std::fs::read_to_string(&expect_path) {
                    if let Some(expected) = parse_expected_code(&md) {
                        let norm_a: String = output.js.split_whitespace().collect();
                        let norm_e: String = expected.split_whitespace().collect();
                        if norm_a == norm_e {
                            total_match += 1;
                            continue;
                        }
                        total_wrong += 1;
                        let issues = classify_mismatch(&expected, &output.js);
                        for issue in &issues {
                            *issue_counts.entry(issue.clone()).or_insert(0) += 1;
                            examples.entry(issue.clone()).or_insert_with(Vec::new).push(name.clone());
                        }
                        
                        if full_diffs.len() < 15 {
                            full_diffs.push((name.clone(), expected.clone(), output.js.clone()));
                        }
                    }
                }
            }
            Err(_) => {
                total_error += 1;
            }
        }
    }
    
    println!("=== SUMMARY ===");
    println!("MATCH: {}", total_match);
    println!("WRONG: {}", total_wrong);
    println!("ERROR: {}", total_error);
    println!("");
    
    println!("=== ISSUE PATTERNS (sorted by count) ===");
    let mut sorted: Vec<_> = issue_counts.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(a.1));
    for (issue, count) in &sorted {
        let exs = examples.get(*issue).unwrap();
        let first3: Vec<&str> = exs.iter().take(3).map(|s| s.as_str()).collect();
        println!("{}: {} fixtures (e.g. {})", issue, count, first3.join(", "));
    }
    
    println!("\n=== FULL DIFFS FOR 15 REPRESENTATIVE WRONG FIXTURES ===\n");
    for (name, expected, actual) in &full_diffs {
        println!("=== {} ===", name);
        println!("--- EXPECTED ---");
        println!("{}", expected);
        println!("--- ACTUAL ---");
        println!("{}", actual);
        println!("--- END ---\n");
    }
}
