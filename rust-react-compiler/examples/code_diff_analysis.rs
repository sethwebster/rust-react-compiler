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

    // Detailed pattern checks for same-slot, same-scope mismatches
    let mut inner_scope_order = 0u32;      // instructions inside a scope are reordered
    let mut var_decl_placement = 0u32;     // let tN declared inside vs outside scope
    let mut extra_assignment = 0u32;       // extra assignments in else branch
    let mut missing_const = 0u32;          // const vs let difference
    let mut props_destructure_diff = 0u32; // t0 destructuring pattern differs
    let mut fn_expr_diff = 0u32;           // function expression body differs
    let mut jsx_content_diff = 0u32;       // JSX expression content differs
    let mut store_order_diff = 0u32;       // $[N] = x assignments in different order

    let mut total_same_slot_diff = 0u32;

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
        if actual.contains("$t") { continue; }

        let exp_slots = extract_c_count(&expected);
        let act_slots = extract_c_count(&actual);
        if exp_slots != act_slots { continue; }

        total_same_slot_diff += 1;

        // Check specific patterns
        let exp_lines: Vec<&str> = expected.lines().map(|l| l.trim()).collect();
        let act_lines: Vec<&str> = actual.lines().map(|l| l.trim()).collect();

        // Props destructuring: expected uses `const { x } = t0` but actual uses different
        if expected.contains("const {") && actual.contains("const {") {
            let exp_destruct = exp_lines.iter().filter(|l| l.starts_with("const {")).collect::<Vec<_>>();
            let act_destruct = act_lines.iter().filter(|l| l.starts_with("const {")).collect::<Vec<_>>();
            if exp_destruct != act_destruct {
                props_destructure_diff += 1;
            }
        }

        // Variable declaration placement: let tN inside vs outside
        let exp_let_t = exp_lines.iter().filter(|l| l.starts_with("let t")).count();
        let act_let_t = act_lines.iter().filter(|l| l.starts_with("let t")).count();
        if exp_let_t != act_let_t {
            var_decl_placement += 1;
        }

        // Store order: $[N] = assignments
        let exp_stores: Vec<_> = exp_lines.iter().filter(|l| l.starts_with("$[")).collect();
        let act_stores: Vec<_> = act_lines.iter().filter(|l| l.starts_with("$[")).collect();
        if exp_stores != act_stores {
            store_order_diff += 1;
        }

        // JSX content differences
        if expected.contains("<") && actual.contains("<") {
            let exp_jsx: Vec<_> = exp_lines.iter().filter(|l| l.contains("<") && !l.starts_with("//") && !l.starts_with("*")).collect();
            let act_jsx: Vec<_> = act_lines.iter().filter(|l| l.contains("<") && !l.starts_with("//") && !l.starts_with("*")).collect();
            if exp_jsx != act_jsx {
                jsx_content_diff += 1;
            }
        }

        // Else branch differences
        let exp_else: Vec<_> = exp_lines.iter().enumerate()
            .filter(|(_, l)| l.starts_with("} else {"))
            .map(|(i, _)| exp_lines.get(i+1).unwrap_or(&""))
            .collect();
        let act_else: Vec<_> = act_lines.iter().enumerate()
            .filter(|(_, l)| l.starts_with("} else {"))
            .map(|(i, _)| act_lines.get(i+1).unwrap_or(&""))
            .collect();
        if exp_else != act_else {
            extra_assignment += 1;
        }
    }

    println!("=== Code Diff Detail (same slot count, no $tN) ===");
    println!("Total in this category:       {}", total_same_slot_diff);
    println!();
    println!("Props destructure differs:    {}", props_destructure_diff);
    println!("let tN count differs:         {}", var_decl_placement);
    println!("Store ($[N]=) order differs:  {}", store_order_diff);
    println!("JSX content differs:          {}", jsx_content_diff);
    println!("Else branch differs:          {}", extra_assignment);
}
