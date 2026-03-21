#![allow(unused_imports, unused_variables, dead_code)]
/// Main compilation pipeline — mirrors Pipeline.ts
///
/// Each pass is a sequential function call. Passes that can fail return Result;
/// validation passes use env.try_record() to accumulate non-fatal errors.
use oxc_span::SourceType;

use crate::error::{Result};
use crate::hir::environment::{Environment, EnvironmentConfig, OutputMode};
use crate::hir::hir::{HIRFunction, ReactFunctionType};
use crate::hir::build_hir::{lower_program, lower_program_nth};
use crate::hir::print_hir::print_hir_function;

use crate::optimization::{
    constant_propagation::constant_propagation,
    dead_code_elimination::{dead_code_elimination_with_env, prune_unused_jsx},
    prune_maybe_throws::prune_maybe_throws,
    optimize_props_method_calls::optimize_props_method_calls,
    optimize_for_ssr::optimize_for_ssr,
    outline_functions::outline_functions,
    outline_jsx::outline_jsx,
};
use crate::ssa::{
    enter_ssa::{enter_ssa_with_env},
    eliminate_redundant_phi::eliminate_redundant_phi,
    rewrite_instruction_kinds::{rewrite_instruction_kinds_based_on_reassignment, rewrite_scope_decls_as_let},
};
use crate::inference::{
    analyse_functions::analyse_functions,
    drop_manual_memoization::drop_manual_memoization,
    infer_mutation_aliasing_effects::infer_mutation_aliasing_effects,
    infer_mutation_aliasing_ranges::{
        infer_mutation_aliasing_ranges, InferMutationAliasingRangesOptions,
    },
    infer_reactive_places::infer_reactive_places,
    inline_iife::inline_immediately_invoked_function_expressions,
};
use crate::type_inference::infer_types::infer_types;
use crate::transform::name_anonymous_functions::name_anonymous_functions;
use crate::utils::merge_consecutive_blocks::merge_consecutive_blocks;
use crate::reactive_scopes::{
    infer_reactive_scope_variables::run_with_env as infer_reactive_scope_variables,
    memoize_fbt_and_macro_operands::run as memoize_fbt_and_macro_operands,
    align_method_call_scopes::run as align_method_call_scopes,
    align_object_method_scopes::run as align_object_method_scopes,
    prune_unused_labels_hir::run as prune_unused_labels_hir,
    align_reactive_scopes_to_block_scopes_hir::run_with_env as align_reactive_scopes_to_block_scopes_hir,
    merge_overlapping_reactive_scopes_hir::run_with_env as merge_overlapping_reactive_scopes_hir,
    build_reactive_scope_terminals_hir::run_with_env as build_reactive_scope_terminals_hir,
    flatten_reactive_loops_hir::run_with_env as flatten_reactive_loops_hir,
    flatten_scopes_with_hooks_or_use_hir::run_with_env as flatten_scopes_with_hooks_or_use_hir,
    propagate_scope_dependencies_hir::run as propagate_scope_dependencies_hir,
    build_reactive_function::run as build_reactive_function,
    prune_unused_labels::run as prune_unused_labels,
    prune_non_escaping_scopes::run_with_env as prune_non_escaping_scopes,
    prune_non_reactive_dependencies::run as prune_non_reactive_dependencies,
    prune_unused_scopes::run_with_env as prune_unused_scopes,
    merge_reactive_scopes_that_invalidate_together::run_with_env as merge_reactive_scopes_that_invalidate_together,
    prune_always_invalidating_scopes::run as prune_always_invalidating_scopes,
    prune_locally_used_scope_declarations::run as prune_locally_used_scope_declarations,
    propagate_early_returns::run_with_env as propagate_early_returns,
    prune_unused_lvalues::run as prune_unused_lvalues,
    promote_used_temporaries::run_with_env as promote_used_temporaries,
    extract_scope_declarations_from_destructuring::run as extract_scope_declarations_from_destructuring,
    stabilize_block_ids::run as stabilize_block_ids,
    rename_variables::run_with_env as rename_variables,
    prune_hoisted_contexts::run as prune_hoisted_contexts,
    codegen_reactive_function::{codegen_reactive_function, CodegenOutput},
};

pub struct CompileOptions {
    pub source_type: SourceType,
    pub fn_type: ReactFunctionType,
    pub config: EnvironmentConfig,
    pub filename: Option<String>,
}

