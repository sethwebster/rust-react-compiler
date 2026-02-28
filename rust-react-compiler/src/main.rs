use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: react-compiler <file.js|.jsx|.ts|.tsx>");
        std::process::exit(1);
    }

    let path = PathBuf::from(&args[1]);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };

    let source_type = source_type_for_path(&path);
    let options = CompileOptions {
        source_type,
        filename: Some(path.display().to_string()),
        ..Default::default()
    };

    match compile(&source, options) {
        Ok(output) => {
            println!("{}", output.js);
        }
        Err(e) => {
            eprintln!("Compile error:\n{}", e);
            std::process::exit(1);
        }
    }
}

fn source_type_for_path(path: &PathBuf) -> SourceType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => SourceType::tsx(),
        Some("ts") => SourceType::ts(),
        Some("jsx") => SourceType::jsx(),
        _ => SourceType::mjs(),
    }
}
