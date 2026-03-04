use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler");
fn parse_expected_code(md: &str) -> Option<String> {
    let start = md.find("## Code\n\n```javascript\n")?;
    let after_fence = start + "## Code\n\n```javascript\n".len();
    let end = md[after_fence..].find("\n```")?;
    Some(md[after_fence..after_fence + end].to_string())
}
fn main() {
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handle = builder.spawn(run).expect("spawn"); handle.join().expect("join");
}
fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let fixtures = vec!["concise-arrow-expr.js", "controlled-input.js", "holey-array-pattern-dce-2.js", "holey-array-pattern-dce.js", "repro-object-pattern.js"];
    for name in fixtures {
        let path = dir.join(name);
        let source = std::fs::read_to_string(&path).unwrap();
        let st = SourceType::jsx();
        let opts = CompileOptions { source_type: st, filename: Some(path.display().to_string()), ..Default::default() };
        let actual = compile(&source, opts).unwrap().js;
        let ep = path.parent().unwrap().join(format!("{}.expect.md", path.file_stem().unwrap().to_str().unwrap()));
        let md = std::fs::read_to_string(&ep).unwrap();
        let expected = parse_expected_code(&md).unwrap();
        println!("=== {} ===", name);
        // Find lines that differ
        let al: Vec<&str> = actual.lines().collect();
        let el: Vec<&str> = expected.lines().collect();
        for (i, (a, e)) in al.iter().zip(el.iter()).enumerate() {
            let at = a.trim(); let et = e.trim();
            if at != et {
                println!("Line {}: OURS='{}' EXPECTED='{}'", i+1, at, et);
                break;
            }
        }
        println!();
    }
}
