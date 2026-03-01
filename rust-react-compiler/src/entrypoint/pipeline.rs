#![allow(unused_imports, unused_variables, dead_code)]
/// Main compilation pipeline — mirrors Pipeline.ts
///
/// Each pass is a sequential function call. Passes that can fail return Result;
/// validation passes use env.try_record() to accumulate non-fatal errors.
use oxc_span::SourceType;

use crate::error::{Result};
use crate::hir::environment::{Environment, EnvironmentConfig, OutputMode};
use crate::hir::hir::{HIRFunction, ReactFunctionType};
use crate::hir::build_hir::lower_program;
use crate::hir::print_hir::print_hir_function;

use crate::optimization::{
    constant_propagation::constant_propagation,
    dead_code_elimination::dead_code_elimination,
    prune_maybe_throws::prune_maybe_throws,
    optimize_props_method_calls::optimize_props_method_calls,
    optimize_for_ssr::optimize_for_ssr,
    outline_functions::outline_functions,
    outline_jsx::outline_jsx,
};
use crate::ssa::{
    enter_ssa::enter_ssa,
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
    align_reactive_scopes_to_block_scopes_hir::run as align_reactive_scopes_to_block_scopes_hir,
    merge_overlapping_reactive_scopes_hir::run as merge_overlapping_reactive_scopes_hir,
    build_reactive_scope_terminals_hir::run as build_reactive_scope_terminals_hir,
    flatten_reactive_loops_hir::run as flatten_reactive_loops_hir,
    flatten_scopes_with_hooks_or_use_hir::run as flatten_scopes_with_hooks_or_use_hir,
    propagate_scope_dependencies_hir::run as propagate_scope_dependencies_hir,
    build_reactive_function::run as build_reactive_function,
    prune_unused_labels::run as prune_unused_labels,
    prune_non_escaping_scopes::run as prune_non_escaping_scopes,
    prune_non_reactive_dependencies::run as prune_non_reactive_dependencies,
    prune_unused_scopes::run as prune_unused_scopes,
    merge_reactive_scopes_that_invalidate_together::run as merge_reactive_scopes_that_invalidate_together,
    prune_always_invalidating_scopes::run as prune_always_invalidating_scopes,
    propagate_early_returns::run as propagate_early_returns,
    prune_unused_lvalues::run as prune_unused_lvalues,
    promote_used_temporaries::run as promote_used_temporaries,
    extract_scope_declarations_from_destructuring::run as extract_scope_declarations_from_destructuring,
    stabilize_block_ids::run as stabilize_block_ids,
    rename_variables::run as rename_variables,
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

/// Compile a JS/TS source string containing a React component/hook.
/// Returns the JS code output.
pub fn compile(source: &str, options: CompileOptions) -> Result<CodegenOutput> {
    let mut env = Environment::new(options.fn_type, options.config, options.filename);
    let source_type = options.source_type;
    let mut hir = lower_program(source, source_type, &mut env)?;
    let result = run_with_environment(&mut hir, &mut env);

    // If codegen is a stub (returns stub output), fall back to oxc passthrough.
    match result {
        Ok(ref out) if !out.js.starts_with("// react-compiler (Phase 1 stub)") => result,
        Ok(_) | Err(_) => {
            // Run oxc_codegen to re-emit the source as clean JS (strips TS types).
            let passthrough = emit_passthrough(source, source_type);
            match result {
                Err(e) => Err(e),
                Ok(_) => Ok(CodegenOutput { js: passthrough }),
            }
        }
    }
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
    enter_ssa(hir);
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

    // --- Phase: DCE + cleanup ---
    dead_code_elimination(hir);
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

    // --- Phase: Reactivity ---
    infer_reactive_places(hir);
    rewrite_instruction_kinds_based_on_reassignment(hir);

    if env.enable_memoization() {
        infer_reactive_scope_variables(hir, env);
    }

    memoize_fbt_and_macro_operands(hir);

    if env.config.enable_jsx_outlining {
        outline_jsx(hir);
    }
    if env.config.enable_name_anonymous_functions {
        name_anonymous_functions(hir);
    }
    if env.config.enable_function_outlining {
        outline_functions(hir);
    }

    // --- Phase: Scope alignment ---
    align_method_call_scopes(hir);
    align_object_method_scopes(hir);
    prune_unused_labels_hir(hir);
    align_reactive_scopes_to_block_scopes_hir(hir);
    merge_overlapping_reactive_scopes_hir(hir);
    build_reactive_scope_terminals_hir(hir);
    flatten_reactive_loops_hir(hir);
    flatten_scopes_with_hooks_or_use_hir(hir);
    propagate_scope_dependencies_hir(hir);

    // --- Phase: Reactive function construction ---
    build_reactive_function(hir);

    // NOTE: build_reactive_function above is a stub returning unit.
    // Once it returns a ReactiveFunction, the rest of the passes will operate on it.
    // For Phase 1, we use the HIR directly for a stub codegen.

    // Reactive passes (stubs)
    // prune_unused_labels, prune_non_escaping_scopes, ...

    // --- Codegen stub ---
    // TODO: build actual ReactiveFunction and codegen
    // For Phase 1, return placeholder output
    if env.has_errors() {
        return Err(env.aggregate_errors());
    }

    Ok(CodegenOutput {
        js: format!(
            "// react-compiler (Phase 1 stub)\n// HIR printed below:\n/*\n{}\n*/",
            print_hir_function(hir, env)
        ),
    })
}
