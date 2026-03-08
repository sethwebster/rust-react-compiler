use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

fn main() {
    let path = "../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/alias-capture-in-method-receiver.js";
    let src = std::fs::read_to_string(path).expect("file not found");
    let opts = CompileOptions {
        source_type: SourceType::default().with_module(true).with_jsx(true),
        filename: Some(path.to_string()),
        ..Default::default()
    };
    match compile(&src, opts) {
        Ok(out) => println!("{}", out.js),
        Err(e) => eprintln!("ERROR: {}", e),
    }
}
