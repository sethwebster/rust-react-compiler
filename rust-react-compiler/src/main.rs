use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use react_compiler::hir::build_hir::lower_program;
use react_compiler::hir::environment::{Environment, EnvironmentConfig};
use react_compiler::hir::hir::ReactFunctionType;
use react_compiler::hir::print_hir::print_hir_function;
use react_compiler::ssa::enter_ssa::enter_ssa;
use react_compiler::ssa::eliminate_redundant_phi::eliminate_redundant_phi;
use oxc_span::SourceType;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let debug_hir = args.contains(&"--debug-hir".to_string());
    let file_args: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();

    if file_args.is_empty() {
        eprintln!("Usage: react-compiler [--debug-hir] <file.js|.jsx|.ts|.tsx>");
        std::process::exit(1);
    }

    let path = PathBuf::from(file_args[0]);
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };

    let source_type = source_type_for_path(&path);

    if debug_hir {
        let mut env = Environment::new(ReactFunctionType::Component, EnvironmentConfig::default(), Some(path.display().to_string()));
        match lower_program(&source, source_type, &mut env) {
            Ok(mut hir) => {
                enter_ssa(&mut hir);
                eliminate_redundant_phi(&mut hir);
                println!("{}", print_hir_function(&hir, &env));
            }
            Err(e) => {
                eprintln!("Lower error:\n{}", e);
                std::process::exit(1);
            }
        }
        return;
    }

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
