use std::path::PathBuf;
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
    js.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_c_count(js: &str) -> Option<u32> {
    if let Some(pos) = js.find("_c(") {
        let rest = &js[pos + 3..];
        if let Some(end) = rest.find(')') {
            return rest[..end].parse().ok();
        }
    }
    None
}

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    // Categories for same-slot mismatches
    let mut dep_order_diff = 0u32;      // same deps, different order in if($[N]!==...)
    let mut different_deps = 0u32;      // different dependency variables
    let mut code_inside_scope_diff = 0u32; // code inside if block differs
    let mut whitespace_only = 0u32;
    let mut raw_tvar = 0u32;           // $tN in actual
    let mut assignment_diff = 0u32;    // different variable assignments in else branch

    let mut samples_raw_tvar: Vec<String> = Vec::new();
    let mut samples_dep_diff: Vec<String> = Vec::new();
    let mut samples_code_diff: Vec<String> = Vec::new();

    for path in &paths {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if name.starts_with("error.") || name.starts_with("todo.error.") { continue; }
        if let Ok(src) = std::fs::read_to_string(path) {
            if src.lines().next().unwrap_or("").contains("@flow") { continue; }
        }

        let stem = path.file_stem().unwrap().to_str().unwrap();
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", stem));
        let expected = match std::fs::read_to_string(&expect_path) {
            Ok(md) => match parse_expected_code(&md) { Some(e) => e, None => continue },
            Err(_) => continue,
        };

        let source = match std::fs::read_to_string(path) { Ok(s) => s, Err(_) => continue };
        let source_type = match path.extension().and_then(|e| e.to_str()) {
            Some("tsx") => SourceType::tsx(), Some("ts") => SourceType::ts(), _ => SourceType::jsx(),
        };
        let opts = CompileOptions {
            source_type, filename: Some(path.display().to_string()), ..Default::default()
        };

        let actual = match compile(&source, opts) { Ok(o) => o.js, Err(_) => continue };
        if normalize_js(&actual) == normalize_js(&expected) { continue; }

        let exp_slots = extract_c_count(&expected);
        let act_slots = extract_c_count(&actual);
        if exp_slots != act_slots { continue; } // only analyze same-slot mismatches

        // Check for raw $tN vars
        if actual.contains("$t") {
            raw_tvar += 1;
            if samples_raw_tvar.len() < 3 {
                samples_raw_tvar.push(name.clone());
            }
            continue; // separate category
        }

        // Compare the if-conditions (dependency checks)
        let exp_deps = extract_dep_checks(&expected);
        let act_deps = extract_dep_checks(&actual);

        if exp_deps != act_deps {
            different_deps += 1;
            if samples_dep_diff.len() < 5 {
                samples_dep_diff.push(format!("{}\n  EXP deps: {:?}\n  ACT deps: {:?}", name, &exp_deps[..exp_deps.len().min(3)], &act_deps[..act_deps.len().min(3)]));
            }
        } else {
            code_inside_scope_diff += 1;
            if samples_code_diff.len() < 5 {
                samples_code_diff.push(name.clone());
            }
        }
    }

    println!("=== Same-Slot Mismatch Breakdown ===");
    println!("Raw $tN variables in output: {}", raw_tvar);
    println!("Different dependencies:       {}", different_deps);
    println!("Code inside scope differs:    {}", code_inside_scope_diff);
    println!();

    println!("--- Samples with raw $tN ---");
    for s in &samples_raw_tvar { println!("  {}", s); }

    println!();
    println!("--- Samples with different deps ---");
    for s in &samples_dep_diff { println!("  {}", s); }

    println!();
    println!("--- Samples with code diff (same deps) ---");
    for s in &samples_code_diff { println!("  {}", s); }
}

fn extract_dep_checks(js: &str) -> Vec<String> {
    let mut checks = Vec::new();
    for line in js.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("if ($[") && trimmed.contains("!==") {
            checks.push(trimmed.to_string());
        }
    }
    checks
}
