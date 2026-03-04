use std::path::{Path, PathBuf};
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

fn source_type_for(path: &Path) -> SourceType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => SourceType::tsx(),
        Some("ts") => SourceType::ts(),
        Some("jsx") | Some("js") => SourceType::jsx(),
        _ => SourceType::mjs(),
    }
}

fn main() {
    let fixtures = vec![
        "allow-merge-refs-pattern.js",
        "array-access-assignment.js",
        "allocating-primitive-as-dep-nested-scope.js",
        "allow-mutating-ref-in-callback-passed-to-jsx.tsx",
        "conditional-early-return.js",
        "capturing-func-simple-alias.js",
        "for-in-statement-break.js",
        "object-literal-method-call-in-ternary-test.js",
        "capturing-func-simple-alias-iife.js",
        "repro-slow-validate-preserve-memo.js",
    ];

    let dir = PathBuf::from(FIXTURE_DIR);

    for fixture_name in &fixtures {
        let path = dir.join(fixture_name);
        if !path.exists() {
            eprintln!("SKIP: {} not found", fixture_name);
            continue;
        }

        let stem = path.file_stem().unwrap().to_str().unwrap();
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", stem));

        let expected = match std::fs::read_to_string(&expect_path) {
            Ok(md) => parse_expected_code(&md),
            Err(_) => { eprintln!("SKIP: no expect.md for {}", fixture_name); continue; }
        };

        let source = std::fs::read_to_string(&path).unwrap();
        let source_type = source_type_for(&path);
        let opts = CompileOptions {
            source_type,
            filename: Some(path.display().to_string()),
            ..Default::default()
        };

        let actual = match compile(&source, opts) {
            Ok(o) => o.js,
            Err(e) => { eprintln!("ERROR compiling {}: {}", fixture_name, e); continue; }
        };

        println!("========================================");
        println!("FIXTURE: {}", fixture_name);
        println!("========================================");
        println!("--- EXPECTED ---");
        if let Some(ref exp) = expected {
            println!("{}", exp);
        } else {
            println!("(no expected code found)");
        }
        println!("--- ACTUAL ---");
        println!("{}", actual);
        println!();
    }
}
