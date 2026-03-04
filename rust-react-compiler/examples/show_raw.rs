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

fn main() {
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handle = builder.spawn(run).expect("spawn");
    handle.join().expect("join");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let path = dir.join("alias-capture-in-method-receiver-and-mutate.js");
    let source = std::fs::read_to_string(&path).unwrap();
    let opts = CompileOptions { source_type: SourceType::jsx(), filename: Some(path.display().to_string()), ..Default::default() };
    let actual = compile(&source, opts).unwrap().js;
    let expect_path = path.parent().unwrap().join("alias-capture-in-method-receiver-and-mutate.expect.md");
    let md = std::fs::read_to_string(&expect_path).unwrap();
    let expected = parse_expected_code(&md).unwrap();
    
    // Show last 5 lines of each
    println!("=== OUR OUTPUT (last 10 lines) ===");
    for line in actual.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev() {
        println!("{}", line);
    }
    println!("\n=== EXPECTED OUTPUT (last 10 lines) ===");
    for line in expected.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev() {
        println!("{}", line);
    }
}
