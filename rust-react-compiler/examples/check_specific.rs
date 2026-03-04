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
    let fixtures = vec![
        "infer-function-React-memo.js",
        "infer-function-forwardRef.js",
        "infer-compile-hooks-with-multiple-params.js",
        "default-param-calls-global-function.js",
        "optional-call-simple.js",
        "declare-reassign-variable-in-closure.js",
        "lambda-reassign-primitive.js",
    ];
    for name in fixtures {
        let path = dir.join(name);
        let source = std::fs::read_to_string(&path).unwrap();
        let source_type = if name.ends_with(".ts") { SourceType::ts() } 
                          else if name.ends_with(".tsx") { SourceType::tsx() }
                          else { SourceType::jsx() };
        let opts = CompileOptions { source_type, filename: Some(path.display().to_string()), ..Default::default() };
        let actual = compile(&source, opts).unwrap().js;
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", path.file_stem().unwrap().to_str().unwrap()));
        let md = std::fs::read_to_string(&expect_path).unwrap();
        let expected = parse_expected_code(&md).unwrap();
        
        println!("=== {} ===", name);
        println!("--- SOURCE (first 15 lines) ---");
        for line in source.lines().take(15) { println!("{}", line); }
        println!("--- OUR OUTPUT (first 15 lines) ---");
        for line in actual.lines().take(15) { println!("{}", line); }
        println!("--- EXPECTED (first 15 lines) ---");
        for line in expected.lines().take(15) { println!("{}", line); }
        println!();
    }
}
