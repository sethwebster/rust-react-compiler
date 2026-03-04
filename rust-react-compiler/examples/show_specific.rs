use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

fn main() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let fixtures = [
        "alias-capture-in-method-receiver-and-mutate.js",
        "alias-nested-member-path.js",
        "allow-ref-access-in-effect-indirect.js", // raw $tN
    ];

    for name in &fixtures {
        let path = dir.join(name);
        let source = std::fs::read_to_string(&path).unwrap();
        let source_type = if name.ends_with(".tsx") { SourceType::tsx() } else { SourceType::jsx() };
        let opts = CompileOptions {
            source_type,
            filename: Some(path.display().to_string()),
            ..Default::default()
        };
        match compile(&source, opts) {
            Ok(o) => {
                println!("=== {} (ACTUAL) ===", name);
                println!("{}", o.js);
                println!();
            }
            Err(e) => println!("ERROR {}: {}", name, e),
        }
    }
}
