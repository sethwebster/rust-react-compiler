use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
fn main() {
    let source = r#"// @validateExhaustiveMemoizationDependencies:false
function component() {
  const [count, setCount] = useState(0);
  const increment = useCallback(() => setCount(count + 1));
  return <Foo onClick={increment}></Foo>;
}"#;
    let opts = CompileOptions { source_type: oxc_span::SourceType::jsx(), ..Default::default() };
    match compile(source, opts) {
        Ok(out) => println!("{}", out.js),
        Err(e) => println!("ERROR: {}", e),
    }
}
