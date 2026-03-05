/// Outline pure (no-capture) arrow functions as top-level
/// `function _temp(...)` declarations.
///
/// A function can be outlined only if ALL its free variables are module-level
/// globals/imports (accessed via LoadGlobal). Any component-local capture
/// (even a stable one like `ref = useRef(null)`) prevents outlining.
///
/// This mirrors OutlineFunctions.ts in the TypeScript React compiler.
use std::collections::HashSet;

use crate::hir::environment::Environment;
use crate::hir::hir::{FunctionExpressionType, HIRFunction, InstructionValue, NonLocalBinding};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn outline_functions(hir: &mut HIRFunction, env: &mut Environment) {
    // Build the set of names that are module-level (LoadGlobal of any variant,
    // plus module-level variable declarations collected during lowering).
    // Free variables that appear in this set are safe to reference from an
    // outlined (hoisted) function.
    let mut module_names: HashSet<String> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                let name = match binding {
                    NonLocalBinding::Global { name } => name.clone(),
                    NonLocalBinding::ImportDefault { name, .. } => name.clone(),
                    NonLocalBinding::ImportNamespace { name, .. } => name.clone(),
                    NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                    NonLocalBinding::ModuleLocal { name } => name.clone(),
                };
                module_names.insert(name);
            }
        }
    }
    // Also include module-level variable declarations (let/const/var at module scope).
    // These are stable from the component's perspective — they don't change per render.
    module_names.extend(env.module_level_names.iter().cloned());

    // Collect all named local variables in the outer (component) scope.
    // Used to detect when an outlined function's param shadows an outer variable.
    let mut outer_local_names: HashSet<String> = HashSet::new();
    for (id, ident) in &env.identifiers {
        if let Some(name) = &ident.name {
            let n = name.value().to_string();
            outer_local_names.insert(n);
        }
        let _ = id;
    }

    // Collect FunctionExpression lvalue IDs that are immediately called (IIFEs).
    // These must NOT be outlined — they should be inlined or left as-is.
    let mut iife_fn_ids: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::CallExpression { callee, args, .. } = &instr.value {
                if args.is_empty() {
                    iife_fn_ids.insert(callee.identifier.0);
                }
            }
        }
    }

    let mut temp_counter = 0usize;

    // If @enableNameAnonymousFunctions is set, build a map from FunctionExpression
    // lvalue identifier id → the named variable it flows into (via StoreLocal).
    // This lets us generate meaningful names like `_ComponentOnClick` instead of `_temp`.
    let enable_named = env.config.enable_name_anonymous_functions;
    let component_name = hir.id.as_deref().unwrap_or("").to_string();
    let mut fn_id_to_var_name: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    if enable_named && !component_name.is_empty() {
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    if let Some(lv_name) = env.get_identifier(lvalue.place.identifier)
                        .and_then(|id| id.name.as_ref())
                        .map(|n| n.value().to_string())
                    {
                        fn_id_to_var_name.insert(value.identifier.0, lv_name);
                    }
                }
            }
        }
    }

    for (_, block) in &mut hir.body.blocks {
        for instr in &mut block.instructions {
            if let InstructionValue::FunctionExpression {
                name_hint,
                lowered_func,
                fn_type,
                ..
            } = &mut instr.value
            {
                // Only outline arrow functions.
                if *fn_type != FunctionExpressionType::Arrow {
                    continue;
                }

                // Skip IIFEs — they are immediately called and should not be outlined.
                if iife_fn_ids.contains(&instr.lvalue.identifier.0) {
                    continue;
                }

                let src = lowered_func.func.original_source.clone();
                if src.is_empty() {
                    continue;
                }

                // Parse the arrow function to get param names and body text.
                let Some(info) = parse_arrow_info(&src) else {
                    continue;
                };

                // Check if any free variable in the body is a component-local.
                // A variable prevents outlining ONLY if it's known to the component
                // (in outer_local_names) but is NOT a module-level global/import.
                // Truly unknown globals (not in outer_local_names at all) are safe.
                let free_vars = extract_free_variables(&info.body_text, &info.param_names);
                if free_vars.iter().any(|v| {
                    outer_local_names.contains(v.as_str()) && !module_names.contains(v.as_str())
                }) {
                    continue; // Captures component-local vars — cannot outline.
                }

                // Generate a unique name.
                // With @enableNameAnonymousFunctions, use `_ComponentOnClick` style names
                // (component name + capitalized variable name). Otherwise `_temp`, `_temp2`, ...
                let temp_name = if enable_named && !component_name.is_empty() {
                    if let Some(var_name) = fn_id_to_var_name.get(&instr.lvalue.identifier.0) {
                        // Capitalize first letter of var name.
                        let mut cap = var_name.clone();
                        if let Some(f) = cap.get_mut(0..1) { f.make_ascii_uppercase(); }
                        format!("_{}{}", component_name, cap)
                    } else {
                        if temp_counter == 0 { "_temp".to_string() } else { format!("_temp{}", temp_counter) }
                    }
                } else if temp_counter == 0 {
                    "_temp".to_string()
                } else {
                    format!("_temp{}", temp_counter)
                };
                temp_counter += 1;

                // Rename params that shadow outer local variables (add _0 suffix).
                // This matches the TS compiler's behavior of renaming shadowing params.
                let final_param_names: Vec<String> = info.param_names.iter().map(|p| {
                    if outer_local_names.contains(p.as_str()) {
                        format!("{}_0", p)
                    } else {
                        p.clone()
                    }
                }).collect();

                // Rename uses of renamed params in the body text.
                let mut renamed_body = info.body_text.clone();
                for (orig, renamed) in info.param_names.iter().zip(final_param_names.iter()) {
                    if orig != renamed {
                        renamed_body = rename_word(&renamed_body, orig, renamed);
                    }
                }

                // Rename catch parameters to temp names (t0, t1, ...) to match
                // the reference compiler's rename_variables behavior.
                renamed_body = rename_catch_params(&renamed_body);

                // Build the function declaration.
                let params_str = final_param_names.join(", ");
                let decl = if info.is_expr_body {
                    format!(
                        "function {}({}) {{\n  return {};\n}}",
                        temp_name, params_str, renamed_body
                    )
                } else {
                    // Block body already includes `{ ... }`.
                    format!("function {}({}) {}", temp_name, params_str, renamed_body)
                };

                // Mark this instruction as outlined so codegen emits just the name.
                *name_hint = Some(temp_name.clone());

                env.outlined_functions.push((temp_name, decl));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Arrow function parser
// ---------------------------------------------------------------------------

struct ArrowInfo {
    param_names: Vec<String>,
    body_text: String,
    is_expr_body: bool,
}

/// Parse an arrow function source text (e.g. `item => item` or `(a, b) => a + b`).
/// Returns `None` if parsing fails or the function has complex params.
fn parse_arrow_info(src: &str) -> Option<ArrowInfo> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement};
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    let alloc = Allocator::default();
    // Wrap as a variable initializer so oxc treats it as an expression.
    let stmt_src = format!("let _ = {};", src);
    let parsed = Parser::new(&alloc, &stmt_src, SourceType::jsx()).parse();
    if !parsed.errors.is_empty() {
        return None;
    }

    let stmt = parsed.program.body.first()?;
    let arrow = match stmt {
        Statement::VariableDeclaration(vd) => {
            let decl = vd.declarations.first()?;
            let init = decl.init.as_ref()?;
            match init {
                Expression::ArrowFunctionExpression(a) => a.as_ref(),
                _ => return None,
            }
        }
        _ => return None,
    };

    // Only handle simple binding identifier params.
    let mut param_names = Vec::new();
    for param in &arrow.params.items {
        match &param.pattern.kind {
            BindingPatternKind::BindingIdentifier(id) => {
                param_names.push(id.name.to_string());
            }
            _ => return None, // Destructuring — skip for now.
        }
    }
    if arrow.params.rest.is_some() {
        return None; // Rest params — skip.
    }

    // Extract body text by adjusting the span for the `let _ = ` prefix (8 bytes).
    let prefix_len: usize = "let _ = ".len(); // 8
    let body_span = arrow.body.span;
    let body_start = (body_span.start as usize).saturating_sub(prefix_len);
    let body_end = (body_span.end as usize).saturating_sub(prefix_len);
    let body_text = src.get(body_start..body_end)?.to_string();

    Some(ArrowInfo {
        param_names,
        body_text,
        is_expr_body: arrow.expression,
    })
}

// ---------------------------------------------------------------------------
// Identifier helpers
// ---------------------------------------------------------------------------

/// Collect identifier tokens from `body` that are NOT params and NOT preceded by `.`
/// (property accesses). Excludes JS keywords, string literals, and comments.
fn extract_free_variables(body: &str, param_names: &[String]) -> HashSet<String> {
    let param_set: HashSet<&str> = param_names.iter().map(|s| s.as_str()).collect();
    let mut free_vars = HashSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Skip line comments: // ...
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip block comments: /* ... */
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        // Skip string literals: '...', "...", `...`
        if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != quote {
                if bytes[i] == b'\\' {
                    i += 1; // skip escaped char
                }
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // skip closing quote
            }
            continue;
        }
        // Identifier token
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' || bytes[i] == b'$' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            let tok = &body[start..i];
            // Property access: preceded by `.`
            let is_prop = start > 0 && bytes[start - 1] == b'.';
            if !is_prop && !is_js_keyword(tok) && !param_set.contains(tok) {
                free_vars.insert(tok.to_string());
            }
        } else {
            i += 1;
        }
    }
    free_vars
}

