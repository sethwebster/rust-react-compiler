use std::path::PathBuf;
use react_compiler::entrypoint::pipeline::{compile, CompileOptions};
use oxc_span::SourceType;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler"
);

fn main() {
    let builder = std::thread::Builder::new().stack_size(512 * 1024 * 1024);
    let handle = builder.spawn(run).expect("spawn thread");
    handle.join().expect("join thread");
}

fn run() {
    let dir = PathBuf::from(FIXTURE_DIR);
    let entries = std::fs::read_dir(&dir).expect("fixture dir exists");
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("js" | "jsx" | "ts" | "tsx")))
        .collect();
    paths.sort();

    let mut dollar_t_count = 0;
    let mut dollar_t_fixtures: Vec<(String, Vec<String>)> = Vec::new();
    let mut total = 0;

    for path in &paths {
        let name = path.file_name().unwrap().to_str().unwrap();
        if name.starts_with("error.") || name.starts_with("todo.error.") { continue; }

        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let first = source.lines().next().unwrap_or("");
        if first.contains("@flow") { continue; }

        let source_type = match path.extension().and_then(|e| e.to_str()) {
            Some("tsx") => SourceType::tsx(),
            Some("ts") => SourceType::ts(),
            Some("jsx") | Some("js") => SourceType::jsx(),
            _ => SourceType::mjs(),
        };

        total += 1;
        let opts = CompileOptions {
            source_type,
            filename: Some(path.display().to_string()),
            ..Default::default()
        };
        let output = match compile(&source, opts) {
            Ok(o) => o.js,
            Err(_) => continue,
        };

        // Check for $tN patterns in the output
        let dollar_t_lines: Vec<String> = output.lines()
            .filter(|line| {
                // Check for $t followed by a digit, but not inside strings
                let mut chars = line.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '"' || c == '\'' || c == '`' {
                        // Skip string content
                        let quote = c;
                        while let Some(sc) = chars.next() {
                            if sc == '\\' { chars.next(); continue; }
                            if sc == quote { break; }
                        }
                        continue;
                    }
                    if c == '$' {
                        if let Some(&'t') = chars.peek() {
                            chars.next();
                            if let Some(&d) = chars.peek() {
                                if d.is_ascii_digit() {
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
            })
            .map(|l| l.trim().to_string())
            .collect();

        if !dollar_t_lines.is_empty() {
            dollar_t_count += 1;
            dollar_t_fixtures.push((name.to_string(), dollar_t_lines));
        }
    }

    println!("Total compiled fixtures: {}", total);
    println!("Fixtures with $tN in output: {}", dollar_t_count);
    println!();

    // Show first 20
    for (i, (name, lines)) in dollar_t_fixtures.iter().take(20).enumerate() {
        println!("{}. {} ({} occurrences)", i + 1, name, lines.len());
        for line in lines.iter().take(3) {
            println!("   {}", line);
        }
    }

    if dollar_t_fixtures.len() > 20 {
        println!("... and {} more", dollar_t_fixtures.len() - 20);
    }
}