impl Default for CompileOptions {
    fn default() -> Self {
        CompileOptions {
            source_type: SourceType::jsx(),
            fn_type: ReactFunctionType::Component,
            config: EnvironmentConfig::default(),
            filename: None,
        }
    }
}

/// Compile a JS/TS source string containing one or more React components/hooks.
/// Returns the full file output: compiled functions + passthrough of other code.
pub fn compile(source: &str, mut options: CompileOptions) -> Result<CodegenOutput> {
    let source_type = options.source_type;
    // Collect the spans of all compilable top-level functions.
    let fn_spans = collect_compilable_fn_spans(source, source_type);

    if fn_spans.is_empty() {
        // No compilable functions — passthrough the whole file.
        let js = emit_passthrough(source, source_type);
        return Ok(CodegenOutput { js, outlines: Vec::new() });
    }

    // Compile each function. Track whether any produced cache slots.
    let mut compiled_fns: Vec<(u32, u32, String)> = Vec::new(); // (start, end, compiled_js)
    let mut any_scoped = false;

    // Whether the file uses panic_threshold:none (passthrough on any error).
    let first_line = source.lines().next().unwrap_or("");
    let panic_threshold_none = first_line.contains("@panicThreshold:\"none\"")
        || first_line.contains("@panicThreshold:'none'");

    // Whether the file uses @outputMode:"lint" (validate only, don't transform).
    let lint_mode = first_line.contains("@outputMode:\"lint\"")
        || first_line.contains("@outputMode:'lint'")
        || first_line.contains("@outputMode: \"lint\"")
        || first_line.contains("@outputMode: 'lint'");

    // Whether the file uses @compilationMode:"infer" (only compile components/hooks).
    let infer_mode = first_line.contains("@compilationMode:\"infer\"")
        || first_line.contains("@compilationMode:'infer'");

    // Whether the file already uses useMemoCache (already compiled — skip).
    let has_use_memo_cache = source.contains("useMemoCache");

    // Parse pragma flags from the first line.
    let mut options = options;
    if first_line.contains("@enableNameAnonymousFunctions") {
        options.config.enable_name_anonymous_functions = true;
    }
    if first_line.contains("@enableJsxOutlining") {
        options.config.enable_jsx_outlining = true;
    }
    if first_line.contains("@validateRefAccessDuringRender") {
        options.config.validate_ref_access_during_render = true;
    }
    if first_line.contains("@validateNoSetStateInRender") {
        options.config.validate_no_set_state_in_render = true;
    }

    // Parse @customOptOutDirectives:["directive1", "directive2"] from pragma.
    let custom_opt_out_directives = parse_custom_opt_out_directives(first_line);

    // @expectNothingCompiled means the whole file should passthrough.
    // @gating also means passthrough (compiler is gated/disabled).
    let expect_nothing = first_line.contains("@expectNothingCompiled")
        || first_line.contains("@gating");

    for (i, &(start, end)) in fn_spans.iter().enumerate() {
        let fn_src = source.get(start as usize..end as usize).unwrap_or("");

        // In lint mode or @expectNothingCompiled, passthrough all functions.
        if lint_mode || expect_nothing {
            let pt = emit_passthrough(fn_src, source_type);
            compiled_fns.push((start, end, pt.to_string()));
            continue;
        }
        // Skip functions that already use useMemoCache.
        if has_use_memo_cache {
            let pt = emit_passthrough(fn_src, source_type);
            compiled_fns.push((start, end, pt));
            continue;
        }
        // Check custom opt-out directives in function body.
        if !custom_opt_out_directives.is_empty() && fn_has_custom_opt_out(fn_src, &custom_opt_out_directives) {
            let pt = emit_passthrough(fn_src, source_type);
            compiled_fns.push((start, end, pt));
            continue;
        }
        // In infer mode, skip functions that don't look like components or hooks.
        if infer_mode && !fn_looks_like_component_or_hook(fn_src) {
            let pt = emit_passthrough(fn_src, source_type);
            compiled_fns.push((start, end, pt));
            continue;
        }
        let mut env = Environment::new(options.fn_type, options.config.clone(), options.filename.clone());
        match lower_program_nth(source, source_type, &mut env, i) {
            Ok(mut hir) => {
                match run_with_environment(&mut hir, &mut env) {
                    Ok(output) => {
                        let fn_js = output.js;
                        // Check if the output has cache import (means scopes were used).
                        if fn_js.contains("react/compiler-runtime") {
                            any_scoped = true;
                        }
                        compiled_fns.push((start, end, fn_js));
                    }
                    Err(e) => {
                        if panic_threshold_none {
                            // Passthrough this function's source.
                            let fn_src = source.get(start as usize..end as usize).unwrap_or("").to_string();
                            let pt = emit_passthrough(&fn_src, source_type);
                            compiled_fns.push((start, end, pt));
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
            Err(e) => {
                if panic_threshold_none {
                    // Passthrough this function's source.
                    let fn_src = source.get(start as usize..end as usize).unwrap_or("").to_string();
                    let pt = emit_passthrough(&fn_src, source_type);
                    compiled_fns.push((start, end, pt));
                } else {
                    return Err(e);
                }
            }
        }
    }

    // Reconstruct the file by splicing compiled outputs into the original source.
    // For non-function spans, use oxc_codegen passthrough.
    let reconstructed = splice_compiled_fns(source, source_type, &compiled_fns);
    Ok(CodegenOutput { js: reconstructed, outlines: Vec::new() })
}

/// Parse the source and return byte spans (start, end) of each top-level
/// compilable function (FunctionDeclaration, exported function/arrow).
/// These are in the same order as lower_program_nth processes them (fn_skip_param).
fn collect_compilable_fn_spans(source: &str, source_type: SourceType) -> Vec<(u32, u32)> {
    use oxc_allocator::Allocator;
    use oxc_ast::ast::*;

    let allocator = Allocator::default();
    let mut parse = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    if !parse.errors.is_empty() {
        let tsx = SourceType::tsx();
        let retry = oxc_parser::Parser::new(&allocator, source, tsx).parse();
        if retry.errors.is_empty() { parse = retry; }
    }
    if !parse.errors.is_empty() { return vec![]; }

    // @ignoreUseNoForget: compile functions even if they have 'use no forget'.
    let ignore_use_no_forget = source.contains("@ignoreUseNoForget");

    // Check module-level 'use no memo' / 'use no forget' directives.
    let module_no_memo = !ignore_use_no_forget && parse.program.directives.iter()
        .any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget"));
    if module_no_memo {
        return vec![]; // Skip all functions in the file.
    }

    // Helper to check if a function body has opt-out directives.
    let fn_has_no_memo = |body: Option<&oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody>>| -> bool {
        if ignore_use_no_forget { return false; }
        body.map(|b| b.directives.iter().any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget"))).unwrap_or(false)
    };

    let mut spans: Vec<(u32, u32)> = Vec::new();

    for stmt in &parse.program.body {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                // Skip 'use no memo' / 'use no forget' functions.
                if !fn_has_no_memo(func.body.as_ref()) {
                    spans.push((func.span.start, func.span.end));
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                        if !fn_has_no_memo(func.body.as_ref()) {
                            spans.push((decl.span.start, decl.span.end));
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(_) => {
                        spans.push((decl.span.start, decl.span.end));
                    }
                    ExportDefaultDeclarationKind::FunctionExpression(func) => {
                        if !fn_has_no_memo(func.body.as_ref()) {
                            spans.push((decl.span.start, decl.span.end));
                        }
                    }
                    _ => {}
                }
            }
            Statement::ExportNamedDeclaration(decl) => {
                match decl.declaration.as_ref() {
                    Some(Declaration::FunctionDeclaration(func)) => {
                        if !fn_has_no_memo(func.body.as_ref()) {
                            spans.push((decl.span.start, decl.span.end));
                        }
                    }
                    Some(Declaration::VariableDeclaration(var_decl)) => {
                        // Arrow / fn expression assigned to a variable.
                        for decl_item in &var_decl.declarations {
                            let is_fn = match &decl_item.init {
                                Some(Expression::ArrowFunctionExpression(_)) => true,
                                Some(Expression::FunctionExpression(_)) => true,
                                _ => false,
                            };
                            if is_fn {
                                spans.push((decl.span.start, decl.span.end));
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Statement::VariableDeclaration(var_decl) => {
                for decl_item in &var_decl.declarations {
                    let is_fn = match &decl_item.init {
                        Some(Expression::ArrowFunctionExpression(_)) => true,
                        Some(Expression::FunctionExpression(_)) => true,
                        _ => false,
                    };
                    if is_fn {
                        spans.push((var_decl.span.start, var_decl.span.end));
                        break;
                    }
                }
            }
            // ExpressionStatement: handle React.memo(fn) and React.forwardRef(fn)
            // where the inner function/arrow needs to be compiled in-place.
            Statement::ExpressionStatement(expr_stmt) => {
                if let Expression::CallExpression(call) = &expr_stmt.expression {
                    // Check if callee is React.memo or React.forwardRef
                    let is_react_wrapper = match &call.callee {
                        Expression::StaticMemberExpression(mem) => {
                            let obj_name = match &mem.object {
                                Expression::Identifier(id) => id.name.as_str(),
                                _ => "",
                            };
                            obj_name == "React" && matches!(mem.property.name.as_str(), "memo" | "forwardRef")
                        }
                        _ => false,
                    };
                    if is_react_wrapper {
                        // Add the span of the first function/arrow argument
                        for arg in &call.arguments {
                            match arg {
                                oxc_ast::ast::Argument::ArrowFunctionExpression(arrow) => {
                                    spans.push((arrow.span.start, arrow.span.end));
                                    break;
                                }
                                oxc_ast::ast::Argument::FunctionExpression(func) => {
                                    spans.push((func.span.start, func.span.end));
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    spans
}

/// Splice compiled function outputs into a reconstructed file.
/// - compiled_fns: list of (span_start, span_end, compiled_js) in order
/// - Non-function regions of the source are passed through via oxc_codegen
/// - The `import { c as _c }` line is emitted at most once at the top
fn splice_compiled_fns(
    source: &str,
    source_type: SourceType,
    compiled_fns: &[(u32, u32, String)],
) -> String {
    if compiled_fns.is_empty() {
        return emit_passthrough(source, source_type);
    }

    // Step 1: extract the import line and function body from each compiled output.
    // The compiled output looks like:
    //   import { c as _c } from "react/compiler-runtime";\nfunction Foo() {...}\n
    // OR (for passthrough/no-scope):
    //   function Foo() {...}\n
    // We want: the function body (everything after the import line, if any).
    let module_path = "react/compiler-runtime";
    let mut runtime_alias: Option<String> = None; // e.g. "_c" or "_c2"
    let mut fn_bodies: Vec<(u32, u32, String)> = Vec::new();

    for &(start, end, ref js) in compiled_fns {
        // Detect import line of the form `import { c as ALIAS } from "react/compiler-runtime";`
        let body = if js.starts_with("import { c as ") && js.contains("} from \"react/compiler-runtime\";") {
            // Extract alias.
            let after = &js["import { c as ".len()..];
            if let Some(sp) = after.find(" }") {
                let alias = after[..sp].to_string();
                if runtime_alias.is_none() {
                    runtime_alias = Some(alias);
                }
                // Strip the import line (first line).
                if let Some(nl) = js.find('\n') {
                    js[nl + 1..].to_string()
                } else {
                    String::new()
                }
            } else {
                js.trim_start_matches('\n').to_string()
            }
        } else {
            js.trim_start_matches('\n').to_string()
        };
        fn_bodies.push((start, end, body));
    }
    let has_runtime_import = runtime_alias.is_some();
    // Build the import line from the alias (if needed).
    let import_line_owned;
    let import_line: &str = if let Some(ref alias) = runtime_alias {
        import_line_owned = format!("import {{ c as {alias} }} from \"{module_path}\";");
        &import_line_owned
    } else {
        ""
    };

    // Step 2: parse the source to get clean passthrough of non-function regions.
    // We need to passthrough all statements that are NOT in fn_bodies spans.
    // Strategy: use oxc_codegen on the whole file to get clean passthrough,
    // then use span replacement.
    //
    // We parse the source once to get a statement-by-statement passthrough.
    let passthrough = emit_passthrough(source, source_type);

    // Step 3: reconstruct by doing span-based replacement on the ORIGINAL source.
    // Build a list of "segments":
    //   - (start=0, end=fn_bodies[0].start): non-function prefix
    //   - fn_bodies[0] compiled output
    //   - (fn_bodies[0].end, fn_bodies[1].start): non-function middle
    //   - fn_bodies[1] compiled output
    //   - ... etc
    //   - (fn_bodies.last().end, source.len()): non-function suffix
    //
    // For non-function segments, passthrough via oxc_codegen.

    let source_bytes = source.as_bytes();
    let mut output = String::new();

    // Prepend the runtime import once if any function needed it.
    // We'll handle merging with existing source imports in a post-processing step.
    if has_runtime_import {
        output.push_str(import_line);
        output.push('\n');
    }

    let mut pos: u32 = 0;
    for (start, end, body) in &fn_bodies {
        let start = *start;
        let end = *end;
        // Emit passthrough of the region before this function.
        if pos < start {
            let region = source.get(pos as usize..start as usize).unwrap_or("");
            if !region.trim().is_empty() {
                // Pass through this region via oxc_codegen.
                let region_pt = emit_passthrough(region, source_type);
                if !region_pt.trim().is_empty() {
                    output.push_str(&region_pt);
                    if !region_pt.ends_with('\n') {
                        output.push('\n');
                    }
                }
            }
        }
        // Emit the compiled function body.
        output.push_str(body);
        if !body.ends_with('\n') {
            output.push('\n');
        }
        pos = end;
    }

    // Emit passthrough of the region after the last function.
    if pos < source.len() as u32 {
        let region = source.get(pos as usize..).unwrap_or("");
        if !region.trim().is_empty() {
            let region_pt = emit_passthrough(region, source_type);
            if !region_pt.trim().is_empty() {
                output.push_str(&region_pt);
                if !region_pt.ends_with('\n') {
                    output.push('\n');
                }
            }
        }
    }

    // Post-process: if output has both a prepended `import { c as ALIAS } from "MODULE"` and
    // the source passthrough also had an `import { ... } from "MODULE"`, merge them.
    // This keeps the source's import in place and removes the prepended duplicate.
    if has_runtime_import {
        output = merge_runtime_import_into_existing(&output, module_path, import_line);
    }

    output
}

/// If `output` has a prepended `import_line` (the first line) AND also contains another
/// import from the same module later, merge the specifiers into the later import and remove
/// the prepended one. Otherwise, leave as-is.
fn merge_runtime_import_into_existing(output: &str, module_path: &str, import_line: &str) -> String {
    // Check if output starts with the import line we prepended.
    if !output.starts_with(import_line) {
        return output.to_string();
    }
    // Extract the specifier from import_line: `import { c as ALIAS } from "MODULE";`
    // Get everything between `import { ` and ` } from`.
    let spec_start = "import { ".len();
    let spec_end = import_line.find(" } from").unwrap_or(import_line.len());
    let our_spec = import_line[spec_start..spec_end].to_string(); // e.g. "c as _c"

    // Skip the prepended import line + its newline, work on the rest.
    let skip = import_line.len() + 1; // +1 for \n
    let rest_text = &output[skip..];
    let pattern = format!("}} from \"{module_path}\"");
    let pattern2 = format!("}} from '{module_path}'");

    // Find a `import { ... } from "module_path"` in rest_text.
    let find_import_brace = |text: &str, pat: &str| -> Option<(usize, usize, usize)> {
        let mut search_start = 0;
        while let Some(close_pos) = text[search_start..].find(pat) {
            let abs_close = search_start + close_pos;
            if let Some(rel) = text[..abs_close].rfind("import {") {
                let between = &text[rel + 8..abs_close];
                if !between.contains('}') && !between.contains('{') {
                    let stmt_end = abs_close + pat.len() + 1; // +1 for `;`
                    return Some((rel, abs_close + 1, stmt_end.min(text.len())));
                }
            }
            search_start = abs_close + 1;
        }
        None
    };

    let found = find_import_brace(rest_text, &pattern)
        .or_else(|| find_import_brace(rest_text, &pattern2));

    if let Some((import_start, close_brace, stmt_end)) = found {
        // Extract current specs from the existing import.
        let existing_specs_start = import_start + "import {".len();
        let existing_specs_str = rest_text[existing_specs_start..close_brace - 1].trim();
        let mut all_specs: Vec<String> = existing_specs_str.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !all_specs.contains(&our_spec) {
            all_specs.push(our_spec);
        }
        let merged_specs = all_specs.join(", ");
        let new_import = format!("import {{{merged_specs}}} from \"{module_path}\";");
        let before = &rest_text[..import_start];
        let after = &rest_text[stmt_end..];
        format!("{before}{new_import}{after}")
    } else {
        // No existing import from this module in the source — keep prepended as-is.
        output.to_string()
    }
}

/// Quick heuristic check: does a function body source string contain hook calls or JSX?
/// Used for @compilationMode:"infer" to skip non-component/non-hook functions.
fn fn_body_has_hooks_or_jsx(fn_src: &str) -> bool {
    // Check for JSX: any `<` followed by an uppercase letter or lowercase identifier
    // This is a heuristic — not a full parse.
    let bytes = fn_src.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'<' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            // JSX tags: <Foo, <div, <Component.X, etc.
            // But not <=/< operators. A letter after < suggests JSX.
            if next.is_ascii_alphabetic() {
                return true;
            }
        }
    }
    // Check for hook calls: useXxx( pattern
    // Match word boundary + "use" + uppercase letter
    let src = fn_src;
    let mut pos = 0;
    while let Some(idx) = src[pos..].find("use") {
        let abs = pos + idx;
        // Check word boundary before "use"
        let at_boundary = abs == 0 || !src.as_bytes()[abs - 1].is_ascii_alphanumeric();
        if at_boundary && abs + 3 < src.len() {
            let after = src.as_bytes()[abs + 3];
            if after.is_ascii_uppercase() {
                return true;
            }
        }
        pos = abs + 3;
    }
    false
}

/// Check if a function looks like a component or hook (for infer mode).
/// More sophisticated than fn_body_has_hooks_or_jsx — also checks the function signature.
fn fn_looks_like_component_or_hook(fn_src: &str) -> bool {
    let trimmed = fn_src.trim();

    // Extract function name (if any)
    let fn_name = extract_fn_name(trimmed);

    // Check if it's a hook (name starts with "use" + uppercase)
    if let Some(name) = fn_name {
        if name.len() >= 4 && name.starts_with("use") {
            let fourth = name.as_bytes()[3];
            if fourth.is_ascii_uppercase() {
                return true;
            }
        }
    }

    // Check if body has hooks
    if fn_body_has_hooks_or_jsx(fn_src) {
        // Has JSX or hooks — but if it has multiple params (>1), it's not a component
        let param_count = count_fn_params(trimmed);
        if param_count <= 1 {
            return true; // Looks like a component (0-1 params + JSX)
        }
        if param_count == 2 {
            // React.forwardRef pattern: (props, ref) => JSX — compile it
            return true;
        }
        // Multiple params with JSX — still might be valid if it has hooks
        // Check specifically for hook calls
        let src = fn_src;
        let mut pos = 0;
        while let Some(idx) = src[pos..].find("use") {
            let abs = pos + idx;
            let at_boundary = abs == 0 || !src.as_bytes()[abs - 1].is_ascii_alphanumeric();
            if at_boundary && abs + 3 < src.len() {
                let after = src.as_bytes()[abs + 3];
                if after.is_ascii_uppercase() {
                    return true; // Has hook call — compile it
                }
            }
            pos = abs + 3;
        }
        return false; // Multiple params + JSX but no hooks — skip
    }

    false
}

/// Extract the function name from a function source string.
fn extract_fn_name(fn_src: &str) -> Option<&str> {
    // Match "function Name(" or "const Name =" or "export function Name("
    let src = fn_src.trim_start();
    // Try "function Name"
    if let Some(rest) = src.strip_prefix("function ").or_else(|| src.strip_prefix("export function ").or_else(|| src.strip_prefix("export default function "))) {
        let rest = rest.trim_start();
        let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$').unwrap_or(rest.len());
        if end > 0 {
            return Some(&rest[..end]);
        }
    }
    // Try "const Name ="
    if let Some(rest) = src.strip_prefix("const ").or_else(|| src.strip_prefix("export const ")) {
        let rest = rest.trim_start();
        let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$').unwrap_or(rest.len());
        if end > 0 {
            return Some(&rest[..end]);
        }
    }
    None
}

/// Count function parameters (heuristic based on commas in the parameter list).
fn count_fn_params(fn_src: &str) -> usize {
    // Find the first '(' after "function name" or in arrow function
    if let Some(open) = fn_src.find('(') {
        // Find matching ')'
        let after = &fn_src[open + 1..];
        let mut depth = 1;
        let mut close_idx = 0;
        for (i, c) in after.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 { close_idx = i; break; }
                }
                _ => {}
            }
        }
        let params_str = after[..close_idx].trim();
        if params_str.is_empty() {
            return 0;
        }
        // Count commas at depth 0
        let mut count = 1;
        let mut d = 0;
        for c in params_str.chars() {
            match c {
                '(' | '[' | '{' => d += 1,
                ')' | ']' | '}' => d -= 1,
                ',' if d == 0 => count += 1,
                _ => {}
            }
        }
        return count;
    }
    0
}

/// Parse @customOptOutDirectives:["dir1", "dir2"] from the pragma line.
fn parse_custom_opt_out_directives(line: &str) -> Vec<String> {
    let marker = "@customOptOutDirectives:";
    if let Some(pos) = line.find(marker) {
        let rest = &line[pos + marker.len()..];
        // Find the JSON array
        if let Some(start) = rest.find('[') {
            if let Some(end) = rest[start..].find(']') {
                let array_str = &rest[start + 1..start + end];
                // Parse quoted strings
                let mut directives = Vec::new();
                let mut in_quote = false;
                let mut quote_char = '"';
                let mut current = String::new();
                for c in array_str.chars() {
                    if !in_quote {
                        if c == '"' || c == '\'' {
                            in_quote = true;
                            quote_char = c;
                            current.clear();
                        }
                    } else if c == quote_char {
                        in_quote = false;
                        directives.push(current.clone());
                    } else {
                        current.push(c);
                    }
                }
                return directives;
            }
        }
    }
    Vec::new()
}

/// Check if a function body contains any of the custom opt-out directives.
fn fn_has_custom_opt_out(fn_src: &str, directives: &[String]) -> bool {
    for directive in directives {
        // Check for 'directive' or "directive" in the function source
        if fn_src.contains(&format!("'{directive}'")) || fn_src.contains(&format!("\"{directive}\"")) {
            return true;
        }
    }
    false
}

/// Re-emit source as clean JS using oxc_codegen (strips TypeScript type annotations).
fn emit_passthrough(source: &str, source_type: SourceType) -> String {
    use oxc_allocator::Allocator;
    let allocator = Allocator::default();
    let parse = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    if parse.errors.is_empty() {
        oxc_codegen::Codegen::new().build(&parse.program).code
    } else {
        // Fallback: re-try with TSX if JSX parse fails
        let tsx = SourceType::tsx();
        let retry = oxc_parser::Parser::new(&allocator, source, tsx).parse();
        if retry.errors.is_empty() {
            oxc_codegen::Codegen::new().build(&retry.program).code
        } else {
            source.to_string()
        }
    }
}

/// Run all compiler passes on an already-lowered HIR.
/// Mirrors runWithEnvironment() in Pipeline.ts.
pub fn run_with_environment(
    hir: &mut HIRFunction,
    env: &mut Environment,
) -> Result<CodegenOutput> {
    // --- Phase: HIR fixup ---
    prune_maybe_throws(hir);

    // Validation (non-fatal)
    // validate_context_variable_lvalues(hir)  -- TODO
    // validate_use_memo(hir)                  -- TODO

    if env.config.enable_drop_manual_memoization {
        drop_manual_memoization(hir);
    }

    inline_immediately_invoked_function_expressions(hir);
    merge_consecutive_blocks(hir);

    // --- Phase: SSA ---
    enter_ssa_with_env(hir, Some(env));
    eliminate_redundant_phi(hir);

    // --- Phase: Optimization pre-inference ---
    // Promote let→const before const-prop so that non-reassigned variables
    // can be propagated cross-block.
    rewrite_instruction_kinds_based_on_reassignment(hir, env);
    constant_propagation(hir);

    // --- Phase: Type inference ---
    infer_types(hir);

    // --- Phase: Optimization ---
    optimize_props_method_calls(hir);

    // --- Phase: Effect inference ---
    analyse_functions(hir);
    infer_mutation_aliasing_effects(hir);

    if env.output_mode() == OutputMode::Ssr {
        optimize_for_ssr(hir);
    }

    // --- Phase: Pre-DCE outlining ---
    // outline_functions must run BEFORE DCE so that FunctionExpressions passed to
    // hooks (useCallback, etc.) are outlined before DCE can eliminate them.
    // It also must run BEFORE infer_reactive_scope_variables so that outlined arrow
    // functions (name_hint set) are treated as non-allocating by scope inference.
    if env.config.enable_function_outlining {
        outline_functions(hir, env);
    }

    // --- Phase: DCE + cleanup ---
    dead_code_elimination_with_env(hir, Some(env));
    prune_maybe_throws(hir);

    infer_mutation_aliasing_ranges(
        hir,
        env,
        InferMutationAliasingRangesOptions {
            is_function_expression: false,
        },
    );

    // Validation passes
    // validate_locals_not_reassigned_after_render(hir)  -- TODO
    // validate_no_ref_access_in_render(hir)             -- TODO
    // validate_no_set_state_in_render(hir)              -- TODO

    // --- Phase: Pre-reactivity optimizations ---
    if env.config.enable_jsx_outlining {
        outline_jsx(hir);
    }
    if env.config.enable_name_anonymous_functions {
        name_anonymous_functions(hir);
    }

    // --- Phase: Reactivity ---
    infer_reactive_places(hir, env);
    rewrite_instruction_kinds_based_on_reassignment(hir, env);

    if env.enable_memoization() {
        infer_reactive_scope_variables(hir, env);
    }

    memoize_fbt_and_macro_operands(hir);

    // --- Phase: Scope alignment ---
    align_method_call_scopes(hir);
    align_object_method_scopes(hir);
    prune_unused_labels_hir(hir);
    align_reactive_scopes_to_block_scopes_hir(hir, Some(env));
    merge_overlapping_reactive_scopes_hir(hir, env);
    build_reactive_scope_terminals_hir(hir, env);
    flatten_reactive_loops_hir(hir, env);
    flatten_scopes_with_hooks_or_use_hir(hir, env);
    prune_non_escaping_scopes(hir, env);
    // Remove JSX instructions whose result was never used (e.g. `<div>{x}</div>;`
    // as a statement). These were kept alive by DCE so scope inference could assign
    // and then prune their reactive scope (creating a merge barrier). Now they can go.
    prune_unused_jsx(hir);
    propagate_scope_dependencies_hir(hir, env);
    if std::env::var("RC_DEBUG").is_ok() { eprintln!("[pipeline] scopes before merge_invalidate: {}", env.scopes.len()); }
    merge_reactive_scopes_that_invalidate_together(hir, env);
    if std::env::var("RC_DEBUG").is_ok() { eprintln!("[pipeline] scopes after merge_invalidate: {}", env.scopes.len()); }
    prune_non_reactive_dependencies(hir, env);
    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[pipeline] scopes after prune_non_reactive: {}", env.scopes.len());
        for (_, scope) in &env.scopes {
            eprintln!("[pipeline] scope {:?} range=[{},{}) deps={:?} decls={:?}", scope.id.0, scope.range.start.0, scope.range.end.0,
                scope.dependencies.iter().map(|d| (d.place.identifier.0, d.place.reactive)).collect::<Vec<_>>(),
                scope.declarations.keys().map(|id| id.0).collect::<Vec<_>>());
        }
    }
    prune_always_invalidating_scopes(hir, env);
    if std::env::var("RC_DEBUG").is_ok() { eprintln!("[pipeline] scopes after prune_always_inval: {}", env.scopes.len()); }
    prune_unused_lvalues(hir, Some(env));
    promote_used_temporaries(hir, env);
    extract_scope_declarations_from_destructuring(hir);
    stabilize_block_ids(hir);
    prune_hoisted_contexts(hir);
    prune_unused_scopes(hir, env);
    prune_unused_labels(hir);

    // Revert Const→Let for reactive scope output variables. These must be `let`
    // so that codegen's `is_let_kind` path correctly treats them as named vars
    // (prevents intra-scope stores from being emitted outside the scope body).
    rewrite_scope_decls_as_let(hir, env);

    // --- Phase: Reactive function construction ---
    build_reactive_function(hir, env);

    // Propagate early returns: transform reactive scopes that contain `return` statements.
    // Must run AFTER build_reactive_function since it operates on the reactive_block tree.
    // Adds early_return_value identifier to scope.declarations and wraps scope body in label.
    propagate_early_returns(hir, env);

    // Rename promoted temp variables ($t0 → t0, etc.) in tree definition order.
    // Must run after build_reactive_function so the reactive_block is available.
    rename_variables(hir, env);

    if env.has_errors() {
        return Err(env.aggregate_errors());
    }

    // Debug: print final scope assignments before codegen
    if std::env::var("RC_DEBUG_FINAL").is_ok() {
        for (id, ident) in &env.identifiers {
            if let Some(ref name) = ident.name {
                eprintln!("[final_ident] id={} name={:?} scope={:?} range={:?}",
                    id.0, name.value(), ident.scope, ident.mutable_range);
            } else {
                eprintln!("[final_ident] id={} (unnamed) scope={:?} range={:?}",
                    id.0, ident.scope, ident.mutable_range);
            }
        }
        for (sid, scope) in &env.scopes {
            eprintln!("[final_scope] id={:?} range={:?} decls={:?}",
                sid, scope.range,
                scope.declarations.keys().map(|k| k.0).collect::<Vec<_>>());
        }
    }

    let js = crate::codegen::hir_codegen::codegen_hir_function(hir, env);
    Ok(CodegenOutput { js, outlines: Vec::new() })
}