/// Replace whole-word occurrences of `old` with `new` in `text`.
fn rename_word(text: &str, old: &str, new: &str) -> String {
    let mut result = String::new();
    let bytes = text.as_bytes();
    let old_bytes = old.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(old_bytes) {
            let before_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let after_pos = i + old.len();
            let after_ok = after_pos >= bytes.len() || !is_word_byte(bytes[after_pos]);
            if before_ok && after_ok {
                result.push_str(new);
                i = after_pos;
                continue;
            }
        }
        // Append one character. Assumes source is mostly ASCII; multi-byte safe
        // because we only replace ASCII identifiers.
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Rename catch parameters to temp names (t0, t1, ...) in source text.
/// Finds `catch (NAME)` or `catch(NAME)` patterns and renames the identifier + all uses.
fn rename_catch_params(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = text.to_string();
    let mut counter = 0u32;
    let catch_keyword = b"catch";
    let mut i = 0;
    let mut catches: Vec<String> = Vec::new();
    while i + 5 < bytes.len() {
        if &bytes[i..i + 5] == catch_keyword {
            // Check word boundary before "catch"
            if i > 0 && is_word_byte(bytes[i - 1]) {
                i += 1;
                continue;
            }
            let mut j = i + 5;
            // Skip whitespace
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                j += 1;
                // Skip whitespace
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                // Extract identifier
                let start = j;
                while j < bytes.len() && is_word_byte(bytes[j]) {
                    j += 1;
                }
                if j > start {
                    let name = std::str::from_utf8(&bytes[start..j]).unwrap_or("").to_string();
                    if !name.is_empty() {
                        catches.push(name);
                    }
                }
            }
        }
        i += 1;
    }
    for name in catches {
        let new_name = format!("t{}", counter);
        counter += 1;
        if name != new_name {
            result = rename_word(&result, &name, &new_name);
        }
    }
    result
}

fn is_js_keyword(tok: &str) -> bool {
    matches!(
        tok,
        "break"
            | "case"
            | "catch"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "class"
            | "const"
            | "enum"
            | "export"
            | "extends"
            | "import"
            | "super"
            | "implements"
            | "interface"
            | "let"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "static"
            | "yield"
            | "null"
            | "true"
            | "false"
            | "undefined"
            | "async"
            | "await"
            | "of"
            | "arguments"
    )
}
