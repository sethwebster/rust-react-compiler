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
    let handle = builder.spawn(run).expect("spawn");
    handle.join().expect("join");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let fixtures = vec![
        "alias-capture-in-method-receiver-and-mutate.js",
        "allow-global-mutation-in-effect-indirect-usecallback.js",
        "capturing-function-1.js",
    ];
    for name in fixtures {
        let path = dir.join(name);
        let source = std::fs::read_to_string(&path).unwrap();
        let source_type = if name.ends_with(".tsx") { SourceType::tsx() } 
                          else if name.ends_with(".ts") { SourceType::ts() }
                          else { SourceType::jsx() };
        let opts = CompileOptions { source_type, filename: Some(path.display().to_string()), ..Default::default() };
        let actual = compile(&source, opts).unwrap().js;
        let expect_path = path.parent().unwrap().join(format!("{}.expect.md", path.file_stem().unwrap().to_str().unwrap()));
        let md = std::fs::read_to_string(&expect_path).unwrap();
        let expected = parse_expected_code(&md).unwrap();
        
        let na = normalize_js(&actual);
        let ne = normalize_js(&expected);
        
        println!("=== {} ===", name);
        // Find the differing tokens
        let at: Vec<&str> = na.split_whitespace().collect();
        let et: Vec<&str> = ne.split_whitespace().collect();
        for (i, (a, e)) in at.iter().zip(et.iter()).enumerate() {
            if a != e {
                let context_start = if i > 3 { i - 3 } else { 0 };
                let context_end = (i + 4).min(at.len()).min(et.len());
                println!("Diff at token {}:", i);
                println!("  ours:     ...{}", at[context_start..context_end].join(" "));
                println!("  expected: ...{}", et[context_start..context_end].join(" "));
                break;
            }
        }
        if at.len() != et.len() {
            println!("  token count: ours={} expected={}", at.len(), et.len());
        }
        println!();
    }
}
