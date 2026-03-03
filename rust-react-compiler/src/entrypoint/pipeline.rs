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
    dead_code_elimination::dead_code_elimination_with_env,
    prune_maybe_throws::prune_maybe_throws,
    optimize_props_method_calls::optimize_props_method_calls,
    optimize_for_ssr::optimize_for_ssr,
    outline_functions::outline_functions,
    outline_jsx::outline_jsx,
};
use crate::ssa::{
    enter_ssa::{enter_ssa_with_env},
    eliminate_redundant_phi::eliminate_redundant_phi,
    rewrite_instruction_kinds::rewrite_instruction_kinds_based_on_reassignment,
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
    build_reactive_scope_terminals_hir::run as build_reactive_scope_terminals_hir,
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
    propagate_early_returns::run as propagate_early_returns,
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
pub fn compile(source: &str, options: CompileOptions) -> Result<CodegenOutput> {
    let source_type = options.source_type;
    // Collect the spans of all compilable top-level functions.
    let fn_spans = collect_compilable_fn_spans(source, source_type);

    if fn_spans.is_empty() {
        // No compilable functions — passthrough the whole file.
        let js = emit_passthrough(source, source_type);
        return Ok(CodegenOutput { js });
    }

    // Compile each function. Track whether any produced cache slots.
    let mut compiled_fns: Vec<(u32, u32, String)> = Vec::new(); // (start, end, compiled_js)
    let mut any_scoped = false;

    // Whether the file uses panic_threshold:none (passthrough on any error).
    let first_line = source.lines().next().unwrap_or("");
    let panic_threshold_none = first_line.contains("@panicThreshold:\"none\"")
        || first_line.contains("@panicThreshold:'none'");

    for (i, &(start, end)) in fn_spans.iter().enumerate() {
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
    Ok(CodegenOutput { js: reconstructed })
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

    let mut spans: Vec<(u32, u32)> = Vec::new();

    for stmt in &parse.program.body {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                // Skip 'use no memo' / 'use no forget' functions.
                let no_memo = func.body.as_ref()
                    .map(|b| b.directives.iter().any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget")))
                    .unwrap_or(false);
                if !no_memo {
                    spans.push((func.span.start, func.span.end));
                }
            }
            Statement::ExportDefaultDeclaration(decl) => {
                match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                        let no_memo = func.body.as_ref()
                            .map(|b| b.directives.iter().any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget")))
                            .unwrap_or(false);
                        if !no_memo {
                            spans.push((decl.span.start, decl.span.end));
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(_) => {
                        spans.push((decl.span.start, decl.span.end));
                    }
                    ExportDefaultDeclarationKind::FunctionExpression(func) => {
                        let no_memo = func.body.as_ref()
                            .map(|b| b.directives.iter().any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget")))
                            .unwrap_or(false);
                        if !no_memo {
                            spans.push((decl.span.start, decl.span.end));
                        }
                    }
                    _ => {}
                }
            }
            Statement::ExportNamedDeclaration(decl) => {
                match decl.declaration.as_ref() {
                    Some(Declaration::FunctionDeclaration(func)) => {
                        let no_memo = func.body.as_ref()
                            .map(|b| b.directives.iter().any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget")))
                            .unwrap_or(false);
                        if !no_memo {
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
    let import_line = "import { c as _c } from \"react/compiler-runtime\";";
    let mut has_runtime_import = false;
    let mut fn_bodies: Vec<(u32, u32, String)> = Vec::new();

    for &(start, end, ref js) in compiled_fns {
        let body = if let Some(rest) = js.strip_prefix(import_line) {
            has_runtime_import = true;
            rest.trim_start_matches('\n').to_string()
        } else {
            js.trim_start_matches('\n').to_string()
        };
        fn_bodies.push((start, end, body));
    }

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

    output
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
    infer_reactive_places(hir);
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
    build_reactive_scope_terminals_hir(hir);
    flatten_reactive_loops_hir(hir, env);
    flatten_scopes_with_hooks_or_use_hir(hir, env);
    prune_non_escaping_scopes(hir, env);
    propagate_scope_dependencies_hir(hir, env);
    merge_reactive_scopes_that_invalidate_together(hir, env);
    prune_non_reactive_dependencies(hir, env);
    prune_always_invalidating_scopes(hir);
    propagate_early_returns(hir);
    prune_unused_lvalues(hir);
    promote_used_temporaries(hir, env);
    extract_scope_declarations_from_destructuring(hir);
    stabilize_block_ids(hir);
    rename_variables(hir, env);
    prune_hoisted_contexts(hir);
    prune_unused_scopes(hir, env);
    prune_unused_labels(hir);

    // --- Phase: Reactive function construction ---
    build_reactive_function(hir);

    if env.has_errors() {
        return Err(env.aggregate_errors());
    }

    // --- Codegen ---
    let js = crate::codegen::hir_codegen::codegen_hir_function(hir, env);
    Ok(CodegenOutput { js })
}
