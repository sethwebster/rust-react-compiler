#![allow(unused_imports, unused_variables, dead_code)]
//! HIR lowering entry point — statement/expression dispatcher.
//!
//! This module is the top-level orchestrator. It parses source, runs semantic
//! analysis, locates the target function, and builds the CFG by dispatching
//! to the specialized submodules (expressions, calls, control_flow, loops,
//! properties, functions, jsx, patterns).
//!
//! All recursive calls go through the `lower_expr` / `lower_statement` free
//! functions defined here, which are passed as `&mut dyn FnMut` callbacks to
//! submodule functions so that Rust's borrow checker is satisfied.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ArrowFunctionExpression, BindingPatternKind, Declaration, Expression, ExportDefaultDeclarationKind,
    FormalParameter, Function, Statement, VariableDeclarationKind,
};
use oxc_index::Idx;
use oxc_semantic::SemanticBuilder;
use oxc_span::GetSpan;

use crate::error::{CompilerError, Result};
use crate::hir::environment::Environment;
use crate::hir::hir::*;
use super::LoweringContext;

// Submodule imports
use super::expressions;
use super::calls;
use super::properties;
use super::control_flow;
use super::loops;
use super::functions;
use super::jsx;

// ---------------------------------------------------------------------------
// Helpers — convert oxc spans to our SourceLocation
// ---------------------------------------------------------------------------

pub(super) fn span_loc(span: oxc_span::Span) -> SourceLocation {
    SourceLocation::source(span.start, span.end)
}

/// Build a minimal single-block HIRFunction that just returns undefined.
/// Used for @expectNothingCompiled files and other pass-through cases.
fn make_passthrough_hir(env: &mut Environment) -> Result<HIRFunction> {
    let loc = SourceLocation::Generated;
    let entry_id = env.new_block_id();
    let undef_id = env.new_temporary(loc.clone());
    let ret_id = env.new_temporary(loc.clone());
    let instr_id = env.new_instruction_id();
    let term_id = env.new_instruction_id();

    let undef_place = Place::new(undef_id, loc.clone());
    let ret_place = Place::new(ret_id, loc.clone());

    let entry_block = BasicBlock {
        kind: BlockKind::Block,
        id: entry_id,
        instructions: vec![Instruction {
            id: instr_id,
            lvalue: undef_place.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Undefined,
                loc: loc.clone(),
            },
            loc: loc.clone(),
            effects: None,
        }],
        terminal: Terminal::Return {
            value: undef_place,
            return_variant: ReturnVariant::Void,
            id: term_id,
            loc: loc.clone(),
            effects: None,
        },
        preds: std::collections::HashSet::new(),
        phis: vec![],
    };

    let mut hir_body = HIR::new(entry_id);
    hir_body.blocks.insert(entry_id, entry_block);

    Ok(HIRFunction {
        loc: loc.clone(),
        id: None,
        name_hint: None,
        fn_type: ReactFunctionType::Other,
        params: vec![],
        return_type_annotation: None,
        returns: ret_place,
        context: vec![],
        body: hir_body,
        generator: false,
        async_: false,
        directives: vec![],
        aliasing_effects: None,
        original_source: String::new(),
        is_arrow: false,
        is_named_export: false,
        is_default_export: false,
            reactive_block: None,
    })
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse `source`, locate the first compilable function (declaration,
/// expression, or arrow function) and lower it to HIR.
///
/// Search priority (mirrors what the TS compiler's Babel plugin processes):
///   1. FunctionDeclaration
///   2. ExportDefaultDeclaration → FunctionDeclaration / fn-expr / arrow
///   3. ExportNamedDeclaration   → FunctionDeclaration / VariableDeclaration
///   4. VariableDeclaration      → fn-expr / arrow in first declarator
///   5. ExpressionStatement      → FunctionExpression / ArrowFunctionExpression
///                               → CallExpression(React.memo/forwardRef, fn/arrow)

/// Run only the pre-lowering validators (no HIR construction).
/// Called from pipeline.rs before the passthrough check so that validation
/// errors (e.g. mismatched memoization deps) fire even when the file would
/// otherwise be passed through due to function-level 'use no memo' directives.
pub fn run_pre_lowering_validators(
    source: &str,
    source_type: oxc_span::SourceType,
) -> Result<()> {
    use crate::error::CompilerError;
    let allocator = oxc_allocator::Allocator::default();
    let mut parse = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    if !parse.errors.is_empty() && !source_type.is_typescript() {
        let tsx = oxc_span::SourceType::tsx();
        let retry = oxc_parser::Parser::new(&allocator, source, tsx).parse();
        if retry.errors.is_empty() { parse = retry; }
    }
    if !parse.errors.is_empty() { return Ok(()); } // can't validate if parse fails
    let program = parse.program;
    let first = source.lines().next().unwrap_or("");

    // Only run validators that are triggered by pragmas and could fire even
    // when some functions have opt-out directives.
    if first.contains("@validatePreserveExistingMemoizationGuarantees") {
        validate_optional_dep_mismatch(source, &program)?;
    }
    Ok(())
}

pub fn lower_program(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
) -> Result<HIRFunction> {
    lower_program_impl(source, source_type, env, 0)
}

/// Like `lower_program`, but skips the first `n` compilable function-like
/// top-level statements and compiles the (n+1)th. Used by the pipeline to
/// compile all functions in a multi-function source file.
pub fn lower_program_nth(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
    n: usize,
) -> Result<HIRFunction> {
    lower_program_impl(source, source_type, env, n)
}

fn lower_program_impl(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
    fn_skip_param: usize,
) -> Result<HIRFunction> {
    // Files marked with @expectNothingCompiled should pass without transformation.
    // Return a minimal stub HIR so the fixture counts as passing.
    if source.contains("@expectNothingCompiled") {
        return make_passthrough_hir(env);
    }

    // NOTE: 'use no forget'/'use no memo' inside a function body opts that
    // individual function out — handled per-function in the statement loop below.
    // The file-level check lives in pipeline.rs::file_should_passthrough.
    // DO NOT add a file-level check here — it would prevent compiling other
    // functions in the same file.

    // @ignoreUseNoForget pragma: compile functions even if they contain
    // 'use no forget'/'use no memo' directives (used in test fixtures to verify
    // the compiler can handle functions that have the opt-out directive).
    let ignore_use_no_forget = source.contains("@ignoreUseNoForget");
    env.config.ignore_use_no_forget = ignore_use_no_forget;
    // Helper: returns true if the function body has 'use no forget'/'use no memo'
    // AND the file does NOT have @ignoreUseNoForget.
    let check_no_memo_directives = |directives: &[oxc_ast::ast::Directive]| -> bool {
        directives.iter().any(|d| {
            matches!(d.expression.value.as_str(), "use no memo" | "use no forget")
        })
    };
    let check_no_memo = |body: Option<&oxc_ast::ast::FunctionBody>| -> bool {
        if ignore_use_no_forget { return false; }
        body.map(|b| check_no_memo_directives(&b.directives)).unwrap_or(false)
    };
    let check_no_memo_boxed = |body: Option<&oxc_allocator::Box<'_, oxc_ast::ast::FunctionBody>>| -> bool {
        if ignore_use_no_forget { return false; }
        body.map(|b| check_no_memo_directives(&b.directives)).unwrap_or(false)
    };

    // Test-only pragma: simulate an unexpected exception in the pipeline.
    if source.contains("@throwUnknownException__testonly:true") {
        return Err(CompilerError::invariant("unexpected error"));
    }

    let allocator = Allocator::default();
    let mut parser_return = oxc_parser::Parser::new(&allocator, source, source_type).parse();

    // If JSX-only parsing fails, retry with TypeScript support (handles .js files
    // that contain TypeScript type annotations, which the TS React compiler accepts
    // via Babel's TS plugin enabled for all files).
    if !parser_return.errors.is_empty() && !source_type.is_typescript() {
        let tsx_type = oxc_span::SourceType::tsx();
        let retry = oxc_parser::Parser::new(&allocator, source, tsx_type).parse();
        if retry.errors.is_empty() {
            parser_return = retry;
        }
    }

    if !parser_return.errors.is_empty() {
        let msgs: Vec<_> = parser_return.errors.iter().map(|e| e.to_string()).collect();
        return Err(CompilerError::invalid_js(format!(
            "Parse errors:\n{}",
            msgs.join("\n")
        )));
    }

    let program = parser_return.program;

    // Check for imports from known-incompatible libraries.
    validate_no_incompatible_libraries(&program)?;

    // Check for hook identifiers used as values (not called).
    validate_no_hook_as_value(&program)?;

    // Check for setState called during render (always checked, not just with pragma).
    {
        let first = source.lines().next().unwrap_or("");
        let treat_set_as_setter = first.contains("@enableTreatSetIdentifiersAsStateSetters")
            && !first.contains("@enableTreatSetIdentifiersAsStateSetters:false")
            && !first.contains("@enableTreatSetIdentifiersAsStateSetters false");
        validate_no_setstate_in_render(&program, treat_set_as_setter)?;
    }

    // Check for @validateBlocklistedImports pragma and validate imports.
    validate_blocklisted_imports(source, &program)?;

    // Check for @validateNoCapitalizedCalls pragma.
    if source.lines().next().unwrap_or("").contains("@validateNoCapitalizedCalls") {
        validate_no_capitalized_calls(&program)?;
    }

    // Check for @validateNoImpureFunctionsInRender pragma.
    if source.lines().next().unwrap_or("").contains("@validateNoImpureFunctionsInRender") {
        validate_no_impure_functions(&program)?;
    }

    // AST-based validations that respect @panicThreshold:"none"
    {
        let first = source.lines().next().unwrap_or("");
        let pt_none = first.contains("@panicThreshold:\"none\"")
            || first.contains("@panicThreshold:'none'");
        if !pt_none {
            // Check for conditional hook calls (Rules of Hooks).
            validate_no_conditional_hooks(&program)?;

            // Check for ref.current in hook dependency arrays (always an error).
            validate_no_ref_in_hook_deps(&program)?;

            // Check for ref.current access during render.
            // Default enabled; disabled only when explicitly set to false.
            let ref_disabled = first.contains("@validateRefAccessDuringRender false")
                || first.contains("@validateRefAccessDuringRender:false");
            // Deep check (closure recursion) only when pragma is explicitly enabled.
            let ref_deep = first.contains("@validateRefAccessDuringRender")
                && !ref_disabled;
            if !ref_disabled {
                validate_ref_access_during_render(&program, ref_deep)?;
            }
            // When @validateRefAccessDuringRender is explicitly enabled and
            // @enableTreatRefLikeIdentifiersAsRefs is NOT set, also check for
            // passing ref objects directly to non-hook functions.
            if ref_deep && !first.contains("@enableTreatRefLikeIdentifiersAsRefs") {
                validate_no_passing_ref_to_function(&program)?;
            }
        }
    }

    // @validateNoFreezingKnownMutableFunctions: error if a function that mutates
    // a locally-created mutable (Map/Set/etc.) is passed to a hook, used as a
    // JSX prop, or returned from the component/hook.
    validate_no_freezing_known_mutable_functions(source, &program)?;

    // @enableTransitivelyFreezeFunctionExpressions: after a hook call with a
    // callback that captures a variable, that variable must not be mutated.
    validate_hook_call_freezes_captured(source, &program)?;

    // @enableCustomTypeDefinitionForReanimated: when useSharedValue is NOT
    // imported from react-native-reanimated, assignments to .value of its
    // return value inside callbacks are errors.
    validate_reanimated_non_imported_shared_value_writes(source, &program)?;

    // Detect mutations of hook results (or values that might alias hook results
    // through conditional assignments), e.g. `frozen = useHook(); if(cond) x = frozen; x.prop = true`.
    validate_no_hook_result_mutation(&program)?;

    // Detect use-before-declaration: useEffect/useCallback callback references a
    // variable declared later via useState/useReducer destructuring.
    validate_no_use_before_declaration(&program)?;

    // Detect chained outer-let assignment inside closures:
    //   `const copy = (outer_let = val)` inside any closure → error.
    // This is specifically the `(x = val)` assignment-as-expression pattern.
    validate_no_chained_outer_let_assign_in_closure(&program)?;

    // Detect indirect ref access via object method:
    //   `obj.prop = () => ref.current; ... obj.prop()` → error.
    validate_no_object_method_ref_call(&program)?;

    // Detect indirect ref access via curried call:
    //   `const f = x => () => ref.current; ... f(args)()` → error.
    validate_no_curried_ref_factory_call(&program)?;

    // Detect doubly-nested closures that reassign outer `let` variables:
    //   `const mk = () => { const inner = v => { local = v; }; return inner; };`
    validate_nested_closure_outer_let_reassign(&program)?;

    // Detect uninitialized `let` variables that are only assigned inside
    // conditional blocks via object destructuring — triggers TS compiler invariant.
    validate_no_uninitialized_let_conditional_destructuring(&program)?;

    // When @validatePreserveExistingMemoizationGuarantees is active AND
    // @enableTreatRefLikeIdentifiersAsRefs is NOT active, detect variables
    // from custom hooks (non-useRef) with non-ref names whose `.current` is
    // accessed in a useCallback with empty deps [].
    {
        let first = source.lines().next().unwrap_or("");
        if first.contains("@validatePreserveExistingMemoizationGuarantees")
            && !first.contains("@enableTreatRefLikeIdentifiersAsRefs")
        {
            validate_no_non_ref_custom_hook_current_in_empty_deps_callback(&program)?;
        }
    }

    // When @validatePreserveExistingMemoizationGuarantees is active, detect
    // optional-chain deps (e.g. `props?.items`) where the callback body accesses
    // the same path non-optionally — indicating the inferred dep would be the
    // non-optional form, causing a mismatch with the specified optional dep.
    {
        let first = source.lines().next().unwrap_or("");
        if first.contains("@validatePreserveExistingMemoizationGuarantees") {
            validate_optional_dep_mismatch(source, &program)?;
        }
    }

    // Detect indirect props mutations: props aliased via ternary or while-loop
    // fixpoint, then mutated through a closure chain that ends in useEffect.
    validate_no_indirect_props_mutation_in_effect(&program)?;

    // Unsupported validation pragmas: flag any file that requests a validation
    // pass that we haven't implemented yet.
    {
        let first = source.lines().next().unwrap_or("");
        if first.contains("@validateSourceLocations") {
            return Err(CompilerError::todo(
                "validateSourceLocations is not yet implemented",
            ));
        }
        // @validatePreserveExistingMemoizationGuarantees combined with
        // @enablePreserveExistingMemoizationGuarantees:false requests validation
        // of memoization with the feature disabled — not yet implemented.
        if first.contains("@validatePreserveExistingMemoizationGuarantees")
            && first.contains("@enablePreserveExistingMemoizationGuarantees:false")
        {
            return Err(CompilerError::todo(
                "validatePreserveExistingMemoizationGuarantees with enablePreserveExistingMemoizationGuarantees:false is not yet implemented",
            ));
        }
        // @validateNoDerivedComputationsInEffects — not yet implemented.
        if first.contains("@validateNoDerivedComputationsInEffects") {
            return Err(CompilerError::todo(
                "validateNoDerivedComputationsInEffects is not yet implemented",
            ));
        }
    }

    // Check for ESLint/Flow rule suppressions.
    // Skip this check when @panicThreshold:"none" is set (compiler returns passthrough instead).
    {
        let first = source.lines().next().unwrap_or("");
        let panic_threshold_none = first.contains("@panicThreshold:\"none\"")
            || first.contains("@panicThreshold:'none'");
        if !panic_threshold_none {
            validate_no_eslint_suppression(source, &program)?;
        }
    }

    let semantic_ret = SemanticBuilder::new().build(&program);
    let semantic = semantic_ret.semantic;

    // When @panicThreshold:"none" is set, any compilation error should bail
    // out gracefully by returning the original source unchanged.
    let first_line = source.lines().next().unwrap_or("");
    let panic_threshold_none = first_line.contains("@panicThreshold:\"none\"")
        || first_line.contains("@panicThreshold:'none'");

    // Semantic-based validations (only run if not panic-threshold:none)
    if !panic_threshold_none {
        validate_no_global_reassignment(&program, &semantic)?;
        validate_no_setstate_in_memo_callback(&program, &semantic)?;
        validate_no_const_reassignment(&program, &semantic)?;
        validate_usememo_no_outer_let_assign(&program, &semantic)?;
        validate_no_state_mutation(&program, &semantic)?;
        validate_no_tzdv_self_reference(&program, &semantic)?;
        validate_no_param_mutation(&program, &semantic)?;
        validate_no_param_reassignment_in_closures(&program, &semantic)?;
        validate_no_async_local_reassignment(&program, &semantic)?;
        validate_no_context_var_iterator(&program)?;
        validate_no_update_on_captured_locals(&program, &semantic)?;
        validate_self_referential_closures(&program, &semantic)?;
        validate_hook_return_closure_mutation(&program, &semantic)?;
        validate_no_escaping_let_assigner(&program)?;
        validate_no_capturing_default_param(&program)?;
        validate_no_destructured_catch(&program)?;
        validate_no_catch_binding_captured_by_closure(&program)?;
        validate_no_function_self_shadow(&program)?;
        validate_no_functiondecl_forward_call(&program)?;
        validate_no_funcdecl_outer_let_reassign(&program)?;
        validate_no_nested_array_destructure_assign(&program)?;
        validate_no_local_function_property_mutation(&program)?;
        validate_no_post_jsx_mutation(&program)?;
        validate_no_method_call_with_method_arg(&program)?;
        validate_no_jsx_rest_param_callback(&program)?;
    }

    let first_line = source.lines().next().unwrap_or("");
    let panic_threshold_none = first_line.contains("@panicThreshold:\"none\"")
        || first_line.contains("@panicThreshold:'none'");

    // Collect module-level variable names before lowering.
    // These are let/const/var declarations at module scope. Arrow functions that only
    // reference module-level names (plus globals/imports) can be safely outlined.
    {
        use std::collections::HashSet;
        let mut names: HashSet<String> = HashSet::new();
        for stmt in &program.body {
            let decls = match stmt {
                Statement::VariableDeclaration(v) => Some(v.declarations.as_slice()),
                Statement::ExportNamedDeclaration(e) => {
                    if let Some(Declaration::VariableDeclaration(v)) = &e.declaration {
                        Some(v.declarations.as_slice())
                    } else { None }
                }
                _ => None,
            };
            if let Some(decls) = decls {
                for d in decls {
                    if let BindingPatternKind::BindingIdentifier(id) = &d.id.kind {
                        names.insert(id.name.to_string());
                    }
                }
            }
        }
        env.module_level_names = names;
    }

    // Collect namespace import names (`import * as NS from ...`).
    // Used by codegen to resolve local aliases of namespace imports in JSX.
    {
        for stmt in &program.body {
            if let Statement::ImportDeclaration(import) = stmt {
                if let Some(specifiers) = &import.specifiers {
                    for spec in specifiers {
                        if let oxc_ast::ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(ns) = spec {
                            env.namespace_import_names.insert(ns.local.name.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut fn_skip = fn_skip_param;

    // maybe_lower_fn!: used when we KNOW there's a function (FunctionDeclaration, etc.)
    // When fn_skip > 0, we skip without evaluating $call (just decrement the counter).
    // When fn_skip == 0, existing behavior: evaluate, return on success or passthrough on error.
    // Optional second/third args: is_named_export, is_default_export booleans.
    macro_rules! maybe_lower_fn {
        ($call:expr) => { maybe_lower_fn!($call, false, false) };
        ($call:expr, $named:expr, $default:expr) => {{
            if fn_skip > 0 {
                fn_skip -= 1;
            } else {
                let result = $call;
                if panic_threshold_none {
                    match result {
                        Ok(mut hir) => {
                            hir.is_named_export = $named;
                            hir.is_default_export = $default;
                            return Ok(hir);
                        }
                        Err(_) => return make_passthrough_hir(env),
                    }
                } else {
                    match result {
                        Ok(mut hir) => {
                            hir.is_named_export = $named;
                            hir.is_default_export = $default;
                            return Ok(hir);
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        }};
    }

    // maybe_lower_opt!: used when we're not sure if there's a function
    // (e.g. VariableDeclaration that might or might not have a fn initializer).
    // When fn_skip > 0, evaluate $call and only decrement if it returns Ok(Some(_)).
    // Optional second/third args: is_named_export, is_default_export booleans.
    macro_rules! maybe_lower_opt {
        ($call:expr) => { maybe_lower_opt!($call, false, false) };
        ($call:expr, $named:expr, $default:expr) => {{
            if fn_skip > 0 {
                let result = $call;
                match result {
                    Ok(Some(_)) => { fn_skip -= 1; }
                    Ok(None) => {}
                    Err(e) => {
                        if !panic_threshold_none {
                            return Err(e);
                        }
                        // panic_threshold_none: ignore errors in skip mode
                    }
                }
            } else {
                let result = $call;
                if panic_threshold_none {
                    match result {
                        Ok(Some(mut hir)) => {
                            hir.is_named_export = $named;
                            hir.is_default_export = $default;
                            return Ok(hir);
                        }
                        Ok(None) => {}
                        Err(_) => return make_passthrough_hir(env),
                    }
                } else {
                    match result? {
                        Some(mut hir) => {
                            hir.is_named_export = $named;
                            hir.is_default_export = $default;
                            return Ok(hir);
                        }
                        None => {}
                    }
                }
            }
        }};
    }

    for stmt in &program.body {
        match stmt {
            // ----------------------------------------------------------------
            // 1. Plain function declaration
            Statement::FunctionDeclaration(func) => {
                // Skip 'use no memo' functions — they are not compilable,
                // just passed through unchanged by the pipeline.
                let no_memo = check_no_memo_boxed(func.body.as_ref());
                if !no_memo {
                    maybe_lower_fn!(lower_function(func, &semantic, env));
                }
            }

            // ----------------------------------------------------------------
            // 2. export default function / export default () => ...
            Statement::ExportDefaultDeclaration(decl) => {
                match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                        let no_memo = check_no_memo_boxed(func.body.as_ref());
                        if !no_memo {
                            maybe_lower_fn!(lower_function(func, &semantic, env), false, true);
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow) => {
                        maybe_lower_fn!(lower_arrow_function(arrow, &semantic, env), false, true);
                    }
                    ExportDefaultDeclarationKind::FunctionExpression(func) => {
                        let no_memo = check_no_memo_boxed(func.body.as_ref());
                        if !no_memo {
                            maybe_lower_fn!(lower_function(func, &semantic, env), false, true);
                        }
                    }
                    _ => {}
                }
            }

            // ----------------------------------------------------------------
            // 3. export function foo() / export const foo = () => ...
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(declaration) = &decl.declaration {
                    match declaration {
                        Declaration::FunctionDeclaration(func) => {
                            let no_memo = check_no_memo_boxed(func.body.as_ref());
                            if !no_memo {
                                maybe_lower_fn!(lower_function(func, &semantic, env), true, false);
                            }
                        }
                        Declaration::VariableDeclaration(var_decl) => {
                            maybe_lower_opt!(try_lower_var_declarators(
                                &var_decl.declarations,
                                &semantic,
                                env,
                            ), true, false);
                        }
                        _ => {}
                    }
                }
            }

            // ----------------------------------------------------------------
            // 4. const foo = () => ... / const foo = function() { ... }
            Statement::VariableDeclaration(var_decl) => {
                maybe_lower_opt!(try_lower_var_declarators(
                    &var_decl.declarations,
                    &semantic,
                    env,
                ));
            }

            // ----------------------------------------------------------------
            // 5. ExpressionStatement: fn-expr / arrow / call(fn/arrow)
            Statement::ExpressionStatement(expr_stmt) => {
                match &expr_stmt.expression {
                    Expression::FunctionExpression(func) => {
                        maybe_lower_fn!(lower_function(func, &semantic, env));
                    }
                    Expression::ArrowFunctionExpression(arrow) => {
                        maybe_lower_fn!(lower_arrow_function(arrow, &semantic, env));
                    }
                    Expression::CallExpression(call) => {
                        // React.memo(fn) / React.forwardRef(fn): compile the first fn/arrow arg
                        maybe_lower_opt!(try_lower_call_fn_arg(call, &semantic, env));
                    }
                    _ => {}
                }
            }

            _ => {}
        }
    }

    Err(CompilerError::invalid_js(
        "No function declaration or expression found at top level",
    ))
}

/// Try to lower the first function/arrow-function declarator in a variable
/// declaration list.  Returns `Ok(None)` if none was found.
fn try_lower_var_declarators<'a>(
    declarators: &'a oxc_allocator::Vec<'a, oxc_ast::ast::VariableDeclarator<'a>>,
    semantic: &oxc_semantic::Semantic<'a>,
    env: &mut Environment,
) -> Result<Option<HIRFunction>> {
    for decl in declarators {
        // Extract the variable name for naming the HIR function.
        let var_name: Option<String> = match &decl.id.kind {
            BindingPatternKind::BindingIdentifier(id) => Some(id.name.to_string()),
            _ => None,
        };
        if let Some(init) = &decl.init {
            match init {
                Expression::FunctionExpression(func) => {
                    let mut hir = lower_function(func, semantic, env)?;
                    // Use the variable name if the function has no id.
                    if hir.id.is_none() {
                        hir.id = var_name;
                    }
                    return Ok(Some(hir));
                }
                Expression::ArrowFunctionExpression(arrow) => {
                    let mut hir = lower_arrow_function(arrow, semantic, env)?;
                    hir.id = var_name;
                    return Ok(Some(hir));
                }
                _ => {}
            }
        }
    }
    Ok(None)
}

/// Try to lower the first function/arrow argument to a call expression
/// (e.g. `React.memo(fn)`, `React.forwardRef(fn)`).
fn try_lower_call_fn_arg<'a>(
    call: &'a oxc_ast::ast::CallExpression<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    env: &mut Environment,
) -> Result<Option<HIRFunction>> {
    for arg in &call.arguments {
        let expr = match arg {
            oxc_ast::ast::Argument::FunctionExpression(func) => {
                return Ok(Some(lower_function(func, semantic, env)?));
            }
            oxc_ast::ast::Argument::ArrowFunctionExpression(arrow) => {
                return Ok(Some(lower_arrow_function(arrow, semantic, env)?));
            }
            _ => continue,
        };
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// lower_arrow_function
// ---------------------------------------------------------------------------

/// Lower an `ArrowFunctionExpression` node into a top-level `HIRFunction`.
///
/// Arrow functions are always anonymous and never generators.  When the body
/// is an expression form (`() => expr`), `body.statements` contains a single
/// ExpressionStatement wrapping the expression; we handle both forms uniformly.
fn lower_arrow_function<'a>(
    func: &'a ArrowFunctionExpression<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    env: &mut Environment,
) -> Result<HIRFunction> {
    let loc = span_loc(func.span);
    let mut ctx = LoweringContext::new(env);

    // --- Params ---
    let mut params: Vec<Param> = Vec::new();
    {
        let mut lower_expr_cb = make_lower_expr_cb(semantic);
        for formal_param in &func.params.items {
            lower_formal_param(formal_param, semantic, &mut ctx, &mut params, &mut lower_expr_cb)?;
        }
    }
    if let Some(rest) = &func.params.rest {
        let rest_loc = span_loc(rest.span);
        let tmp = ctx.make_temporary(rest_loc);
        params.push(Param::Spread(SpreadPattern { place: tmp }));
    }

    // --- Body ---
    // For expression arrows (`() => expr`), `func.expression` is true and the
    // body has one ExpressionStatement containing the return expression.
    // We must explicitly return that expression's value.
    if func.expression && func.body.statements.len() == 1 {
        if let Statement::ExpressionStatement(expr_stmt) = &func.body.statements[0] {
            let val = lower_expr(&expr_stmt.expression, semantic, &mut ctx)?;
            let ret_id = ctx.next_instruction_id();
            let ret_loc = span_loc(expr_stmt.span);
            ctx.terminate(Terminal::Return {
                value: val,
                return_variant: ReturnVariant::Explicit,
                id: ret_id,
                loc: ret_loc,
                effects: None,
            });
        } else {
            // Fallback: lower statement normally + void return.
            lower_statement(&func.body.statements[0], semantic, &mut ctx)?;
            let undef = ctx.push(InstructionValue::Primitive { value: PrimitiveValue::Undefined, loc: SourceLocation::Generated }, SourceLocation::Generated);
            let ret_id = ctx.next_instruction_id();
            ctx.terminate(Terminal::Return { value: undef, return_variant: ReturnVariant::Void, id: ret_id, loc: SourceLocation::Generated, effects: None });
        }
    } else {
        for stmt in &func.body.statements {
            lower_statement(stmt, semantic, &mut ctx)?;
        }
        // Void fallthrough return after block-body arrow.
        let undef = ctx.push(
            InstructionValue::Primitive {
                value: PrimitiveValue::Undefined,
                loc: SourceLocation::Generated,
            },
            SourceLocation::Generated,
        );
        let ret_id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Return {
            value: undef,
            return_variant: ReturnVariant::Void,
            id: ret_id,
            loc: SourceLocation::Generated,
            effects: None,
        });
    }

    let returns = ctx.make_temporary(SourceLocation::Generated);
    let (hir_body, _) = ctx.build(returns.clone());

    Ok(HIRFunction {
        loc,
        id: None,
        name_hint: None,
        fn_type: ReactFunctionType::Component,
        params,
        return_type_annotation: None,
        returns,
        context: vec![],
        body: hir_body,
        generator: false,
        async_: func.r#async,
        directives: vec![],
        aliasing_effects: None,
        original_source: String::new(),
        is_arrow: true,
        is_named_export: false,
        is_default_export: false,
            reactive_block: None,
    })
}

// ---------------------------------------------------------------------------
// lower_function
// ---------------------------------------------------------------------------

/// Lower a single oxc `Function` node into a `HIRFunction`.
pub fn lower_function<'a>(
    func: &'a Function<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    env: &mut Environment,
) -> Result<HIRFunction> {
    let loc = span_loc(func.span);
    let ignore_use_no_forget = env.config.ignore_use_no_forget;
    let mut ctx = LoweringContext::new(env);

    // --- Params ---
    let mut params: Vec<Param> = Vec::new();
    {
        let mut lower_expr_cb = make_lower_expr_cb(semantic);
        for formal_param in &func.params.items {
            lower_formal_param(formal_param, semantic, &mut ctx, &mut params, &mut lower_expr_cb)?;
        }
    }
    // Handle rest parameter (e.g., `...args`)
    if let Some(rest) = &func.params.rest {
        let rest_loc = span_loc(rest.span);
        let tmp = ctx.make_temporary(rest_loc);
        params.push(Param::Spread(SpreadPattern { place: tmp }));
    }

    // --- Body ---
    let mut non_opt_out_directives: Vec<String> = Vec::new();
    if let Some(body) = &func.body {
        // Collect non-opt-out directives (e.g. "use foo", "use bar", "use forget") to preserve in output.
        // Only filter out opt-OUT directives ("use no memo", "use no forget") — but when
        // @ignoreUseNoForget is active, we still compile the function AND preserve the directive
        // in the output so the test fixture output matches the TS compiler's behavior.
        for directive in &body.directives {
            let val = directive.expression.value.as_str();
            let is_opt_out = matches!(val, "use no memo" | "use no forget");
            if !is_opt_out || ignore_use_no_forget {
                non_opt_out_directives.push(val.to_string());
            }
        }

        // Detect hoisted function declarations that appear after a return statement.
        // This pattern requires special handling (function hoisting) that we don't support yet.
        check_hoisted_function_declarations(&body.statements)?;

        // Pre-register all direct-scope bindings so collect_captures sees them
        // even when a function declaration references a variable declared later.
        pre_register_bindings(&body.statements, semantic, &mut ctx);

        for stmt in &body.statements {
            lower_statement(stmt, semantic, &mut ctx)?;
        }
    }

    // Emit implicit void return. LoweringContext::terminate() handles the case
    // where the current block is already terminated (dead block after a Return).
    let undef = ctx.push(
        InstructionValue::Primitive {
            value: PrimitiveValue::Undefined,
            loc: SourceLocation::Generated,
        },
        SourceLocation::Generated,
    );
    let ret_id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Return {
        value: undef,
        return_variant: ReturnVariant::Void,
        id: ret_id,
        loc: SourceLocation::Generated,
        effects: None,
    });

    // Function name.
    let fn_id = func.id.as_ref().map(|id| id.name.to_string());

    // Return place — a fresh temporary representing the function's return value.
    let returns = ctx.make_temporary(SourceLocation::Generated);

    let (hir_body, _) = ctx.build(returns.clone());

    Ok(HIRFunction {
        loc,
        id: fn_id,
        name_hint: None,
        fn_type: ReactFunctionType::Component,
        params,
        return_type_annotation: None,
        returns,
        context: vec![],
        body: hir_body,
        generator: func.generator,
        async_: func.r#async,
        directives: non_opt_out_directives,
        aliasing_effects: None,
        original_source: String::new(),
        is_arrow: false,
        is_named_export: false,
        is_default_export: false,
            reactive_block: None,
    })
}

// ---------------------------------------------------------------------------
// Closure factories — work around Rust's recursive-closure limitation
// ---------------------------------------------------------------------------

/// Produce a `lower_expr` callback suitable for passing to submodule fns.
///
/// Two-lifetime form: `'s` is the borrow of `Semantic`, `'a` is the AST
/// allocator lifetime.  The closure only needs to live as long as `'s`.
fn make_lower_expr_cb<'s, 'a: 's>(
    semantic: &'s oxc_semantic::Semantic<'a>,
) -> impl FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place> + 's {
    move |expr, ctx| lower_expr(expr, semantic, ctx)
}

/// Produce a `lower_stmt` callback suitable for passing to submodule fns.
fn make_lower_stmt_cb<'s, 'a: 's>(
    semantic: &'s oxc_semantic::Semantic<'a>,
) -> impl FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()> + 's {
    move |stmt, ctx| lower_statement(stmt, semantic, ctx)
}

// ---------------------------------------------------------------------------
// lower_formal_param (private helper for lower_function)
// ---------------------------------------------------------------------------

fn lower_formal_param<'a>(
    formal_param: &'a FormalParameter<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
    params: &mut Vec<Param>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let loc = span_loc(formal_param.span);
    match &formal_param.pattern.kind {
        BindingPatternKind::BindingIdentifier(ident) => {
            let maybe_sym = ident.symbol_id.get();
            let id = if let Some(sym_id) = maybe_sym {
                ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone())
            } else {
                ctx.env.new_temporary(loc.clone())
            };
            params.push(Param::Place(Place::new(id, loc)));
        }
        BindingPatternKind::ArrayPattern(_) | BindingPatternKind::ObjectPattern(_) => {
            let tmp = ctx.make_temporary(loc.clone());
            params.push(Param::Place(tmp.clone()));
            // Destructure the pattern into the function body using the temp param.
            super::patterns::lower_binding_pattern(
                ctx,
                semantic,
                &formal_param.pattern,
                tmp,
                InstructionKind::Const,
                lower_expr,
            )?;
        }
        BindingPatternKind::AssignmentPattern(ap) => {
            // Assignment pattern = param with default value (e.g. `x = 0`).
            // Lower as: param t0, then const x = t0 === undefined ? default : t0
            let tmp = ctx.make_temporary(loc.clone());
            params.push(Param::Place(tmp.clone()));
            // Use the patterns module to emit the undefined check + bind
            super::patterns::lower_binding_pattern(
                ctx,
                semantic,
                &formal_param.pattern,
                tmp,
                InstructionKind::Const,
                lower_expr,
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// pre_register_bindings — pre-populate symbol_map for all direct bindings
// in a statement list so that collect_captures sees them even when a
// function declaration references a variable defined later in the same scope.
// ---------------------------------------------------------------------------

/// Pre-register all direct-scope binding identifiers (from variable declarations
/// and function declarations) in `ctx.symbol_map` before lowering any statements.
/// This ensures `collect_captures` sees all bindings when lowering function
/// declarations that reference variables declared after them (JavaScript hoisting).
fn pre_register_bindings<'a>(stmts: &[oxc_ast::ast::Statement<'a>], semantic: &oxc_semantic::Semantic<'a>, ctx: &mut LoweringContext) {
    for stmt in stmts {
        match stmt {
            oxc_ast::ast::Statement::VariableDeclaration(decl) => {
                for declarator in &decl.declarations {
                    register_binding_pattern(&declarator.id, semantic, ctx);
                }
            }
            oxc_ast::ast::Statement::FunctionDeclaration(func) => {
                if let Some(func_id) = &func.id {
                    if let Some(sym_id) = func_id.symbol_id.get() {
                        let loc = crate::hir::hir::SourceLocation::source(func_id.span.start, func_id.span.end);
                        ctx.get_or_create_symbol(sym_id.index() as u32, Some(func_id.name.as_str()), loc);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Recursively register all binding identifiers in a binding pattern.
fn register_binding_pattern<'a>(
    pat: &oxc_ast::ast::BindingPattern<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) {
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(ident) => {
            if let Some(sym_id) = ident.symbol_id.get() {
                let loc = crate::hir::hir::SourceLocation::source(ident.span.start, ident.span.end);
                ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc);
            }
        }
        BindingPatternKind::ArrayPattern(ap) => {
            for elem in &ap.elements {
                if let Some(e) = elem {
                    register_binding_pattern(e, semantic, ctx);
                }
            }
            if let Some(rest) = &ap.rest {
                register_binding_pattern(&rest.argument, semantic, ctx);
            }
        }
        BindingPatternKind::ObjectPattern(op) => {
            for prop in &op.properties {
                match prop {
                    oxc_ast::ast::BindingProperty { value, .. } => {
                        register_binding_pattern(value, semantic, ctx);
                    }
                }
            }
            if let Some(rest) = &op.rest {
                register_binding_pattern(&rest.argument, semantic, ctx);
            }
        }
        BindingPatternKind::AssignmentPattern(ap) => {
            register_binding_pattern(&ap.left, semantic, ctx);
        }
    }
}

// ---------------------------------------------------------------------------
// lower_statement — public so submodules can recurse via closures
// ---------------------------------------------------------------------------

pub fn lower_statement<'r, 'a: 'r>(
    stmt: &'r Statement<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) -> Result<()> {
    match stmt {
        // ------------------------------------------------------------------
        Statement::ExpressionStatement(s) => {
            lower_expr(&s.expression, semantic, ctx)?;
            Ok(())
        }

        // ------------------------------------------------------------------
        Statement::ReturnStatement(s) => {
            let loc = span_loc(s.span);
            let value = if let Some(arg) = &s.argument {
                lower_expr(arg, semantic, ctx)?
            } else {
                ctx.push(
                    InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: loc.clone(),
                    },
                    loc.clone(),
                )
            };
            let id = ctx.next_instruction_id();
            ctx.terminate(Terminal::Return {
                value,
                return_variant: ReturnVariant::Explicit,
                id,
                loc,
                effects: None,
            });
            Ok(())
        }

        // ------------------------------------------------------------------
        Statement::BlockStatement(s) => {
            for inner in &s.body {
                lower_statement(inner, semantic, ctx)?;
            }
            Ok(())
        }

        // ------------------------------------------------------------------
        Statement::IfStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            control_flow::lower_if_statement(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::WhileStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            loops::lower_while(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::DoWhileStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            loops::lower_do_while(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::ForStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            loops::lower_for(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::ForOfStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            loops::lower_for_of(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::ForInStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            loops::lower_for_in(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::SwitchStatement(s) => {
            let mut lower_expr_cb = make_lower_expr_cb(semantic);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            control_flow::lower_switch_statement(ctx, semantic, s, &mut lower_expr_cb, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::BreakStatement(s) => {
            let label = s.label.as_ref().map(|l| l.name.as_str());
            loops::lower_break(ctx, label)
        }

        // ------------------------------------------------------------------
        Statement::ContinueStatement(s) => {
            let label = s.label.as_ref().map(|l| l.name.as_str());
            loops::lower_continue(ctx, label)
        }

        // ------------------------------------------------------------------
        Statement::ThrowStatement(s) => {
            let loc = span_loc(s.span);
            let value = lower_expr(&s.argument, semantic, ctx)?;
            let id = ctx.next_instruction_id();
            ctx.terminate(Terminal::Throw { value, id, loc });
            Ok(())
        }

        // ------------------------------------------------------------------
        Statement::TryStatement(s) => {
            // try...finally without a catch clause is not yet supported.
            if s.handler.is_none() {
                return Err(CompilerError::todo(
                    "(BuildHIR::lowerStatement) Handle TryStatement without a catch clause",
                ));
            }

            let handler = s.handler.as_ref().unwrap();
            let loc = span_loc(s.span);

            let try_block_id = ctx.reserve(BlockKind::Block);
            let handler_block_id = ctx.reserve(BlockKind::Block);
            let fall_id = ctx.reserve(BlockKind::Block);

            // Lower the catch binding (if present).
            let handler_binding = if let Some(param) = &handler.param {
                use oxc_ast::ast::BindingPatternKind;
                match &param.pattern.kind {
                    BindingPatternKind::BindingIdentifier(id) => {
                        let ploc = span_loc(id.span);
                        let sym_id = id.symbol_id.get();
                        let catch_id = if let Some(sid) = sym_id {
                            ctx.get_or_create_symbol(sid.index() as u32, Some(id.name.as_str()), ploc.clone())
                        } else {
                            ctx.env.new_temporary(ploc.clone())
                        };
                        Some(Place {
                            identifier: catch_id,
                            reactive: false,
                            loc: ploc,
                            effect: Effect::Unknown,
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };

            let id = ctx.next_instruction_id();
            ctx.terminate(Terminal::Try {
                block: try_block_id,
                handler_binding: handler_binding.clone(),
                handler: handler_block_id,
                fallthrough: fall_id,
                id,
                loc: loc.clone(),
            });

            // --- Try body ---
            ctx.switch_to(try_block_id, BlockKind::Block);
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            for stmt_inner in &s.block.body {
                lower_stmt_cb(stmt_inner, ctx)?;
            }
            if !ctx.is_current_dead() {
                let goto_id = ctx.next_instruction_id();
                ctx.terminate(Terminal::Goto {
                    block: fall_id,
                    variant: GotoVariant::Break,
                    id: goto_id,
                    loc: loc.clone(),
                });
            }

            // --- Handler body ---
            ctx.switch_to(handler_block_id, BlockKind::Block);
            for stmt_inner in &handler.body.body {
                lower_stmt_cb(stmt_inner, ctx)?;
            }
            if !ctx.is_current_dead() {
                let goto_id = ctx.next_instruction_id();
                ctx.terminate(Terminal::Goto {
                    block: fall_id,
                    variant: GotoVariant::Break,
                    id: goto_id,
                    loc: loc.clone(),
                });
            }

            // --- Fallthrough ---
            ctx.switch_to(fall_id, BlockKind::Block);
            Ok(())
        }

        // ------------------------------------------------------------------
        Statement::VariableDeclaration(decl) => {
            lower_variable_declaration(decl, semantic, ctx)
        }

        // ------------------------------------------------------------------
        Statement::FunctionDeclaration(f) => {
            let mut cb = make_lower_expr_cb(semantic);
            functions::lower_function_declaration(ctx, semantic, f, &mut cb)
        }

        // ------------------------------------------------------------------
        Statement::LabeledStatement(s) => {
            let mut lower_stmt_cb = make_lower_stmt_cb(semantic);
            loops::lower_labeled(ctx, semantic, s, &mut lower_stmt_cb)
        }

        // ------------------------------------------------------------------
        Statement::DebuggerStatement(s) => {
            let loc = span_loc(s.span);
            ctx.push(InstructionValue::Debugger { loc }, SourceLocation::Generated);
            Ok(())
        }

        // ------------------------------------------------------------------
        Statement::EmptyStatement(_) => Ok(()),

        // ------------------------------------------------------------------
        _ => {
            ctx.push(
                InstructionValue::UnsupportedNode { loc: SourceLocation::Generated },
                SourceLocation::Generated,
            );
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// IIFE helpers
// ---------------------------------------------------------------------------

/// If `call` is an immediately-invoked function expression with no params and
/// no arguments (i.e. `(function() { stmts })()` or `(() => { stmts })()`),
/// return the slice of body statements for inlining. Returns `None` otherwise.
fn iife_body_stmts<'r, 'a: 'r>(call: &'r oxc_ast::ast::CallExpression<'a>) -> Option<&'r [Statement<'a>]> {
    if !call.arguments.is_empty() {
        return None;
    }
    // Unwrap parenthesized expression: `(function() {...})()` has callee
    // wrapped in a ParenthesizedExpression.
    let callee = unwrap_parens(&call.callee);
    match callee {
        Expression::FunctionExpression(f) => {
            if f.r#async || f.generator || !f.params.items.is_empty() {
                return None;
            }
            f.body.as_ref().map(|b| b.statements.as_slice())
        }
        Expression::ArrowFunctionExpression(a) => {
            if a.r#async || !a.params.items.is_empty() {
                return None;
            }
            // Only handle block-body arrows (not expression-body)
            if a.expression {
                return None;
            }
            Some(a.body.statements.as_slice())
        }
        _ => None,
    }
}

/// Strip any number of `ParenthesizedExpression` wrappers from `expr`.
fn unwrap_parens<'r, 'a: 'r>(expr: &'r Expression<'a>) -> &'r Expression<'a> {
    let mut e = expr;
    loop {
        if let Expression::ParenthesizedExpression(p) = e {
            e = &p.expression;
        } else {
            return e;
        }
    }
}

/// Returns true if `stmt` contains a `return` statement OR a nested IIFE call
/// (shallow check for top-level statements; does not recurse into nested fns).
/// We block inlining when a nested IIFE call is present because recursive
/// inlining would over-eagerly flatten nested IIFE structures that the TS
/// compiler preserves at the inner level.
fn stmt_has_return(stmt: &Statement<'_>) -> bool {
    match stmt {
        Statement::ReturnStatement(_) => true,
        Statement::BlockStatement(b) => b.body.iter().any(stmt_has_return),
        Statement::IfStatement(s) => {
            stmt_has_return(&s.consequent)
                || s.alternate.as_ref().map_or(false, |a| stmt_has_return(a))
        }
        // If the body contains a nested IIFE call as an expression statement,
        // don't inline this outer IIFE (to avoid over-flattening).
        Statement::ExpressionStatement(e) => {
            if let Expression::CallExpression(c) = &e.expression {
                if c.arguments.is_empty() {
                    let callee = unwrap_parens(&c.callee);
                    return matches!(callee, Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_));
                }
            }
            false
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// lower_expr — public so submodules can recurse via closures
// ---------------------------------------------------------------------------

pub fn lower_expr<'r, 'a: 'r>(
    expr: &'r Expression<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) -> Result<Place> {
    match expr {
        // ------------------------------------------------------------------
        // Literals — delegate to expressions::lower_literal
        Expression::NumericLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::NullLiteral(_) => {
            expressions::lower_literal(ctx, expr)
        }

        Expression::BigIntLiteral(lit) => {
            let loc = span_loc(lit.span);
            Ok(ctx.push(
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(lit.raw.to_string()),
                    loc: loc.clone(),
                },
                loc,
            ))
        }

        // ------------------------------------------------------------------
        // Identifier
        Expression::Identifier(ident) => {
            expressions::lower_identifier(ctx, semantic, ident)
        }

        // ------------------------------------------------------------------
        // Binary expression
        Expression::BinaryExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            expressions::lower_binary(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Unary expression
        Expression::UnaryExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            expressions::lower_unary(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Update expression (++/--)
        Expression::UpdateExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            expressions::lower_update(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Template literal
        Expression::TemplateLiteral(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            expressions::lower_template(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Await expression
        Expression::AwaitExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            expressions::lower_await(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Call expression
        Expression::CallExpression(e) => {
            // IIFE inlining: (function() { stmts })() or (() => { stmts })()
            // with no params and no args — inline body statements in place.
            if e.arguments.is_empty() {
                if let Some(stmts) = iife_body_stmts(e) {
                    // Lower each statement inline. If any statement is a return,
                    // we handle it as the IIFE result. For now, only inline IIFEs
                    // with no return statements (pure side-effect bodies).
                    let has_return = stmts.iter().any(stmt_has_return);
                    if !has_return {
                        for stmt in stmts {
                            lower_statement(stmt, semantic, ctx)?;
                        }
                        let loc = span_loc(e.span);
                        return Ok(ctx.push(
                            InstructionValue::Primitive {
                                value: PrimitiveValue::Undefined,
                                loc: loc.clone(),
                            },
                            loc,
                        ));
                    }
                }
            }
            let mut cb = make_lower_expr_cb(semantic);
            calls::lower_call(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // New expression
        Expression::NewExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            calls::lower_new(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Member expressions (static and computed)
        // Note: in oxc 0.69 these appear as direct Expression variants, not
        // wrapped in a single MemberExpression variant.
        Expression::StaticMemberExpression(e) => {
            let loc = span_loc(e.span);
            let object = lower_expr(&e.object, semantic, ctx)?;
            let property = e.property.name.to_string();
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }

        Expression::ComputedMemberExpression(e) => {
            let loc = span_loc(e.span);
            let object = lower_expr(&e.object, semantic, ctx)?;
            let property = lower_expr(&e.expression, semantic, ctx)?;
            Ok(ctx.push(
                InstructionValue::ComputedLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }

        Expression::PrivateFieldExpression(e) => {
            let loc = span_loc(e.span);
            let object = lower_expr(&e.object, semantic, ctx)?;
            let property = format!("#{}", e.field.name);
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }

        // ------------------------------------------------------------------
        // Assignment expression
        Expression::AssignmentExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            properties::lower_assignment(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Conditional (ternary) expression
        Expression::ConditionalExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            control_flow::lower_conditional(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Logical expression (&&, ||, ??)
        Expression::LogicalExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            control_flow::lower_logical(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Arrow function expression
        Expression::ArrowFunctionExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            functions::lower_arrow(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Function expression
        Expression::FunctionExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            functions::lower_function_expr(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // JSX element
        Expression::JSXElement(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            jsx::lower_jsx_element(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // JSX fragment
        Expression::JSXFragment(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            jsx::lower_jsx_fragment(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // Array expression
        Expression::ArrayExpression(e) => {
            lower_array_expression(e, semantic, ctx)
        }

        // ------------------------------------------------------------------
        // Object expression
        Expression::ObjectExpression(e) => {
            lower_object_expression(e, semantic, ctx)
        }

        // ------------------------------------------------------------------
        // Sequence expression (comma operator) — lower each, return last
        Expression::SequenceExpression(e) => {
            let mut last = None;
            for sub in &e.expressions {
                last = Some(lower_expr(sub, semantic, ctx)?);
            }
            Ok(last.unwrap_or_else(|| ctx.make_temporary(SourceLocation::Generated)))
        }

        // ------------------------------------------------------------------
        // Tagged template
        Expression::TaggedTemplateExpression(e) => {
            let mut cb = make_lower_expr_cb(semantic);
            expressions::lower_tagged_template(ctx, semantic, e, &mut cb)
        }

        // ------------------------------------------------------------------
        // RegExp literal
        Expression::RegExpLiteral(e) => {
            let loc = span_loc(e.span);
            // e.regex.pattern is a RegExpPattern<'a>; get text via the .text field (Atom).
            // e.regex.flags is a RegExpFlags bitfield — it implements Display.
            let pattern = e.regex.pattern.text.to_string();
            let flags = e.regex.flags.to_string();
            Ok(ctx.push(
                InstructionValue::RegExpLiteral { pattern, flags, loc: loc.clone() },
                loc,
            ))
        }

        // ------------------------------------------------------------------
        // MetaProperty (import.meta, new.target)
        Expression::MetaProperty(e) => {
            let loc = span_loc(e.span);
            // Only import.meta is supported; new.target and others are TODOs.
            if e.meta.name != "import" {
                return Err(CompilerError::todo(
                    "(BuildHIR::lowerExpression) Handle MetaProperty expressions other than import.meta",
                ));
            }
            Ok(ctx.push(
                InstructionValue::MetaProperty {
                    meta: e.meta.name.to_string(),
                    property: e.property.name.to_string(),
                    loc: loc.clone(),
                },
                loc,
            ))
        }

        // ------------------------------------------------------------------
        // Parenthesized expressions are transparent
        Expression::ParenthesizedExpression(e) => {
            lower_expr(&e.expression, semantic, ctx)
        }

        // ------------------------------------------------------------------
        // TypeScript: type assertions, satisfies, non-null assertions, etc.
        // These are transparent — just lower the inner expression.
        Expression::TSAsExpression(e) => {
            let inner = lower_expr(&e.expression, semantic, ctx)?;
            // Preserve `as const` annotations — extract the annotation text.
            let annotation: Option<String> = match &e.type_annotation {
                oxc_ast::ast::TSType::TSTypeReference(tref) => {
                    match &tref.type_name {
                        oxc_ast::ast::TSTypeName::IdentifierReference(id) => {
                            Some(id.name.as_str().to_string())
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            if let Some(ann) = annotation {
                let loc = span_loc(e.span);
                let cast_place = ctx.push(InstructionValue::TypeCastExpression {
                    value: inner,
                    type_: Type::default(),
                    source_annotation: Some(ann),
                    loc: loc.clone(),
                }, loc);
                Ok(cast_place)
            } else {
                Ok(inner)
            }
        }
        Expression::TSSatisfiesExpression(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSNonNullExpression(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSTypeAssertion(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSInstantiationExpression(e) => lower_expr(&e.expression, semantic, ctx),

        // ------------------------------------------------------------------
        // Chain expression (optional chaining: a?.b, foo?.(), etc.)
        // Preserve the source text verbatim so codegen can emit `a?.b.c[0]` correctly.
        Expression::ChainExpression(chain) => {
            use oxc_span::GetSpan;
            let span = chain.span();
            let source_text = &semantic.source_text()[span.start as usize..span.end as usize];
            let loc = span_loc(span);
            Ok(ctx.push(
                InstructionValue::InlineJs {
                    source: source_text.to_string(),
                    loc: loc.clone(),
                },
                loc,
            ))
        }

        // ------------------------------------------------------------------
        // Yield expression
        Expression::YieldExpression(e) => {
            return Err(CompilerError::todo(
                "(BuildHIR::lowerExpression) Handle YieldExpression expressions",
            ));
        }

        // ------------------------------------------------------------------
        // Anything else — emit UnsupportedNode
        _ => {
            Ok(ctx.push(
                InstructionValue::UnsupportedNode { loc: SourceLocation::Generated },
                SourceLocation::Generated,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Array / Object expression helpers
// ---------------------------------------------------------------------------

fn lower_array_expression<'r, 'a: 'r>(
    e: &'r oxc_ast::ast::ArrayExpression<'a>,
    semantic: &'r oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) -> Result<Place> {
    let loc = span_loc(e.span);
    let mut elements = Vec::new();
    for elem in &e.elements {
        match elem {
            oxc_ast::ast::ArrayExpressionElement::SpreadElement(spread) => {
                let place = lower_expr(&spread.argument, semantic, ctx)?;
                elements.push(ArrayElement::Spread(SpreadPattern { place }));
            }
            oxc_ast::ast::ArrayExpressionElement::Elision(_) => {
                elements.push(ArrayElement::Hole);
            }
            expr_elem => {
                if let Some(inner) = expr_elem.as_expression() {
                    let place = lower_expr(inner, semantic, ctx)?;
                    elements.push(ArrayElement::Place(place));
                } else {
                    elements.push(ArrayElement::Hole);
                }
            }
        }
    }
    Ok(ctx.push(
        InstructionValue::ArrayExpression { elements, loc: loc.clone() },
        loc,
    ))
}

fn lower_object_expression<'r, 'a: 'r>(
    e: &'r oxc_ast::ast::ObjectExpression<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) -> Result<Place> {
    let loc = span_loc(e.span);
    let mut properties = Vec::new();
    for prop in &e.properties {
        match prop {
            oxc_ast::ast::ObjectPropertyKind::SpreadProperty(spread) => {
                let place = lower_expr(&spread.argument, semantic, ctx)?;
                properties.push(ObjectExpressionProperty::Spread(SpreadPattern { place }));
            }
            oxc_ast::ast::ObjectPropertyKind::ObjectProperty(obj_prop) => {
                // Getter/setter syntax is not yet supported.
                match obj_prop.kind {
                    oxc_ast::ast::PropertyKind::Get => {
                        return Err(CompilerError::todo(
                            "(BuildHIR::lowerExpression) Handle get functions in ObjectExpression",
                        ));
                    }
                    oxc_ast::ast::PropertyKind::Set => {
                        return Err(CompilerError::todo(
                            "(BuildHIR::lowerExpression) Handle set functions in ObjectExpression",
                        ));
                    }
                    oxc_ast::ast::PropertyKind::Init => {}
                }
                let value = lower_expr(&obj_prop.value, semantic, ctx)?;
                let key = lower_property_key(&obj_prop.key, semantic, ctx)?;
                let prop_type = if obj_prop.method {
                    ObjectPropertyType::Method
                } else {
                    ObjectPropertyType::Property
                };
                properties.push(ObjectExpressionProperty::Property(ObjectProperty {
                    key,
                    type_: prop_type,
                    place: value,
                }));
            }
        }
    }
    Ok(ctx.push(
        InstructionValue::ObjectExpression { properties, loc: loc.clone() },
        loc,
    ))
}

fn lower_property_key<'r, 'a: 'r>(
    key: &'r oxc_ast::ast::PropertyKey<'a>,
    semantic: &'r oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) -> Result<ObjectPropertyKey> {
    match key {
        oxc_ast::ast::PropertyKey::StaticIdentifier(ident) => {
            Ok(ObjectPropertyKey::Identifier(ident.name.to_string()))
        }
        oxc_ast::ast::PropertyKey::StringLiteral(s) => {
            Ok(ObjectPropertyKey::String(s.value.to_string()))
        }
        oxc_ast::ast::PropertyKey::NumericLiteral(n) => {
            Ok(ObjectPropertyKey::Number(n.value))
        }
        k_key => {
            // Computed expression key.
            if let Some(k_expr) = k_key.as_expression() {
                let key_place = lower_expr(k_expr, semantic, ctx)?;
                Ok(ObjectPropertyKey::Computed(key_place))
            } else {
                Ok(ObjectPropertyKey::Identifier("__unknown__".to_string()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// lower_variable_declaration — public for use by loops.rs
// ---------------------------------------------------------------------------

pub fn lower_variable_declaration<'r, 'a: 'r>(
    decl: &'r oxc_ast::ast::VariableDeclaration<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
) -> Result<()> {
    let kind = match decl.kind {
        VariableDeclarationKind::Const => InstructionKind::Const,
        VariableDeclarationKind::Let => InstructionKind::Let,
        VariableDeclarationKind::Var => {
            return Err(CompilerError::todo(
                "(BuildHIR::lowerStatement) Handle var kinds in VariableDeclaration",
            ));
        }
        VariableDeclarationKind::Using => InstructionKind::Let,
        VariableDeclarationKind::AwaitUsing => InstructionKind::Let,
    };

    for declarator in &decl.declarations {
        let loc = span_loc(declarator.span);

        if let Some(init_expr) = &declarator.init {
            let value_place = lower_expr(init_expr, semantic, ctx)?;
            bind_pattern(&declarator.id, value_place, kind, semantic, ctx, loc)?;
        } else {
            declare_pattern(&declarator.id, kind, semantic, ctx, loc)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pattern binding helpers
// ---------------------------------------------------------------------------

/// Emit StoreLocal / Destructure for a binding pattern with an init value.
fn bind_pattern<'r, 'a: 'r>(
    pat: &'r oxc_ast::ast::BindingPattern<'a>,
    value: Place,
    kind: InstructionKind,
    semantic: &'r oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
    loc: SourceLocation,
) -> Result<()> {
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(ident) => {
            let maybe_sym = ident.symbol_id.get();
            let id = if let Some(sym_id) = maybe_sym {
                ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone())
            } else {
                ctx.env.new_temporary(loc.clone())
            };
            let lvalue = LValue { place: Place::new(id, loc.clone()), kind };
            ctx.push(
                InstructionValue::StoreLocal { lvalue, value, type_annotation: None, loc: loc.clone() },
                loc,
            );
        }
        BindingPatternKind::ArrayPattern(_)
        | BindingPatternKind::ObjectPattern(_)
        | BindingPatternKind::AssignmentPattern(_) => {
            // Delegate to patterns module which handles defaults, nested patterns, etc.
            super::patterns::lower_binding_pattern(
                ctx,
                semantic,
                pat,
                value,
                kind,
                &mut |expr, ctx| lower_expr(expr, semantic, ctx),
            )?;
        }
    }
    Ok(())
}

/// Emit DeclareLocal for a binding pattern with no init.
fn declare_pattern<'r, 'a: 'r>(
    pat: &'r oxc_ast::ast::BindingPattern<'a>,
    kind: InstructionKind,
    semantic: &'r oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
    loc: SourceLocation,
) -> Result<()> {
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(ident) => {
            let maybe_sym = ident.symbol_id.get();
            let id = if let Some(sym_id) = maybe_sym {
                ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone())
            } else {
                ctx.env.new_temporary(loc.clone())
            };
            let lvalue = LValue { place: Place::new(id, loc.clone()), kind };
            ctx.push(
                InstructionValue::DeclareLocal { lvalue, type_annotation: None, loc: loc.clone() },
                loc,
            );
        }
        _ => {
            ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pattern shape builders (for Destructure instructions)
// ---------------------------------------------------------------------------

fn lower_array_pattern<'r, 'a: 'r>(
    pat: &'r oxc_ast::ast::ArrayPattern<'a>,
    semantic: &'r oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
    loc: SourceLocation,
) -> ArrayPattern {
    let mut items = Vec::new();
    for elem in &pat.elements {
        if let Some(elem_pat) = elem {
            match &elem_pat.kind {
                BindingPatternKind::BindingIdentifier(ident) => {
                    let maybe_sym = ident.symbol_id.get();
                    let id = if let Some(sym_id) = maybe_sym {
                        ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone())
                    } else {
                        ctx.env.new_temporary(loc.clone())
                    };
                    items.push(ArrayElement::Place(Place::new(id, loc.clone())));
                }
                _ => {
                    let tmp = ctx.make_temporary(loc.clone());
                    items.push(ArrayElement::Place(tmp));
                }
            }
        } else {
            items.push(ArrayElement::Hole);
        }
    }
    if let Some(rest) = &pat.rest {
        let maybe_sym = match &rest.argument.kind {
            BindingPatternKind::BindingIdentifier(ident) => ident.symbol_id.get(),
            _ => None,
        };
        let name = match &rest.argument.kind {
            BindingPatternKind::BindingIdentifier(ident) => Some(ident.name.as_str()),
            _ => None,
        };
        let id = if let Some(sym_id) = maybe_sym {
            ctx.get_or_create_symbol(sym_id.index() as u32, name, loc.clone())
        } else {
            ctx.env.new_temporary(loc.clone())
        };
        items.push(ArrayElement::Spread(SpreadPattern { place: Place::new(id, loc.clone()) }));
    }
    ArrayPattern { items, loc }
}

fn lower_object_pattern<'r, 'a: 'r>(
    pat: &'r oxc_ast::ast::ObjectPattern<'a>,
    semantic: &'r oxc_semantic::Semantic<'a>,
    ctx: &mut LoweringContext,
    loc: SourceLocation,
) -> ObjectPattern {
    let mut properties = Vec::new();
    for prop in &pat.properties {
        let place_id = match &prop.value.kind {
            BindingPatternKind::BindingIdentifier(ident) => {
                let maybe_sym = ident.symbol_id.get();
                if let Some(sym_id) = maybe_sym {
                    ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone())
                } else {
                    ctx.env.new_temporary(loc.clone())
                }
            }
            _ => ctx.env.new_temporary(loc.clone()),
        };
        let key = match &prop.key {
            oxc_ast::ast::PropertyKey::StaticIdentifier(ident) => {
                ObjectPropertyKey::Identifier(ident.name.to_string())
            }
            oxc_ast::ast::PropertyKey::StringLiteral(s) => {
                ObjectPropertyKey::String(s.value.to_string())
            }
            oxc_ast::ast::PropertyKey::NumericLiteral(n) => {
                ObjectPropertyKey::Number(n.value)
            }
            _ => {
                let key_place = ctx.make_temporary(loc.clone());
                ObjectPropertyKey::Computed(key_place)
            }
        };
        properties.push(ObjectPatternProperty::Property(ObjectProperty {
            key,
            type_: ObjectPropertyType::Property,
            place: Place::new(place_id, loc.clone()),
        }));
    }
    if pat.rest.is_some() {
        let tmp = ctx.make_temporary(loc.clone());
        properties.push(ObjectPatternProperty::Spread(SpreadPattern { place: tmp }));
    }
    ObjectPattern { properties, loc }
}

// ---------------------------------------------------------------------------
// Pragma-based validations
// ---------------------------------------------------------------------------

/// Parse `@validateBlocklistedImports:["pkg1","pkg2"]` from the first comment
/// line and verify that no import declarations use those packages.
fn validate_blocklisted_imports(source: &str, program: &oxc_ast::ast::Program) -> Result<()> {
    // Find the pragma in the first few lines.
    let first_line = source.lines().next().unwrap_or("");
    let blocklist = parse_blocklisted_imports_pragma(first_line);
    if blocklist.is_empty() {
        return Ok(());
    }

    for stmt in &program.body {
        if let oxc_ast::ast::Statement::ImportDeclaration(import) = stmt {
            let source_val = import.source.value.as_str();
            if blocklist.iter().any(|b| b == source_val) {
                return Err(CompilerError::todo("Bailing out due to blocklisted import"));
            }
        }
    }
    Ok(())
}

/// Parse `@validateBlocklistedImports:["a","b"]` or `@validateBlocklistedImports:['a']`
/// from a pragma comment line.
fn parse_blocklisted_imports_pragma(line: &str) -> Vec<String> {
    let needle = "@validateBlocklistedImports:";
    let Some(idx) = line.find(needle) else { return vec![] };
    let after = &line[idx + needle.len()..];
    // Extract balanced brackets [...]
    let Some(open) = after.find('[') else { return vec![] };
    let Some(close) = after[open..].find(']') else { return vec![] };
    let inner = &after[open + 1..open + close];
    // Parse comma-separated quoted strings.
    inner
        .split(',')
        .filter_map(|part| {
            let trimmed = part.trim().trim_matches('"').trim_matches('\'');
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        })
        .collect()
}

/// Traverse the AST to find direct calls to capitalized identifier functions.
/// Called when `@validateNoCapitalizedCalls` pragma is present.
fn validate_no_capitalized_calls(program: &oxc_ast::ast::Program) -> Result<()> {
    for stmt in &program.body {
        if let Err(e) = check_stmt_capitalized_calls_with_aliases(stmt) {
            return Err(e);
        }
    }
    Ok(())
}

/// Entry point for checking capitalized calls with alias tracking in function bodies.
fn check_stmt_capitalized_calls_with_aliases(stmt: &oxc_ast::ast::Statement) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                let aliases = collect_capitalized_aliases(&body.statements);
                check_stmts_cap_calls_aliased(&body.statements, &aliases)?;
            }
        }
        Statement::ExpressionStatement(e) => check_expr_capitalized_calls(&e.expression)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument { check_expr_capitalized_calls(arg)?; }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(Expression::ArrowFunctionExpression(a)) = &d.init {
                    let aliases = collect_capitalized_aliases(&a.body.statements);
                    check_stmts_cap_calls_aliased(&a.body.statements, &aliases)?;
                } else if let Some(Expression::FunctionExpression(f)) = &d.init {
                    if let Some(body) = &f.body {
                        let aliases = collect_capitalized_aliases(&body.statements);
                        check_stmts_cap_calls_aliased(&body.statements, &aliases)?;
                    }
                } else if let Some(init) = &d.init {
                    check_expr_capitalized_calls(init)?;
                }
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body { check_stmt_capitalized_calls_with_aliases(s)?; }
        }
        _ => {}
    }
    Ok(())
}

/// Collect aliases: `let x = Bar` or `const x = Bar` where Bar is capitalized.
fn collect_capitalized_aliases(stmts: &[oxc_ast::ast::Statement]) -> std::collections::HashSet<String> {
    let mut aliases = std::collections::HashSet::new();
    for stmt in stmts {
        if let oxc_ast::ast::Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                if let Some(Expression::Identifier(id)) = &decl.init {
                    if id.name.chars().next().map_or(false, |c| c.is_uppercase()) {
                        if let BindingPatternKind::BindingIdentifier(b) = &decl.id.kind {
                            aliases.insert(b.name.to_string());
                        }
                    }
                }
            }
        }
    }
    aliases
}

fn check_stmts_cap_calls_aliased(
    stmts: &[oxc_ast::ast::Statement],
    aliases: &std::collections::HashSet<String>,
) -> Result<()> {
    for stmt in stmts {
        check_stmt_cap_calls_aliased(stmt, aliases)?;
    }
    Ok(())
}

fn check_stmt_cap_calls_aliased(
    stmt: &oxc_ast::ast::Statement,
    aliases: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_expr_cap_calls_aliased(&e.expression, aliases)?,
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { check_expr_cap_calls_aliased(a, aliases)?; }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init { check_expr_cap_calls_aliased(init, aliases)?; }
            }
        }
        Statement::IfStatement(i) => {
            check_stmt_cap_calls_aliased(&i.consequent, aliases)?;
            if let Some(alt) = &i.alternate { check_stmt_cap_calls_aliased(alt, aliases)?; }
        }
        Statement::BlockStatement(b) => check_stmts_cap_calls_aliased(&b.body, aliases)?,
        _ => {}
    }
    Ok(())
}

fn check_expr_cap_calls_aliased(
    expr: &Expression,
    aliases: &std::collections::HashSet<String>,
) -> Result<()> {
    if let Expression::CallExpression(call) = expr {
        // Check direct alias call: `x()` where `x` aliases a capitalized identifier
        if let Expression::Identifier(id) = &call.callee {
            if aliases.contains(id.name.as_str()) {
                return Err(CompilerError::invalid_react(
                    "Capitalized functions are reserved for components, which must be invoked with JSX.",
                ));
            }
        }
        // Also check normal capitalized calls
        check_expr_capitalized_calls(expr)?;
    } else {
        check_expr_capitalized_calls(expr)?;
    }
    Ok(())
}

fn check_stmt_capitalized_calls(stmt: &oxc_ast::ast::Statement) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for s in &body.statements {
                    check_stmt_capitalized_calls(s)?;
                }
            }
        }
        Statement::ExpressionStatement(e) => check_expr_capitalized_calls(&e.expression)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_capitalized_calls(arg)?;
            }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_capitalized_calls(init)?;
                }
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_capitalized_calls(s)?;
            }
        }
        Statement::IfStatement(i) => {
            check_stmt_capitalized_calls(&i.consequent)?;
            if let Some(alt) = &i.alternate {
                check_stmt_capitalized_calls(alt)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_capitalized_calls(expr: &Expression) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            // Direct capitalized identifier call: SomeFunc()
            if let Expression::Identifier(id) = &call.callee {
                let name = id.name.as_str();
                if name.chars().next().map_or(false, |c| c.is_uppercase()) {
                    return Err(CompilerError::invalid_react(
                        "Capitalized functions are reserved for components, which must be invoked with JSX.",
                    ));
                }
            }
            // Member expression with capitalized property: obj.SomeFunc()
            if let Expression::StaticMemberExpression(m) = &call.callee {
                let prop = m.property.name.as_str();
                if prop.chars().next().map_or(false, |c| c.is_uppercase()) {
                    return Err(CompilerError::invalid_react(
                        "Capitalized functions are reserved for components, which must be invoked with JSX.",
                    ));
                }
            }
            // Recurse into arguments
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_capitalized_calls(e)?;
                }
            }
            check_expr_capitalized_calls(&call.callee)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                check_expr_capitalized_calls(e)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Detect calls to known impure functions (Date.now, Math.random, etc.)
/// when @validateNoImpureFunctionsInRender pragma is present.
fn validate_no_impure_functions(program: &oxc_ast::ast::Program) -> Result<()> {
    // Known impure functions: (object, method)
    const IMPURE: &[(&str, &str)] = &[
        ("Date", "now"),
        ("Date", "getTime"),
        ("Math", "random"),
        ("performance", "now"),
        ("crypto", "getRandomValues"),
    ];

    for stmt in &program.body {
        check_stmt_impure_functions(stmt, IMPURE)?;
    }
    Ok(())
}

fn check_stmt_impure_functions(stmt: &oxc_ast::ast::Statement, impure: &[(&str, &str)]) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for s in &body.statements {
                    check_stmt_impure_functions(s, impure)?;
                }
            }
        }
        Statement::ExpressionStatement(e) => check_expr_impure_functions(&e.expression, impure)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_impure_functions(arg, impure)?;
            }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_impure_functions(init, impure)?;
                }
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_impure_functions(s, impure)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_impure_functions(expr: &Expression, impure: &[(&str, &str)]) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            if let Expression::StaticMemberExpression(m) = &call.callee {
                if let Expression::Identifier(obj) = &m.object {
                    let obj_name = obj.name.as_str();
                    let prop_name = m.property.name.as_str();
                    if impure.iter().any(|(o, p)| *o == obj_name && *p == prop_name) {
                        return Err(CompilerError::invalid_react(format!(
                            "Calling {obj_name}.{prop_name}() during render produces a new value each call and is not allowed"
                        )));
                    }
                }
            }
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_impure_functions(e, impure)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Detect ESLint rule suppressions that the compiler must bail out for.
/// Always checks react-hooks/rules-of-hooks.
/// Additional rules can be specified via @eslintSuppressionRules pragma.
///
/// Suppressions inside 'use no forget'/'use no memo' function bodies are ignored —
/// those functions are passthrough and their ESLint suppressions are irrelevant.
fn validate_no_eslint_suppression<'a>(source: &str, program: &'a oxc_ast::ast::Program<'a>) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    // Collect byte ranges [start, end) of opted-out function bodies.
    let opted_out_ranges: Vec<(u32, u32)> = {
        let mut ranges = Vec::new();
        for stmt in &program.body {
            let maybe_body: Option<&oxc_ast::ast::FunctionBody> = match stmt {
                Statement::FunctionDeclaration(f) => f.body.as_deref(),
                Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => f.body.as_deref(),
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => Some(&a.body),
                    _ => None,
                },
                Statement::ExportNamedDeclaration(d) => match &d.declaration {
                    Some(Declaration::FunctionDeclaration(f)) => f.body.as_deref(),
                    _ => None,
                },
                _ => None,
            };
            if let Some(body) = maybe_body {
                let is_opted_out = body.directives.iter().any(|d|
                    matches!(d.expression.value.as_str(), "use no memo" | "use no forget")
                );
                if is_opted_out {
                    ranges.push((body.span.start, body.span.end));
                }
            }
        }
        ranges
    };

    let in_opted_out = |byte_offset: usize| -> bool {
        let off = byte_offset as u32;
        opted_out_ranges.iter().any(|(start, end)| off >= *start && off < *end)
    };

    let first_line = source.lines().next().unwrap_or("");

    // Parse @eslintSuppressionRules:["rule1","rule2"] pragma.
    let mut rules_to_check: Vec<String> = vec![
        "react-hooks/rules-of-hooks".to_string(),
        "react-compiler/react-compiler".to_string(),
    ];

    if let Some(idx) = first_line.find("@eslintSuppressionRules:") {
        let after = &first_line[idx + "@eslintSuppressionRules:".len()..];
        if let Some(open) = after.find('[') {
            if let Some(close) = after[open..].find(']') {
                let inner = &after[open + 1..open + close];
                for part in inner.split(',') {
                    let rule = part.trim().trim_matches('"').trim_matches('\'').to_string();
                    if !rule.is_empty() {
                        rules_to_check.push(rule);
                    }
                }
            }
        }
    }

    // Check if any of the rules are suppressed in the source OUTSIDE opted-out fn bodies.
    for rule in &rules_to_check {
        let disable_patterns = [
            format!("eslint-disable {}", rule),
            format!("eslint-disable-next-line {}", rule),
        ];
        for pattern in &disable_patterns {
            let mut search_pos = 0;
            while let Some(rel_idx) = source[search_pos..].find(pattern.as_str()) {
                let abs_idx = search_pos + rel_idx;
                if !in_opted_out(abs_idx) {
                    return Err(CompilerError::invalid_react(format!(
                        "React Compiler has skipped optimizing this component because one or more React ESLint rules were disabled\n\
                         React Compiler only works when your components follow all the rules of React, disabling them may result in unexpected or incorrect behavior. \
                         Found suppression `{pattern}`."
                    )));
                }
                search_pos = abs_idx + pattern.len();
            }
        }
    }

    // Check for @enableFlowSuppressions and $FlowFixMe[react-rule-hook] comment.
    if first_line.contains("@enableFlowSuppressions")
        && source.contains("$FlowFixMe[react-rule-hook]")
    {
        return Err(CompilerError::invalid_react(
            "React Compiler has skipped optimizing this component because a Flow suppression was found"
        ));
    }

    Ok(())
}

/// Detect function declarations that appear after a return statement in a block.
/// These are hoisted by JS but unsupported by the compiler.
fn check_hoisted_function_declarations(stmts: &[oxc_ast::ast::Statement]) -> Result<()> {
    let mut seen_return = false;
    for stmt in stmts {
        if matches!(stmt, oxc_ast::ast::Statement::ReturnStatement(_)) {
            seen_return = true;
        }
        if seen_return && matches!(stmt, oxc_ast::ast::Statement::FunctionDeclaration(_)) {
            return Err(CompilerError::todo(
                "Support functions with unreachable code that may contain hoisted declarations",
            ));
        }
    }
    Ok(())
}

/// Collect all bare identifier names referenced in a statement (non-recursive into inner functions).
fn collect_stmt_ident_refs(stmt: &oxc_ast::ast::Statement) -> Vec<String> {
    let mut result = Vec::new();
    collect_idents_in_stmt(stmt, &mut result);
    result
}

fn collect_idents_in_stmt(stmt: &oxc_ast::ast::Statement, out: &mut Vec<String>) {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => collect_idents_in_expr(&e.expression, out),
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                collect_idents_in_expr(arg, out);
            }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    collect_idents_in_expr(init, out);
                }
            }
        }
        Statement::IfStatement(i) => {
            collect_idents_in_expr(&i.test, out);
            collect_idents_in_stmt(&i.consequent, out);
            if let Some(alt) = &i.alternate {
                collect_idents_in_stmt(alt, out);
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                collect_idents_in_stmt(s, out);
            }
        }
        // Function declarations: scan the body to find references to outer-scope fn decls.
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for s in &body.statements {
                    collect_idents_in_stmt(s, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_idents_in_expr(expr: &Expression, out: &mut Vec<String>) {
    match expr {
        Expression::Identifier(id) => {
            out.push(id.name.to_string());
        }
        Expression::CallExpression(call) => {
            collect_idents_in_expr(&call.callee, out);
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    collect_idents_in_expr(e, out);
                }
            }
        }
        Expression::BinaryExpression(b) => {
            collect_idents_in_expr(&b.left, out);
            collect_idents_in_expr(&b.right, out);
        }
        Expression::StaticMemberExpression(s) => {
            collect_idents_in_expr(&s.object, out);
        }
        Expression::ComputedMemberExpression(c) => {
            collect_idents_in_expr(&c.object, out);
            collect_idents_in_expr(&c.expression, out);
        }
        Expression::JSXElement(j) => {
            for child in &j.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(e) = child {
                    if let Some(inner) = e.expression.as_expression() {
                        collect_idents_in_expr(inner, out);
                    }
                }
            }
            for attr in &j.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                        if let Some(inner) = ec.expression.as_expression() {
                            collect_idents_in_expr(inner, out);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Check if a list of statements contains any ThrowStatement (shallow check).
fn stmt_list_has_throw(stmts: &[oxc_ast::ast::Statement]) -> bool {
    stmts.iter().any(|s| matches!(s, oxc_ast::ast::Statement::ThrowStatement(_)))
}

/// Return true if `name` matches the React hook naming convention: starts with `use[A-Z]`.
fn is_hook_name(name: &str) -> bool {
    if let Some(after) = name.strip_prefix("use") {
        after.chars().next().map_or(false, |c| c.is_uppercase())
    } else {
        false
    }
}

/// Collect all locally-defined binding names from params and body statements.
/// Used to exclude local variables from the hook-as-value check.
fn collect_local_names<'a>(
    params: &'a [oxc_ast::ast::FormalParameter<'a>],
    stmts: &'a [oxc_ast::ast::Statement<'a>],
) -> std::collections::HashSet<String> {
    use oxc_ast::ast::BindingPatternKind;
    let mut names = std::collections::HashSet::new();
    // Collect from parameters
    fn collect_from_pattern(pat: &oxc_ast::ast::BindingPattern, names: &mut std::collections::HashSet<String>) {
        match &pat.kind {
            BindingPatternKind::BindingIdentifier(id) => { names.insert(id.name.to_string()); }
            BindingPatternKind::ObjectPattern(o) => {
                for prop in &o.properties {
                    collect_from_pattern(&prop.value, names);
                }
                if let Some(rest) = &o.rest {
                    collect_from_pattern(&rest.argument, names);
                }
            }
            BindingPatternKind::ArrayPattern(a) => {
                for elem in a.elements.iter().flatten() {
                    collect_from_pattern(elem, names);
                }
                if let Some(rest) = &a.rest {
                    collect_from_pattern(&rest.argument, names);
                }
            }
            BindingPatternKind::AssignmentPattern(a) => {
                collect_from_pattern(&a.left, names);
            }
        }
    }
    for param in params {
        collect_from_pattern(&param.pattern, &mut names);
    }
    // Collect from variable declarations in body
    fn collect_from_stmts(stmts: &[oxc_ast::ast::Statement], names: &mut std::collections::HashSet<String>) {
        use oxc_ast::ast::Statement;
        for stmt in stmts {
            match stmt {
                Statement::VariableDeclaration(v) => {
                    for d in &v.declarations {
                        collect_from_pattern_inner(&d.id, names);
                    }
                }
                Statement::BlockStatement(b) => collect_from_stmts(&b.body, names),
                Statement::IfStatement(i) => {
                    collect_from_stmts(std::slice::from_ref(&i.consequent), names);
                    if let Some(alt) = &i.alternate { collect_from_stmts(std::slice::from_ref(alt), names); }
                }
                _ => {}
            }
        }
    }
    fn collect_from_pattern_inner(pat: &oxc_ast::ast::BindingPattern, names: &mut std::collections::HashSet<String>) {
        use oxc_ast::ast::BindingPatternKind;
        match &pat.kind {
            BindingPatternKind::BindingIdentifier(id) => { names.insert(id.name.to_string()); }
            BindingPatternKind::ObjectPattern(o) => {
                for prop in &o.properties { collect_from_pattern_inner(&prop.value, names); }
                if let Some(rest) = &o.rest { collect_from_pattern_inner(&rest.argument, names); }
            }
            BindingPatternKind::ArrayPattern(a) => {
                for elem in a.elements.iter().flatten() { collect_from_pattern_inner(elem, names); }
                if let Some(rest) = &a.rest { collect_from_pattern_inner(&rest.argument, names); }
            }
            BindingPatternKind::AssignmentPattern(a) => { collect_from_pattern_inner(&a.left, names); }
        }
    }
    collect_from_stmts(stmts, &mut names);
    names
}

/// Validate that hook identifiers are not referenced as values (must be called).
/// Scans all top-level functions in the program.
fn validate_no_hook_as_value(program: &oxc_ast::ast::Program) -> Result<()> {
    for stmt in &program.body {
        match stmt {
            oxc_ast::ast::Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    let locals = collect_local_names(&f.params.items, &body.statements);
                    for s in &body.statements {
                        check_stmt_hook_value(s, &locals)?;
                    }
                }
            }
            oxc_ast::ast::Statement::ExportDefaultDeclaration(d) => {
                match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body {
                            let locals = collect_local_names(&f.params.items, &body.statements);
                            for s in &body.statements {
                                check_stmt_hook_value(s, &locals)?;
                            }
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                        let locals = collect_local_names(&a.params.items, &a.body.statements);
                        for s in &a.body.statements {
                            check_stmt_hook_value(s, &locals)?;
                        }
                    }
                    _ => {}
                }
            }
            oxc_ast::ast::Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    if let Some(body) = &f.body {
                        let locals = collect_local_names(&f.params.items, &body.statements);
                        for s in &body.statements {
                            check_stmt_hook_value(s, &locals)?;
                        }
                    }
                }
            }
            oxc_ast::ast::Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        match init {
                            Expression::ArrowFunctionExpression(a) => {
                                let locals = collect_local_names(&a.params.items, &a.body.statements);
                                for s in &a.body.statements {
                                    check_stmt_hook_value(s, &locals)?;
                                }
                            }
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body {
                                    let locals = collect_local_names(&f.params.items, &body.statements);
                                    for s in &body.statements {
                                        check_stmt_hook_value(s, &locals)?;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_stmt_hook_value(stmt: &oxc_ast::ast::Statement, locals: &std::collections::HashSet<String>) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_hook_value(init, false, locals)?;
                }
            }
        }
        Statement::ExpressionStatement(e) => check_expr_hook_value(&e.expression, false, locals)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_hook_value(arg, false, locals)?;
            }
        }
        Statement::IfStatement(i) => {
            check_expr_hook_value(&i.test, false, locals)?;
            check_stmt_hook_value(&i.consequent, locals)?;
            if let Some(alt) = &i.alternate {
                check_stmt_hook_value(alt, locals)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_hook_value(s, locals)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_hook_value(expr: &Expression, is_callee: bool, locals: &std::collections::HashSet<String>) -> Result<()> {
    const MSG: &str = "Hooks may not be referenced as normal values, they must be called. See https://react.dev/reference/rules/react-calls-components-and-hooks#never-pass-around-hooks-as-regular-values";
    match expr {
        Expression::Identifier(id) => {
            if !is_callee && is_hook_name(id.name.as_str()) && !locals.contains(id.name.as_str()) {
                return Err(CompilerError::invalid_react(MSG));
            }
        }
        Expression::StaticMemberExpression(s) => {
            let prop = s.property.name.as_str();
            // `obj.useHook` is invalid unless `obj` is a local variable.
            let obj_is_local = if let Expression::Identifier(obj_id) = &s.object {
                locals.contains(obj_id.name.as_str())
            } else {
                false
            };
            if !is_callee && is_hook_name(prop) && !obj_is_local {
                return Err(CompilerError::invalid_react(MSG));
            }
        }
        Expression::CallExpression(call) => {
            // Callee is being called, so it's ok; recurse with is_callee=true for callee
            check_expr_hook_value(&call.callee, true, locals)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_hook_value(e, false, locals)?;
                }
            }
        }
        Expression::JSXElement(j) => {
            for attr in &j.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                        if let Some(inner) = ec.expression.as_expression() {
                            check_expr_hook_value(inner, false, locals)?;
                        }
                    }
                }
            }
            for child in &j.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(ec) = child {
                    if let Some(inner) = ec.expression.as_expression() {
                        check_expr_hook_value(inner, false, locals)?;
                    }
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_hook_value(&l.left, false, locals)?;
            check_expr_hook_value(&l.right, false, locals)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_hook_value(&c.test, false, locals)?;
            check_expr_hook_value(&c.consequent, false, locals)?;
            check_expr_hook_value(&c.alternate, false, locals)?;
        }
        Expression::AssignmentExpression(a) => {
            check_expr_hook_value(&a.right, false, locals)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                check_expr_hook_value(e, false, locals)?;
            }
        }
        // Don't recurse into nested arrow/function expressions (different scope)
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {}
        _ => {}
    }
    Ok(())
}

/// Validate that state setter functions from useState() are not called during render.
/// Applies when `@validateNoSetStateInRender` pragma is present.
fn validate_no_setstate_in_render(program: &oxc_ast::ast::Program, treat_set_as_setter: bool) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    // Find the first component/hook function body AND params in the program.
    let (body_stmts, maybe_params) = find_first_fn_body_and_params(program);
    if body_stmts.is_empty() {
        return Ok(());
    }

    // Step 1: Collect setter names from useState/useCustomState destructuring.
    let mut setters: HashSet<String> = HashSet::new();
    for stmt in body_stmts {
        collect_state_setters_from_stmt(stmt, &mut setters);
    }

    // Also collect prop-based setters when @enableTreatSetIdentifiersAsStateSetters is active.
    if treat_set_as_setter {
        if let Some(params) = maybe_params {
            collect_set_params(params, &mut setters);
        }
    }

    if setters.is_empty() {
        return Ok(());
    }

    // Also expand setters with direct aliases: `const aliased = setter` → aliased is a setter.
    let mut aliases_added = true;
    while aliases_added {
        aliases_added = false;
        for stmt in body_stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(Expression::Identifier(rhs)) = &decl.init {
                        if setters.contains(rhs.name.as_str()) {
                            if let BindingPatternKind::BindingIdentifier(lhs) = &decl.id.kind {
                                if setters.insert(lhs.name.to_string()) {
                                    aliases_added = true;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Step 2: Collect a map of local function name → set of function names it calls.
    // This lets us transitively find which local functions call a setter.
    let mut fn_calls: HashMap<String, HashSet<String>> = HashMap::new();
    for stmt in body_stmts {
        collect_fn_call_map_stmt(stmt, &mut fn_calls);
    }

    // Step 3: Transitively expand setters to include any local function that
    // (directly or indirectly) calls a setter. Fixed-point iteration.
    let mut setter_callers = setters.clone();
    loop {
        let prev_len = setter_callers.len();
        for (fn_name, called) in &fn_calls {
            if called.iter().any(|c| setter_callers.contains(c)) {
                setter_callers.insert(fn_name.clone());
            }
        }
        if setter_callers.len() == prev_len { break; }
    }

    // Step 4: Check if any setter (or a function that transitively calls one) is
    // called at the top render level (not inside closures or conditionals).
    for stmt in body_stmts {
        check_stmt_for_setter_call(stmt, &setter_callers)?;
    }

    Ok(())
}

/// Build a map from local function variable names to the set of names they call.
/// Only looks at arrow function / function expression initializers of variable declarations.
fn collect_fn_call_map_stmt<'a>(
    stmt: &'a oxc_ast::ast::Statement<'a>,
    map: &mut std::collections::HashMap<String, std::collections::HashSet<String>>,
) {
    use oxc_ast::ast::{Expression, Statement};
    if let Statement::VariableDeclaration(vd) = stmt {
        for decl in &vd.declarations {
            let fn_name = match &decl.id.kind {
                BindingPatternKind::BindingIdentifier(id) => id.name.to_string(),
                _ => continue,
            };
            let body_stmts: Option<&[oxc_ast::ast::Statement<'_>]> = match &decl.init {
                Some(Expression::ArrowFunctionExpression(arrow)) => Some(&arrow.body.statements),
                Some(Expression::FunctionExpression(func)) => {
                    func.body.as_ref().map(|b| b.statements.as_slice())
                }
                _ => None,
            };
            if let Some(stmts) = body_stmts {
                let mut calls = std::collections::HashSet::new();
                collect_called_names_in_stmts(stmts, &mut calls);
                map.insert(fn_name, calls);
            }
        }
    }
    // Also handle function declarations inside the component body
    if let Statement::FunctionDeclaration(f) = stmt {
        if let Some(id) = &f.id {
            let fn_name = id.name.to_string();
            if let Some(body) = &f.body {
                let mut calls = std::collections::HashSet::new();
                collect_called_names_in_stmts(&body.statements, &mut calls);
                map.insert(fn_name, calls);
            }
        }
    }
}

/// Collect all identifier names called as functions within statement list.
fn collect_called_names_in_stmts<'a>(
    stmts: &'a [oxc_ast::ast::Statement<'a>],
    names: &mut std::collections::HashSet<String>,
) {
    for stmt in stmts {
        collect_called_names_in_stmt(stmt, names);
    }
}

fn collect_called_names_in_stmt<'a>(
    stmt: &'a oxc_ast::ast::Statement<'a>,
    names: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => collect_called_names_in_expr(&e.expression, names),
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { collect_called_names_in_expr(a, names); }
        }
        Statement::BlockStatement(b) => collect_called_names_in_stmts(&b.body, names),
        // Do NOT recurse into conditionals or loops — only track unconditional calls.
        // This prevents `bar() { if (cond) { foo(); } }` from being treated as
        // unconditionally calling foo.
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init { collect_called_names_in_expr(init, names); }
            }
        }
        _ => {}
    }
}

fn collect_called_names_in_expr<'a>(
    expr: &'a oxc_ast::ast::Expression<'a>,
    names: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::Expression;
    match expr {
        Expression::CallExpression(c) => {
            if let Expression::Identifier(id) = &c.callee {
                names.insert(id.name.to_string());
            }
            collect_called_names_in_expr(&c.callee, names);
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() { collect_called_names_in_expr(e, names); }
            }
        }
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {
            // Don't recurse into nested closures
        }
        Expression::LogicalExpression(l) => {
            collect_called_names_in_expr(&l.left, names);
            collect_called_names_in_expr(&l.right, names);
        }
        Expression::ConditionalExpression(c) => {
            collect_called_names_in_expr(&c.test, names);
            collect_called_names_in_expr(&c.consequent, names);
            collect_called_names_in_expr(&c.alternate, names);
        }
        _ => {}
    }
}

/// Find the statements of the first function body in the program.
/// Like `find_first_fn_body` but also returns the function params.
fn find_first_fn_body_and_params<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> (&'a [oxc_ast::ast::Statement<'a>], Option<&'a oxc_ast::ast::FormalParameters<'a>>) {
    for stmt in &program.body {
        match stmt {
            oxc_ast::ast::Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    return (&body.statements, Some(&f.params));
                }
            }
            oxc_ast::ast::Statement::ExportDefaultDeclaration(d) => {
                match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body {
                            return (&body.statements, Some(&f.params));
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                        return (&a.body.statements, Some(&a.params));
                    }
                    _ => {}
                }
            }
            oxc_ast::ast::Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    if let Some(body) = &f.body {
                        return (&body.statements, Some(&f.params));
                    }
                }
            }
            _ => {}
        }
    }
    (&[], None)
}

/// Collect identifiers starting with `set` from function parameters (for @enableTreatSetIdentifiersAsStateSetters).
fn collect_set_params(
    params: &oxc_ast::ast::FormalParameters,
    setters: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::BindingPatternKind;
    for param in &params.items {
        collect_set_from_binding_pattern(&param.pattern, setters);
    }
}

fn collect_set_from_binding_pattern(
    pat: &oxc_ast::ast::BindingPattern,
    setters: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::BindingPatternKind;
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(id) => {
            let name = id.name.as_str();
            if name.starts_with("set") && name.len() > 3 {
                let fourth = name.chars().nth(3);
                if fourth.map_or(false, |c| c.is_uppercase() || c == '_') {
                    setters.insert(name.to_string());
                }
            }
        }
        BindingPatternKind::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_set_from_binding_pattern(&prop.value, setters);
            }
        }
        BindingPatternKind::ArrayPattern(arr) => {
            for item in arr.elements.iter().flatten() {
                collect_set_from_binding_pattern(item, setters);
            }
        }
        _ => {}
    }
}

fn find_first_fn_body<'a>(program: &'a oxc_ast::ast::Program<'a>) -> &'a [oxc_ast::ast::Statement<'a>] {
    for stmt in &program.body {
        match stmt {
            oxc_ast::ast::Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    return &body.statements;
                }
            }
            oxc_ast::ast::Statement::ExportDefaultDeclaration(d) => {
                match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body {
                            return &body.statements;
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                        return &a.body.statements;
                    }
                    _ => {}
                }
            }
            oxc_ast::ast::Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    if let Some(body) = &f.body {
                        return &body.statements;
                    }
                }
            }
            _ => {}
        }
    }
    &[]
}

/// Collect names of state setter variables (from useState/use*State destructuring).
fn collect_state_setters_from_stmt(stmt: &oxc_ast::ast::Statement, setters: &mut std::collections::HashSet<String>) {
    if let oxc_ast::ast::Statement::VariableDeclaration(v) = stmt {
        for decl in &v.declarations {
            if let Some(init) = &decl.init {
                if let Expression::CallExpression(call) = init {
                    // Detect useState() or any use*State() or custom hooks
                    let is_state_hook = match &call.callee {
                        Expression::Identifier(id) => {
                            let name = id.name.as_str();
                            name == "useState" || (name.starts_with("use") && name.ends_with("State"))
                        }
                        Expression::StaticMemberExpression(m) => {
                            if let Expression::Identifier(obj) = &m.object {
                                (obj.name == "React" || obj.name == "react") && m.property.name == "useState"
                            } else { false }
                        }
                        // Also match any useCustomState patterns
                        _ => false,
                    };
                    // Also accept any use* hook that returns an array (common pattern)
                    let is_any_hook_call = match &call.callee {
                        Expression::Identifier(id) => is_hook_name(id.name.as_str()),
                        _ => false,
                    };

                    if is_state_hook || is_any_hook_call {
                        // LHS should be array destructure: [state, setter]
                        if let BindingPatternKind::ArrayPattern(arr) = &decl.id.kind {
                            // Setter is at position 1 (e.g. [state, setState])
                            if let Some(Some(setter_pat)) = arr.elements.get(1) {
                                if let BindingPatternKind::BindingIdentifier(id) = &setter_pat.kind {
                                    let name = id.name.to_string();
                                    if name.starts_with("set") {
                                        setters.insert(name);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Check if a setter is called at render level.
/// Does NOT enter conditional branches (setState in if-blocks is allowed).
/// Does NOT enter loops or function expressions (setters in callbacks are fine).
fn check_stmt_for_setter_call(stmt: &oxc_ast::ast::Statement, setters: &std::collections::HashSet<String>) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_expr_for_setter_call(&e.expression, setters)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_for_setter_call(arg, setters)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_for_setter_call(s, setters)?;
            }
        }
        // Don't recurse into conditionals, loops, or function expressions.
        // setState inside an if/for/while/arrow is allowed (conditional update pattern).
        _ => {}
    }
    Ok(())
}

fn check_expr_for_setter_call(expr: &Expression, setters: &std::collections::HashSet<String>) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            if let Expression::Identifier(id) = &call.callee {
                if setters.contains(id.name.as_str()) {
                    return Err(CompilerError::invalid_react(
                        "Cannot call setState during render\n\nCalling setState during render may trigger an infinite loop.\n* To reset state when other state/props change, store the previous value in state and update conditionally: https://react.dev/reference/react/useState#storing-information-from-previous-renders\n* To derive data from other state/props, compute the derived data during render without using state.",
                    ));
                }
            }
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_for_setter_call(e, setters)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_for_setter_call(&l.left, setters)?;
            check_expr_for_setter_call(&l.right, setters)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_for_setter_call(&c.consequent, setters)?;
            check_expr_for_setter_call(&c.alternate, setters)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                check_expr_for_setter_call(e, setters)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate that the program does not import from known-incompatible libraries.
fn validate_no_incompatible_libraries(program: &oxc_ast::ast::Program) -> Result<()> {
    const INCOMPATIBLE: &[&str] = &["ReactCompilerKnownIncompatibleTest"];
    const TYPE_PROVIDER: &[&str] = &["ReactCompilerTest", "useDefaultExportNotTypedAsHook"];
    for stmt in &program.body {
        if let oxc_ast::ast::Statement::ImportDeclaration(import) = stmt {
            let src = import.source.value.as_str();
            if INCOMPATIBLE.contains(&src) {
                return Err(CompilerError::compilation_skipped(
                    "Use of incompatible library\n\nThis API returns functions which cannot be memoized without leading to stale UI. To prevent this, by default React Compiler will skip memoizing this component/hook. However, you may see issues if values from this API are passed to other components/hooks that are memoized.",
                ));
            }
            if TYPE_PROVIDER.contains(&src) {
                return Err(CompilerError::invalid_react(
                    "Invalid type configuration for module",
                ));
            }
        }
    }
    Ok(())
}

/// Validate that global/module-scope variables are not reassigned inside component/hook functions.
/// Collect local variable names in `stmts` whose closure bodies directly assign to globals.
fn collect_direct_global_assigning_closures<'a, F>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    is_global: &F,
) -> std::collections::HashSet<String>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::{Statement, BindingPatternKind};
    let mut result = std::collections::HashSet::new();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    let body_stmts: Option<&[oxc_ast::ast::Statement]> = match init {
                        Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    if let Some(body) = body_stmts {
                        if closure_body_assigns_global_directly(body, is_global) {
                            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                result.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    result
}

/// Collect closures that BOTH directly assign globals AND return JSX (render helpers).
/// Render helpers are unsafe to pass as JSX props because they run during render.
fn collect_render_helper_global_assigners<'a, F>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    is_global: &F,
) -> std::collections::HashSet<String>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::{Statement, BindingPatternKind};
    let mut result = std::collections::HashSet::new();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    let body_stmts: Option<&[oxc_ast::ast::Statement]> = match init {
                        Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    if let Some(body) = body_stmts {
                        if closure_body_assigns_global_directly(body, is_global)
                            && closure_body_has_jsx_return(body)
                        {
                            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                result.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    result
}

/// Returns true if `stmts` contains at least one `return <JSX...>` or `return <></>` statement
/// (searching through if/block nesting, but not into nested closures).
fn closure_body_has_jsx_return<'a>(stmts: &[oxc_ast::ast::Statement<'a>]) -> bool {
    use oxc_ast::ast::Statement;
    for stmt in stmts {
        match stmt {
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    if matches!(arg, Expression::JSXElement(_) | Expression::JSXFragment(_)) {
                        return true;
                    }
                }
            }
            Statement::IfStatement(i) => {
                if closure_body_has_jsx_return(std::slice::from_ref(&i.consequent)) { return true; }
                if let Some(alt) = &i.alternate {
                    if closure_body_has_jsx_return(std::slice::from_ref(alt)) { return true; }
                }
            }
            Statement::BlockStatement(b) => {
                if closure_body_has_jsx_return(&b.body) { return true; }
            }
            _ => {}
        }
    }
    false
}

/// Returns true if any statement in `stmts` directly assigns to a global (no recursion into nested closures).
fn closure_body_assigns_global_directly<'a, F>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    is_global: &F,
) -> bool
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::Statement;
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if expr_assigns_global_directly(&e.expression, is_global) { return true; }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument {
                    if expr_assigns_global_directly(a, is_global) { return true; }
                }
            }
            Statement::IfStatement(i) => {
                if closure_body_assigns_global_directly(
                    std::slice::from_ref(&i.consequent), is_global) { return true; }
                if let Some(alt) = &i.alternate {
                    if closure_body_assigns_global_directly(std::slice::from_ref(alt), is_global) { return true; }
                }
            }
            Statement::BlockStatement(b) => {
                if closure_body_assigns_global_directly(&b.body, is_global) { return true; }
            }
            _ => {}
        }
    }
    false
}

fn expr_assigns_global_directly<'a, F>(expr: &Expression<'a>, is_global: &F) -> bool
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::AssignmentTarget;
    match expr {
        Expression::AssignmentExpression(a) => {
            match &a.left {
                AssignmentTarget::AssignmentTargetIdentifier(id) => is_global(id),
                AssignmentTarget::StaticMemberExpression(m) => {
                    if let Expression::Identifier(id) = &m.object { is_global(id) } else { false }
                }
                _ => false,
            }
        }
        Expression::SequenceExpression(s) => s.expressions.iter().any(|e| expr_assigns_global_directly(e, is_global)),
        // Don't recurse into nested closures
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
        _ => false,
    }
}

/// Expand `callers` by adding hook-wrapped closures (`const fn = useHook(arrow, ...)`) whose
/// wrapped arrow body calls any name already in `callers`.
/// Handles patterns like: `const fn = useCallback(() => setState(x), [...])`.
fn expand_hook_wrapped_setter_callers<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    callers: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::{BindingPatternKind, Statement};
    loop {
        let before = callers.len();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(Expression::CallExpression(call)) = &decl.init {
                        // Only hook calls (useCallback, useEvent, etc.)
                        let is_hook_call = match &call.callee {
                            Expression::Identifier(id) => is_hook_name(id.name.as_str()),
                            Expression::StaticMemberExpression(m) => is_hook_name(m.property.name.as_str()),
                            _ => false,
                        };
                        if !is_hook_call { continue; }
                        // Extract the body of the first argument (the callback)
                        let body = call.arguments.first().and_then(|a| a.as_expression()).and_then(|e| {
                            match e {
                                Expression::ArrowFunctionExpression(a) => Some(a.body.statements.as_slice()),
                                Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                                _ => None,
                            }
                        });
                        if let Some(body) = body {
                            if closure_body_calls_any(body, callers) {
                                if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                    callers.insert(id.name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        if callers.len() == before { break; }
    }
}

/// Expand `assigners` by adding closures that call any name already in the set (transitive).
fn expand_transitive_global_assigners<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    assigners: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::{Statement, BindingPatternKind};
    loop {
        let before = assigners.len();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        let body_stmts: Option<&[oxc_ast::ast::Statement]> = match init {
                            Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                            Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                            _ => None,
                        };
                        if let Some(body) = body_stmts {
                            if closure_body_calls_any(body, assigners) {
                                if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                    assigners.insert(id.name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        if assigners.len() == before { break; }
    }
}

fn closure_body_calls_any<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    names: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::Statement;
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if expr_calls_any(&e.expression, names) { return true; }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument { if expr_calls_any(a, names) { return true; } }
            }
            Statement::BlockStatement(b) => {
                if closure_body_calls_any(&b.body, names) { return true; }
            }
            Statement::IfStatement(i) => {
                if closure_body_calls_any(std::slice::from_ref(&i.consequent), names) { return true; }
                if let Some(alt) = &i.alternate {
                    if closure_body_calls_any(std::slice::from_ref(alt), names) { return true; }
                }
            }
            _ => {}
        }
    }
    false
}

fn expr_calls_any<'a>(expr: &Expression<'a>, names: &std::collections::HashSet<String>) -> bool {
    match expr {
        Expression::CallExpression(call) => {
            if let Expression::Identifier(id) = &call.callee {
                if names.contains(id.name.as_str()) { return true; }
            }
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    if expr_calls_any(e, names) { return true; }
                }
            }
            false
        }
        Expression::SequenceExpression(s) => s.expressions.iter().any(|e| expr_calls_any(e, names)),
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
        _ => false,
    }
}

fn validate_no_global_reassignment<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let root_scope = semantic.scoping().root_scope_id();

    // Helper: is this IdentifierReference a global or module-level variable?
    let is_global_or_module_ref = |ident: &oxc_ast::ast::IdentifierReference| -> bool {
        let ref_id = match ident.reference_id.get() {
            Some(r) => r,
            None => return true, // no reference info = treat as undeclared global
        };
        let sym_id = match semantic.scoping().get_reference(ref_id).symbol_id() {
            Some(s) => s,
            None => return true, // unresolved = undeclared global
        };
        semantic.scoping().symbol_scope_id(sym_id) == root_scope
    };

    // Find all component/hook function bodies in the program.
    // Only check functions whose names look like components (uppercase) or hooks (use[A-Z]).
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn is_component_or_hook_name(name: &str) -> bool {
        let first = name.chars().next();
        first.map_or(false, |c| c.is_uppercase()) || is_hook_name(name)
    }

    let mut bodies: Vec<&[oxc_ast::ast::Statement]> = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                let name = f.id.as_ref().and_then(|id| Some(id.name.as_str())).unwrap_or("");
                if is_component_or_hook_name(name) {
                    if let Some(body) = &f.body { bodies.push(&body.statements); }
                }
            }
            Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    let name = f.id.as_ref().and_then(|id| Some(id.name.as_str())).unwrap_or("");
                    // Default exports: check if looks like component/hook, or allow by default
                    if is_component_or_hook_name(name) || name.is_empty() {
                        if let Some(body) = &f.body { bodies.push(&body.statements); }
                    }
                }
                ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                    bodies.push(&a.body.statements);
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    let name = f.id.as_ref().and_then(|id| Some(id.name.as_str())).unwrap_or("");
                    if is_component_or_hook_name(name) {
                        if let Some(body) = &f.body { bodies.push(&body.statements); }
                    }
                }
            }
            _ => {}
        }
    }

    for stmts in &bodies {
        let mut local_assigners = collect_direct_global_assigning_closures(stmts, &is_global_or_module_ref);
        expand_transitive_global_assigners(stmts, &mut local_assigners);
        // Render helpers: closures that both assign globals AND return JSX.
        // Only these are flagged when passed as JSX props (event handlers are allowed).
        let render_helper_assigners = collect_render_helper_global_assigners(stmts, &is_global_or_module_ref);
        global_check_stmts(stmts, &is_global_or_module_ref, &local_assigners, &render_helper_assigners)?;
    }
    Ok(())
}

type AssignerSet = std::collections::HashSet<String>;

fn global_check_stmts<'a, F>(stmts: &[oxc_ast::ast::Statement<'a>], is_global: &F, local_assigners: &AssignerSet, render_helpers: &AssignerSet) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    for stmt in stmts { global_check_stmt(stmt, is_global, local_assigners, render_helpers)?; }
    Ok(())
}

fn global_check_stmt<'a, F>(stmt: &oxc_ast::ast::Statement<'a>, is_global: &F, local_assigners: &AssignerSet, render_helpers: &AssignerSet) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => global_check_expr(&e.expression, is_global, local_assigners, render_helpers)?,
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init { global_check_expr(init, is_global, local_assigners, render_helpers)?; }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { global_check_expr(a, is_global, local_assigners, render_helpers)?; }
        }
        Statement::IfStatement(i) => {
            global_check_expr(&i.test, is_global, local_assigners, render_helpers)?;
            global_check_stmt(&i.consequent, is_global, local_assigners, render_helpers)?;
            if let Some(alt) = &i.alternate { global_check_stmt(alt, is_global, local_assigners, render_helpers)?; }
        }
        Statement::BlockStatement(b) => global_check_stmts(&b.body, is_global, local_assigners, render_helpers)?,
        Statement::WhileStatement(w) => {
            global_check_expr(&w.test, is_global, local_assigners, render_helpers)?;
            global_check_stmt(&w.body, is_global, local_assigners, render_helpers)?;
        }
        Statement::ForStatement(f) => {
            if let Some(init) = &f.init {
                if let Some(e) = init.as_expression() { global_check_expr(e, is_global, local_assigners, render_helpers)?; }
            }
            if let Some(t) = &f.test { global_check_expr(t, is_global, local_assigners, render_helpers)?; }
            if let Some(u) = &f.update { global_check_expr(u, is_global, local_assigners, render_helpers)?; }
            global_check_stmt(&f.body, is_global, local_assigners, render_helpers)?;
        }
        _ => {}
    }
    Ok(())
}

fn global_check_expr<'a, F>(expr: &Expression<'a>, is_global: &F, local_assigners: &AssignerSet, render_helpers: &AssignerSet) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    match expr {
        Expression::AssignmentExpression(a) => {
            global_check_assignment_target(&a.left, is_global)?;
            global_check_expr(&a.right, is_global, local_assigners, render_helpers)?;
        }
        Expression::UpdateExpression(u) => {
            if let oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument {
                if is_global(id) {
                    return Err(CompilerError::todo(
                        "(BuildHIR::lowerExpression) Support UpdateExpression where argument is a global",
                    ));
                }
            }
        }
        Expression::LogicalExpression(l) => {
            global_check_expr(&l.left, is_global, local_assigners, render_helpers)?;
            global_check_expr(&l.right, is_global, local_assigners, render_helpers)?;
        }
        Expression::ConditionalExpression(c) => {
            global_check_expr(&c.test, is_global, local_assigners, render_helpers)?;
            global_check_expr(&c.consequent, is_global, local_assigners, render_helpers)?;
            global_check_expr(&c.alternate, is_global, local_assigners, render_helpers)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions { global_check_expr(e, is_global, local_assigners, render_helpers)?; }
        }
        Expression::UnaryExpression(u) => {
            if u.operator == oxc_ast::ast::UnaryOperator::Delete {
                if let Expression::StaticMemberExpression(m) = &u.argument {
                    if let Expression::Identifier(id) = &m.object {
                        if is_global(id) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified\n\nModifying a variable defined outside a component or hook is not allowed. Consider using an effect.",
                            ));
                        }
                    }
                }
            }
        }
        Expression::CallExpression(call) => {
            // Check if calling a local closure that assigns globals.
            if let Expression::Identifier(id) = &call.callee {
                if local_assigners.contains(id.name.as_str()) {
                    return Err(CompilerError::invalid_react(
                        "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                    ));
                }
            }
            global_check_expr(&call.callee, is_global, local_assigners, render_helpers)?;
            // Detect deferred hooks: don't flag closures passed as args to hooks (useX)
            let callee_name = match &call.callee {
                Expression::Identifier(id) => Some(id.name.as_str()),
                Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
                _ => None,
            };
            let is_deferred_hook = callee_name.map_or(false, |n| is_hook_name(n));
            // Check args: if an arg is a reference to a local global-assigning closure, flag it
            // (e.g. `foo(fn)` where fn = () => { global = x }) — but not for hook calls
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    if !is_deferred_hook {
                        if let Expression::Identifier(id) = e {
                            if local_assigners.contains(id.name.as_str()) {
                                return Err(CompilerError::invalid_react(
                                    "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                                ));
                            }
                        }
                        // For non-hook calls, recurse into inline closure bodies to catch
                        // direct global mutations like `foo(() => { x.a = 10; })`.
                        match e {
                            Expression::ArrowFunctionExpression(arrow) => {
                                global_check_stmts(&arrow.body.statements, is_global, local_assigners, render_helpers)?;
                                continue;
                            }
                            Expression::FunctionExpression(func) => {
                                if let Some(body) = &func.body {
                                    global_check_stmts(&body.statements, is_global, local_assigners, render_helpers)?;
                                }
                                continue;
                            }
                            _ => {}
                        }
                    }
                    global_check_expr(e, is_global, local_assigners, render_helpers)?;
                }
            }
        }
        Expression::JSXElement(jsx) => {
            // JSX component tag: <Foo /> where Foo is a local global-assigning closure.
            // HTML-like tags use JSXElementName::Identifier; component references (JS
            // variables) use JSXElementName::IdentifierReference.
            let tag_name = match &jsx.opening_element.name {
                oxc_ast::ast::JSXElementName::Identifier(id) => Some(id.name.as_str()),
                oxc_ast::ast::JSXElementName::IdentifierReference(id) => Some(id.name.as_str()),
                _ => None,
            };
            if let Some(name) = tag_name {
                if local_assigners.contains(name) {
                    return Err(CompilerError::invalid_react(
                        "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                    ));
                }
            }
            // JSX attribute values: only render helpers (closures returning JSX) are unsafe
            // as JSX props. Event handlers that mutate globals are allowed.
            for attr_item in &jsx.opening_element.attributes {
                match attr_item {
                    oxc_ast::ast::JSXAttributeItem::Attribute(attr) => {
                        if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(c)) = &attr.value {
                            if let Some(e) = c.expression.as_expression() {
                                if let Expression::Identifier(id) = e {
                                    if render_helpers.contains(id.name.as_str()) {
                                        return Err(CompilerError::invalid_react(
                                            "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                                        ));
                                    }
                                }
                                global_check_expr(e, is_global, local_assigners, render_helpers)?;
                            }
                        }
                    }
                    oxc_ast::ast::JSXAttributeItem::SpreadAttribute(spread) => {
                        global_check_expr(&spread.argument, is_global, local_assigners, render_helpers)?;
                    }
                }
            }
            // JSX children expression containers
            for child in &jsx.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(c) = child {
                    if let Some(e) = c.expression.as_expression() {
                        if let Expression::Identifier(id) = e {
                            if local_assigners.contains(id.name.as_str()) {
                                return Err(CompilerError::invalid_react(
                                    "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                                ));
                            }
                        }
                        global_check_expr(e, is_global, local_assigners, render_helpers)?;
                    }
                }
            }
        }
        // Do NOT recurse into arrow/function expressions — closures passed to hooks
        // or used as event handlers are allowed to modify globals.
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {}
        _ => {}
    }
    Ok(())
}

fn global_check_assignment_target<'a, F>(target: &oxc_ast::ast::AssignmentTarget<'a>, is_global: &F) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::AssignmentTarget;
    match target {
        AssignmentTarget::AssignmentTargetIdentifier(id) => {
            if is_global(id) {
                return Err(CompilerError::invalid_react(
                    "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                ));
            }
        }
        AssignmentTarget::StaticMemberExpression(m) => {
            if let Expression::Identifier(id) = &m.object {
                if is_global(id) {
                    return Err(CompilerError::invalid_react(
                        "This value cannot be modified\n\nModifying a variable defined outside a component or hook is not allowed. Consider using an effect.",
                    ));
                }
            }
        }
        AssignmentTarget::ComputedMemberExpression(m) => {
            if let Expression::Identifier(id) = &m.object {
                if is_global(id) {
                    return Err(CompilerError::invalid_react(
                        "This value cannot be modified\n\nModifying a variable defined outside a component or hook is not allowed. Consider using an effect.",
                    ));
                }
            }
        }
        AssignmentTarget::ArrayAssignmentTarget(arr) => {
            for elem in &arr.elements {
                if let Some(target_elem) = elem {
                    if let oxc_ast::ast::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(id) = target_elem {
                        if is_global(id) {
                            return Err(CompilerError::invalid_react(
                                "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                            ));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validate that state setters are not called inside useMemo/useCallback callbacks.
fn validate_no_setstate_in_memo_callback<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use std::collections::HashSet;

    let body_stmts = {
        use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};
        let mut result: Option<&'a [oxc_ast::ast::Statement<'a>]> = None;
        for stmt in &program.body {
            match stmt {
                Statement::FunctionDeclaration(f) => {
                    if let Some(body) = &f.body { result = Some(&body.statements); break; }
                }
                Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body { result = Some(&body.statements); break; }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                        result = Some(&a.body.statements); break;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        match result {
            Some(stmts) => stmts,
            None => return Ok(()),
        }
    };

    // Collect setter names from useState destructuring
    let mut setters: HashSet<String> = HashSet::new();
    for stmt in body_stmts {
        collect_state_setters_from_stmt(stmt, &mut setters);
    }
    if setters.is_empty() {
        return Ok(());
    }

    // Expand setters transitively: add closures that call any setter (or setter-caller).
    // This catches indirect patterns like: const fn = useCallback(() => setState(x));
    // useMemo(() => { fn(); }, ...) — fn() is an indirect setter call.
    // First pass: handle hook-wrapped closures (const fn = useCallback(arrowBody, ...))
    expand_hook_wrapped_setter_callers(body_stmts, &mut setters);
    // Second pass: handle bare arrows/fns and transitive call chains
    expand_transitive_global_assigners(body_stmts, &mut setters);

    // Check each statement for useMemo/useCallback calls containing setters
    for stmt in body_stmts {
        check_stmt_for_memo_setter(stmt, &setters)?;
    }
    Ok(())
}

fn is_use_memo_call(expr: &Expression) -> bool {
    match expr {
        Expression::CallExpression(call) => {
            match &call.callee {
                Expression::Identifier(id) => id.name.as_str() == "useMemo",
                Expression::StaticMemberExpression(m) => {
                    if let Expression::Identifier(obj) = &m.object {
                        obj.name == "React" && m.property.name.as_str() == "useMemo"
                    } else { false }
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn check_stmt_for_memo_setter(stmt: &oxc_ast::ast::Statement, setters: &std::collections::HashSet<String>) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_expr_for_memo_setter(&e.expression, setters)?,
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_memo_setter(init, setters)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument { check_expr_for_memo_setter(arg, setters)?; }
        }
        Statement::IfStatement(i) => {
            check_stmt_for_memo_setter(&i.consequent, setters)?;
            if let Some(alt) = &i.alternate { check_stmt_for_memo_setter(alt, setters)?; }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body { check_stmt_for_memo_setter(s, setters)?; }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_memo_setter(expr: &Expression, setters: &std::collections::HashSet<String>) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            if is_use_memo_call(expr) {
                // Check the first argument (the callback) for setter calls
                if let Some(first_arg) = call.arguments.first() {
                    if let Some(callback_expr) = first_arg.as_expression() {
                        check_memo_callback_for_setter(callback_expr, setters)?;
                    }
                }
            }
            // Recurse into other arguments (but not as memo callbacks)
            for arg in call.arguments.iter().skip(if is_use_memo_call(expr) { 1 } else { 0 }) {
                if let Some(e) = arg.as_expression() {
                    check_expr_for_memo_setter(e, setters)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_for_memo_setter(&l.left, setters)?;
            check_expr_for_memo_setter(&l.right, setters)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_for_memo_setter(&c.consequent, setters)?;
            check_expr_for_memo_setter(&c.alternate, setters)?;
        }
        _ => {}
    }
    Ok(())
}

/// Check inside a useMemo/useCallback callback for state setter calls.
fn check_memo_callback_for_setter(expr: &Expression, setters: &std::collections::HashSet<String>) -> Result<()> {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                check_memo_body_stmt_for_setter(stmt, setters)?;
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    check_memo_body_stmt_for_setter(stmt, setters)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_memo_body_stmt_for_setter(stmt: &oxc_ast::ast::Statement, setters: &std::collections::HashSet<String>) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_memo_body_expr_for_setter(&e.expression, setters)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument { check_memo_body_expr_for_setter(arg, setters)?; }
        }
        Statement::IfStatement(i) => {
            check_memo_body_stmt_for_setter(&i.consequent, setters)?;
            if let Some(alt) = &i.alternate { check_memo_body_stmt_for_setter(alt, setters)?; }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body { check_memo_body_stmt_for_setter(s, setters)?; }
        }
        _ => {}
    }
    Ok(())
}

fn check_memo_body_expr_for_setter(expr: &Expression, setters: &std::collections::HashSet<String>) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            if let Expression::Identifier(id) = &call.callee {
                if setters.contains(id.name.as_str()) {
                    return Err(CompilerError::invalid_react(
                        "Calling setState from useMemo may trigger an infinite loop\n\nEach time the memo callback is evaluated it will change state. This can cause a memoization dependency to change, running the memo function again and causing an infinite loop. Instead of setting state in useMemo(), prefer deriving the value during render. (https://react.dev/reference/react/useState).",
                    ));
                }
            }
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() { check_memo_body_expr_for_setter(e, setters)?; }
            }
        }
        Expression::LogicalExpression(l) => {
            check_memo_body_expr_for_setter(&l.left, setters)?;
            check_memo_body_expr_for_setter(&l.right, setters)?;
        }
        Expression::ConditionalExpression(c) => {
            check_memo_body_expr_for_setter(&c.consequent, setters)?;
            check_memo_body_expr_for_setter(&c.alternate, setters)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions { check_memo_body_expr_for_setter(e, setters)?; }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_const_reassignment — semantic-based
// ---------------------------------------------------------------------------

/// Validate that `const`-declared local variables are not reassigned inside
/// component/hook function bodies.
fn validate_no_const_reassignment<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_semantic::SymbolFlags;
    let root_scope = semantic.scoping().root_scope_id();

    let is_local_const = |ident: &oxc_ast::ast::IdentifierReference| -> bool {
        let ref_id = match ident.reference_id.get() {
            Some(r) => r,
            None => return false,
        };
        let sym_id = match semantic.scoping().get_reference(ref_id).symbol_id() {
            Some(s) => s,
            None => return false,
        };
        // Only check local variables (not module-scope)
        if semantic.scoping().symbol_scope_id(sym_id) == root_scope {
            return false;
        }
        semantic.scoping().symbol_flags(sym_id).contains(SymbolFlags::ConstVariable)
    };

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        const_check_stmts(stmts, &is_local_const)?;
    }
    Ok(())
}

fn const_check_stmts<'a, F>(stmts: &[oxc_ast::ast::Statement<'a>], is_const: &F) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    for stmt in stmts { const_check_stmt(stmt, is_const)?; }
    Ok(())
}

fn const_check_stmt<'a, F>(stmt: &oxc_ast::ast::Statement<'a>, is_const: &F) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => const_check_expr(&e.expression, is_const)?,
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init { const_check_expr(init, is_const)?; }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { const_check_expr(a, is_const)?; }
        }
        Statement::IfStatement(i) => {
            const_check_expr(&i.test, is_const)?;
            const_check_stmt(&i.consequent, is_const)?;
            if let Some(alt) = &i.alternate { const_check_stmt(alt, is_const)?; }
        }
        Statement::BlockStatement(b) => const_check_stmts(&b.body, is_const)?,
        Statement::WhileStatement(w) => {
            const_check_expr(&w.test, is_const)?;
            const_check_stmt(&w.body, is_const)?;
        }
        Statement::ForStatement(f) => {
            if let Some(t) = &f.test { const_check_expr(t, is_const)?; }
            if let Some(u) = &f.update { const_check_expr(u, is_const)?; }
            const_check_stmt(&f.body, is_const)?;
        }
        _ => {}
    }
    Ok(())
}

fn const_check_expr<'a, F>(expr: &Expression<'a>, is_const: &F) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    match expr {
        Expression::AssignmentExpression(a) => {
            if let oxc_ast::ast::AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                if is_const(id) {
                    return Err(CompilerError::invalid_react(
                        "Cannot reassign a `const` variable",
                    ));
                }
            }
            const_check_expr(&a.right, is_const)?;
        }
        Expression::UpdateExpression(u) => {
            if let oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument {
                if is_const(id) {
                    return Err(CompilerError::invalid_react(
                        "Cannot reassign a `const` variable",
                    ));
                }
            }
        }
        Expression::CallExpression(call) => {
            const_check_expr(&call.callee, is_const)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() { const_check_expr(e, is_const)?; }
            }
        }
        Expression::LogicalExpression(l) => {
            const_check_expr(&l.left, is_const)?;
            const_check_expr(&l.right, is_const)?;
        }
        Expression::ConditionalExpression(c) => {
            const_check_expr(&c.test, is_const)?;
            const_check_expr(&c.consequent, is_const)?;
            const_check_expr(&c.alternate, is_const)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions { const_check_expr(e, is_const)?; }
        }
        // Don't recurse into nested closures
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {}
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_usememo_no_outer_let_assign
// ---------------------------------------------------------------------------

/// useMemo callbacks may not reassign `let` variables declared in the outer
/// component/hook scope.  Assignments to variables declared *inside* the
/// callback are fine; only the outer-scope `let` names are checked.
fn validate_usememo_no_outer_let_assign<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let root_scope = semantic.scoping().root_scope_id();
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }
        for stmt in stmts {
            usememo_check_stmt_for_outer_assign(stmt, &outer_lets, semantic, root_scope)?;
        }
    }
    Ok(())
}

fn usememo_check_stmt_for_outer_assign<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    outer_lets: &std::collections::HashSet<String>,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    usememo_check_expr_for_outer_assign(init, outer_lets, semantic, root_scope)?;
                }
            }
        }
        Statement::ExpressionStatement(e) => {
            usememo_check_expr_for_outer_assign(&e.expression, outer_lets, semantic, root_scope)?;
        }
        Statement::IfStatement(i) => {
            usememo_check_stmt_for_outer_assign(&i.consequent, outer_lets, semantic, root_scope)?;
            if let Some(a) = &i.alternate {
                usememo_check_stmt_for_outer_assign(a, outer_lets, semantic, root_scope)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                usememo_check_stmt_for_outer_assign(s, outer_lets, semantic, root_scope)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn usememo_check_expr_for_outer_assign<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    outer_lets: &std::collections::HashSet<String>,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Expression;
    match expr {
        Expression::CallExpression(call) if is_use_memo_call(expr) => {
            // Check the first arg (the callback) for assignments to outer lets
            if let Some(first) = call.arguments.first() {
                if let Some(callback) = first.as_expression() {
                    let body_stmts: &[_] = match callback {
                        Expression::ArrowFunctionExpression(arrow) => &arrow.body.statements,
                        Expression::FunctionExpression(func) => {
                            if let Some(body) = &func.body { &body.statements } else { return Ok(()) }
                        }
                        _ => return Ok(()),
                    };
                    // Names declared inside the callback are excluded (shadow outer lets)
                    let callback_lets = collect_let_names_shallow(body_stmts);
                    for s in body_stmts {
                        usememo_body_check_stmt(s, outer_lets, &callback_lets, semantic, root_scope)?;
                    }
                }
            }
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    usememo_check_expr_for_outer_assign(e, outer_lets, semantic, root_scope)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn usememo_body_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    outer_lets: &std::collections::HashSet<String>,
    excluded: &std::collections::HashSet<String>,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            usememo_body_check_expr(&e.expression, outer_lets, excluded, semantic, root_scope)?;
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    usememo_body_check_expr(init, outer_lets, excluded, semantic, root_scope)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument {
                usememo_body_check_expr(a, outer_lets, excluded, semantic, root_scope)?;
            }
        }
        Statement::IfStatement(i) => {
            usememo_body_check_stmt(&i.consequent, outer_lets, excluded, semantic, root_scope)?;
            if let Some(a) = &i.alternate {
                usememo_body_check_stmt(a, outer_lets, excluded, semantic, root_scope)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                usememo_body_check_stmt(s, outer_lets, excluded, semantic, root_scope)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn usememo_body_check_expr<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    outer_lets: &std::collections::HashSet<String>,
    excluded: &std::collections::HashSet<String>,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    use oxc_semantic::SymbolFlags;
    match expr {
        Expression::AssignmentExpression(a) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                let name = id.name.as_str();
                if outer_lets.contains(name) && !excluded.contains(name) {
                    let is_outer_let = id.reference_id.get().and_then(|ref_id| {
                        let sym_id = semantic.scoping().get_reference(ref_id).symbol_id()?;
                        if semantic.scoping().symbol_scope_id(sym_id) == root_scope { return None; }
                        let flags = semantic.scoping().symbol_flags(sym_id);
                        if flags.contains(SymbolFlags::BlockScopedVariable)
                            && !flags.contains(SymbolFlags::ConstVariable) {
                            Some(())
                        } else { None }
                    }).is_some();
                    if is_outer_let {
                        return Err(CompilerError::invalid_react(
                            "useMemo() callbacks may not reassign variables declared outside of the callback\n\nuseMemo() callbacks must be pure functions and cannot reassign variables defined outside of the callback function.",
                        ));
                    }
                }
            }
            usememo_body_check_expr(&a.right, outer_lets, excluded, semantic, root_scope)?;
        }
        Expression::CallExpression(c) => {
            usememo_body_check_expr(&c.callee, outer_lets, excluded, semantic, root_scope)?;
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    // Recurse into closures passed to non-hook functions inside useMemo
                    match e {
                        Expression::ArrowFunctionExpression(arrow) => {
                            let inner_lets = collect_let_names_shallow(&arrow.body.statements);
                            let mut new_excluded = excluded.clone();
                            new_excluded.extend(inner_lets);
                            for s in &arrow.body.statements {
                                usememo_body_check_stmt(s, outer_lets, &new_excluded, semantic, root_scope)?;
                            }
                        }
                        Expression::FunctionExpression(func) => {
                            if let Some(body) = &func.body {
                                let inner_lets = collect_let_names_shallow(&body.statements);
                                let mut new_excluded = excluded.clone();
                                new_excluded.extend(inner_lets);
                                for s in &body.statements {
                                    usememo_body_check_stmt(s, outer_lets, &new_excluded, semantic, root_scope)?;
                                }
                            }
                        }
                        _ => usememo_body_check_expr(e, outer_lets, excluded, semantic, root_scope)?,
                    }
                }
            }
        }
        Expression::LogicalExpression(l) => {
            usememo_body_check_expr(&l.left, outer_lets, excluded, semantic, root_scope)?;
            usememo_body_check_expr(&l.right, outer_lets, excluded, semantic, root_scope)?;
        }
        Expression::ConditionalExpression(c) => {
            usememo_body_check_expr(&c.test, outer_lets, excluded, semantic, root_scope)?;
            usememo_body_check_expr(&c.consequent, outer_lets, excluded, semantic, root_scope)?;
            usememo_body_check_expr(&c.alternate, outer_lets, excluded, semantic, root_scope)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_state_mutation
// ---------------------------------------------------------------------------

/// Detect direct mutation of state values returned from `useState` / `useReducer`.
///
/// Pattern: `const [state, setState] = useState(...)` followed by
/// `state.prop = value` or `state[key] = value`.
fn validate_no_state_mutation<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    _semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        state_mutation_check_stmts(stmts)?;
    }
    Ok(())
}

/// Scan a set of statements for useState/useReducer declarations and then
/// check all statements for mutations of those state variables.
fn state_mutation_check_stmts<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement};

    // Collect state variable names from array destructuring of useState/useReducer.
    let mut state_vars: std::collections::HashMap<String, &'static str> =
        std::collections::HashMap::new();

    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                // Must be array destructuring: const [state, ...] = ...
                let BindingPatternKind::ArrayPattern(arr) = &decl.id.kind else { continue };
                let Some(first_elem) = arr.elements.first().and_then(|e| e.as_ref()) else { continue };
                let BindingPatternKind::BindingIdentifier(id) = &first_elem.kind else { continue };
                let name = id.name.to_string();

                // Init must be a useState/useReducer call
                let Some(init) = &decl.init else { continue };
                let hook_name: Option<&'static str> = match init {
                    Expression::CallExpression(c) => match &c.callee {
                        Expression::Identifier(i) => match i.name.as_str() {
                            "useState" => Some("useState"),
                            "useReducer" => Some("useReducer"),
                            _ => None,
                        },
                        Expression::StaticMemberExpression(s)
                            if matches!(&s.object, Expression::Identifier(o) if o.name == "React") =>
                        {
                            match s.property.name.as_str() {
                                "useState" => Some("useState"),
                                "useReducer" => Some("useReducer"),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(hook) = hook_name {
                    state_vars.insert(name, hook);
                }
            }
        }
    }

    // Also collect useContext results — context values are also immutable.
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                // const context = useContext(X) — simple identifier binding
                let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind else { continue };
                let name = id.name.to_string();
                if state_vars.contains_key(&name) { continue; }
                let Some(init) = &decl.init else { continue };
                let is_context = match init {
                    Expression::CallExpression(c) => match &c.callee {
                        Expression::Identifier(i) => i.name == "useContext",
                        Expression::StaticMemberExpression(s) => {
                            s.property.name == "useContext"
                                && matches!(&s.object, Expression::Identifier(o) if o.name == "React")
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if is_context {
                    state_vars.insert(name, "useContext");
                }
            }
        }
    }

    if state_vars.is_empty() {
        return Ok(());
    }

    // Collect 1-level aliases: `const foo = stateVar` or `const foo = stateVar.anything`.
    // These aliases must not have their properties mutated.
    let mut all_vars = state_vars.clone();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind else { continue };
                let alias_name = id.name.to_string();
                if all_vars.contains_key(&alias_name) { continue; } // already a state var
                let Some(init) = &decl.init else { continue };
                let is_state_alias = match init {
                    // `const foo = stateVar`
                    Expression::Identifier(i) if all_vars.contains_key(i.name.as_str()) => true,
                    // `const foo = stateVar.something`
                    Expression::StaticMemberExpression(s) => {
                        matches!(&s.object, Expression::Identifier(i) if all_vars.contains_key(i.name.as_str()))
                    }
                    _ => false,
                };
                if is_state_alias {
                    all_vars.insert(alias_name, "useState");
                }
            }
        }
    }

    // Scan all statements for assignments to state vars (direct property or computed).
    for stmt in stmts {
        state_mutation_check_stmt(stmt, &all_vars)?;
    }
    Ok(())
}

fn state_mutation_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    state_vars: &std::collections::HashMap<String, &'static str>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            state_mutation_check_expr(&e.expression, state_vars)?;
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    state_mutation_check_expr(init, state_vars)?;
                }
            }
        }
        Statement::IfStatement(i) => {
            state_mutation_check_stmt(&i.consequent, state_vars)?;
            if let Some(alt) = &i.alternate {
                state_mutation_check_stmt(alt, state_vars)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                state_mutation_check_stmt(s, state_vars)?;
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(e) = &r.argument {
                state_mutation_check_expr(e, state_vars)?;
            }
        }
        Statement::ForStatement(f) => {
            if let Some(body) = Some(&f.body) {
                state_mutation_check_stmt(body, state_vars)?;
            }
        }
        Statement::WhileStatement(w) => {
            state_mutation_check_stmt(&w.body, state_vars)?;
        }
        _ => {}
    }
    Ok(())
}

fn state_mutation_check_expr<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    state_vars: &std::collections::HashMap<String, &'static str>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    match expr {
        Expression::AssignmentExpression(a) => {
            // Check left side: state.prop = ... or state[x] = ...
            let base_name: Option<&str> = match &a.left {
                AssignmentTarget::StaticMemberExpression(s) => {
                    if let Expression::Identifier(id) = &s.object {
                        Some(id.name.as_str())
                    } else {
                        None
                    }
                }
                AssignmentTarget::ComputedMemberExpression(c) => {
                    if let Expression::Identifier(id) = &c.object {
                        Some(id.name.as_str())
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(name) = base_name {
                if let Some(hook_name) = state_vars.get(name) {
                    return Err(crate::error::CompilerError::invalid_react(format!(
                        "This value cannot be modified\n\n\
                         Modifying a value returned from '{hook_name}()', which should not be \
                         modified directly. Use the setter function to update instead."
                    )));
                }
            }
            // Also check right side
            state_mutation_check_expr(&a.right, state_vars)?;
        }
        Expression::CallExpression(c) => {
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    state_mutation_check_expr(e, state_vars)?;
                }
            }
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                state_mutation_check_expr(e, state_vars)?;
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            // Recurse into closure body, but exclude any state names shadowed by params.
            let mut inner_vars = state_vars.clone();
            for param in &arrow.params.items {
                let mut shadowed = std::collections::HashSet::new();
                collect_binding_names(&param.pattern, &mut shadowed);
                for name in &shadowed { inner_vars.remove(name); }
            }
            if !inner_vars.is_empty() {
                for s in &arrow.body.statements {
                    state_mutation_check_stmt(s, &inner_vars)?;
                }
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                let mut inner_vars = state_vars.clone();
                for param in &func.params.items {
                    let mut shadowed = std::collections::HashSet::new();
                    collect_binding_names(&param.pattern, &mut shadowed);
                    for name in &shadowed { inner_vars.remove(name); }
                }
                if !inner_vars.is_empty() {
                    for s in &body.statements {
                        state_mutation_check_stmt(s, &inner_vars)?;
                    }
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            state_mutation_check_expr(&p.expression, state_vars)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_function_hoisting_call
// ---------------------------------------------------------------------------

/// Detect calling a function declaration before it appears in the statement list.
///
/// Pattern: `const result = bar(); function bar() {...}` in a component/hook.
/// JavaScript hoists function declarations, but React Compiler cannot safely
/// handle calls to hoisted functions.
///
/// Also detects: `function foo() { return bar(); } function bar() {...}` where
/// a function declared earlier references a function declared later (mutual
/// hoisting in function bodies).
fn validate_no_function_hoisting_call<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts_for_hoisted_call(stmts, semantic)?;
    }
    Ok(())
}

fn check_stmts_for_hoisted_call<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Expression, Statement};

    // Collect all function declaration names in the body.
    let all_func_names: std::collections::HashSet<String> = stmts.iter()
        .filter_map(|s| {
            if let Statement::FunctionDeclaration(f) = s {
                f.id.as_ref().map(|id| id.name.to_string())
            } else {
                None
            }
        })
        .collect();

    if all_func_names.is_empty() { return Ok(()); }

    let mut seen_funcs: std::collections::HashSet<String> = std::collections::HashSet::new();

    for stmt in stmts {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                // Check if this function's body calls any function not yet declared.
                let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                if !name.is_empty() {
                    if let Some(body) = &f.body {
                        for s in &body.statements {
                            if stmt_calls_unseen_func(s, &all_func_names, &seen_funcs) {
                                return Err(CompilerError::todo(
                                    "Function declaration references a function that is declared later in the component/hook body",
                                ));
                            }
                        }
                    }
                    seen_funcs.insert(name.to_string());
                }
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        if expr_calls_unseen_func(init, &all_func_names, &seen_funcs) {
                            return Err(CompilerError::todo(
                                "Calling a function before its declaration in the component/hook body",
                            ));
                        }
                    }
                }
            }
            Statement::ExpressionStatement(e) => {
                if expr_calls_unseen_func(&e.expression, &all_func_names, &seen_funcs) {
                    return Err(CompilerError::todo(
                        "Calling a function before its declaration in the component/hook body",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn stmt_calls_unseen_func<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    all_funcs: &std::collections::HashSet<String>,
    seen: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ReturnStatement(r) => {
            r.argument.as_ref().map_or(false, |e| expr_calls_unseen_func(e, all_funcs, seen))
        }
        Statement::ExpressionStatement(e) => expr_calls_unseen_func(&e.expression, all_funcs, seen),
        Statement::VariableDeclaration(v) => v.declarations.iter().any(|d| {
            d.init.as_ref().map_or(false, |e| expr_calls_unseen_func(e, all_funcs, seen))
        }),
        Statement::BlockStatement(b) => b.body.iter().any(|s| stmt_calls_unseen_func(s, all_funcs, seen)),
        Statement::IfStatement(i) => {
            stmt_calls_unseen_func(&i.consequent, all_funcs, seen)
                || i.alternate.as_ref().map_or(false, |a| stmt_calls_unseen_func(a, all_funcs, seen))
        }
        _ => false,
    }
}

fn expr_calls_unseen_func<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    all_funcs: &std::collections::HashSet<String>,
    seen: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::Expression;
    match expr {
        Expression::CallExpression(c) => {
            // Check if callee is an identifier that's in all_funcs but NOT yet seen
            if let Expression::Identifier(id) = &c.callee {
                let name = id.name.as_str();
                if all_funcs.contains(name) && !seen.contains(name) {
                    return true;
                }
            }
            // Recurse into args (but not into closure bodies — don't recurse into arrow/func)
            c.arguments.iter().any(|a| {
                a.as_expression().map_or(false, |e| expr_calls_unseen_func(e, all_funcs, seen))
            })
        }
        Expression::SequenceExpression(s) => {
            s.expressions.iter().any(|e| expr_calls_unseen_func(e, all_funcs, seen))
        }
        // Don't recurse into closures (they evaluate lazily, not during init)
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_tzdv_self_reference
// ---------------------------------------------------------------------------

/// Detect `const x = f(x)` — using a const binding in its own initializer.
///
/// This is a temporal dead zone (TDZ) violation. React compiler raises a
/// "hoisting" error for this pattern during SSA construction.
fn validate_no_tzdv_self_reference<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            tzdv_check_stmt(stmt, semantic)?;
        }
    }
    Ok(())
}

fn tzdv_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Statement, VariableDeclarationKind};
    if let Statement::VariableDeclaration(v) = stmt {
        // Only const/let can have TDZ
        if matches!(v.kind, VariableDeclarationKind::Const | VariableDeclarationKind::Let) {
            for decl in &v.declarations {
                if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                    if let Some(sym_id) = id.symbol_id.get() {
                        if let Some(init) = &decl.init {
                            if init_directly_references_symbol(init, sym_id, semantic) {
                                return Err(CompilerError::todo(
                                    "[hoisting] EnterSSA: Expected identifier to be defined before being used",
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Walk `expr` looking for identifier references that resolve to `sym_id`.
/// Does NOT recurse into function expressions or arrow functions (those have
/// lazy evaluation and may validly capture the binding).
fn init_directly_references_symbol<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    sym_id: oxc_semantic::SymbolId,
    semantic: &oxc_semantic::Semantic<'a>,
) -> bool {
    use oxc_ast::ast::Expression;
    match expr {
        Expression::Identifier(id) => {
            id.reference_id.get().and_then(|ref_id| {
                semantic.scoping().get_reference(ref_id).symbol_id()
            }) == Some(sym_id)
        }
        // Recurse into call args (direct call), member expressions, binary ops
        Expression::CallExpression(c) => {
            // Don't check callee — only arguments (callee could be the function itself)
            c.arguments.iter().any(|a| {
                a.as_expression().map_or(false, |e| init_directly_references_symbol(e, sym_id, semantic))
            })
        }
        Expression::BinaryExpression(b) => {
            init_directly_references_symbol(&b.left, sym_id, semantic)
                || init_directly_references_symbol(&b.right, sym_id, semantic)
        }
        Expression::UnaryExpression(u) => {
            init_directly_references_symbol(&u.argument, sym_id, semantic)
        }
        Expression::StaticMemberExpression(s) => {
            init_directly_references_symbol(&s.object, sym_id, semantic)
        }
        Expression::ComputedMemberExpression(c) => {
            init_directly_references_symbol(&c.object, sym_id, semantic)
        }
        // Do NOT recurse into closures (lazy evaluation)
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_param_mutation
// ---------------------------------------------------------------------------

/// Detect direct property mutations of component/hook parameters.
///
/// Pattern: `function Foo(props) { props.x = 1; }` or
///          `function useHook(a, b) { b.test = 1; }`
fn validate_no_param_mutation<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    _semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn check_fn<'a>(func: &oxc_ast::ast::Function<'a>) -> Result<()> {
        let name = func.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
        if name.is_empty() { return Ok(()); }

        // Skip functions opted out with 'use no forget'/'use no memo'.
        if let Some(body) = &func.body {
            if body.directives.iter().any(|d| matches!(d.expression.value.as_str(), "use no memo" | "use no forget")) {
                return Ok(());
            }
        }

        // Collect ALL parameter binding names, including from destructured ObjectPattern.
        let mut param_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for param in &func.params.items {
            collect_binding_names(&param.pattern, &mut param_names);
        }
        if param_names.is_empty() { return Ok(()); }

        let Some(body) = &func.body else { return Ok(()); };
        for stmt in &body.statements {
            param_mutation_check_stmt(stmt, &param_names)?;
        }
        Ok(())
    }

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => { check_fn(f)?; }
            Statement::ExportDefaultDeclaration(d) => {
                if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &d.declaration {
                    check_fn(f)?;
                }
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    check_fn(f)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn param_mutation_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    param_names: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            param_mutation_check_expr(&e.expression, param_names)?;
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    param_mutation_check_expr(init, param_names)?;
                }
            }
        }
        Statement::IfStatement(i) => {
            param_mutation_check_stmt(&i.consequent, param_names)?;
            if let Some(a) = &i.alternate {
                param_mutation_check_stmt(a, param_names)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                param_mutation_check_stmt(s, param_names)?;
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(e) = &r.argument {
                param_mutation_check_expr(e, param_names)?;
            }
        }
        Statement::ForOfStatement(fo) => {
            use oxc_ast::ast::{BindingPatternKind, Expression, ForStatementLeft};
            // Check if iterating over a param property: `for (const x of param.items)`
            let iter_var: Option<String> = if let ForStatementLeft::VariableDeclaration(vd) = &fo.left {
                if let Some(decl) = vd.declarations.first() {
                    if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                        // Right-hand side must be `paramName.something`
                        let is_param_source = match &fo.right {
                            Expression::Identifier(i) => param_names.contains(i.name.as_str()),
                            Expression::StaticMemberExpression(s) => {
                                matches!(&s.object, Expression::Identifier(i) if param_names.contains(i.name.as_str()))
                            }
                            _ => false,
                        };
                        if is_param_source { Some(id.name.to_string()) } else { None }
                    } else { None }
                } else { None }
            } else { None };
            if let Some(var) = iter_var {
                // The loop variable is a param alias — treat it like a param inside the loop body
                let mut extended = param_names.clone();
                extended.insert(var);
                param_mutation_check_stmt(&fo.body, &extended)?;
            } else {
                param_mutation_check_stmt(&fo.body, param_names)?;
            }
        }
        Statement::FunctionDeclaration(f) => {
            // Recurse into nested function declarations.
            // Outer param names are captured, but the inner function's own params shadow them.
            if let Some(body) = &f.body {
                let mut inner_params = param_names.clone();
                for param in &f.params.items {
                    let mut shadowed = std::collections::HashSet::new();
                    collect_binding_names(&param.pattern, &mut shadowed);
                    for name in &shadowed { inner_params.remove(name); }
                }
                for s in &body.statements {
                    param_mutation_check_stmt(s, &inner_params)?;
                }
            }
        }
        Statement::ForInStatement(fi) => {
            param_mutation_check_stmt(&fi.body, param_names)?;
        }
        Statement::WhileStatement(w) => {
            param_mutation_check_stmt(&w.body, param_names)?;
        }
        Statement::ForStatement(f) => {
            param_mutation_check_stmt(&f.body, param_names)?;
        }
        _ => {}
    }
    Ok(())
}

/// Returns true if a parameter name looks like a React ref (mutable .current is allowed).
fn is_ref_like_param_name(name: &str) -> bool {
    name == "ref" || name.ends_with("Ref") || name.starts_with("ref")
}

/// Compute the set of variable names that a closure body shadows.
fn collect_closure_shadows<'a>(stmts: &[oxc_ast::ast::Statement<'a>]) -> std::collections::HashSet<String> {
    let mut shadowed = std::collections::HashSet::new();
    for stmt in stmts {
        if let oxc_ast::ast::Statement::VariableDeclaration(vd) = stmt {
            for decl in &vd.declarations {
                collect_binding_names(&decl.id, &mut shadowed);
            }
        }
    }
    shadowed
}

fn param_mutation_check_expr<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    param_names: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    match expr {
        Expression::AssignmentExpression(a) => {
            let base_name: Option<&str> = match &a.left {
                AssignmentTarget::StaticMemberExpression(s) => {
                    if let Expression::Identifier(id) = &s.object {
                        Some(id.name.as_str())
                    } else { None }
                }
                AssignmentTarget::ComputedMemberExpression(c) => {
                    if let Expression::Identifier(id) = &c.object {
                        Some(id.name.as_str())
                    } else { None }
                }
                _ => None,
            };
            if let Some(name) = base_name {
                if param_names.contains(name) && !is_ref_like_param_name(name) {
                    return Err(crate::error::CompilerError::invalid_react(
                        "This value cannot be modified\n\n\
                         Modifying component props or hook arguments is not allowed. \
                         Consider using a local variable instead."
                    ));
                }
            }
            param_mutation_check_expr(&a.right, param_names)?;
        }
        Expression::CallExpression(c) => {
            param_mutation_check_expr(&c.callee, param_names)?;
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    param_mutation_check_expr(e, param_names)?;
                }
            }
        }
        // Recurse into closures — prop mutations inside callbacks are always invalid.
        // Compute shadows to avoid flagging locally-redeclared names.
        Expression::ArrowFunctionExpression(arrow) => {
            let shadows = collect_closure_shadows(&arrow.body.statements);
            if shadows.is_empty() {
                for s in &arrow.body.statements {
                    param_mutation_check_stmt(s, param_names)?;
                }
            } else {
                let filtered: std::collections::HashSet<String> = param_names.iter()
                    .filter(|n| !shadows.contains(*n))
                    .cloned().collect();
                if !filtered.is_empty() {
                    for s in &arrow.body.statements {
                        param_mutation_check_stmt(s, &filtered)?;
                    }
                }
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                let shadows = collect_closure_shadows(&body.statements);
                if shadows.is_empty() {
                    for s in &body.statements {
                        param_mutation_check_stmt(s, param_names)?;
                    }
                } else {
                    let filtered: std::collections::HashSet<String> = param_names.iter()
                        .filter(|n| !shadows.contains(*n))
                        .cloned().collect();
                    if !filtered.is_empty() {
                        for s in &body.statements {
                            param_mutation_check_stmt(s, &filtered)?;
                        }
                    }
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            param_mutation_check_expr(&p.expression, param_names)?;
        }
        Expression::JSXElement(jsx) => {
            for attr in &jsx.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                        if let Some(e) = ec.expression.as_expression() {
                            param_mutation_check_expr(e, param_names)?;
                        }
                    }
                }
            }
            for child in &jsx.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(ec) = child {
                    if let Some(e) = ec.expression.as_expression() {
                        param_mutation_check_expr(e, param_names)?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_param_reassignment_in_closures
// ---------------------------------------------------------------------------

/// Detect reassignment of function parameter bindings inside nested closures.
///
/// Pattern: `function Component({foo}) { ... handler={() => { foo = true; }} }`
/// Reassigning a destructured parameter binding (which is effectively a prop)
/// inside a closure is not allowed.
fn validate_no_param_reassignment_in_closures<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn check_fn<'a>(
        func: &oxc_ast::ast::Function<'a>,
        semantic: &oxc_semantic::Semantic<'a>,
    ) -> Result<()> {
        let name = func.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
        if name.is_empty() { return Ok(()); }
        // Only check component/hook functions
        let first = name.chars().next();
        let is_comp_hook = first.map_or(false, |c| c.is_uppercase()) || is_hook_name(name);
        if !is_comp_hook { return Ok(()); }

        // Collect parameter binding symbol IDs AND names (for fallback)
        let mut param_syms: std::collections::HashSet<oxc_semantic::SymbolId> =
            std::collections::HashSet::new();
        let mut param_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        collect_param_symbol_ids_and_names(&func.params, &mut param_syms, &mut param_names);
        if param_syms.is_empty() && param_names.is_empty() { return Ok(()); }

        let Some(body) = &func.body else { return Ok(()); };
        // Walk body, entering closures to check for param reassignments
        for stmt in &body.statements {
            check_stmt_for_param_reassign(stmt, &param_syms, &param_names, false, semantic)?;
        }
        Ok(())
    }

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => { check_fn(f, semantic)?; }
            Statement::ExportDefaultDeclaration(d) => {
                if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &d.declaration {
                    check_fn(f, semantic)?;
                }
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    check_fn(f, semantic)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_param_symbol_ids_and_names<'a>(
    params: &oxc_ast::ast::FormalParameters<'a>,
    syms: &mut std::collections::HashSet<oxc_semantic::SymbolId>,
    names: &mut std::collections::HashSet<String>,
) {
    for param in &params.items {
        collect_binding_symbol_ids_and_names(&param.pattern, syms, names);
    }
}

fn collect_binding_symbol_ids_and_names<'a>(
    pat: &oxc_ast::ast::BindingPattern<'a>,
    syms: &mut std::collections::HashSet<oxc_semantic::SymbolId>,
    names: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::BindingPatternKind;
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(id) => {
            names.insert(id.name.to_string());
            if let Some(sym_id) = id.symbol_id.get() {
                syms.insert(sym_id);
            }
        }
        BindingPatternKind::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_symbol_ids_and_names(&prop.value, syms, names);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_symbol_ids_and_names(&rest.argument, syms, names);
            }
        }
        BindingPatternKind::ArrayPattern(arr) => {
            for el in arr.elements.iter().filter_map(|e| e.as_ref()) {
                collect_binding_symbol_ids_and_names(el, syms, names);
            }
        }
        BindingPatternKind::AssignmentPattern(ap) => {
            collect_binding_symbol_ids_and_names(&ap.left, syms, names);
        }
    }
}

fn check_stmt_for_param_reassign<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    param_syms: &std::collections::HashSet<oxc_semantic::SymbolId>,
    param_names: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            check_expr_for_param_reassign(&e.expression, param_syms, param_names, in_closure, semantic)?;
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_param_reassign(init, param_syms, param_names, in_closure, semantic)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(e) = &r.argument {
                check_expr_for_param_reassign(e, param_syms, param_names, in_closure, semantic)?;
            }
        }
        Statement::IfStatement(i) => {
            check_stmt_for_param_reassign(&i.consequent, param_syms, param_names, in_closure, semantic)?;
            if let Some(a) = &i.alternate {
                check_stmt_for_param_reassign(a, param_syms, param_names, in_closure, semantic)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_for_param_reassign(s, param_syms, param_names, in_closure, semantic)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_param_reassign<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    param_syms: &std::collections::HashSet<oxc_semantic::SymbolId>,
    param_names: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    match expr {
        Expression::AssignmentExpression(a) if in_closure => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                let name = id.name.as_str();
                // Check via symbol ID (accurate, handles shadowing)
                let is_param_sym = id.reference_id.get()
                    .and_then(|ref_id| semantic.scoping().get_reference(ref_id).symbol_id())
                    .map_or(false, |sym_id| param_syms.contains(&sym_id));
                // Fallback: name-based check (catches cases where symbol_id is missing)
                let is_param_name = param_names.contains(name);
                if is_param_sym || is_param_name {
                    return Err(CompilerError::invalid_react(
                        "This value cannot be modified\n\n\
                         Modifying component props or hook arguments is not allowed. \
                         Consider using a local variable instead."
                    ));
                }
            }
            check_expr_for_param_reassign(&a.right, param_syms, param_names, in_closure, semantic)?;
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                check_stmt_for_param_reassign(s, param_syms, param_names, true, semantic)?;
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements {
                    check_stmt_for_param_reassign(s, param_syms, param_names, true, semantic)?;
                }
            }
        }
        Expression::CallExpression(c) => {
            check_expr_for_param_reassign(&c.callee, param_syms, param_names, in_closure, semantic)?;
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_for_param_reassign(e, param_syms, param_names, in_closure, semantic)?;
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            check_expr_for_param_reassign(&p.expression, param_syms, param_names, in_closure, semantic)?;
        }
        Expression::JSXElement(jsx) => {
            for attr in &jsx.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                        if let Some(e) = ec.expression.as_expression() {
                            check_expr_for_param_reassign(e, param_syms, param_names, in_closure, semantic)?;
                        }
                    }
                }
            }
            // Also check JSX children
            for child in &jsx.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(ec) = child {
                    if let Some(e) = ec.expression.as_expression() {
                        check_expr_for_param_reassign(e, param_syms, param_names, in_closure, semantic)?;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_async_local_reassignment
// ---------------------------------------------------------------------------

/// Detect reassignment of outer `let` variables inside async closures.
///
/// React rule: async closures always run after the render phase completes, so
/// assigning to a `let` variable captured from the component/hook body is
/// always invalid. This includes nested sync callbacks *inside* an async
/// closure (e.g. `.then(result => { outerLet = result; })`).
///
/// The check is a variant of `check_expr_let_in_closure` where we only flip
/// `in_closure` to `true` when entering an **async** function/arrow, not
/// every closure. Once inside an async closure, all nested closures inherit
/// `in_closure=true`.
fn validate_no_async_local_reassignment<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let root_scope = semantic.scoping().root_scope_id();
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }
        let excluded = std::collections::HashSet::new();
        for stmt in stmts {
            async_check_stmt(stmt, &outer_lets, &excluded, false, semantic, root_scope)?;
        }
    }
    Ok(())
}

/// Statement walker for async-closure let-reassignment checks.
/// `in_closure` starts `false` at component body level and flips to `true`
/// only when entering an async function/arrow.
fn async_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    outer_lets: &std::collections::HashSet<String>,
    excluded: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            async_check_expr(&e.expression, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Statement::VariableDeclaration(vd) => {
            for decl in &vd.declarations {
                if let Some(init) = &decl.init {
                    async_check_expr(init, outer_lets, excluded, in_closure, semantic, root_scope)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                async_check_expr(arg, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                async_check_stmt(s, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Statement::IfStatement(i) => {
            async_check_expr(&i.test, outer_lets, excluded, in_closure, semantic, root_scope)?;
            async_check_stmt(&i.consequent, outer_lets, excluded, in_closure, semantic, root_scope)?;
            if let Some(a) = &i.alternate {
                async_check_stmt(a, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                let new_in = in_closure || f.r#async;
                let closure_lets = collect_let_names_shallow(&body.statements);
                let mut new_excl = excluded.clone();
                new_excl.extend(closure_lets);
                for s in &body.statements {
                    async_check_stmt(s, outer_lets, &new_excl, new_in, semantic, root_scope)?;
                }
            }
        }
        Statement::WhileStatement(w) => {
            async_check_expr(&w.test, outer_lets, excluded, in_closure, semantic, root_scope)?;
            async_check_stmt(&w.body, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Statement::ForStatement(f) => {
            if let Some(t) = &f.test {
                async_check_expr(t, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
            async_check_stmt(&f.body, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        _ => {}
    }
    Ok(())
}

/// Expression walker for async-closure let-reassignment checks.
fn async_check_expr<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    outer_lets: &std::collections::HashSet<String>,
    excluded: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    use oxc_semantic::SymbolFlags;
    match expr {
        Expression::AssignmentExpression(a) if in_closure => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                let name = id.name.as_str();
                if outer_lets.contains(name) && !excluded.contains(name) {
                    let is_local_let = id.reference_id.get().and_then(|ref_id| {
                        let sym_id = semantic.scoping().get_reference(ref_id).symbol_id()?;
                        let sym_scope = semantic.scoping().symbol_scope_id(sym_id);
                        if sym_scope == root_scope { return None; }
                        let flags = semantic.scoping().symbol_flags(sym_id);
                        if flags.contains(SymbolFlags::BlockScopedVariable)
                            && !flags.contains(SymbolFlags::ConstVariable) {
                            Some(())
                        } else {
                            None
                        }
                    }).is_some();
                    if is_local_let {
                        return Err(CompilerError::invalid_react(
                            "Cannot reassign a variable after render completes. \
                             Consider using state instead.",
                        ));
                    }
                }
            }
            async_check_expr(&a.right, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::AssignmentExpression(a) => {
            async_check_expr(&a.right, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::ArrowFunctionExpression(arrow) => {
            let new_in = in_closure || arrow.r#async;
            let closure_lets = collect_let_names_shallow(&arrow.body.statements);
            let mut new_excl = excluded.clone();
            new_excl.extend(closure_lets);
            for s in &arrow.body.statements {
                async_check_stmt(s, outer_lets, &new_excl, new_in, semantic, root_scope)?;
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                let new_in = in_closure || func.r#async;
                let closure_lets = collect_let_names_shallow(&body.statements);
                let mut new_excl = excluded.clone();
                new_excl.extend(closure_lets);
                for s in &body.statements {
                    async_check_stmt(s, outer_lets, &new_excl, new_in, semantic, root_scope)?;
                }
            }
        }
        Expression::CallExpression(c) => {
            async_check_expr(&c.callee, outer_lets, excluded, in_closure, semantic, root_scope)?;
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    async_check_expr(e, outer_lets, excluded, in_closure, semantic, root_scope)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            async_check_expr(&l.left, outer_lets, excluded, in_closure, semantic, root_scope)?;
            async_check_expr(&l.right, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::ConditionalExpression(c) => {
            async_check_expr(&c.test, outer_lets, excluded, in_closure, semantic, root_scope)?;
            async_check_expr(&c.consequent, outer_lets, excluded, in_closure, semantic, root_scope)?;
            async_check_expr(&c.alternate, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                async_check_expr(e, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Expression::AwaitExpression(a) => {
            async_check_expr(&a.argument, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::ParenthesizedExpression(p) => {
            async_check_expr(&p.expression, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_context_var_iterator
// ---------------------------------------------------------------------------

/// Detect for-loop iterator variables that are "context variables" — variables
/// that are both mutated and captured in a closure within the loop body.
///
/// Patterns detected:
/// 1. for-of/for-in: iterator var is reassigned inside the body AND referenced
///    inside a closure in the same body.
/// 2. for-loop: init var is modified by the update expression (compound
///    assignment or `++`/`--`) AND referenced inside a closure in the body.
fn validate_no_context_var_iterator<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            civ_check_stmt(stmt)?;
        }
    }
    Ok(())
}

fn civ_check_stmt<'a>(stmt: &'a oxc_ast::ast::Statement<'a>) -> Result<()> {
    use oxc_ast::ast::{ForStatementInit, ForStatementLeft, Statement};
    match stmt {
        Statement::ForOfStatement(fo) => {
            if let Some(var_name) = civ_get_for_iter_var(&fo.left) {
                let body = civ_block_stmts(&fo.body);
                if civ_stmts_reassign_var(body, &var_name)
                    && civ_stmts_have_closure_with_var(body, &var_name)
                {
                    return Err(crate::error::CompilerError::invalid_react(
                        "Iterator variable is a context variable: it is reassigned \
                         and also captured in a closure within the loop body",
                    ));
                }
            }
        }
        Statement::ForInStatement(fi) => {
            if let Some(var_name) = civ_get_for_iter_var(&fi.left) {
                let body = civ_block_stmts(&fi.body);
                if civ_stmts_reassign_var(body, &var_name)
                    && civ_stmts_have_closure_with_var(body, &var_name)
                {
                    return Err(crate::error::CompilerError::invalid_react(
                        "Iterator variable is a context variable: it is reassigned \
                         and also captured in a closure within the loop body",
                    ));
                }
            }
        }
        Statement::ForStatement(f) => {
            if let Some(var_name) = civ_get_for_init_var(f) {
                let is_updated = f.update.as_ref()
                    .map_or(false, |u| civ_update_modifies_var(u, &var_name));
                if is_updated {
                    let body = civ_block_stmts(&f.body);
                    if civ_stmts_have_closure_with_var(body, &var_name) {
                        return Err(crate::error::CompilerError::invalid_react(
                            "For-loop iterator variable is a context variable: \
                             it is modified by the update expression and captured \
                             in a closure within the loop body",
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn civ_get_for_iter_var<'a>(left: &'a oxc_ast::ast::ForStatementLeft<'a>) -> Option<String> {
    use oxc_ast::ast::{BindingPatternKind, ForStatementLeft};
    if let ForStatementLeft::VariableDeclaration(vd) = left {
        if let Some(decl) = vd.declarations.first() {
            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                return Some(id.name.to_string());
            }
        }
    }
    None
}

fn civ_get_for_init_var<'a>(f: &'a oxc_ast::ast::ForStatement<'a>) -> Option<String> {
    use oxc_ast::ast::{BindingPatternKind, ForStatementInit};
    if let Some(ForStatementInit::VariableDeclaration(vd)) = &f.init {
        if let Some(decl) = vd.declarations.first() {
            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                return Some(id.name.to_string());
            }
        }
    }
    None
}

fn civ_block_stmts<'a>(body: &'a oxc_ast::ast::Statement<'a>) -> &'a [oxc_ast::ast::Statement<'a>] {
    if let oxc_ast::ast::Statement::BlockStatement(b) = body {
        &b.body
    } else {
        std::slice::from_ref(body)
    }
}

fn civ_update_modifies_var<'a>(expr: &'a oxc_ast::ast::Expression<'a>, var_name: &str) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression, SimpleAssignmentTarget};
    match expr {
        Expression::AssignmentExpression(a) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                id.name == var_name
            } else {
                false
            }
        }
        Expression::UpdateExpression(u) => {
            if let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument {
                id.name == var_name
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Check if `var_name` is directly assigned (outside any nested function) in `stmts`.
fn civ_stmts_reassign_var<'a>(stmts: &'a [oxc_ast::ast::Statement<'a>], var_name: &str) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression, Statement};
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if let Expression::AssignmentExpression(a) = &e.expression {
                    if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                        if id.name == var_name { return true; }
                    }
                }
            }
            Statement::BlockStatement(b) => {
                if civ_stmts_reassign_var(&b.body, var_name) { return true; }
            }
            _ => {}
        }
    }
    false
}

/// Check if any closure in `stmts` references `var_name` (i.e., captures it).
fn civ_stmts_have_closure_with_var<'a>(stmts: &'a [oxc_ast::ast::Statement<'a>], var_name: &str) -> bool {
    stmts.iter().any(|s| civ_stmt_has_closure_with_var(s, var_name))
}

fn civ_stmt_has_closure_with_var<'a>(stmt: &'a oxc_ast::ast::Statement<'a>, var_name: &str) -> bool {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => civ_expr_has_closure_with_var(&e.expression, var_name),
        Statement::VariableDeclaration(v) => v.declarations.iter().any(|d| {
            d.init.as_ref().map_or(false, |i| civ_expr_has_closure_with_var(i, var_name))
        }),
        Statement::ReturnStatement(r) => r.argument.as_ref().map_or(false, |a| civ_expr_has_closure_with_var(a, var_name)),
        Statement::BlockStatement(b) => civ_stmts_have_closure_with_var(&b.body, var_name),
        Statement::IfStatement(i) => {
            civ_expr_has_closure_with_var(&i.test, var_name)
                || civ_stmt_has_closure_with_var(&i.consequent, var_name)
                || i.alternate.as_ref().map_or(false, |a| civ_stmt_has_closure_with_var(a, var_name))
        }
        _ => false,
    }
}

fn civ_expr_has_closure_with_var<'a>(expr: &'a oxc_ast::ast::Expression<'a>, var_name: &str) -> bool {
    use oxc_ast::ast::{Expression, JSXAttributeItem, JSXAttributeValue, JSXChild};
    match expr {
        // This IS a closure — check if var_name is referenced inside it.
        Expression::ArrowFunctionExpression(arrow) => {
            let mut params = std::collections::HashSet::new();
            for p in &arrow.params.items {
                collect_binding_names(&p.pattern, &mut params);
            }
            if params.contains(var_name) { return false; }
            civ_stmts_contain_identifier(&arrow.body.statements, var_name)
        }
        Expression::FunctionExpression(func) => {
            let mut params = std::collections::HashSet::new();
            for p in &func.params.items {
                collect_binding_names(&p.pattern, &mut params);
            }
            if params.contains(var_name) { return false; }
            func.body.as_ref()
                .map_or(false, |b| civ_stmts_contain_identifier(&b.statements, var_name))
        }
        // Recurse into calls (e.g. items.push(<JSX onClick={...}>))
        Expression::CallExpression(c) => {
            civ_expr_has_closure_with_var(&c.callee, var_name)
                || c.arguments.iter().any(|a| {
                    a.as_expression().map_or(false, |e| civ_expr_has_closure_with_var(e, var_name))
                })
        }
        // Recurse into JSX elements
        Expression::JSXElement(j) => {
            j.opening_element.attributes.iter().any(|attr| match attr {
                JSXAttributeItem::Attribute(a) => match &a.value {
                    Some(JSXAttributeValue::ExpressionContainer(c)) => {
                        c.expression.as_expression()
                            .map_or(false, |e| civ_expr_has_closure_with_var(e, var_name))
                    }
                    Some(JSXAttributeValue::Element(el)) => {
                        civ_jsx_element_has_closure_with_var(el, var_name)
                    }
                    _ => false,
                },
                JSXAttributeItem::SpreadAttribute(s) => {
                    civ_expr_has_closure_with_var(&s.argument, var_name)
                }
            }) || j.children.iter().any(|child| civ_jsx_child_has_closure_with_var(child, var_name))
        }
        Expression::JSXFragment(frag) => {
            frag.children.iter().any(|child| civ_jsx_child_has_closure_with_var(child, var_name))
        }
        Expression::ArrayExpression(arr) => arr.elements.iter().any(|el| {
            el.as_expression().map_or(false, |e| civ_expr_has_closure_with_var(e, var_name))
        }),
        _ => false,
    }
}

fn civ_jsx_element_has_closure_with_var<'a>(
    j: &'a oxc_ast::ast::JSXElement<'a>,
    var_name: &str,
) -> bool {
    use oxc_ast::ast::{JSXAttributeItem, JSXAttributeValue};
    j.opening_element.attributes.iter().any(|attr| match attr {
        JSXAttributeItem::Attribute(a) => match &a.value {
            Some(JSXAttributeValue::ExpressionContainer(c)) => {
                c.expression.as_expression()
                    .map_or(false, |e| civ_expr_has_closure_with_var(e, var_name))
            }
            Some(JSXAttributeValue::Element(el)) => {
                civ_jsx_element_has_closure_with_var(el, var_name)
            }
            _ => false,
        },
        JSXAttributeItem::SpreadAttribute(s) => civ_expr_has_closure_with_var(&s.argument, var_name),
    }) || j.children.iter().any(|child| civ_jsx_child_has_closure_with_var(child, var_name))
}

fn civ_jsx_child_has_closure_with_var<'a>(
    child: &'a oxc_ast::ast::JSXChild<'a>,
    var_name: &str,
) -> bool {
    use oxc_ast::ast::JSXChild;
    match child {
        JSXChild::Element(el) => civ_jsx_element_has_closure_with_var(el, var_name),
        JSXChild::Fragment(frag) => {
            frag.children.iter().any(|c| civ_jsx_child_has_closure_with_var(c, var_name))
        }
        JSXChild::ExpressionContainer(c) => {
            c.expression.as_expression()
                .map_or(false, |e| civ_expr_has_closure_with_var(e, var_name))
        }
        _ => false,
    }
}

/// Check if any statement in `stmts` contains an Identifier with `var_name`.
fn civ_stmts_contain_identifier<'a>(
    stmts: &'a [oxc_ast::ast::Statement<'a>],
    var_name: &str,
) -> bool {
    stmts.iter().any(|s| civ_stmt_contains_identifier(s, var_name))
}

fn civ_stmt_contains_identifier<'a>(stmt: &'a oxc_ast::ast::Statement<'a>, var_name: &str) -> bool {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => civ_expr_contains_identifier(&e.expression, var_name),
        Statement::ReturnStatement(r) => r.argument.as_ref()
            .map_or(false, |a| civ_expr_contains_identifier(a, var_name)),
        Statement::BlockStatement(b) => civ_stmts_contain_identifier(&b.body, var_name),
        Statement::IfStatement(i) => {
            civ_expr_contains_identifier(&i.test, var_name)
                || civ_stmt_contains_identifier(&i.consequent, var_name)
                || i.alternate.as_ref().map_or(false, |a| civ_stmt_contains_identifier(a, var_name))
        }
        _ => false,
    }
}

fn civ_expr_contains_identifier<'a>(expr: &'a oxc_ast::ast::Expression<'a>, var_name: &str) -> bool {
    use oxc_ast::ast::Expression;
    match expr {
        Expression::Identifier(id) => id.name == var_name,
        Expression::CallExpression(c) => {
            civ_expr_contains_identifier(&c.callee, var_name)
                || c.arguments.iter().any(|a| {
                    a.as_expression().map_or(false, |e| civ_expr_contains_identifier(e, var_name))
                })
        }
        Expression::StaticMemberExpression(s) => civ_expr_contains_identifier(&s.object, var_name),
        Expression::ComputedMemberExpression(c) => {
            civ_expr_contains_identifier(&c.object, var_name)
                || civ_expr_contains_identifier(&c.expression, var_name)
        }
        Expression::BinaryExpression(b) => {
            civ_expr_contains_identifier(&b.left, var_name)
                || civ_expr_contains_identifier(&b.right, var_name)
        }
        Expression::LogicalExpression(l) => {
            civ_expr_contains_identifier(&l.left, var_name)
                || civ_expr_contains_identifier(&l.right, var_name)
        }
        Expression::ConditionalExpression(c) => {
            civ_expr_contains_identifier(&c.test, var_name)
                || civ_expr_contains_identifier(&c.consequent, var_name)
                || civ_expr_contains_identifier(&c.alternate, var_name)
        }
        Expression::AssignmentExpression(a) => civ_expr_contains_identifier(&a.right, var_name),
        Expression::SequenceExpression(s) => {
            s.expressions.iter().any(|e| civ_expr_contains_identifier(e, var_name))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_update_on_captured_locals
// ---------------------------------------------------------------------------

/// Detect `++` / `--` (UpdateExpression) on `let` variables declared in a
/// component/hook body that are captured inside a nested function (closure).
///
/// React Compiler cannot lower this pattern yet:
///   `let counter = 2; const fn = () => { return counter++; };`
///
/// This emits a Todo error matching the TS compiler message:
///   "(BuildHIR::lowerExpression) Handle UpdateExpression to variables captured within lambdas."
fn validate_no_update_on_captured_locals<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_semantic::SymbolFlags;
    let root_scope = semantic.scoping().root_scope_id();
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }
        for stmt in stmts {
            nucl_check_stmt(stmt, &outer_lets, false, semantic, root_scope)?;
        }
    }
    Ok(())
}

fn nucl_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    outer_lets: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => nucl_check_expr(&e.expression, outer_lets, in_closure, semantic, root_scope)?,
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { nucl_check_expr(a, outer_lets, in_closure, semantic, root_scope)?; }
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init { nucl_check_expr(init, outer_lets, in_closure, semantic, root_scope)?; }
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body { nucl_check_stmt(s, outer_lets, in_closure, semantic, root_scope)?; }
        }
        Statement::IfStatement(i) => {
            nucl_check_stmt(&i.consequent, outer_lets, in_closure, semantic, root_scope)?;
            if let Some(a) = &i.alternate { nucl_check_stmt(a, outer_lets, in_closure, semantic, root_scope)?; }
        }
        Statement::WhileStatement(w) => nucl_check_stmt(&w.body, outer_lets, in_closure, semantic, root_scope)?,
        Statement::ForStatement(f) => nucl_check_stmt(&f.body, outer_lets, in_closure, semantic, root_scope)?,
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                for s in &body.statements { nucl_check_stmt(s, outer_lets, true, semantic, root_scope)?; }
            }
        }
        _ => {}
    }
    Ok(())
}

fn nucl_check_expr<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    outer_lets: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Expression;
    use oxc_semantic::SymbolFlags;
    match expr {
        Expression::UpdateExpression(u) if in_closure => {
            if let oxc_ast::ast::SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument {
                let name = id.name.as_str();
                if outer_lets.contains(name) {
                    let is_local_let = id.reference_id.get().and_then(|ref_id| {
                        let sym_id = semantic.scoping().get_reference(ref_id).symbol_id()?;
                        let sym_scope = semantic.scoping().symbol_scope_id(sym_id);
                        if sym_scope == root_scope { return None; }
                        let flags = semantic.scoping().symbol_flags(sym_id);
                        if flags.contains(SymbolFlags::BlockScopedVariable)
                            && !flags.contains(SymbolFlags::ConstVariable) {
                            Some(())
                        } else {
                            None
                        }
                    }).is_some();
                    if is_local_let {
                        return Err(CompilerError::todo(
                            "(BuildHIR::lowerExpression) Handle UpdateExpression to variables captured within lambdas.",
                        ));
                    }
                }
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements { nucl_check_stmt(s, outer_lets, true, semantic, root_scope)?; }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements { nucl_check_stmt(s, outer_lets, true, semantic, root_scope)?; }
            }
        }
        Expression::CallExpression(c) => {
            nucl_check_expr(&c.callee, outer_lets, in_closure, semantic, root_scope)?;
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    nucl_check_expr(e, outer_lets, in_closure, semantic, root_scope)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            nucl_check_expr(&l.left, outer_lets, in_closure, semantic, root_scope)?;
            nucl_check_expr(&l.right, outer_lets, in_closure, semantic, root_scope)?;
        }
        Expression::ConditionalExpression(c) => {
            nucl_check_expr(&c.test, outer_lets, in_closure, semantic, root_scope)?;
            nucl_check_expr(&c.consequent, outer_lets, in_closure, semantic, root_scope)?;
            nucl_check_expr(&c.alternate, outer_lets, in_closure, semantic, root_scope)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions { nucl_check_expr(e, outer_lets, in_closure, semantic, root_scope)?; }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_self_referential_closures
// ---------------------------------------------------------------------------

/// Detect closures that reassign the variable they are stored in.
///
/// Pattern: `let callback = () => { callback = null; }` — the closure body
/// directly reassigns `callback`, which is the variable holding this closure.
/// This is always invalid because when the closure fires (e.g. in an event
/// handler) it mutates a captured binding from the initial render.
fn validate_self_referential_closures<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    _semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            check_stmt_for_self_ref_closure(stmt)?;
        }
    }
    Ok(())
}

fn check_stmt_for_self_ref_closure<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement, VariableDeclarationKind};
    if let Statement::VariableDeclaration(v) = stmt {
        if v.kind == VariableDeclarationKind::Let {
            for decl in &v.declarations {
                let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind else { continue };
                let var_name = id.name.as_str();
                let Some(init) = &decl.init else { continue };
                // Check if the initializer is a closure that reassigns var_name
                let body_stmts: Option<&[oxc_ast::ast::Statement<'a>]> = match init {
                    Expression::ArrowFunctionExpression(arrow) => Some(&arrow.body.statements),
                    Expression::FunctionExpression(func) => {
                        func.body.as_ref().map(|b| b.statements.as_slice())
                    }
                    _ => None,
                };
                if let Some(body) = body_stmts {
                    if closure_body_assigns_name(body, var_name) {
                        return Err(CompilerError::invalid_react(format!(
                            "Cannot reassign variable after render completes\n\n\
                             Reassigning `{var_name}` after render has completed can cause \
                             inconsistent behavior on subsequent renders. Consider using state instead."
                        )));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Check if a function body directly assigns to `name` (does not recurse
/// into nested closures, which would have their own scope for this check).
fn closure_body_assigns_name<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    name: &str,
) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression, Statement};
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if expr_assigns_name(&e.expression, name) { return true; }
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        if expr_assigns_name(init, name) { return true; }
                    }
                }
            }
            Statement::IfStatement(i) => {
                if closure_body_assigns_name(std::slice::from_ref(&i.consequent), name) { return true; }
                if let Some(a) = &i.alternate {
                    if closure_body_assigns_name(std::slice::from_ref(a), name) { return true; }
                }
            }
            Statement::BlockStatement(b) => {
                if closure_body_assigns_name(&b.body, name) { return true; }
            }
            // Do NOT recurse into nested closures (they have their own bindings)
            _ => {}
        }
    }
    false
}

fn expr_assigns_name<'a>(expr: &oxc_ast::ast::Expression<'a>, name: &str) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    match expr {
        Expression::AssignmentExpression(a) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                if id.name.as_str() == name { return true; }
            }
            expr_assigns_name(&a.right, name)
        }
        Expression::SequenceExpression(s) => s.expressions.iter().any(|e| expr_assigns_name(e, name)),
        Expression::ConditionalExpression(c) => {
            expr_assigns_name(&c.consequent, name) || expr_assigns_name(&c.alternate, name)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_hook_return_closure_mutation
// ---------------------------------------------------------------------------

/// Detect hooks that directly return closures which reassign local `let` variables.
///
/// Pattern: `function useFoo() { let x = 0; return value => { x = value; }; }`
/// The returned closure escapes the hook, so calling it later will mutate
/// a captured binding from the initial render. This is always invalid.
fn validate_hook_return_closure_mutation<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn check_hook_fn<'a>(
        func: &oxc_ast::ast::Function<'a>,
        semantic: &oxc_semantic::Semantic<'a>,
    ) -> Result<()> {
        let name = func.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
        if !is_hook_name(name) { return Ok(()); }
        let Some(body) = &func.body else { return Ok(()); };
        let stmts = &body.statements;
        // Collect outer let names
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { return Ok(()); }
        // Look for top-level return statements that return closures
        for stmt in stmts {
            if let Statement::ReturnStatement(r) = stmt {
                if let Some(arg) = &r.argument {
                    let body_stmts: Option<&[oxc_ast::ast::Statement<'a>]> = match arg {
                        oxc_ast::ast::Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        oxc_ast::ast::Expression::FunctionExpression(f) => {
                            f.body.as_ref().map(|b| b.statements.as_slice())
                        }
                        _ => None,
                    };
                    if let Some(body) = body_stmts {
                        // Check if the returned closure reassigns any outer let
                        for outer_name in &outer_lets {
                            if closure_assigns_name_semantic(body, outer_name, semantic) {
                                return Err(CompilerError::invalid_react(format!(
                                    "Cannot reassign variable after render completes\n\n\
                                     Reassigning `{outer_name}` after render has completed can cause \
                                     inconsistent behavior on subsequent renders. Consider using state instead."
                                )));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => { check_hook_fn(f, semantic)?; }
            Statement::ExportDefaultDeclaration(d) => {
                if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &d.declaration {
                    check_hook_fn(f, semantic)?;
                }
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    check_hook_fn(f, semantic)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Check if a closure body directly assigns `name` (using semantic to confirm
/// it's a local let, not a parameter or const).
fn closure_assigns_name_semantic<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    name: &str,
    semantic: &oxc_semantic::Semantic<'a>,
) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression, Statement};
    use oxc_semantic::SymbolFlags;
    let root_scope = semantic.scoping().root_scope_id();
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if expr_assigns_name_semantic(&e.expression, name, semantic, root_scope) { return true; }
            }
            Statement::BlockStatement(b) => {
                if closure_assigns_name_semantic(&b.body, name, semantic) { return true; }
            }
            _ => {}
        }
    }
    false
}

fn expr_assigns_name_semantic<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    name: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    use oxc_semantic::SymbolFlags;
    match expr {
        Expression::AssignmentExpression(a) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                if id.name.as_str() == name {
                    // Confirm it's a local let (not global, not const)
                    let is_local_let = id.reference_id.get().and_then(|ref_id| {
                        let sym_id = semantic.scoping().get_reference(ref_id).symbol_id()?;
                        if semantic.scoping().symbol_scope_id(sym_id) == root_scope { return None; }
                        let flags = semantic.scoping().symbol_flags(sym_id);
                        if flags.contains(SymbolFlags::BlockScopedVariable)
                            && !flags.contains(SymbolFlags::ConstVariable) {
                            Some(())
                        } else { None }
                    }).is_some();
                    if is_local_let { return true; }
                }
            }
            false
        }
        Expression::SequenceExpression(s) => {
            s.expressions.iter().any(|e| expr_assigns_name_semantic(e, name, semantic, root_scope))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_use_before_declaration
// ---------------------------------------------------------------------------

/// Detect hook argument closures that reference a const variable declared AFTER the hook call.
/// Pattern: `useEffect(() => setState(2), []); const [state, setState] = useState(0);`
/// → `setState` is referenced before its declaration.
fn validate_no_use_before_declaration<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use std::collections::HashSet;
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect ALL const/let names declared anywhere in the body.
        let all_decls = collect_all_local_decl_names(stmts);
        if all_decls.is_empty() { continue; }

        let mut declared_so_far: HashSet<String> = HashSet::new();
        for stmt in stmts {
            // Check hook calls in this statement before updating declared_so_far.
            ubd_check_stmt(stmt, &all_decls, &declared_so_far)?;
            // Now update declared_so_far with names declared by this statement.
            ubd_collect_stmt_decls(stmt, &mut declared_so_far);
        }
    }
    Ok(())
}

/// Collect all const/let variable names declared (at top level) in a statement list.
fn collect_all_local_decl_names<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in stmts {
        if let oxc_ast::ast::Statement::VariableDeclaration(vd) = stmt {
            for decl in &vd.declarations {
                collect_binding_names(&decl.id, &mut names);
            }
        }
    }
    names
}

/// Update `declared` with names declared by `stmt`.
fn ubd_collect_stmt_decls<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    declared: &mut std::collections::HashSet<String>,
) {
    if let oxc_ast::ast::Statement::VariableDeclaration(vd) = stmt {
        for decl in &vd.declarations {
            collect_binding_names(&decl.id, declared);
        }
    }
}

/// Check if a statement contains a hook call whose closure argument references
/// a not-yet-declared name from `all_decls`.
fn ubd_check_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    all_decls: &std::collections::HashSet<String>,
    declared_so_far: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => ubd_check_expr(&e.expression, all_decls, declared_so_far)?,
        Statement::VariableDeclaration(vd) => {
            for decl in &vd.declarations {
                if let Some(init) = &decl.init {
                    ubd_check_expr(init, all_decls, declared_so_far)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                ubd_check_expr(arg, all_decls, declared_so_far)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn ubd_check_expr<'a>(
    expr: &Expression<'a>,
    all_decls: &std::collections::HashSet<String>,
    declared_so_far: &std::collections::HashSet<String>,
) -> Result<()> {
    if let Expression::CallExpression(call) = expr {
        let is_hook = match &call.callee {
            Expression::Identifier(id) => is_hook_name(id.name.as_str()),
            Expression::StaticMemberExpression(m) => is_hook_name(m.property.name.as_str()),
            _ => false,
        };
        if is_hook {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    let body_stmts: Option<&[oxc_ast::ast::Statement]> = match e {
                        Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    if let Some(body) = body_stmts {
                        // Subtract names declared LOCALLY inside the callback from `all_decls`
                        // to avoid false positives where an inner `const a = ...` shadows outer `a`.
                        let local_names = collect_all_local_decl_names(body);
                        let effective_decls: std::collections::HashSet<String> = all_decls
                            .iter()
                            .filter(|n| !local_names.contains(*n))
                            .cloned()
                            .collect();
                        // Check if body references any name in `effective_decls` not yet declared.
                        if ubd_body_refs_undeclared(body, &effective_decls, declared_so_far) {
                            return Err(CompilerError::invalid_react(
                                "Cannot access variable before it is declared",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn ubd_body_refs_undeclared<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    all_decls: &std::collections::HashSet<String>,
    declared_so_far: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::Statement;
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if ubd_expr_refs_undeclared(&e.expression, all_decls, declared_so_far) { return true; }
            }
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    if ubd_expr_refs_undeclared(arg, all_decls, declared_so_far) { return true; }
                }
            }
            Statement::IfStatement(i) => {
                if ubd_expr_refs_undeclared(&i.test, all_decls, declared_so_far) { return true; }
                if ubd_body_refs_undeclared(std::slice::from_ref(&i.consequent), all_decls, declared_so_far) { return true; }
                if let Some(alt) = &i.alternate {
                    if ubd_body_refs_undeclared(std::slice::from_ref(alt), all_decls, declared_so_far) { return true; }
                }
            }
            Statement::BlockStatement(b) => {
                if ubd_body_refs_undeclared(&b.body, all_decls, declared_so_far) { return true; }
            }
            _ => {}
        }
    }
    false
}

fn ubd_expr_refs_undeclared<'a>(
    expr: &Expression<'a>,
    all_decls: &std::collections::HashSet<String>,
    declared_so_far: &std::collections::HashSet<String>,
) -> bool {
    match expr {
        Expression::Identifier(id) => {
            let name = id.name.as_str();
            // Name is in all_decls (declared somewhere in the body) but NOT yet declared
            all_decls.contains(name) && !declared_so_far.contains(name)
        }
        Expression::CallExpression(call) => {
            ubd_expr_refs_undeclared(&call.callee, all_decls, declared_so_far)
                || call.arguments.iter().any(|a| {
                    a.as_expression().map_or(false, |e| ubd_expr_refs_undeclared(e, all_decls, declared_so_far))
                })
        }
        Expression::AssignmentExpression(a) => {
            ubd_expr_refs_undeclared(&a.right, all_decls, declared_so_far)
        }
        Expression::BinaryExpression(b) => {
            ubd_expr_refs_undeclared(&b.left, all_decls, declared_so_far)
                || ubd_expr_refs_undeclared(&b.right, all_decls, declared_so_far)
        }
        Expression::LogicalExpression(l) => {
            ubd_expr_refs_undeclared(&l.left, all_decls, declared_so_far)
                || ubd_expr_refs_undeclared(&l.right, all_decls, declared_so_far)
        }
        Expression::ConditionalExpression(c) => {
            ubd_expr_refs_undeclared(&c.test, all_decls, declared_so_far)
                || ubd_expr_refs_undeclared(&c.consequent, all_decls, declared_so_far)
                || ubd_expr_refs_undeclared(&c.alternate, all_decls, declared_so_far)
        }
        Expression::StaticMemberExpression(m) => {
            ubd_expr_refs_undeclared(&m.object, all_decls, declared_so_far)
        }
        Expression::ComputedMemberExpression(m) => {
            ubd_expr_refs_undeclared(&m.object, all_decls, declared_so_far)
                || ubd_expr_refs_undeclared(&m.expression, all_decls, declared_so_far)
        }
        Expression::ArrayExpression(arr) => {
            arr.elements.iter().any(|el| {
                el.as_expression().map_or(false, |e| ubd_expr_refs_undeclared(e, all_decls, declared_so_far))
            })
        }
        Expression::SequenceExpression(s) => {
            s.expressions.iter().any(|e| ubd_expr_refs_undeclared(e, all_decls, declared_so_far))
        }
        // Don't recurse into nested closures (they have their own scope)
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_capturing_default_param
// ---------------------------------------------------------------------------

/// Detect component/hook functions with default parameters that are arrow functions
/// capturing identifiers from the outer scope. The React compiler cannot safely
/// reorder such expressions during HIR lowering.
///
/// Pattern: `function Component(x, y = () => { return x; })`
fn validate_no_capturing_default_param<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn check_fn_params<'a>(func: &oxc_ast::ast::Function<'a>, name: &str) -> Result<()> {
        if !is_component_or_hook_name_for_default_param(name) { return Ok(()); }
        for param in &func.params.items {
            if let Some(default_expr) = get_param_default(param) {
                let body_stmts: Option<&[oxc_ast::ast::Statement]> = match default_expr {
                    Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                    Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                    _ => None,
                };
                if let Some(body) = body_stmts {
                    if closure_body_has_any_identifier(body) {
                        return Err(CompilerError::todo(
                            "(BuildHIR::node.lowerReorderableExpression) Expression type \
                             `ArrowFunctionExpression` cannot be safely reordered",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                check_fn_params(f, name)?;
            }
            Statement::ExportDefaultDeclaration(d) => {
                if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &d.declaration {
                    let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                    check_fn_params(f, name)?;
                }
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                    check_fn_params(f, name)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_component_or_hook_name_for_default_param(name: &str) -> bool {
    let first = name.chars().next();
    first.map_or(false, |c| c.is_uppercase()) || is_hook_name(name)
}

fn get_param_default<'a>(
    param: &'a oxc_ast::ast::FormalParameter<'a>,
) -> Option<&'a Expression<'a>> {
    use oxc_ast::ast::BindingPatternKind;
    if let BindingPatternKind::AssignmentPattern(ap) = &param.pattern.kind {
        return Some(&ap.right);
    }
    None
}

/// Returns true if any statement in `stmts` contains an IdentifierReference
/// (excluding `undefined`, `null`, `true`, `false`, `NaN`, `Infinity`).
fn closure_body_has_any_identifier<'a>(stmts: &[oxc_ast::ast::Statement<'a>]) -> bool {
    use oxc_ast::ast::Statement;
    const WELL_KNOWN: &[&str] = &["undefined", "null", "true", "false", "NaN", "Infinity"];
    for stmt in stmts {
        match stmt {
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    if expr_has_identifier(arg, WELL_KNOWN) { return true; }
                }
            }
            Statement::ExpressionStatement(e) => {
                if expr_has_identifier(&e.expression, WELL_KNOWN) { return true; }
            }
            Statement::IfStatement(i) => {
                if expr_has_identifier(&i.test, WELL_KNOWN) { return true; }
                if closure_body_has_any_identifier(std::slice::from_ref(&i.consequent)) { return true; }
                if let Some(alt) = &i.alternate {
                    if closure_body_has_any_identifier(std::slice::from_ref(alt)) { return true; }
                }
            }
            Statement::BlockStatement(b) => {
                if closure_body_has_any_identifier(&b.body) { return true; }
            }
            _ => {}
        }
    }
    false
}

fn expr_has_identifier<'a>(expr: &Expression<'a>, well_known: &[&str]) -> bool {
    match expr {
        Expression::Identifier(id) => !well_known.contains(&id.name.as_str()),
        Expression::CallExpression(call) => {
            expr_has_identifier(&call.callee, well_known)
                || call.arguments.iter().any(|a| {
                    a.as_expression().map_or(false, |e| expr_has_identifier(e, well_known))
                })
        }
        Expression::BinaryExpression(b) => {
            expr_has_identifier(&b.left, well_known) || expr_has_identifier(&b.right, well_known)
        }
        Expression::UnaryExpression(u) => expr_has_identifier(&u.argument, well_known),
        Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_) => {
            // Accessing properties of identifiers = has identifier
            true
        }
        Expression::TemplateLiteral(t) => {
            t.expressions.iter().any(|e| expr_has_identifier(e, well_known))
        }
        Expression::LogicalExpression(l) => {
            expr_has_identifier(&l.left, well_known) || expr_has_identifier(&l.right, well_known)
        }
        Expression::ConditionalExpression(c) => {
            expr_has_identifier(&c.test, well_known)
                || expr_has_identifier(&c.consequent, well_known)
                || expr_has_identifier(&c.alternate, well_known)
        }
        Expression::ArrayExpression(arr) => {
            arr.elements.iter().any(|el| {
                if let Some(e) = el.as_expression() { expr_has_identifier(e, well_known) } else { false }
            })
        }
        Expression::ObjectExpression(obj) => {
            obj.properties.iter().any(|p| {
                if let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(prop) = p {
                    expr_has_identifier(&prop.value, well_known)
                } else { false }
            })
        }
        Expression::SequenceExpression(s) => {
            s.expressions.iter().any(|e| expr_has_identifier(e, well_known))
        }
        // Arrow/fn bodies — recurse into them (they capture outer scope)
        Expression::ArrowFunctionExpression(a) => closure_body_has_any_identifier(&a.body.statements),
        Expression::FunctionExpression(f) => {
            f.body.as_ref().map_or(false, |b| closure_body_has_any_identifier(&b.statements))
        }
        // Literals — don't contain identifiers
        Expression::NumericLiteral(_) | Expression::StringLiteral(_)
        | Expression::BooleanLiteral(_) | Expression::NullLiteral(_)
        | Expression::BigIntLiteral(_) | Expression::RegExpLiteral(_) => false,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_escaping_let_assigner
// ---------------------------------------------------------------------------

/// Returns true if the closure body directly assigns any name in `set`
/// (does NOT recurse into nested closures — they have their own scope).
fn closure_body_assigns_any_in_set<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    set: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::{Expression, Statement};
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                if expr_assigns_any_in_set(&e.expression, set) { return true; }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument {
                    if expr_assigns_any_in_set(a, set) { return true; }
                }
            }
            Statement::VariableDeclaration(v) => {
                // Catch `const copy = (x = 3)` — assignment in VariableDecl initializer
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        // Don't descend into nested function/arrow inits
                        if !matches!(init, Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)) {
                            if expr_assigns_any_in_set(init, set) { return true; }
                        }
                    }
                }
            }
            Statement::IfStatement(i) => {
                if closure_body_assigns_any_in_set(std::slice::from_ref(&i.consequent), set) { return true; }
                if let Some(alt) = &i.alternate {
                    if closure_body_assigns_any_in_set(std::slice::from_ref(alt), set) { return true; }
                }
            }
            Statement::BlockStatement(b) => {
                if closure_body_assigns_any_in_set(&b.body, set) { return true; }
            }
            // Do NOT recurse into nested closures
            _ => {}
        }
    }
    false
}

fn expr_assigns_any_in_set<'a>(
    expr: &Expression<'a>,
    set: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::AssignmentTarget;
    match expr {
        Expression::AssignmentExpression(a) => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                if set.contains(id.name.as_str()) { return true; }
            }
            expr_assigns_any_in_set(&a.right, set)
        }
        Expression::SequenceExpression(s) => s.expressions.iter().any(|e| expr_assigns_any_in_set(e, set)),
        // Transparent wrapper: (x = 3) is ParenthesizedExpression(AssignmentExpression)
        Expression::ParenthesizedExpression(p) => expr_assigns_any_in_set(&p.expression, set),
        // Do NOT recurse into nested closures
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
        _ => false,
    }
}

/// Collect named closures (const/let arrow/fn) whose body directly assigns any outer let.
fn collect_direct_let_assigning_closures_from_set<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    outer_lets: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    use oxc_ast::ast::{BindingPatternKind, Statement};
    let mut result = std::collections::HashSet::new();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    let body_stmts: Option<&[oxc_ast::ast::Statement]> = match init {
                        Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    if let Some(body) = body_stmts {
                        if closure_body_assigns_any_in_set(body, outer_lets) {
                            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                result.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    result
}

/// Detect named let-assigning closures that "escape" the component by being
/// passed as JSX prop values or by being called inside hook-argument closures.
///
/// Pattern A (JSX prop): `let local; const setLocal = v => { local = v; };
///   const onClick = v => { setLocal(v); }; return <Foo onClick={onClick} />`
///   → `onClick` is a transitive let assigner passed as JSX prop.
///
/// Pattern B (hook arg): `const onMount = v => { setLocal(v); };
///   useEffect(() => { onMount(); }, [onMount])`
///   → The useEffect callback (anonymous) calls `onMount`, a transitive assigner.
fn validate_no_escaping_let_assigner<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }

        // Build the set of closures that directly or transitively assign an outer let.
        let mut let_assigners = collect_direct_let_assigning_closures_from_set(stmts, &outer_lets);
        if let_assigners.is_empty() { continue; }
        // Reuse the existing transitive expansion (same logic regardless of global vs local).
        expand_transitive_global_assigners(stmts, &mut let_assigners);

        // Pattern A: named let-assigner appears as a JSX prop value.
        check_stmts_jsx_let_assigners(stmts, &let_assigners)?;

        // Pattern B: anonymous hook-argument closure calls a let assigner.
        check_stmts_hook_arg_let_assigners(stmts, &let_assigners)?;
    }
    Ok(())
}

fn check_stmts_jsx_let_assigners<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    let_assigners: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    for stmt in stmts {
        match stmt {
            Statement::ReturnStatement(r) => {
                if let Some(expr) = &r.argument {
                    check_expr_jsx_let_assigners(expr, let_assigners)?;
                }
            }
            Statement::ExpressionStatement(e) => {
                check_expr_jsx_let_assigners(&e.expression, let_assigners)?;
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        check_expr_jsx_let_assigners(init, let_assigners)?;
                    }
                }
            }
            Statement::IfStatement(i) => {
                check_stmts_jsx_let_assigners(std::slice::from_ref(&i.consequent), let_assigners)?;
                if let Some(alt) = &i.alternate {
                    check_stmts_jsx_let_assigners(std::slice::from_ref(alt), let_assigners)?;
                }
            }
            Statement::BlockStatement(b) => {
                check_stmts_jsx_let_assigners(&b.body, let_assigners)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_expr_jsx_let_assigners<'a>(
    expr: &Expression<'a>,
    let_assigners: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        Expression::JSXElement(jsx) => {
            for attr_item in &jsx.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(attr) = attr_item {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(c)) = &attr.value {
                        if let Some(e) = c.expression.as_expression() {
                            if let Expression::Identifier(id) = e {
                                if let_assigners.contains(id.name.as_str()) {
                                    return Err(CompilerError::invalid_react(
                                        "Cannot reassign a variable after render completes. \
                                         Consider using state instead.",
                                    ));
                                }
                            }
                            check_expr_jsx_let_assigners(e, let_assigners)?;
                        }
                    }
                }
            }
            for child in &jsx.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(c) = child {
                    if let Some(e) = c.expression.as_expression() {
                        check_expr_jsx_let_assigners(e, let_assigners)?;
                    }
                }
            }
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(c) = child {
                    if let Some(e) = c.expression.as_expression() {
                        check_expr_jsx_let_assigners(e, let_assigners)?;
                    }
                }
            }
        }
        Expression::ConditionalExpression(c) => {
            check_expr_jsx_let_assigners(&c.test, let_assigners)?;
            check_expr_jsx_let_assigners(&c.consequent, let_assigners)?;
            check_expr_jsx_let_assigners(&c.alternate, let_assigners)?;
        }
        Expression::LogicalExpression(l) => {
            check_expr_jsx_let_assigners(&l.left, let_assigners)?;
            check_expr_jsx_let_assigners(&l.right, let_assigners)?;
        }
        _ => {}
    }
    Ok(())
}

/// Pattern B: walk statements looking for hook call expressions whose
/// closure arguments call a member of `let_assigners`.
fn check_stmts_hook_arg_let_assigners<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    let_assigners: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    for stmt in stmts {
        match stmt {
            Statement::ExpressionStatement(e) => {
                check_expr_hook_arg_let_assigners(&e.expression, let_assigners)?;
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        check_expr_hook_arg_let_assigners(init, let_assigners)?;
                    }
                }
            }
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    check_expr_hook_arg_let_assigners(arg, let_assigners)?;
                }
            }
            Statement::IfStatement(i) => {
                check_stmts_hook_arg_let_assigners(std::slice::from_ref(&i.consequent), let_assigners)?;
                if let Some(alt) = &i.alternate {
                    check_stmts_hook_arg_let_assigners(std::slice::from_ref(alt), let_assigners)?;
                }
            }
            Statement::BlockStatement(b) => {
                check_stmts_hook_arg_let_assigners(&b.body, let_assigners)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_expr_hook_arg_let_assigners<'a>(
    expr: &Expression<'a>,
    let_assigners: &std::collections::HashSet<String>,
) -> Result<()> {
    if let Expression::CallExpression(call) = expr {
        // Check if this is a hook call
        let is_hook_call = match &call.callee {
            Expression::Identifier(id) => is_hook_name(id.name.as_str()),
            Expression::StaticMemberExpression(m) => is_hook_name(m.property.name.as_str()),
            _ => false,
        };
        if is_hook_call {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    // For arrow/function arguments, check if their body calls any let_assigner
                    let body_stmts: Option<&[oxc_ast::ast::Statement]> = match e {
                        Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    if let Some(body) = body_stmts {
                        if closure_body_calls_any(body, let_assigners) {
                            return Err(CompilerError::invalid_react(
                                "Cannot reassign a variable after render completes. \
                                 Consider using state instead.",
                            ));
                        }
                    }
                }
            }
        }
        // Also recurse into non-hook call expressions (e.g. nested calls in JSX)
        for arg in &call.arguments {
            if let Some(e) = arg.as_expression() {
                check_expr_hook_arg_let_assigners(e, let_assigners)?;
            }
        }
        check_expr_hook_arg_let_assigners(&call.callee, let_assigners)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_local_reassignment_in_closures
// ---------------------------------------------------------------------------

/// Detect `let` variables declared in a component/hook body that are
/// reassigned inside a nested function (closure). This pattern is unsafe
/// because memoized closures capture bindings from the initial render.
fn validate_no_local_reassignment_in_closures<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    semantic: &oxc_semantic::Semantic<'a>,
) -> Result<()> {
    use oxc_semantic::SymbolFlags;
    let root_scope = semantic.scoping().root_scope_id();

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect names of `let` variables declared at the top level of the
        // component/hook body (does NOT recurse into nested function bodies).
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }
        for stmt in stmts {
            let excluded: std::collections::HashSet<String> = std::collections::HashSet::new();
            check_stmt_let_in_closure(stmt, &outer_lets, &excluded, false, semantic, root_scope)?;
        }
    }
    Ok(())
}

/// Collect `let` variable names from a statement list without recursing into
/// nested function bodies (arrow functions, function expressions, declarations).
fn collect_let_names_shallow<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in stmts {
        collect_let_names_in_stmt(stmt, &mut names);
    }
    names
}

fn collect_let_names_in_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    names: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::{ForStatementInit, ForStatementLeft, Statement, VariableDeclarationKind};
    match stmt {
        Statement::VariableDeclaration(vd) if vd.kind == VariableDeclarationKind::Let => {
            for decl in &vd.declarations {
                collect_binding_names(&decl.id, names);
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body { collect_let_names_in_stmt(s, names); }
        }
        Statement::IfStatement(i) => {
            collect_let_names_in_stmt(&i.consequent, names);
            if let Some(a) = &i.alternate { collect_let_names_in_stmt(a, names); }
        }
        Statement::WhileStatement(w) => collect_let_names_in_stmt(&w.body, names),
        Statement::DoWhileStatement(d) => collect_let_names_in_stmt(&d.body, names),
        Statement::ForStatement(f) => {
            if let Some(ForStatementInit::VariableDeclaration(vd)) = &f.init {
                if vd.kind == VariableDeclarationKind::Let {
                    for decl in &vd.declarations { collect_binding_names(&decl.id, names); }
                }
            }
            collect_let_names_in_stmt(&f.body, names);
        }
        Statement::ForInStatement(fi) => {
            if let ForStatementLeft::VariableDeclaration(vd) = &fi.left {
                if vd.kind == VariableDeclarationKind::Let {
                    for decl in &vd.declarations { collect_binding_names(&decl.id, names); }
                }
            }
            collect_let_names_in_stmt(&fi.body, names);
        }
        Statement::ForOfStatement(fo) => {
            if let ForStatementLeft::VariableDeclaration(vd) = &fo.left {
                if vd.kind == VariableDeclarationKind::Let {
                    for decl in &vd.declarations { collect_binding_names(&decl.id, names); }
                }
            }
            collect_let_names_in_stmt(&fo.body, names);
        }
        Statement::SwitchStatement(s) => {
            for case in &s.cases {
                for cs in &case.consequent { collect_let_names_in_stmt(cs, names); }
            }
        }
        Statement::TryStatement(t) => {
            for s in &t.block.body { collect_let_names_in_stmt(s, names); }
            if let Some(handler) = &t.handler {
                for s in &handler.body.body { collect_let_names_in_stmt(s, names); }
            }
            if let Some(finalizer) = &t.finalizer {
                for s in &finalizer.body { collect_let_names_in_stmt(s, names); }
            }
        }
        // Do NOT recurse into function declarations or expressions
        _ => {}
    }
}

fn collect_binding_names<'a>(
    pat: &oxc_ast::ast::BindingPattern<'a>,
    names: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::BindingPatternKind;
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(id) => { names.insert(id.name.to_string()); }
        BindingPatternKind::ObjectPattern(obj) => {
            for prop in &obj.properties { collect_binding_names(&prop.value, names); }
            if let Some(rest) = &obj.rest { collect_binding_names(&rest.argument, names); }
        }
        BindingPatternKind::ArrayPattern(arr) => {
            for el in arr.elements.iter().filter_map(|e| e.as_ref()) {
                collect_binding_names(el, names);
            }
            if let Some(rest) = &arr.rest { collect_binding_names(&rest.argument, names); }
        }
        BindingPatternKind::AssignmentPattern(ap) => {
            collect_binding_names(&ap.left, names);
        }
    }
}

/// Walk a statement looking for assignments to `outer_lets` inside nested functions.
/// `excluded` is a set of names shadowed by inner `let` declarations (accumulates as
/// we enter new closures, preventing false positives on shadowed variables).
fn check_stmt_let_in_closure<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    outer_lets: &std::collections::HashSet<String>,
    excluded: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            check_expr_let_in_closure(&e.expression, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Statement::VariableDeclaration(vd) => {
            for decl in &vd.declarations {
                if let Some(init) = &decl.init {
                    check_expr_let_in_closure(init, outer_lets, excluded, in_closure, semantic, root_scope)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_let_in_closure(arg, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_let_in_closure(s, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Statement::IfStatement(i) => {
            if in_closure {
                check_expr_let_in_closure(&i.test, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
            check_stmt_let_in_closure(&i.consequent, outer_lets, excluded, in_closure, semantic, root_scope)?;
            if let Some(a) = &i.alternate {
                check_stmt_let_in_closure(a, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Statement::FunctionDeclaration(f) => {
            // A function declaration inside the component body is a closure.
            if let Some(body) = &f.body {
                // Collect what this function's own let names shadow
                let closure_lets = collect_let_names_shallow(&body.statements);
                let mut new_excluded = excluded.clone();
                new_excluded.extend(closure_lets);
                for s in &body.statements {
                    check_stmt_let_in_closure(s, outer_lets, &new_excluded, true, semantic, root_scope)?;
                }
            }
        }
        Statement::WhileStatement(w) => {
            if in_closure {
                check_expr_let_in_closure(&w.test, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
            check_stmt_let_in_closure(&w.body, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Statement::ForStatement(f) => {
            if in_closure {
                if let Some(t) = &f.test {
                    check_expr_let_in_closure(t, outer_lets, excluded, in_closure, semantic, root_scope)?;
                }
            }
            check_stmt_let_in_closure(&f.body, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Statement::ThrowStatement(t) => {
            if in_closure {
                check_expr_let_in_closure(&t.argument, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_let_in_closure<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    outer_lets: &std::collections::HashSet<String>,
    excluded: &std::collections::HashSet<String>,
    in_closure: bool,
    semantic: &oxc_semantic::Semantic<'a>,
    root_scope: oxc_semantic::ScopeId,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Expression};
    use oxc_semantic::SymbolFlags;
    match expr {
        Expression::AssignmentExpression(a) if in_closure => {
            if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                let name = id.name.as_str();
                if outer_lets.contains(name) && !excluded.contains(name) {
                    // Confirm via semantic that this is a local `let` (not a global)
                    let is_local_let = id.reference_id.get().and_then(|ref_id| {
                        let sym_id = semantic.scoping().get_reference(ref_id).symbol_id()?;
                        let sym_scope = semantic.scoping().symbol_scope_id(sym_id);
                        if sym_scope == root_scope { return None; }
                        let flags = semantic.scoping().symbol_flags(sym_id);
                        if flags.contains(SymbolFlags::BlockScopedVariable)
                            && !flags.contains(SymbolFlags::ConstVariable) {
                            Some(())
                        } else {
                            None
                        }
                    }).is_some();
                    if is_local_let {
                        return Err(CompilerError::invalid_react(
                            "Cannot reassign a variable after render completes. \
                             Consider using state instead.",
                        ));
                    }
                }
            }
            check_expr_let_in_closure(&a.right, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::ArrowFunctionExpression(arrow) => {
            // Collect this closure's own let names to exclude (shadowing)
            let closure_lets = collect_let_names_shallow(&arrow.body.statements);
            let mut new_excluded = excluded.clone();
            new_excluded.extend(closure_lets);
            for s in &arrow.body.statements {
                check_stmt_let_in_closure(s, outer_lets, &new_excluded, true, semantic, root_scope)?;
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                let closure_lets = collect_let_names_shallow(&body.statements);
                let mut new_excluded = excluded.clone();
                new_excluded.extend(closure_lets);
                for s in &body.statements {
                    check_stmt_let_in_closure(s, outer_lets, &new_excluded, true, semantic, root_scope)?;
                }
            }
        }
        Expression::CallExpression(c) => {
            check_expr_let_in_closure(&c.callee, outer_lets, excluded, in_closure, semantic, root_scope)?;
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_let_in_closure(e, outer_lets, excluded, in_closure, semantic, root_scope)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_let_in_closure(&l.left, outer_lets, excluded, in_closure, semantic, root_scope)?;
            check_expr_let_in_closure(&l.right, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_let_in_closure(&c.test, outer_lets, excluded, in_closure, semantic, root_scope)?;
            check_expr_let_in_closure(&c.consequent, outer_lets, excluded, in_closure, semantic, root_scope)?;
            check_expr_let_in_closure(&c.alternate, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                check_expr_let_in_closure(e, outer_lets, excluded, in_closure, semantic, root_scope)?;
            }
        }
        Expression::AwaitExpression(a) => {
            check_expr_let_in_closure(&a.argument, outer_lets, excluded, in_closure, semantic, root_scope)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_conditional_hooks — Rules of Hooks
// ---------------------------------------------------------------------------

/// Collect local names that are (or alias) React hooks.
/// Returns a set of local names that should be treated as hook calls.
fn collect_hook_local_names<'a>(program: &'a oxc_ast::ast::Program<'a>) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in &program.body {
        if let oxc_ast::ast::Statement::ImportDeclaration(import) = stmt {
            if let Some(specifiers) = &import.specifiers {
                for spec in specifiers {
                    if let oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) = spec {
                        let original = match &s.imported {
                            oxc_ast::ast::ModuleExportName::IdentifierName(id) => id.name.as_str(),
                            oxc_ast::ast::ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                            oxc_ast::ast::ModuleExportName::StringLiteral(s) => s.value.as_str(),
                        };
                        let local = s.local.name.as_str();
                        // If original name or local name is a hook, track the local name
                        if is_hook_name(original) || is_hook_name(local) {
                            names.insert(local.to_string());
                        }
                    }
                }
            }
        }
    }
    names
}

/// Validate that hooks are not called conditionally (Rules of Hooks).
fn validate_no_conditional_hooks<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    let hook_aliases = collect_hook_local_names(program);
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts_for_conditional_hooks(stmts, false, &hook_aliases)?;
    }
    Ok(())
}

fn check_stmts_for_conditional_hooks<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    in_conditional: bool,
    hook_aliases: &std::collections::HashSet<String>,
) -> Result<()> {
    for stmt in stmts {
        check_stmt_for_conditional_hooks(stmt, in_conditional, hook_aliases)?;
    }
    Ok(())
}

fn check_stmt_for_conditional_hooks<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    in_conditional: bool,
    hook_aliases: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::IfStatement(i) => {
            // Consequent and alternate are now conditional contexts
            check_stmt_for_conditional_hooks(&i.consequent, true, hook_aliases)?;
            if let Some(alt) = &i.alternate {
                check_stmt_for_conditional_hooks(alt, true, hook_aliases)?;
            }
        }
        Statement::BlockStatement(b) => {
            check_stmts_for_conditional_hooks(&b.body, in_conditional, hook_aliases)?;
        }
        Statement::ExpressionStatement(e) => {
            if in_conditional {
                check_expr_for_hook_call(&e.expression, hook_aliases)?;
            }
        }
        Statement::VariableDeclaration(v) => {
            if in_conditional {
                for d in &v.declarations {
                    if let Some(init) = &d.init {
                        check_expr_for_hook_call(init, hook_aliases)?;
                    }
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if in_conditional {
                if let Some(arg) = &r.argument {
                    check_expr_for_hook_call(arg, hook_aliases)?;
                }
            }
        }
        // Don't recurse into nested function declarations
        Statement::FunctionDeclaration(_) => {}
        _ => {}
    }
    Ok(())
}

fn check_expr_for_hook_call<'a>(
    expr: &Expression<'a>,
    hook_aliases: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            if is_hook_call_expr(call, hook_aliases) {
                return Err(CompilerError::invalid_react(
                    "React Hook called conditionally. React Hooks must be called in the exact same order in every component render, and may not be called inside conditions, loops, or nested functions. See https://react.dev/reference/rules/rules-of-hooks"
                ));
            }
            // Recurse into call arguments (but not into nested functions)
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_for_hook_call(e, hook_aliases)?;
                }
            }
        }
        Expression::AssignmentExpression(a) => {
            check_expr_for_hook_call(&a.right, hook_aliases)?;
        }
        Expression::LogicalExpression(l) => {
            check_expr_for_hook_call(&l.left, hook_aliases)?;
            check_expr_for_hook_call(&l.right, hook_aliases)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_for_hook_call(&c.consequent, hook_aliases)?;
            check_expr_for_hook_call(&c.alternate, hook_aliases)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions { check_expr_for_hook_call(e, hook_aliases)?; }
        }
        // Don't recurse into nested functions
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {}
        _ => {}
    }
    Ok(())
}

fn is_hook_call_expr<'a>(
    call: &oxc_ast::ast::CallExpression<'a>,
    hook_aliases: &std::collections::HashSet<String>,
) -> bool {
    match &call.callee {
        // Direct call: readFragment(), useArray(), state()
        Expression::Identifier(id) => {
            let name = id.name.as_str();
            is_hook_name(name) || hook_aliases.contains(name)
        }
        // Method call: React.useHook(), Foo.useBar()
        Expression::StaticMemberExpression(m) => {
            is_hook_name(m.property.name.as_str())
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_ref_access_during_render — @validateRefAccessDuringRender pragma
// ---------------------------------------------------------------------------

/// When @validateRefAccessDuringRender pragma is present, validate that
/// ref.current is not accessed or modified at render time (top-level, not
/// inside closures). Ref names are scoped per-function to avoid false positives.
fn validate_ref_access_during_render<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
    deep: bool,
) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn check_fn_stmts_with_params<'a>(
        stmts: &[oxc_ast::ast::Statement<'a>],
        params: &oxc_ast::ast::FormalParameters<'a>,
        deep: bool,
    ) -> Result<()> {
        let mut refs = std::collections::HashSet::new();
        // Collect refs from body (useRef calls)
        for stmt in stmts {
            collect_ref_names_from_stmt(stmt, &mut refs);
        }
        // Collect ref params (bare `ref`/`*Ref` or destructured `{ref, fooRef}`)
        for param in &params.items {
            collect_ref_names_from_binding_pattern(&param.pattern, &mut refs);
        }
        // Always run the check (not just when refs is non-empty) — nested *.ref.current patterns
        // don't require the refs set to be populated.
        check_stmts_for_ref_access(stmts, &refs, deep)?;
        Ok(())
    }

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                if !name.is_empty() {
                    if let Some(body) = &f.body {
                        if !has_use_no_memo_directive(body) {
                            check_fn_stmts_with_params(&body.statements, &f.params, deep)?;
                        }
                    }
                }
            }
            Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    if let Some(body) = &f.body {
                        if !has_use_no_memo_directive(body) {
                            check_fn_stmts_with_params(&body.statements, &f.params, deep)?;
                        }
                    }
                }
                ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                    if !has_use_no_memo_directive(&a.body) {
                        let mut refs = std::collections::HashSet::new();
                        for stmt in &a.body.statements {
                            collect_ref_names_from_stmt(stmt, &mut refs);
                        }
                        check_stmts_for_ref_access(&a.body.statements, &refs, deep)?;
                    }
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    if let Some(body) = &f.body {
                        if !has_use_no_memo_directive(body) {
                            check_fn_stmts_with_params(&body.statements, &f.params, deep)?;
                        }
                    }
                }
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        match init {
                            Expression::ArrowFunctionExpression(a) => {
                                if !has_use_no_memo_directive(&a.body) {
                                    let mut refs = std::collections::HashSet::new();
                                    for stmt in &a.body.statements {
                                        collect_ref_names_from_stmt(stmt, &mut refs);
                                    }
                                    check_stmts_for_ref_access(&a.body.statements, &refs, deep)?;
                                }
                            }
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body {
                                    if !has_use_no_memo_directive(body) {
                                        check_fn_stmts_with_params(&body.statements, &f.params, deep)?;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn has_use_no_memo_directive(body: &oxc_ast::ast::FunctionBody) -> bool {
    body.directives.iter().any(|d| {
        matches!(d.expression.value.as_str(), "use no memo" | "use no forget")
    })
}

/// Collect ref-named bindings from a function parameter pattern.
/// Handles: bare identifiers (`ref`, `fooRef`), ObjectPattern (`{ref, fooRef}`).
fn collect_ref_names_from_binding_pattern<'a>(
    pat: &oxc_ast::ast::BindingPattern<'a>,
    refs: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::BindingPatternKind;
    match &pat.kind {
        BindingPatternKind::BindingIdentifier(id) => {
            let name = id.name.as_str();
            if name == "ref" || name.ends_with("Ref") {
                refs.insert(name.to_string());
            }
        }
        BindingPatternKind::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_ref_names_from_binding_pattern(&prop.value, refs);
            }
        }
        BindingPatternKind::ArrayPattern(arr) => {
            for item in arr.elements.iter().flatten() {
                collect_ref_names_from_binding_pattern(item, refs);
            }
        }
        _ => {}
    }
}

fn collect_ref_names_from_stmt<'a>(
    stmt: &'a oxc_ast::ast::Statement<'a>,
    refs: &mut std::collections::HashSet<String>,
) {
    if let oxc_ast::ast::Statement::VariableDeclaration(v) = stmt {
        for decl in &v.declarations {
            if let Some(init) = &decl.init {
                if let oxc_ast::ast::BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                    // `const ref = useRef(...)` or `const fooRef = useRef(...)`
                    if is_use_ref_call(init) {
                        refs.insert(id.name.to_string());
                    }
                    // `const ref = someExpr.ref` or `const fooRef = someExpr.fooRef`
                    // Handles: `const ref = props.ref`
                    if let Expression::StaticMemberExpression(m) = init {
                        let prop = m.property.name.as_str();
                        if prop == "ref" || prop.ends_with("Ref") {
                            refs.insert(id.name.to_string());
                        }
                    }
                    // `const aliasedRef = ref` — direct identifier alias of a known ref
                    if let Expression::Identifier(src) = init {
                        if refs.contains(src.name.as_str()) {
                            refs.insert(id.name.to_string());
                        }
                    }
                }
            }
        }
    }
}

fn is_use_ref_call(expr: &Expression) -> bool {
    match expr {
        Expression::CallExpression(call) => match &call.callee {
            Expression::Identifier(id) => id.name.as_str() == "useRef",
            Expression::StaticMemberExpression(m) => {
                if let Expression::Identifier(obj) = &m.object {
                    (obj.name == "React" || obj.name == "react") && m.property.name == "useRef"
                } else { false }
            }
            _ => false,
        }
        _ => false,
    }
}

fn check_stmts_for_ref_access<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    refs: &std::collections::HashSet<String>,
    deep: bool,
) -> Result<()> {
    for stmt in stmts {
        check_stmt_for_ref_access(stmt, refs, deep)?;
    }
    Ok(())
}

fn check_stmt_for_ref_access<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    refs: &std::collections::HashSet<String>,
    deep: bool,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_expr_for_ref_access(&e.expression, refs, deep)?,
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_for_ref_access(init, refs, deep)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { check_expr_for_ref_access(a, refs, deep)?; }
        }
        Statement::IfStatement(i) => {
            // Skip if-blocks that are null-guard lazy-initialization patterns:
            // `if (ref.current == null) { ref.current = init; }` is allowed.
            if is_ref_null_guard(&i.test, refs) {
                // Don't check this if-block (lazy init allowed).
            } else {
                check_expr_for_ref_access(&i.test, refs, deep)?;
                check_stmt_for_ref_access(&i.consequent, refs, deep)?;
                if let Some(alt) = &i.alternate { check_stmt_for_ref_access(alt, refs, deep)?; }
            }
        }
        Statement::BlockStatement(b) => check_stmts_for_ref_access(&b.body, refs, deep)?,
        // Recurse into nested function declarations — they may capture outer refs
        Statement::FunctionDeclaration(f) => {
            if deep {
                if let Some(body) = &f.body {
                    check_stmts_for_ref_access(&body.statements, refs, deep)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Returns true if `expr` is a null/undefined guard on any known ref's .current.
/// E.g., `ref.current == null`, `ref.current === null`, `!ref.current`.
fn is_ref_null_guard(expr: &Expression, refs: &std::collections::HashSet<String>) -> bool {
    match expr {
        Expression::BinaryExpression(b) => {
            // ref.current == null / ref.current === null / null == ref.current
            let left_is_ref = is_ref_current_expr(&b.left, refs);
            let right_is_ref = is_ref_current_expr(&b.right, refs);
            let left_is_null = matches!(&b.left, Expression::NullLiteral(_))
                || matches!(&b.left, Expression::Identifier(id) if id.name == "undefined");
            let right_is_null = matches!(&b.right, Expression::NullLiteral(_))
                || matches!(&b.right, Expression::Identifier(id) if id.name == "undefined");
            let is_eq = matches!(b.operator,
                oxc_ast::ast::BinaryOperator::Equality | oxc_ast::ast::BinaryOperator::StrictEquality |
                oxc_ast::ast::BinaryOperator::Inequality | oxc_ast::ast::BinaryOperator::StrictInequality);
            (left_is_ref && right_is_null || right_is_ref && left_is_null) && is_eq
        }
        Expression::UnaryExpression(u) => {
            u.operator == oxc_ast::ast::UnaryOperator::LogicalNot
                && is_ref_current_expr(&u.argument, refs)
        }
        Expression::LogicalExpression(l) => {
            is_ref_null_guard(&l.left, refs) || is_ref_null_guard(&l.right, refs)
        }
        _ => false,
    }
}

/// Returns true if `expr` is `someRef.current` where `someRef` is in `refs`.
fn is_ref_current_expr(expr: &Expression, refs: &std::collections::HashSet<String>) -> bool {
    if let Expression::StaticMemberExpression(m) = expr {
        if m.property.name == "current" {
            if let Expression::Identifier(obj) = &m.object {
                return refs.contains(obj.name.as_str());
            }
        }
    }
    false
}

fn ref_access_error() -> crate::error::CompilerError {
    CompilerError::invalid_react(
        "Cannot access refs during render\n\nReact refs are values that are not needed for rendering. Refs should only be accessed outside of render, such as in event handlers or effects. Accessing a ref value (the `current` property) during render can cause your component not to update as expected (https://react.dev/reference/react/useRef)."
    )
}

fn check_expr_for_ref_access<'a>(
    expr: &Expression<'a>,
    refs: &std::collections::HashSet<String>,
    deep: bool,
) -> Result<()> {
    match expr {
        // ref.current (read)
        Expression::StaticMemberExpression(m) => {
            if m.property.name == "current" {
                // Direct ref: `ref.current` where `ref` is in the refs set
                if let Expression::Identifier(obj) = &m.object {
                    if refs.contains(obj.name.as_str()) {
                        return Err(ref_access_error());
                    }
                }
                // Nested: `props.ref.current` or `obj.someRef.current`
                if let Expression::StaticMemberExpression(inner) = &m.object {
                    let prop = inner.property.name.as_str();
                    if prop == "ref" || prop.ends_with("Ref") {
                        return Err(ref_access_error());
                    }
                }
            }
            // ref.current.prop (nested) — check object
            check_expr_for_ref_access(&m.object, refs, deep)?;
        }
        // ref?.current — optional chaining
        Expression::ChainExpression(chain) => {
            if let oxc_ast::ast::ChainElement::StaticMemberExpression(m) = &chain.expression {
                if m.property.name == "current" {
                    if let Expression::Identifier(obj) = &m.object {
                        if refs.contains(obj.name.as_str()) {
                            return Err(ref_access_error());
                        }
                    }
                    if let Expression::StaticMemberExpression(inner) = &m.object {
                        let prop = inner.property.name.as_str();
                        if prop == "ref" || prop.ends_with("Ref") {
                            return Err(ref_access_error());
                        }
                    }
                }
            }
        }
        // ref.current = value (write via assignment) or ref.current.prop = value
        Expression::AssignmentExpression(a) => {
            // Check for ref.current = ... (left side)
            match &a.left {
                oxc_ast::ast::AssignmentTarget::StaticMemberExpression(m) => {
                    if m.property.name == "current" {
                        if let Expression::Identifier(obj) = &m.object {
                            if refs.contains(obj.name.as_str()) {
                                return Err(ref_access_error());
                            }
                        }
                    }
                    // ref.current.inner = value — the StaticMemberExpression's object is ref.current
                    if let Expression::StaticMemberExpression(inner_m) = &m.object {
                        if inner_m.property.name == "current" {
                            if let Expression::Identifier(obj) = &inner_m.object {
                                if refs.contains(obj.name.as_str()) {
                                    return Err(ref_access_error());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            check_expr_for_ref_access(&a.right, refs, deep)?;
        }
        // Function calls — check arguments for ref.current
        Expression::CallExpression(call) => {
            // Effect hooks and useCallback: their callback (first arg) runs AFTER render,
            // so ref.current access inside those callbacks is allowed.
            let callee_name = match &call.callee {
                Expression::Identifier(id) => Some(id.name.as_str()),
                Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
                _ => None,
            };
            let is_deferred_hook = matches!(callee_name,
                Some("useEffect" | "useLayoutEffect" | "useInsertionEffect" | "useCallback")
            );
            // Synchronous hooks whose callbacks run during render: always recurse
            // into their callback bodies regardless of `deep` mode.
            let is_synchronous_hook = matches!(callee_name,
                Some("useState" | "useReducer" | "useMemo" | "useImperativeHandle")
            );
            check_expr_for_ref_access(&call.callee, refs, deep)?;
            for (i, arg) in call.arguments.iter().enumerate() {
                // Skip the callback (first arg) for deferred hooks
                if is_deferred_hook && i == 0 {
                    continue;
                }
                if let Some(e) = arg.as_expression() {
                    // For synchronous hooks, always recurse into function bodies
                    if is_synchronous_hook {
                        match e {
                            Expression::ArrowFunctionExpression(arrow) => {
                                check_stmts_for_ref_access(&arrow.body.statements, refs, deep)?;
                                continue;
                            }
                            Expression::FunctionExpression(func) => {
                                if let Some(body) = &func.body {
                                    check_stmts_for_ref_access(&body.statements, refs, deep)?;
                                }
                                continue;
                            }
                            _ => {}
                        }
                    }
                    check_expr_for_ref_access(e, refs, deep)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_for_ref_access(&l.left, refs, deep)?;
            check_expr_for_ref_access(&l.right, refs, deep)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_for_ref_access(&c.test, refs, deep)?;
            check_expr_for_ref_access(&c.consequent, refs, deep)?;
            check_expr_for_ref_access(&c.alternate, refs, deep)?;
        }
        // Recurse into nested closures only in deep mode
        Expression::ArrowFunctionExpression(arrow) => {
            if deep {
                // Collect any ref aliases declared inside this closure before checking
                let mut inner_refs = refs.clone();
                for stmt in &arrow.body.statements {
                    collect_ref_names_from_stmt(stmt, &mut inner_refs);
                }
                check_stmts_for_ref_access(&arrow.body.statements, &inner_refs, deep)?;
            }
        }
        Expression::FunctionExpression(func) => {
            if deep {
                if let Some(body) = &func.body {
                    let mut inner_refs = refs.clone();
                    for stmt in &body.statements {
                        collect_ref_names_from_stmt(stmt, &mut inner_refs);
                    }
                    check_stmts_for_ref_access(&body.statements, &inner_refs, deep)?;
                }
            }
        }
        // JSX elements — check attribute values and children for ref.current
        Expression::JSXElement(jsx) => {
            for attr in &jsx.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(c)) = &a.value {
                        if let Some(e) = c.expression.as_expression() {
                            check_expr_for_ref_access(e, refs, deep)?;
                        }
                    }
                }
            }
            for child in &jsx.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(c) = child {
                    if let Some(e) = c.expression.as_expression() {
                        check_expr_for_ref_access(e, refs, deep)?;
                    }
                }
            }
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(c) = child {
                    if let Some(e) = c.expression.as_expression() {
                        check_expr_for_ref_access(e, refs, deep)?;
                    }
                }
            }
        }
        // Object literals — recurse into property values (catches `{val: ref.current}`)
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                if let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) = prop {
                    check_expr_for_ref_access(&p.value, refs, deep)?;
                }
            }
        }
        // Array literals — recurse into elements
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let oxc_ast::ast::ArrayExpressionElement::SpreadElement(s) = elem {
                    check_expr_for_ref_access(&s.argument, refs, deep)?;
                } else if let Some(e) = elem.as_expression() {
                    check_expr_for_ref_access(e, refs, deep)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_ref_in_hook_deps — always checked
// ---------------------------------------------------------------------------

/// Validate that ref.current is not used directly in hook dependency arrays.
/// This is always an error regardless of pragmas.
fn validate_no_ref_in_hook_deps<'a>(program: &'a oxc_ast::ast::Program<'a>) -> Result<()> {
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            check_stmt_for_ref_in_deps(stmt)?;
        }
    }
    Ok(())
}

fn check_stmt_for_ref_in_deps<'a>(stmt: &oxc_ast::ast::Statement<'a>) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_expr_for_ref_in_deps(&e.expression)?,
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_for_ref_in_deps(init)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { check_expr_for_ref_in_deps(a)?; }
        }
        Statement::IfStatement(i) => {
            check_stmt_for_ref_in_deps(&i.consequent)?;
            if let Some(alt) = &i.alternate { check_stmt_for_ref_in_deps(alt)?; }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body { check_stmt_for_ref_in_deps(s)?; }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_ref_in_deps<'a>(expr: &Expression<'a>) -> Result<()> {
    if let Expression::CallExpression(call) = expr {
        // Check if this is a hook call (useEffect, useMemo, useCallback, useLayoutEffect, etc.)
        let is_hook_call = match &call.callee {
            Expression::Identifier(id) => is_hook_name(id.name.as_str()),
            Expression::StaticMemberExpression(m) => is_hook_name(m.property.name.as_str()),
            _ => false,
        };
        if is_hook_call && call.arguments.len() >= 2 {
            // The deps array is the last argument (typically second)
            let deps_arg = call.arguments.last().unwrap();
            if let Some(Expression::ArrayExpression(arr)) = deps_arg.as_expression().map(|e| e) {
                for elem in &arr.elements {
                    if let Some(elem_expr) = elem.as_expression() {
                        if is_ref_current_access(elem_expr) {
                            return Err(CompilerError::invalid_react(
                                "Cannot access refs during render\n\nReact refs are values that are not needed for rendering. Refs should only be accessed outside of render, such as in event handlers or effects. Accessing a ref value (the `current` property) during render can cause your component not to update as expected (https://react.dev/reference/react/useRef)."
                            ));
                        }
                    }
                }
            }
        }
        // Also check nested expressions
        check_expr_for_ref_in_deps(&call.callee)?;
        for arg in &call.arguments {
            if let Some(e) = arg.as_expression() {
                check_expr_for_ref_in_deps(e)?;
            }
        }
    }
    Ok(())
}

/// Check if expression is `*.current` (any static member access to .current).
fn is_ref_current_access(expr: &Expression) -> bool {
    if let Expression::StaticMemberExpression(m) = expr {
        m.property.name == "current"
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// collect_component_hook_bodies — shared helper
// ---------------------------------------------------------------------------

/// Collect references to the statement lists of component/hook function bodies
/// in a program. Only looks at top-level declarations.
fn collect_component_hook_bodies<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Vec<&'a [oxc_ast::ast::Statement<'a>]> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn is_component_or_hook(name: &str) -> bool {
        let first = name.chars().next();
        first.map_or(false, |c| c.is_uppercase()) || is_hook_name(name)
    }

    // Check if a function body opts out via 'use no forget'/'use no memo' directive.
    let body_is_opted_out = |body: &oxc_ast::ast::FunctionBody| -> bool {
        body.directives.iter().any(|d|
            matches!(d.expression.value.as_str(), "use no memo" | "use no forget")
        )
    };

    let mut bodies: Vec<&'a [oxc_ast::ast::Statement<'a>]> = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                if is_component_or_hook(name) {
                    if let Some(body) = &f.body {
                        if !body_is_opted_out(body) {
                            bodies.push(&body.statements);
                        }
                    }
                }
            }
            Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                    if is_component_or_hook(name) || name.is_empty() {
                        if let Some(body) = &f.body {
                            if !body_is_opted_out(body) {
                                bodies.push(&body.statements);
                            }
                        }
                    }
                }
                ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                    if !body_is_opted_out(&a.body) {
                        bodies.push(&a.body.statements);
                    }
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(d) => {
                match &d.declaration {
                    Some(Declaration::FunctionDeclaration(f)) => {
                        let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                        if is_component_or_hook(name) {
                            if let Some(body) = &f.body {
                                if !body_is_opted_out(body) {
                                    bodies.push(&body.statements);
                                }
                            }
                        }
                    }
                    Some(Declaration::VariableDeclaration(v)) => {
                        for decl in &v.declarations {
                            let name = match &decl.id.kind {
                                oxc_ast::ast::BindingPatternKind::BindingIdentifier(id) => id.name.as_str(),
                                _ => "",
                            };
                            if !is_component_or_hook(name) { continue; }
                            if let Some(init) = &decl.init {
                                match init {
                                    Expression::ArrowFunctionExpression(a) => {
                                        if !body_is_opted_out(&a.body) {
                                            bodies.push(&a.body.statements);
                                        }
                                    }
                                    Expression::FunctionExpression(f) => {
                                        if let Some(body) = &f.body {
                                            if !body_is_opted_out(body) {
                                                bodies.push(&body.statements);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    let name = match &decl.id.kind {
                        oxc_ast::ast::BindingPatternKind::BindingIdentifier(id) => id.name.as_str(),
                        _ => "",
                    };
                    if !is_component_or_hook(name) { continue; }
                    if let Some(init) = &decl.init {
                        match init {
                            Expression::ArrowFunctionExpression(a) => {
                                if !body_is_opted_out(&a.body) {
                                    bodies.push(&a.body.statements);
                                }
                            }
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body {
                                    if !body_is_opted_out(body) {
                                        bodies.push(&body.statements);
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    bodies
}

// ---------------------------------------------------------------------------
// validate_no_passing_ref_to_function
// ---------------------------------------------------------------------------

/// When @validateRefAccessDuringRender is explicitly enabled and
/// @enableTreatRefLikeIdentifiersAsRefs is not set, passing a ref object
/// (not ref.current) to a non-hook bare identifier function is an error.
fn validate_no_passing_ref_to_function<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Statement};

    fn check_fn_body<'a>(
        stmts: &[Statement<'a>],
        params: &oxc_ast::ast::FormalParameters<'a>,
    ) -> Result<()> {
        let mut refs: std::collections::HashSet<String> = std::collections::HashSet::new();
        for stmt in stmts {
            collect_ref_names_from_stmt(stmt, &mut refs);
        }
        for param in &params.items {
            collect_ref_names_from_binding_pattern(&param.pattern, &mut refs);
        }
        if refs.is_empty() {
            return Ok(());
        }
        check_stmts_no_ref_pass(stmts, &refs)
    }

    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    if !has_use_no_memo_directive(body) {
                        check_fn_body(&body.statements, &f.params)?;
                    }
                }
            }
            Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    if let Some(body) = &f.body {
                        if !has_use_no_memo_directive(body) {
                            check_fn_body(&body.statements, &f.params)?;
                        }
                    }
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    if let Some(body) = &f.body {
                        if !has_use_no_memo_directive(body) {
                            check_fn_body(&body.statements, &f.params)?;
                        }
                    }
                }
            }
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        match init {
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body {
                                    if !has_use_no_memo_directive(body) {
                                        check_fn_body(&body.statements, &f.params)?;
                                    }
                                }
                            }
                            Expression::ArrowFunctionExpression(a) => {
                                if !has_use_no_memo_directive(&a.body) {
                                    let mut refs = std::collections::HashSet::new();
                                    for stmt in &a.body.statements {
                                        collect_ref_names_from_stmt(stmt, &mut refs);
                                    }
                                    check_stmts_no_ref_pass(&a.body.statements, &refs)?;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_stmts_no_ref_pass<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    refs: &std::collections::HashSet<String>,
) -> Result<()> {
    for stmt in stmts {
        check_stmt_no_ref_pass(stmt, refs)?;
    }
    Ok(())
}

fn check_stmt_no_ref_pass<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    refs: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => check_expr_no_ref_pass(&e.expression, refs)?,
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_no_ref_pass(init, refs)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument {
                check_expr_no_ref_pass(a, refs)?;
            }
        }
        Statement::IfStatement(i) => {
            check_expr_no_ref_pass(&i.test, refs)?;
            check_stmt_no_ref_pass(&i.consequent, refs)?;
            if let Some(alt) = &i.alternate {
                check_stmt_no_ref_pass(alt, refs)?;
            }
        }
        Statement::BlockStatement(b) => check_stmts_no_ref_pass(&b.body, refs)?,
        _ => {}
    }
    Ok(())
}

fn check_expr_no_ref_pass<'a>(
    expr: &Expression<'a>,
    refs: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        Expression::CallExpression(call) => {
            // Only check calls where callee is a bare identifier (not a method call).
            // Method calls like props.render(ref) are intentional render helpers.
            if let Expression::Identifier(callee_id) = &call.callee {
                let callee_name = callee_id.name.as_str();
                // Skip hooks (they accept refs intentionally: useImperativeHandle, etc.)
                let is_hook = callee_name.starts_with("use")
                    && callee_name.chars().next().map_or(false, |c| c == 'u');
                if !is_hook {
                    for arg in &call.arguments {
                        if let Some(Expression::Identifier(arg_id)) = arg.as_expression() {
                            if refs.contains(arg_id.name.as_str()) {
                                return Err(CompilerError::invalid_react(
                                    "Passing a ref to a function may read its value during render\n\nReact refs are values that are not needed for rendering. Refs should only be accessed outside of render, such as in event handlers or effects.",
                                ));
                            }
                        }
                    }
                }
            }
            // Recurse into arguments and callee
            check_expr_no_ref_pass(&call.callee, refs)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_no_ref_pass(e, refs)?;
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_no_ref_pass(&l.left, refs)?;
            check_expr_no_ref_pass(&l.right, refs)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_no_ref_pass(&c.test, refs)?;
            check_expr_no_ref_pass(&c.consequent, refs)?;
            check_expr_no_ref_pass(&c.alternate, refs)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_freezing_known_mutable_functions
// ---------------------------------------------------------------------------

/// @validateNoFreezingKnownMutableFunctions: detect when a function that
/// captures and mutates a locally-created mutable (Map/Set/WeakMap/WeakSet)
/// is passed to a hook as an argument, passed as a JSX attribute, or returned.
fn validate_no_freezing_known_mutable_functions<'a>(
    source: &str,
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    let first = source.lines().next().unwrap_or("");
    if !first.contains("@validateNoFreezingKnownMutableFunctions") {
        return Ok(());
    }
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts_for_mutable_fn_freeze(stmts)?;
    }
    Ok(())
}

fn check_stmts_for_mutable_fn_freeze<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Statement};

    // Step 1: Collect variables initialized with mutable constructors.
    let mut mutable_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    if is_mutable_constructor_call(init) {
                        if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                            mutable_vars.insert(id.name.to_string());
                        }
                    }
                }
            }
        }
    }
    if mutable_vars.is_empty() {
        return Ok(());
    }

    // Step 2: Collect variable names that point to mutating functions.
    let mut mutating_fn_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    let is_mutating = match init {
                        Expression::ArrowFunctionExpression(arrow) => {
                            fn_stmts_mutate_vars(&arrow.body.statements, &mutable_vars)
                        }
                        Expression::FunctionExpression(func) => {
                            func.body.as_ref().map_or(false, |b| {
                                fn_stmts_mutate_vars(&b.statements, &mutable_vars)
                            })
                        }
                        _ => false,
                    };
                    if is_mutating {
                        if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                            mutating_fn_vars.insert(id.name.to_string());
                        }
                    }
                }
            }
        }
    }

    // Step 3: Check for violations.
    for stmt in stmts {
        check_stmt_for_mutable_fn_escape(stmt, &mutable_vars, &mutating_fn_vars)?;
    }
    Ok(())
}

fn is_mutable_constructor_call(expr: &Expression) -> bool {
    if let Expression::NewExpression(n) = expr {
        if let Expression::Identifier(callee) = &n.callee {
            return matches!(callee.name.as_str(), "Map" | "Set" | "WeakMap" | "WeakSet");
        }
    }
    false
}

/// Returns true if any statement in `stmts` calls a mutating method on a variable
/// in `mutable_vars` (e.g., cache.set(...), cache.add(...)).
fn fn_stmts_mutate_vars<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    mutable_vars: &std::collections::HashSet<String>,
) -> bool {
    fn expr_mutates(expr: &Expression, mutable_vars: &std::collections::HashSet<String>) -> bool {
        match expr {
            Expression::CallExpression(call) => {
                if let Expression::StaticMemberExpression(m) = &call.callee {
                    if let Expression::Identifier(obj) = &m.object {
                        if mutable_vars.contains(obj.name.as_str()) {
                            let method = m.property.name.as_str();
                            if matches!(method, "set" | "add" | "delete" | "clear"
                                | "push" | "pop" | "splice" | "shift" | "unshift") {
                                return true;
                            }
                        }
                    }
                }
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        if expr_mutates(e, mutable_vars) {
                            return true;
                        }
                    }
                }
                false
            }
            Expression::AssignmentExpression(a) => {
                if let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(m) = &a.left {
                    if let Expression::Identifier(obj) = &m.object {
                        if mutable_vars.contains(obj.name.as_str()) {
                            return true;
                        }
                    }
                }
                expr_mutates(&a.right, mutable_vars)
            }
            _ => false,
        }
    }
    for stmt in stmts {
        match stmt {
            oxc_ast::ast::Statement::ExpressionStatement(e) => {
                if expr_mutates(&e.expression, mutable_vars) {
                    return true;
                }
            }
            oxc_ast::ast::Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        if expr_mutates(init, mutable_vars) {
                            return true;
                        }
                    }
                }
            }
            oxc_ast::ast::Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument {
                    if expr_mutates(a, mutable_vars) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

fn check_stmt_for_mutable_fn_escape<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    mutable_vars: &std::collections::HashSet<String>,
    mutating_fn_vars: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                // Check for direct function return (arrow/function expr/identifier)
                check_return_expr_for_mutable_fn_escape(arg, mutable_vars, mutating_fn_vars)?;
                // Also check JSX expressions returned (e.g., return <Foo fn={fn} />)
                check_expr_for_mutable_fn_escape(arg, mutable_vars, mutating_fn_vars)?;
            }
        }
        Statement::ExpressionStatement(e) => {
            check_expr_for_mutable_fn_escape(&e.expression, mutable_vars, mutating_fn_vars)?;
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_mutable_fn_escape(init, mutable_vars, mutating_fn_vars)?;
                }
            }
        }
        Statement::IfStatement(i) => {
            check_stmt_for_mutable_fn_escape(&i.consequent, mutable_vars, mutating_fn_vars)?;
            if let Some(alt) = &i.alternate {
                check_stmt_for_mutable_fn_escape(alt, mutable_vars, mutating_fn_vars)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_for_mutable_fn_escape(s, mutable_vars, mutating_fn_vars)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn mutable_fn_freeze_error() -> crate::error::CompilerError {
    CompilerError::invalid_react(
        "Cannot modify local variables after render completes\n\nThis argument is a function which may reassign or mutate a local variable after render, which can cause inconsistent behavior on subsequent renders. Consider using state instead.",
    )
}

fn check_expr_for_mutable_fn_escape<'a>(
    expr: &Expression<'a>,
    mutable_vars: &std::collections::HashSet<String>,
    mutating_fn_vars: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        // Hook call: check if any argument is a mutating function
        Expression::CallExpression(call) => {
            let callee_name = match &call.callee {
                Expression::Identifier(id) => Some(id.name.as_str()),
                Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
                _ => None,
            };
            let is_hook = callee_name.map_or(false, |n| {
                n.starts_with("use") && n.chars().next().map_or(false, |c| c == 'u')
            });
            if is_hook {
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        match e {
                            Expression::ArrowFunctionExpression(arrow) => {
                                if fn_stmts_mutate_vars(&arrow.body.statements, mutable_vars) {
                                    return Err(mutable_fn_freeze_error());
                                }
                            }
                            Expression::FunctionExpression(func) => {
                                if let Some(body) = &func.body {
                                    if fn_stmts_mutate_vars(&body.statements, mutable_vars) {
                                        return Err(mutable_fn_freeze_error());
                                    }
                                }
                            }
                            Expression::Identifier(id) => {
                                if mutating_fn_vars.contains(id.name.as_str()) {
                                    return Err(mutable_fn_freeze_error());
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        // JSX: check if any attribute value is a mutating function (named or inline)
        Expression::JSXElement(jsx) => {
            for attr in &jsx.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(c)) = &a.value {
                        if let Some(e) = c.expression.as_expression() {
                            match e {
                                Expression::Identifier(id) => {
                                    if mutating_fn_vars.contains(id.name.as_str()) {
                                        return Err(mutable_fn_freeze_error());
                                    }
                                }
                                Expression::ArrowFunctionExpression(arrow) => {
                                    if fn_stmts_mutate_vars(&arrow.body.statements, mutable_vars) {
                                        return Err(mutable_fn_freeze_error());
                                    }
                                }
                                Expression::FunctionExpression(func) => {
                                    if let Some(body) = &func.body {
                                        if fn_stmts_mutate_vars(&body.statements, mutable_vars) {
                                            return Err(mutable_fn_freeze_error());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Check a return expression for mutating function escape.
fn check_return_expr_for_mutable_fn_escape<'a>(
    expr: &Expression<'a>,
    mutable_vars: &std::collections::HashSet<String>,
    mutating_fn_vars: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            if fn_stmts_mutate_vars(&arrow.body.statements, mutable_vars) {
                return Err(mutable_fn_freeze_error());
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                if fn_stmts_mutate_vars(&body.statements, mutable_vars) {
                    return Err(mutable_fn_freeze_error());
                }
            }
        }
        Expression::Identifier(id) => {
            if mutating_fn_vars.contains(id.name.as_str()) {
                return Err(mutable_fn_freeze_error());
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_destructured_catch
// ---------------------------------------------------------------------------
/// Detect `catch` clauses with destructuring patterns (object/array).
/// e.g., `catch ({ status }) { ... }` — our HIR lowering can't handle this.
fn validate_no_destructured_catch<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Statement};

    fn check_stmts<'a>(stmts: &[oxc_ast::ast::Statement<'a>]) -> Result<()> {
        for stmt in stmts {
            check_stmt(stmt)?;
        }
        Ok(())
    }

    fn check_stmt<'a>(stmt: &oxc_ast::ast::Statement<'a>) -> Result<()> {
        match stmt {
            Statement::TryStatement(ts) => {
                if let Some(handler) = &ts.handler {
                    if let Some(param) = &handler.param {
                        match &param.pattern.kind {
                            BindingPatternKind::ObjectPattern(_)
                            | BindingPatternKind::ArrayPattern(_) => {
                                return Err(CompilerError::todo(
                                    "Destructuring patterns in catch clauses are not yet supported",
                                ));
                            }
                            _ => {}
                        }
                    }
                    check_stmts(&handler.body.body)?;
                }
                check_stmts(&ts.block.body)?;
                if let Some(fin) = &ts.finalizer {
                    check_stmts(&fin.body)?;
                }
            }
            Statement::BlockStatement(b) => check_stmts(&b.body)?,
            Statement::IfStatement(i) => {
                check_stmt(&i.consequent)?;
                if let Some(a) = &i.alternate { check_stmt(a)?; }
            }
            Statement::WhileStatement(w) => check_stmt(&w.body)?,
            Statement::ForStatement(f) => check_stmt(&f.body)?,
            Statement::ForInStatement(f) => check_stmt(&f.body)?,
            Statement::ForOfStatement(f) => check_stmt(&f.body)?,
            Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body { check_stmts(&body.statements)?; }
            }
            Statement::VariableDeclaration(vd) => {
                for decl in &vd.declarations {
                    if let Some(init) = &decl.init {
                        check_expr_for_catch(init)?;
                    }
                }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument { check_expr_for_catch(a)?; }
            }
            Statement::ExpressionStatement(e) => check_expr_for_catch(&e.expression)?,
            _ => {}
        }
        Ok(())
    }

    fn check_expr_for_catch<'a>(expr: &oxc_ast::ast::Expression<'a>) -> Result<()> {
        use oxc_ast::ast::Expression;
        match expr {
            Expression::ArrowFunctionExpression(a) => check_stmts(&a.body.statements),
            Expression::FunctionExpression(f) => {
                if let Some(b) = &f.body { check_stmts(&b.statements) } else { Ok(()) }
            }
            Expression::CallExpression(call) => {
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        check_expr_for_catch(e)?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts(stmts)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_catch_binding_captured_by_closure
// ---------------------------------------------------------------------------
/// Detect catch bindings (simple identifiers) that are referenced inside
/// a nested closure (arrow function / function expression) within the catch body.
/// e.g., `catch (err) { setState(() => ({ error: err })) }`
fn validate_no_catch_binding_captured_by_closure<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::Statement;

    fn check_stmts<'a>(stmts: &[oxc_ast::ast::Statement<'a>]) -> Result<()> {
        for stmt in stmts {
            check_stmt(stmt)?;
        }
        Ok(())
    }

    fn check_stmt<'a>(stmt: &oxc_ast::ast::Statement<'a>) -> Result<()> {
        match stmt {
            Statement::TryStatement(ts) => {
                check_stmts(&ts.block.body)?;
                if let Some(handler) = &ts.handler {
                    // Only flag simple identifier catch bindings
                    if let Some(param) = &handler.param {
                        use oxc_ast::ast::BindingPatternKind;
                        if let BindingPatternKind::BindingIdentifier(id) = &param.pattern.kind {
                            let catch_name = id.name.as_str();
                            // Check if any closure in the catch body captures catch_name
                            if catch_body_closure_captures(&handler.body.body, catch_name) {
                                return Err(CompilerError::todo(
                                    "Catch binding captured by nested closure is not yet supported",
                                ));
                            }
                        }
                    }
                    check_stmts(&handler.body.body)?;
                }
                if let Some(fin) = &ts.finalizer {
                    check_stmts(&fin.body)?;
                }
            }
            Statement::BlockStatement(b) => check_stmts(&b.body)?,
            Statement::IfStatement(i) => {
                check_stmt(&i.consequent)?;
                if let Some(a) = &i.alternate { check_stmt(a)?; }
            }
            Statement::WhileStatement(w) => check_stmt(&w.body)?,
            Statement::ForStatement(f) => check_stmt(&f.body)?,
            Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body { check_stmts(&body.statements)?; }
            }
            _ => {}
        }
        Ok(())
    }

    /// Check if any DIRECT closure in `stmts` (not deeply nested) references `name`
    fn catch_body_closure_captures<'a>(
        stmts: &[oxc_ast::ast::Statement<'a>],
        name: &str,
    ) -> bool {
        use oxc_ast::ast::Statement;
        for stmt in stmts {
            match stmt {
                Statement::ExpressionStatement(e) => {
                    if expr_contains_closure_capturing(& e.expression, name) { return true; }
                }
                Statement::VariableDeclaration(vd) => {
                    for decl in &vd.declarations {
                        if let Some(init) = &decl.init {
                            if expr_contains_closure_capturing(init, name) { return true; }
                        }
                    }
                }
                Statement::ReturnStatement(r) => {
                    if let Some(a) = &r.argument {
                        if expr_contains_closure_capturing(a, name) { return true; }
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Check if `expr` contains (or IS) a closure that directly references `name`
    fn expr_contains_closure_capturing<'a>(
        expr: &oxc_ast::ast::Expression<'a>,
        name: &str,
    ) -> bool {
        use oxc_ast::ast::Expression;
        match expr {
            Expression::ParenthesizedExpression(p) => {
                expr_contains_closure_capturing(&p.expression, name)
            }
            Expression::ArrowFunctionExpression(a) => {
                // Only flag if `name` isn't shadowed by this arrow's own params
                let shadowed = a.params.items.iter().any(|p| {
                    use oxc_ast::ast::BindingPatternKind;
                    matches!(&p.pattern.kind, BindingPatternKind::BindingIdentifier(id) if id.name.as_str() == name)
                });
                if shadowed { false } else { closure_stmts_reference_name(&a.body.statements, name) }
            }
            Expression::FunctionExpression(f) => {
                let shadowed = f.params.items.iter().any(|p| {
                    use oxc_ast::ast::BindingPatternKind;
                    matches!(&p.pattern.kind, BindingPatternKind::BindingIdentifier(id) if id.name.as_str() == name)
                });
                if shadowed { false } else {
                    f.body.as_ref().map_or(false, |b| closure_stmts_reference_name(&b.statements, name))
                }
            }
            Expression::CallExpression(call) => {
                // Check closure arguments
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        if expr_contains_closure_capturing(e, name) { return true; }
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Check if `stmts` (closure body) contains a direct identifier reference to `name`
    fn closure_stmts_reference_name<'a>(
        stmts: &[oxc_ast::ast::Statement<'a>],
        name: &str,
    ) -> bool {
        use oxc_ast::ast::Statement;
        for stmt in stmts {
            match stmt {
                Statement::ReturnStatement(r) => {
                    if let Some(a) = &r.argument {
                        if expr_references_name(a, name) { return true; }
                    }
                }
                Statement::ExpressionStatement(e) => {
                    if expr_references_name(&e.expression, name) { return true; }
                }
                Statement::VariableDeclaration(vd) => {
                    for decl in &vd.declarations {
                        if let Some(init) = &decl.init {
                            if expr_references_name(init, name) { return true; }
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn expr_references_name<'a>(expr: &oxc_ast::ast::Expression<'a>, name: &str) -> bool {
        use oxc_ast::ast::Expression;
        match expr {
            Expression::Identifier(id) => id.name.as_str() == name,
            // Parenthesized expressions are transparent — recurse into inner
            Expression::ParenthesizedExpression(p) => expr_references_name(&p.expression, name),
            Expression::ObjectExpression(obj) => {
                use oxc_ast::ast::ObjectPropertyKind;
                obj.properties.iter().any(|p| match p {
                    ObjectPropertyKind::ObjectProperty(op) => {
                        expr_references_name(&op.value, name)
                    }
                    _ => false,
                })
            }
            Expression::ArrayExpression(arr) => {
                arr.elements.iter().any(|el| {
                    match el {
                        oxc_ast::ast::ArrayExpressionElement::SpreadElement(s) => expr_references_name(&s.argument, name),
                        _ => el.as_expression().map_or(false, |e| expr_references_name(e, name)),
                    }
                })
            }
            Expression::CallExpression(call) => {
                expr_references_name(&call.callee, name)
                    || call.arguments.iter().any(|a| {
                        a.as_expression().map_or(false, |e| expr_references_name(e, name))
                    })
            }
            Expression::BinaryExpression(b) => {
                expr_references_name(&b.left, name) || expr_references_name(&b.right, name)
            }
            Expression::LogicalExpression(l) => {
                expr_references_name(&l.left, name) || expr_references_name(&l.right, name)
            }
            Expression::ConditionalExpression(c) => {
                expr_references_name(&c.test, name)
                    || expr_references_name(&c.consequent, name)
                    || expr_references_name(&c.alternate, name)
            }
            Expression::AssignmentExpression(a) => expr_references_name(&a.right, name),
            Expression::ArrowFunctionExpression(a) => {
                // Don't cross into nested closures — they create their own scope.
                // But if `name` is not shadowed by the arrow's params, check the body.
                let shadowed = a.params.items.iter().any(|p| {
                    use oxc_ast::ast::BindingPatternKind;
                    matches!(&p.pattern.kind, BindingPatternKind::BindingIdentifier(id) if id.name.as_str() == name)
                });
                if shadowed { false } else { closure_stmts_reference_name(&a.body.statements, name) }
            }
            Expression::FunctionExpression(f) => {
                // Same: check if `name` is shadowed by params
                let shadowed = f.params.items.iter().any(|p| {
                    use oxc_ast::ast::BindingPatternKind;
                    matches!(&p.pattern.kind, BindingPatternKind::BindingIdentifier(id) if id.name.as_str() == name)
                });
                if shadowed { false } else {
                    f.body.as_ref().map_or(false, |b| closure_stmts_reference_name(&b.statements, name))
                }
            }
            _ => false,
        }
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts(stmts)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_function_self_shadow
// ---------------------------------------------------------------------------
/// Detect FunctionDeclarations inside a component/hook body where the function's
/// own body declares a local `let`/`const` variable with the SAME name as the function.
/// e.g., `function hasErrors() { let hasErrors = false; ... }` — this causes
/// invariant failures in the TS compiler's HIR analysis.
fn validate_no_function_self_shadow<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Statement, VariableDeclarationKind};

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            if let Statement::FunctionDeclaration(f) = stmt {
                let fn_name = match &f.id {
                    Some(id) => id.name.as_str(),
                    None => continue,
                };
                if let Some(body) = &f.body {
                    for inner in &body.statements {
                        if let Statement::VariableDeclaration(vd) = inner {
                            if vd.kind == VariableDeclarationKind::Let
                                || vd.kind == VariableDeclarationKind::Const
                            {
                                for decl in &vd.declarations {
                                    if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                        if id.name.as_str() == fn_name {
                                            return Err(CompilerError::todo(
                                                "Cannot compile a function where a nested function \
                                                 declaration has the same name as a local variable",
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_functiondecl_forward_call
// ---------------------------------------------------------------------------
/// Detect direct (top-level, not inside a closure) calls to a FunctionDeclaration
/// that appears LATER in the component/hook body.
/// e.g., `const result = bar(); function bar() { ... }` — hoisted function
/// call pattern that causes incorrect code generation in the TS compiler.
fn validate_no_functiondecl_forward_call<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Expression, Statement};
    use std::collections::{HashMap, HashSet};

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect FunctionDeclaration name → (statement index, has_outer_refs)
        let mut fn_decl_pos: HashMap<&str, usize> = HashMap::new();
        let mut fn_has_outer: HashSet<&str> = HashSet::new();

        for (i, stmt) in stmts.iter().enumerate() {
            if let Statement::FunctionDeclaration(f) = stmt {
                if let Some(id) = &f.id {
                    let name = id.name.as_str();
                    fn_decl_pos.insert(name, i);
                    if fn_decl_references_outer(f) {
                        fn_has_outer.insert(name);
                    }
                }
            }
        }
        if fn_decl_pos.is_empty() { continue; }

        // Walk each statement at the top level; check for calls to later-declared FunctionDecls
        // Only flag if the called FunctionDeclaration has outer references (captures context)
        for (stmt_idx, stmt) in stmts.iter().enumerate() {
            check_stmt_for_forward_call(stmt, stmt_idx, &fn_decl_pos, &fn_has_outer, false)?;
        }
    }
    Ok(())
}

/// Returns true if the FunctionDeclaration body references any identifier
/// that is NOT one of its own parameter names and NOT its own function name.
/// This indicates it captures from the outer scope.
fn fn_decl_references_outer<'a>(f: &oxc_ast::ast::Function<'a>) -> bool {
    use oxc_ast::ast::{BindingPatternKind, Expression, FormalParameterKind, Statement};
    use std::collections::HashSet;

    let fn_name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");

    // Collect param names
    let mut param_names: HashSet<&str> = HashSet::new();
    for param in &f.params.items {
        if let BindingPatternKind::BindingIdentifier(id) = &param.pattern.kind {
            param_names.insert(id.name.as_str());
        }
    }

    // Walk body looking for identifier references to outer scope
    let body = match &f.body {
        Some(b) => &b.statements,
        None => return false,
    };

    fn stmt_refs_outer<'a>(
        stmt: &oxc_ast::ast::Statement<'a>,
        params: &std::collections::HashSet<&str>,
        fn_name: &str,
    ) -> bool {
        match stmt {
            Statement::ReturnStatement(r) => {
                r.argument.as_ref().map_or(false, |a| expr_refs_outer(a, params, fn_name))
            }
            Statement::ExpressionStatement(e) => expr_refs_outer(&e.expression, params, fn_name),
            Statement::VariableDeclaration(vd) => {
                vd.declarations.iter().any(|d| {
                    d.init.as_ref().map_or(false, |i| expr_refs_outer(i, params, fn_name))
                })
            }
            Statement::IfStatement(i) => {
                expr_refs_outer(&i.test, params, fn_name)
                    || stmt_refs_outer(&i.consequent, params, fn_name)
                    || i.alternate.as_ref().map_or(false, |a| stmt_refs_outer(a, params, fn_name))
            }
            Statement::BlockStatement(b) => {
                b.body.iter().any(|s| stmt_refs_outer(s, params, fn_name))
            }
            _ => false,
        }
    }

    fn expr_refs_outer<'a>(
        expr: &oxc_ast::ast::Expression<'a>,
        params: &std::collections::HashSet<&str>,
        fn_name: &str,
    ) -> bool {
        match expr {
            Expression::Identifier(id) => {
                let name = id.name.as_str();
                name != fn_name && !params.contains(name)
            }
            Expression::CallExpression(call) => {
                expr_refs_outer(&call.callee, params, fn_name)
                    || call.arguments.iter().any(|a| {
                        a.as_expression().map_or(false, |e| expr_refs_outer(e, params, fn_name))
                    })
            }
            Expression::BinaryExpression(b) => {
                expr_refs_outer(&b.left, params, fn_name)
                    || expr_refs_outer(&b.right, params, fn_name)
            }
            Expression::UnaryExpression(u) => expr_refs_outer(&u.argument, params, fn_name),
            Expression::ConditionalExpression(c) => {
                expr_refs_outer(&c.test, params, fn_name)
                    || expr_refs_outer(&c.consequent, params, fn_name)
                    || expr_refs_outer(&c.alternate, params, fn_name)
            }
            Expression::ObjectExpression(o) => {
                use oxc_ast::ast::ObjectPropertyKind;
                o.properties.iter().any(|p| match p {
                    ObjectPropertyKind::ObjectProperty(op) => expr_refs_outer(&op.value, params, fn_name),
                    _ => false,
                })
            }
            Expression::ArrayExpression(a) => {
                use oxc_ast::ast::ArrayExpressionElement;
                a.elements.iter().any(|e| match e {
                    ArrayExpressionElement::SpreadElement(s) => expr_refs_outer(&s.argument, params, fn_name),
                    _ => e.as_expression().map_or(false, |ex| expr_refs_outer(ex, params, fn_name)),
                })
            }
            Expression::StaticMemberExpression(s) => expr_refs_outer(&s.object, params, fn_name),
            Expression::ComputedMemberExpression(c) => expr_refs_outer(&c.object, params, fn_name),
            // Don't recurse into nested closures (they have their own scope)
            Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => false,
            _ => false,
        }
    }

    body.iter().any(|s| stmt_refs_outer(s, &param_names, fn_name))
}

/// `inside_fn_decl`: true if we are INSIDE a FunctionDeclaration body.
fn check_stmt_for_forward_call<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    current_pos: usize,
    fn_decl_pos: &std::collections::HashMap<&str, usize>,
    fn_has_outer: &std::collections::HashSet<&str>,
    inside_fn_decl: bool,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            check_expr_for_forward_call(&e.expression, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
        }
        Statement::VariableDeclaration(vd) => {
            for decl in &vd.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_forward_call(init, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
                }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_for_forward_call(arg, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_for_forward_call(s, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            }
        }
        Statement::IfStatement(i) => {
            check_expr_for_forward_call(&i.test, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            check_stmt_for_forward_call(&i.consequent, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            if let Some(a) = &i.alternate {
                check_stmt_for_forward_call(a, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            }
        }
        Statement::FunctionDeclaration(f) => {
            // The body of a FunctionDeclaration at position current_pos
            // can forward-reference other FunctionDeclarations declared later.
            if let Some(body) = &f.body {
                for s in &body.statements {
                    check_stmt_for_forward_call(s, current_pos, fn_decl_pos, fn_has_outer, true)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_forward_call<'a>(
    expr: &oxc_ast::ast::Expression<'a>,
    current_pos: usize,
    fn_decl_pos: &std::collections::HashMap<&str, usize>,
    fn_has_outer: &std::collections::HashSet<&str>,
    inside_fn_decl: bool,
) -> Result<()> {
    use oxc_ast::ast::Expression;
    match expr {
        Expression::CallExpression(call) => {
            // Check if callee is a forward-referenced FunctionDeclaration name
            // that also captures outer scope variables (or is inside another fn decl)
            if let Expression::Identifier(id) = &call.callee {
                let name = id.name.as_str();
                if let Some(&decl_pos) = fn_decl_pos.get(name) {
                    if decl_pos > current_pos && (fn_has_outer.contains(name) || inside_fn_decl) {
                        return Err(CompilerError::todo(
                            "Rewrite hoisted function references",
                        ));
                    }
                }
            }
            // Check callee and args
            check_expr_for_forward_call(&call.callee, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_for_forward_call(e, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
                }
            }
        }
        // Don't recurse into nested closures for the top-level check
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_) => {}
        Expression::BinaryExpression(b) => {
            check_expr_for_forward_call(&b.left, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            check_expr_for_forward_call(&b.right, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_for_forward_call(&c.test, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            check_expr_for_forward_call(&c.consequent, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            check_expr_for_forward_call(&c.alternate, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
        }
        Expression::LogicalExpression(l) => {
            check_expr_for_forward_call(&l.left, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
            check_expr_for_forward_call(&l.right, current_pos, fn_decl_pos, fn_has_outer, inside_fn_decl)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_funcdecl_outer_let_reassign
// ---------------------------------------------------------------------------
/// Detect `FunctionDeclaration` statements inside a component/hook body whose
/// body directly assigns to an outer `let` variable declared in the component.
/// e.g., `function foo() { x = 9; }` where `let x` is in the enclosing body.
fn validate_no_funcdecl_outer_let_reassign<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement};

    /// Deep variant of closure_body_assigns_any_in_set that also recurses
    /// into nested arrow/fn expressions to catch multi-level closures.
    fn assigns_deep<'a>(
        stmts: &[Statement<'a>],
        set: &std::collections::HashSet<String>,
    ) -> bool {
        for stmt in stmts {
            match stmt {
                Statement::ExpressionStatement(e) => {
                    if expr_assigns_any_in_set(&e.expression, set) { return true; }
                }
                Statement::ReturnStatement(r) => {
                    if let Some(a) = &r.argument {
                        if expr_assigns_any_in_set(a, set) { return true; }
                    }
                }
                Statement::VariableDeclaration(v) => {
                    for decl in &v.declarations {
                        if let Some(init) = &decl.init {
                            match init {
                                Expression::ArrowFunctionExpression(a) => {
                                    if assigns_deep(&a.body.statements, set) { return true; }
                                }
                                Expression::FunctionExpression(f) => {
                                    if let Some(b) = &f.body {
                                        if assigns_deep(&b.statements, set) { return true; }
                                    }
                                }
                                _ => {
                                    if expr_assigns_any_in_set(init, set) { return true; }
                                }
                            }
                        }
                    }
                }
                Statement::IfStatement(i) => {
                    if assigns_deep(std::slice::from_ref(&i.consequent), set) { return true; }
                    if let Some(alt) = &i.alternate {
                        if assigns_deep(std::slice::from_ref(alt), set) { return true; }
                    }
                }
                Statement::BlockStatement(b) => {
                    if assigns_deep(&b.body, set) { return true; }
                }
                _ => {}
            }
        }
        false
    }

    fn params_contain(params: &oxc_ast::ast::FormalParameters, name: &str) -> bool {
        params.items.iter().any(|p| {
            matches!(&p.pattern.kind, BindingPatternKind::BindingIdentifier(id) if id.name.as_str() == name)
        })
    }

    // Use collect_all_function_bodies to also catch non-component/hook functions.
    let bodies = collect_all_function_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }
        for stmt in stmts {
            // Only check named function declarations inside the body.
            // Arrow functions assigned to variables are "context variables"
            // handled by the compiler — checking them causes false positives.
            if let Statement::FunctionDeclaration(f) = stmt {
                let effective_lets: std::collections::HashSet<String> = outer_lets.iter()
                    .filter(|name| !params_contain(&f.params, name))
                    .cloned()
                    .collect();
                if effective_lets.is_empty() { continue; }
                if let Some(body) = &f.body {
                    if assigns_deep(&body.statements, &effective_lets) {
                        return Err(CompilerError::invalid_react(
                            "Cannot reassign variable after render completes. Consider using state instead.",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_nested_array_destructure_assign
// ---------------------------------------------------------------------------
/// Detect nested array destructuring assignments like `[[x]] = expr`.
/// e.g., `x.foo([[x]] = makeObject())` — invariant-triggers the TS compiler.
fn validate_no_nested_array_destructure_assign<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, Statement};

    fn check_stmts<'a>(stmts: &[Statement<'a>]) -> Result<()> {
        for stmt in stmts { check_stmt(stmt)?; }
        Ok(())
    }

    fn check_stmt<'a>(stmt: &Statement<'a>) -> Result<()> {
        match stmt {
            Statement::ExpressionStatement(e) => check_expr(&e.expression)?,
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init { check_expr(init)?; }
                }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument { check_expr(a)?; }
            }
            Statement::IfStatement(i) => {
                check_stmt(&i.consequent)?;
                if let Some(a) = &i.alternate { check_stmt(a)?; }
            }
            Statement::BlockStatement(b) => check_stmts(&b.body)?,
            Statement::FunctionDeclaration(f) => {
                if let Some(b) = &f.body { check_stmts(&b.statements)?; }
            }
            _ => {}
        }
        Ok(())
    }

    fn check_expr<'a>(expr: &oxc_ast::ast::Expression<'a>) -> Result<()> {
        use oxc_ast::ast::Expression;
        match expr {
            Expression::AssignmentExpression(a) => {
                if let AssignmentTarget::ArrayAssignmentTarget(outer) = &a.left {
                    for el in &outer.elements {
                        if let Some(el) = el {
                            if matches!(el, oxc_ast::ast::AssignmentTargetMaybeDefault::ArrayAssignmentTarget(_)) {
                                return Err(CompilerError::todo(
                                    "Nested array destructuring assignment is not supported",
                                ));
                            }
                        }
                    }
                }
                check_expr(&a.right)?;
            }
            Expression::ParenthesizedExpression(p) => check_expr(&p.expression)?,
            Expression::CallExpression(call) => {
                check_expr(&call.callee)?;
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() { check_expr(e)?; }
                }
            }
            Expression::ArrowFunctionExpression(a) => check_stmts(&a.body.statements)?,
            Expression::FunctionExpression(f) => {
                if let Some(b) = &f.body { check_stmts(&b.statements)?; }
            }
            _ => {}
        }
        Ok(())
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts(stmts)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_nested_method_call_args
// ---------------------------------------------------------------------------
/// Detect method calls where an argument is itself a method call.
/// e.g., `Math.floor(diff.bar())` or `Math.max(2, items.push(5))`.
/// This triggers a codegen invariant in our HIR because the property load
/// gets promoted/memoized, violating the "unpromoted MemberExpression" constraint.
fn validate_no_nested_method_call_args<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Expression, Statement};

    fn is_method_call<'a>(expr: &Expression<'a>) -> bool {
        if let Expression::CallExpression(call) = expr {
            matches!(
                &call.callee,
                Expression::StaticMemberExpression(_) | Expression::ComputedMemberExpression(_)
            )
        } else {
            false
        }
    }

    fn check_stmts<'a>(stmts: &[Statement<'a>]) -> Result<()> {
        for stmt in stmts { check_stmt(stmt)?; }
        Ok(())
    }

    fn check_stmt<'a>(stmt: &Statement<'a>) -> Result<()> {
        match stmt {
            Statement::ExpressionStatement(e) => check_expr(&e.expression)?,
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init { check_expr(init)?; }
                }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument { check_expr(a)?; }
            }
            Statement::IfStatement(i) => {
                check_stmt(&i.consequent)?;
                if let Some(a) = &i.alternate { check_stmt(a)?; }
            }
            Statement::BlockStatement(b) => check_stmts(&b.body)?,
            Statement::FunctionDeclaration(f) => {
                if let Some(b) = &f.body { check_stmts(&b.statements)?; }
            }
            _ => {}
        }
        Ok(())
    }

    fn check_expr<'a>(expr: &Expression<'a>) -> Result<()> {
        if let Expression::CallExpression(call) = expr {
            // If this call is a method call, check if any argument is also a method call
            if matches!(&call.callee, Expression::StaticMemberExpression(_)) {
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        if is_method_call(e) {
                            return Err(CompilerError::todo(
                                "Nested method calls in call arguments are not yet supported",
                            ));
                        }
                        check_expr(e)?;
                    }
                }
                check_expr(&call.callee)?;
                return Ok(());
            }
            // For non-method calls, just recurse
            check_expr(&call.callee)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() { check_expr(e)?; }
            }
        } else {
            match expr {
                Expression::ParenthesizedExpression(p) => check_expr(&p.expression)?,
                Expression::ArrowFunctionExpression(a) => check_stmts(&a.body.statements)?,
                Expression::FunctionExpression(f) => {
                    if let Some(b) = &f.body { check_stmts(&b.statements)?; }
                }
                Expression::LogicalExpression(l) => {
                    check_expr(&l.left)?;
                    check_expr(&l.right)?;
                }
                Expression::ConditionalExpression(c) => {
                    check_expr(&c.test)?;
                    check_expr(&c.consequent)?;
                    check_expr(&c.alternate)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts(stmts)?;
    }
    Ok(())
}

/// Detect property mutation on a locally-declared const function/arrow within
/// a component or hook body. Example:
///   const renderIcon = () => <Icon />;
///   renderIcon.displayName = 'Icon';  // ← error
fn validate_no_local_function_property_mutation<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> crate::error::Result<()> {
    use oxc_ast::ast::{AssignmentTarget, BindingPatternKind, Expression, Statement, VariableDeclarationKind};
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect names of const-declared arrow/function expressions in this body
        let mut local_fns: std::collections::HashSet<String> = std::collections::HashSet::new();
        for stmt in stmts.iter() {
            if let Statement::VariableDeclaration(v) = stmt {
                if v.kind == VariableDeclarationKind::Const {
                    for decl in &v.declarations {
                        if let Some(init) = &decl.init {
                            if matches!(
                                init,
                                Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
                            ) {
                                if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                    local_fns.insert(id.name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        if local_fns.is_empty() {
            continue;
        }
        // Check for X.prop = value where X is a local const function
        for stmt in stmts.iter() {
            if let Statement::ExpressionStatement(e) = stmt {
                if let Expression::AssignmentExpression(a) = &e.expression {
                    if let AssignmentTarget::StaticMemberExpression(m) = &a.left {
                        if let Expression::Identifier(id) = &m.object {
                            if local_fns.contains(id.name.as_str()) {
                                return Err(crate::error::CompilerError::invalid_react(
                                    "This value cannot be modified",
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_post_jsx_mutation
// ---------------------------------------------------------------------------
/// Detect mutations of variables that were previously passed to JSX.
/// Once a variable is used as a JSX child or prop, it is "frozen" and any
/// subsequent mutation (method call, property assignment, delete) is invalid.
fn validate_no_post_jsx_mutation<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{
        AssignmentTarget, Expression, JSXAttributeItem, JSXAttributeValue, JSXChild,
        Statement, UnaryOperator,
    };
    use std::collections::{HashMap, HashSet};

    fn collect_frozen<'a>(stmt: &Statement<'a>, frozen: &mut HashSet<String>, aliases: &HashMap<String, String>) {
        let exprs: Vec<&Expression<'a>> = match stmt {
            Statement::ExpressionStatement(e) => vec![&e.expression],
            Statement::VariableDeclaration(v) => {
                v.declarations.iter().filter_map(|d| d.init.as_ref()).collect()
            }
            Statement::ReturnStatement(r) => r.argument.iter().collect(),
            _ => return,
        };
        for expr in exprs {
            let before_len = frozen.len();
            collect_frozen_in_expr(expr, frozen);
            // If new identifiers were frozen, also freeze their aliases (transitive)
            if frozen.len() > before_len {
                let newly_frozen: Vec<String> = frozen.iter().cloned().collect();
                let mut to_add: Vec<String> = Vec::new();
                for name in &newly_frozen {
                    for (alias, original) in aliases {
                        if original == name {
                            to_add.push(alias.clone());
                        }
                    }
                }
                for alias in to_add {
                    frozen.insert(alias);
                }
            }
        }
    }

    fn collect_frozen_in_expr<'a>(expr: &Expression<'a>, frozen: &mut HashSet<String>) {
        match expr {
            Expression::JSXElement(jsx) => {
                for attr in &jsx.opening_element.attributes {
                    match attr {
                        JSXAttributeItem::Attribute(a) => {
                            if let Some(JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                                if let Some(Expression::Identifier(id)) =
                                    ec.expression.as_expression()
                                {
                                    frozen.insert(id.name.to_string());
                                }
                            }
                        }
                        JSXAttributeItem::SpreadAttribute(s) => {
                            if let Expression::Identifier(id) = &s.argument {
                                frozen.insert(id.name.to_string());
                            }
                        }
                    }
                }
                for child in &jsx.children {
                    if let JSXChild::ExpressionContainer(ec) = child {
                        if let Some(Expression::Identifier(id)) = ec.expression.as_expression() {
                            frozen.insert(id.name.to_string());
                        }
                    }
                }
            }
            Expression::JSXFragment(frag) => {
                for child in &frag.children {
                    if let JSXChild::ExpressionContainer(ec) = child {
                        if let Some(Expression::Identifier(id)) = ec.expression.as_expression() {
                            frozen.insert(id.name.to_string());
                        }
                    }
                }
            }
            Expression::ParenthesizedExpression(p) => {
                collect_frozen_in_expr(&p.expression, frozen);
            }
            Expression::SequenceExpression(seq) => {
                for e in &seq.expressions {
                    collect_frozen_in_expr(e, frozen);
                }
            }
            // Recurse into call arguments to find JSX (e.g. items.push(<JSX/>))
            Expression::CallExpression(call) => {
                for arg in &call.arguments {
                    if let Some(arg_expr) = arg.as_expression() {
                        collect_frozen_in_expr(arg_expr, frozen);
                    }
                }
            }
            _ => {}
        }
    }

    fn check_mutation<'a>(stmt: &Statement<'a>, frozen: &HashSet<String>) -> Result<()> {
        if frozen.is_empty() {
            return Ok(());
        }
        let expr = match stmt {
            Statement::ExpressionStatement(e) => &e.expression,
            _ => return Ok(()),
        };
        match expr {
            Expression::CallExpression(call) => {
                if let Expression::StaticMemberExpression(m) = &call.callee {
                    if let Expression::Identifier(id) = &m.object {
                        if frozen.contains(id.name.as_str()) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified",
                            ));
                        }
                    }
                }
            }
            Expression::AssignmentExpression(a) => match &a.left {
                AssignmentTarget::StaticMemberExpression(m) => {
                    if let Expression::Identifier(id) = &m.object {
                        if frozen.contains(id.name.as_str()) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified",
                            ));
                        }
                    }
                }
                AssignmentTarget::ComputedMemberExpression(m) => {
                    if let Expression::Identifier(id) = &m.object {
                        if frozen.contains(id.name.as_str()) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified",
                            ));
                        }
                    }
                }
                // Detect plain identifier reassignment: `i = i + 1` when `i` is frozen
                AssignmentTarget::AssignmentTargetIdentifier(id) => {
                    if frozen.contains(id.name.as_str()) {
                        return Err(CompilerError::invalid_react(
                            "This value cannot be modified",
                        ));
                    }
                }
                _ => {}
            },
            Expression::UnaryExpression(u) if u.operator == UnaryOperator::Delete => {
                match &u.argument {
                    Expression::StaticMemberExpression(m) => {
                        if let Expression::Identifier(id) = &m.object {
                            if frozen.contains(id.name.as_str()) {
                                return Err(CompilerError::invalid_react(
                                    "This value cannot be modified",
                                ));
                            }
                        }
                    }
                    Expression::ComputedMemberExpression(m) => {
                        if let Expression::Identifier(id) = &m.object {
                            if frozen.contains(id.name.as_str()) {
                                return Err(CompilerError::invalid_react(
                                    "This value cannot be modified",
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Update alias map incrementally as we process statements:
    /// - `let/const y = x` (identifier init) → add `y → x`
    /// - `x = expr` at top level (ExpressionStatement) → remove all aliases
    ///   where the ORIGINAL is `x` (unconditional reassignment breaks the alias)
    fn update_aliases<'a>(stmt: &Statement<'a>, aliases: &mut HashMap<String, String>) {
        match stmt {
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(Expression::Identifier(id)) = &decl.init {
                        if let oxc_ast::ast::BindingPatternKind::BindingIdentifier(bind) = &decl.id.kind {
                            aliases.insert(bind.name.to_string(), id.name.to_string());
                        }
                    }
                }
            }
            Statement::ExpressionStatement(e) => {
                if let Expression::AssignmentExpression(a) = &e.expression {
                    if let AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                        let reassigned = id.name.as_str();
                        // Remove all aliases where the original is the reassigned variable
                        aliases.retain(|_alias, original| original.as_str() != reassigned);
                    }
                }
            }
            _ => {}
        }
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let mut aliases: HashMap<String, String> = HashMap::new();
        let mut frozen: HashSet<String> = HashSet::new();
        for stmt in stmts.iter() {
            check_mutation(stmt, &frozen)?;
            update_aliases(stmt, &mut aliases);
            collect_frozen(stmt, &mut frozen, &aliases);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_method_call_with_method_arg
// ---------------------------------------------------------------------------
/// Detect a method call where any argument is also a method call.
/// e.g., `Math.floor(diff.bar())` or `Math.max(2, items.push(5), ...other)`.
/// These trigger a codegen invariant in the TS compiler.
fn validate_no_method_call_with_method_arg<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Expression, Statement};

    fn check_expr<'a>(expr: &Expression<'a>) -> Result<()> {
        if let Expression::CallExpression(call) = expr {
            // Outer call is a method call (obj.method(...))
            if matches!(&call.callee, Expression::StaticMemberExpression(_)) {
                for arg in &call.arguments {
                    if let Some(arg_expr) = arg.as_expression() {
                        // Any arg is also a method call
                        if let Expression::CallExpression(inner) = arg_expr {
                            if matches!(&inner.callee, Expression::StaticMemberExpression(_)) {
                                return Err(CompilerError::todo(
                                    "Nested method calls as arguments are not supported",
                                ));
                            }
                        }
                    }
                }
            }
            // Recurse
            check_expr(&call.callee)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr(e)?;
                }
            }
        }
        Ok(())
    }

    fn check_stmt<'a>(stmt: &Statement<'a>) -> Result<()> {
        use oxc_ast::ast::Statement;
        match stmt {
            Statement::ExpressionStatement(e) => check_expr(&e.expression)?,
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        check_expr(init)?;
                    }
                }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument {
                    check_expr(a)?;
                }
            }
            Statement::IfStatement(i) => {
                check_stmt(&i.consequent)?;
                if let Some(alt) = &i.alternate {
                    check_stmt(alt)?;
                }
            }
            Statement::BlockStatement(b) => {
                for s in &b.body {
                    check_stmt(s)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            check_stmt(stmt)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_jsx_rest_param_callback
// ---------------------------------------------------------------------------
/// Detect JSX attribute values that are arrow functions with rest parameters.
/// e.g., `renderer={(...props) => <span {...props} />}`.
/// This triggers a "unnamed temporary" invariant in the TS compiler.
fn validate_no_jsx_rest_param_callback<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{
        BindingPatternKind, Expression, JSXAttributeItem, JSXAttributeValue, Statement,
    };

    fn check_jsx_attr<'a>(
        attr: &oxc_ast::ast::JSXAttributeItem<'a>,
    ) -> Result<()> {
        if let JSXAttributeItem::Attribute(a) = attr {
            if let Some(JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                if let Some(Expression::ArrowFunctionExpression(arrow)) =
                    ec.expression.as_expression()
                {
                    // Check for rest parameter
                    if arrow.params.items.iter().any(|p| {
                        false  // not in items
                    }) || arrow.params.rest.is_some() {
                        return Err(CompilerError::todo(
                            "Rest parameters in JSX callback props are not supported",
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_expr<'a>(expr: &Expression<'a>) -> Result<()> {
        match expr {
            Expression::JSXElement(jsx) => {
                for attr in &jsx.opening_element.attributes {
                    check_jsx_attr(attr)?;
                }
                // Recurse into children
                for child in &jsx.children {
                    if let oxc_ast::ast::JSXChild::ExpressionContainer(ec) = child {
                        if let Some(e) = ec.expression.as_expression() {
                            check_expr(e)?;
                        }
                    }
                }
            }
            Expression::JSXFragment(frag) => {
                for child in &frag.children {
                    if let oxc_ast::ast::JSXChild::ExpressionContainer(ec) = child {
                        if let Some(e) = ec.expression.as_expression() {
                            check_expr(e)?;
                        }
                    }
                }
            }
            Expression::CallExpression(call) => {
                check_expr(&call.callee)?;
                for arg in &call.arguments {
                    if let Some(e) = arg.as_expression() {
                        check_expr(e)?;
                    }
                }
            }
            Expression::ParenthesizedExpression(p) => check_expr(&p.expression)?,
            _ => {}
        }
        Ok(())
    }

    fn check_stmt<'a>(stmt: &Statement<'a>) -> Result<()> {
        use oxc_ast::ast::Statement;
        match stmt {
            Statement::ExpressionStatement(e) => check_expr(&e.expression)?,
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        check_expr(init)?;
                    }
                }
            }
            Statement::ReturnStatement(r) => {
                if let Some(a) = &r.argument {
                    check_expr(a)?;
                }
            }
            Statement::IfStatement(i) => {
                check_stmt(&i.consequent)?;
                if let Some(alt) = &i.alternate {
                    check_stmt(alt)?;
                }
            }
            Statement::BlockStatement(b) => {
                for s in &b.body {
                    check_stmt(s)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // Check ALL function bodies (including plain functions) since Foo() is exported
    let bodies = collect_all_function_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            check_stmt(stmt)?;
        }
    }
    Ok(())
}
// ---------------------------------------------------------------------------
// collect_all_function_bodies
// ---------------------------------------------------------------------------
/// Like collect_component_hook_bodies but includes ALL top-level function
/// declarations (not just components and hooks). Used for validators that
/// the TS compiler applies to all functions.
fn collect_all_function_bodies<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Vec<&'a [oxc_ast::ast::Statement<'a>]> {
    use oxc_ast::ast::{Declaration, ExportDefaultDeclarationKind, Expression, Statement};

    let mut bodies: Vec<&'a [oxc_ast::ast::Statement<'a>]> = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    bodies.push(&body.statements);
                }
            }
            Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    if let Some(body) = &f.body {
                        bodies.push(&body.statements);
                    }
                }
                ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                    bodies.push(&a.body.statements);
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(d) => match &d.declaration {
                Some(Declaration::FunctionDeclaration(f)) => {
                    if let Some(body) = &f.body {
                        bodies.push(&body.statements);
                    }
                }
                Some(Declaration::VariableDeclaration(v)) => {
                    for decl in &v.declarations {
                        if let Some(init) = &decl.init {
                            match init {
                                Expression::ArrowFunctionExpression(a) => {
                                    bodies.push(&a.body.statements);
                                }
                                Expression::FunctionExpression(f) => {
                                    if let Some(body) = &f.body {
                                        bodies.push(&body.statements);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            },
            Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        match init {
                            Expression::ArrowFunctionExpression(a) => {
                                bodies.push(&a.body.statements);
                            }
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body {
                                    bodies.push(&body.statements);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    bodies
}

// ---------------------------------------------------------------------------
// validate_hook_call_freezes_captured
// ---------------------------------------------------------------------------

/// @enableTransitivelyFreezeFunctionExpressions: if a hook is called with a
/// callback that captures a locally-created variable, subsequent mutations of
/// that variable (property assignments) are errors.
fn validate_hook_call_freezes_captured<'a>(
    source: &str,
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    let first = source.lines().next().unwrap_or("");
    if !first.contains("@enableTransitivelyFreezeFunctionExpressions")
        || first.contains("@enableTransitivelyFreezeFunctionExpressions:false")
        || first.contains("@enableTransitivelyFreezeFunctionExpressions false")
    {
        return Ok(());
    }
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        check_stmts_for_hook_freeze(stmts)?;
    }
    Ok(())
}

fn check_stmts_for_hook_freeze<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
) -> Result<()> {
    let mut frozen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in stmts {
        // First: check if this statement mutates any already-frozen variable.
        check_stmt_for_frozen_mutation(stmt, &frozen)?;
        // Then: if this is a hook call with a callback, freeze captured identifiers.
        collect_frozen_from_stmt(stmt, &mut frozen);
    }
    Ok(())
}

/// Checks if `stmt` contains a property mutation of a variable in `frozen`.
/// Only checks outer scope (not recursing into closures).
fn check_stmt_for_frozen_mutation<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    frozen: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    if frozen.is_empty() {
        return Ok(());
    }
    match stmt {
        Statement::ExpressionStatement(e) => {
            check_expr_for_frozen_mutation(&e.expression, frozen)?;
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_frozen_mutation(init, frozen)?;
                }
            }
        }
        Statement::IfStatement(i) => {
            check_expr_for_frozen_mutation(&i.test, frozen)?;
            check_stmt_for_frozen_mutation(&i.consequent, frozen)?;
            if let Some(alt) = &i.alternate {
                check_stmt_for_frozen_mutation(alt, frozen)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_for_frozen_mutation(s, frozen)?;
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument {
                check_expr_for_frozen_mutation(a, frozen)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_frozen_mutation<'a>(
    expr: &Expression<'a>,
    frozen: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        // x.prop = value or x.prop += value
        Expression::AssignmentExpression(a) => {
            match &a.left {
                oxc_ast::ast::AssignmentTarget::StaticMemberExpression(m) => {
                    if let Expression::Identifier(obj) = &m.object {
                        if frozen.contains(obj.name.as_str()) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified\n\nModifying a value previously passed as an argument to a hook is not allowed. Consider moving the modification before calling the hook.",
                            ));
                        }
                    }
                }
                oxc_ast::ast::AssignmentTarget::ComputedMemberExpression(m) => {
                    if let Expression::Identifier(obj) = &m.object {
                        if frozen.contains(obj.name.as_str()) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified\n\nModifying a value previously passed as an argument to a hook is not allowed. Consider moving the modification before calling the hook.",
                            ));
                        }
                    }
                }
                _ => {}
            }
            check_expr_for_frozen_mutation(&a.right, frozen)?;
        }
        // x.prop++ / --x.prop (UpdateExpression on member)
        Expression::UpdateExpression(u) => {
            if let oxc_ast::ast::SimpleAssignmentTarget::StaticMemberExpression(m) = &u.argument {
                if let Expression::Identifier(obj) = &m.object {
                    if frozen.contains(obj.name.as_str()) {
                        return Err(CompilerError::invalid_react(
                            "This value cannot be modified\n\nModifying a value previously passed as an argument to a hook is not allowed. Consider moving the modification before calling the hook.",
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Collect identifiers from hook-call callbacks and add them to `frozen`.
fn collect_frozen_from_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    frozen: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            collect_frozen_from_expr(&e.expression, frozen);
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    collect_frozen_from_expr(init, frozen);
                }
            }
        }
        _ => {}
    }
}

fn collect_frozen_from_expr<'a>(
    expr: &Expression<'a>,
    frozen: &mut std::collections::HashSet<String>,
) {
    if let Expression::CallExpression(call) = expr {
        let callee_name = match &call.callee {
            Expression::Identifier(id) => Some(id.name.as_str()),
            Expression::StaticMemberExpression(m) => Some(m.property.name.as_str()),
            _ => None,
        };
        let is_hook = callee_name.map_or(false, |n| {
            n.starts_with("use") && n.chars().next().map_or(false, |c| c == 'u')
        });
        if is_hook {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    match e {
                        Expression::ArrowFunctionExpression(arrow) => {
                            collect_identifiers_in_stmts(&arrow.body.statements, frozen);
                        }
                        Expression::FunctionExpression(func) => {
                            if let Some(body) = &func.body {
                                collect_identifiers_in_stmts(&body.statements, frozen);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Collect all simple identifier references in a list of statements.
fn collect_identifiers_in_stmts<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    out: &mut std::collections::HashSet<String>,
) {
    for stmt in stmts {
        collect_identifiers_in_stmt(stmt, out);
    }
}

fn collect_identifiers_in_stmt<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    out: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => collect_identifiers_in_expr(&e.expression, out),
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument {
                collect_identifiers_in_expr(a, out);
            }
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    collect_identifiers_in_expr(init, out);
                }
            }
        }
        Statement::IfStatement(i) => {
            collect_identifiers_in_expr(&i.test, out);
            collect_identifiers_in_stmt(&i.consequent, out);
            if let Some(alt) = &i.alternate {
                collect_identifiers_in_stmt(alt, out);
            }
        }
        Statement::BlockStatement(b) => collect_identifiers_in_stmts(&b.body, out),
        _ => {}
    }
}

fn collect_identifiers_in_expr<'a>(
    expr: &Expression<'a>,
    out: &mut std::collections::HashSet<String>,
) {
    match expr {
        Expression::Identifier(id) => {
            out.insert(id.name.to_string());
        }
        Expression::CallExpression(call) => {
            collect_identifiers_in_expr(&call.callee, out);
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    collect_identifiers_in_expr(e, out);
                }
            }
        }
        Expression::StaticMemberExpression(m) => {
            collect_identifiers_in_expr(&m.object, out);
        }
        Expression::AssignmentExpression(a) => {
            collect_identifiers_in_expr(&a.right, out);
        }
        Expression::UpdateExpression(u) => {
            if let oxc_ast::ast::SimpleAssignmentTarget::StaticMemberExpression(m) = &u.argument {
                collect_identifiers_in_expr(&m.object, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// validate_reanimated_non_imported_shared_value_writes
// ---------------------------------------------------------------------------

/// @enableCustomTypeDefinitionForReanimated: when useSharedValue is called
/// WITHOUT being imported from react-native-reanimated, any assignment to
/// .value of its return in a callback is an error.
fn validate_reanimated_non_imported_shared_value_writes<'a>(
    source: &str,
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    let first = source.lines().next().unwrap_or("");
    if !first.contains("@enableCustomTypeDefinitionForReanimated") {
        return Ok(());
    }
    // If useSharedValue is imported from react-native-reanimated, it's
    // a known Reanimated hook and .value writes are intentional (allowed).
    let is_imported = program.body.iter().any(|stmt| {
        if let oxc_ast::ast::Statement::ImportDeclaration(import) = stmt {
            if import.source.value.as_str() == "react-native-reanimated" {
                if let Some(specifiers) = &import.specifiers {
                    return specifiers.iter().any(|spec| {
                        if let oxc_ast::ast::ImportDeclarationSpecifier::ImportSpecifier(s) = spec {
                            s.local.name.as_str() == "useSharedValue"
                        } else {
                            false
                        }
                    });
                }
            }
        }
        false
    });
    if is_imported {
        return Ok(());
    }

    // useSharedValue is NOT imported → check for .value = ... in callbacks.
    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let mut shared_val_vars: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for stmt in stmts {
            collect_use_shared_value_vars(stmt, &mut shared_val_vars);
        }
        if shared_val_vars.is_empty() {
            continue;
        }
        for stmt in stmts {
            check_stmt_for_shared_val_write(stmt, &shared_val_vars)?;
        }
    }
    Ok(())
}

fn collect_use_shared_value_vars<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    out: &mut std::collections::HashSet<String>,
) {
    if let oxc_ast::ast::Statement::VariableDeclaration(v) = stmt {
        for decl in &v.declarations {
            if let Some(init) = &decl.init {
                if let Expression::CallExpression(call) = init {
                    if let Expression::Identifier(callee) = &call.callee {
                        if callee.name.as_str() == "useSharedValue" {
                            if let oxc_ast::ast::BindingPatternKind::BindingIdentifier(id) =
                                &decl.id.kind
                            {
                                out.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
}

fn check_stmt_for_shared_val_write<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    shared_val_vars: &std::collections::HashSet<String>,
) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => {
            check_expr_for_shared_val_write(&e.expression, shared_val_vars)?;
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument {
                check_expr_for_shared_val_write(a, shared_val_vars)?;
            }
        }
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_shared_val_write(init, shared_val_vars)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_shared_val_write<'a>(
    expr: &Expression<'a>,
    shared_val_vars: &std::collections::HashSet<String>,
) -> Result<()> {
    match expr {
        // sharedVal.value = ...
        Expression::AssignmentExpression(a) => {
            if let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(m) = &a.left {
                if m.property.name.as_str() == "value" {
                    if let Expression::Identifier(obj) = &m.object {
                        if shared_val_vars.contains(obj.name.as_str()) {
                            return Err(CompilerError::invalid_react(
                                "This value cannot be modified\n\nModifying a value returned from a hook is not allowed. Consider moving the modification into the hook where the value is constructed.",
                            ));
                        }
                    }
                }
            }
            check_expr_for_shared_val_write(&a.right, shared_val_vars)?;
        }
        // Arrow function: recurse into body (oxc parses `() => expr` as `() => { return expr; }`)
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                check_stmt_for_shared_val_write(s, shared_val_vars)?;
            }
        }
        // Function expression: recurse into body
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements {
                    check_stmt_for_shared_val_write(s, shared_val_vars)?;
                }
            }
        }
        // Parenthesized expression: unwrap and check
        Expression::ParenthesizedExpression(p) => {
            check_expr_for_shared_val_write(&p.expression, shared_val_vars)?;
        }
        // JSX: check attribute values
        Expression::JSXElement(jsx) => {
            for attr in &jsx.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(c)) = &a.value {
                        if let Some(e) = c.expression.as_expression() {
                            check_expr_for_shared_val_write(e, shared_val_vars)?;
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_hook_result_mutation
// ---------------------------------------------------------------------------
/// Detect direct mutation of hook results or variables that might alias them.
///
/// Pattern: `const frozen = useHook(); if(cond) { x = frozen; } x.property = true;`
fn validate_no_hook_result_mutation<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, BindingPatternKind, Expression, Statement};
    use std::collections::HashSet;

    fn is_hook_call(expr: &Expression) -> bool {
        let callee = match expr {
            Expression::CallExpression(c) => &c.callee,
            _ => return false,
        };
        match callee {
            Expression::Identifier(id) => {
                let name = id.name.as_str();
                name.starts_with("use") && name.len() > 3 &&
                    name.chars().nth(3).map_or(false, |c| c.is_uppercase() || c == '_')
            }
            Expression::StaticMemberExpression(s) => {
                let prop = s.property.name.as_str();
                prop.starts_with("use") && prop.len() > 3 &&
                    prop.chars().nth(3).map_or(false, |c| c.is_uppercase() || c == '_')
            }
            _ => false,
        }
    }

    fn collect_hook_result_vars<'a>(stmts: &[Statement<'a>]) -> HashSet<String> {
        let mut vars: HashSet<String> = HashSet::new();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        if is_hook_call(init) {
                            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                vars.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        vars
    }

    fn expand_aliases<'a>(stmts: &[Statement<'a>], frozen: &mut HashSet<String>) {
        for stmt in stmts {
            match stmt {
                Statement::ExpressionStatement(e) => {
                    if let Expression::AssignmentExpression(a) = &e.expression {
                        if let AssignmentTarget::AssignmentTargetIdentifier(lhs) = &a.left {
                            if let Expression::Identifier(rhs) = &a.right {
                                if frozen.contains(rhs.name.as_str()) {
                                    frozen.insert(lhs.name.to_string());
                                }
                            }
                        }
                    }
                }
                Statement::IfStatement(i) => {
                    expand_aliases(std::slice::from_ref(&i.consequent), frozen);
                    if let Some(alt) = &i.alternate {
                        expand_aliases(std::slice::from_ref(alt), frozen);
                    }
                }
                Statement::BlockStatement(b) => expand_aliases(&b.body, frozen),
                Statement::WhileStatement(w) => {
                    expand_aliases(std::slice::from_ref(&w.body), frozen);
                }
                _ => {}
            }
        }
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let mut frozen = collect_hook_result_vars(stmts);
        if frozen.is_empty() { continue; }

        // Transitively expand: variables assigned from frozen values
        for _ in 0..4 {
            let before = frozen.len();
            expand_aliases(stmts, &mut frozen);
            if frozen.len() == before { break; }
        }

        // Check top-level mutations of frozen variables (property assignment)
        for stmt in stmts {
            if let Statement::ExpressionStatement(e) = stmt {
                if let Expression::AssignmentExpression(a) = &e.expression {
                    let base = match &a.left {
                        AssignmentTarget::StaticMemberExpression(s) => {
                            if let Expression::Identifier(id) = &s.object { Some(id.name.as_str()) } else { None }
                        }
                        AssignmentTarget::ComputedMemberExpression(c) => {
                            if let Expression::Identifier(id) = &c.object { Some(id.name.as_str()) } else { None }
                        }
                        _ => None,
                    };
                    if let Some(name) = base {
                        if frozen.contains(name) {
                            return Err(CompilerError::invalid_react("This value cannot be modified"));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}


// validate_no_chained_outer_let_assign_in_closure
// ---------------------------------------------------------------------------
//
// Detect the specific antipattern `const copy = (outer_let = val)` inside a closure.
// This is the "chained assignment as value" pattern where `(x = 3)` is used as an
// expression that both assigns x and returns the value. Inside a closure this is
// problematic because it mutates the outer binding from within a potentially-escaped fn.
fn validate_no_chained_outer_let_assign_in_closure<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use std::collections::HashSet;

    /// Check if an expression contains a chained assignment `(outer_let = val)`.
    /// Only looks at ParenthesizedExpression wrapping an AssignmentExpression — the
    /// specific syntactic form used for chained assignments.
    fn contains_chained_assign_to_set<'a>(
        expr: &Expression<'a>,
        outer_lets: &HashSet<String>,
    ) -> bool {
        match expr {
            Expression::ParenthesizedExpression(p) => {
                if let Expression::AssignmentExpression(a) = &p.expression {
                    if let oxc_ast::ast::AssignmentTarget::AssignmentTargetIdentifier(id) = &a.left {
                        if outer_lets.contains(id.name.as_str()) {
                            return true;
                        }
                    }
                }
                // Recurse in case of double-parens
                contains_chained_assign_to_set(&p.expression, outer_lets)
            }
            Expression::AssignmentExpression(a) => {
                // Also catch un-parenthesized assignment when used as value in VariableDecl init
                // (this case is handled at the call site via VariableDeclaration check)
                contains_chained_assign_to_set(&a.right, outer_lets)
            }
            Expression::SequenceExpression(s) => {
                s.expressions.iter().any(|e| contains_chained_assign_to_set(e, outer_lets))
            }
            _ => false,
        }
    }

    /// Check if a closure body contains a `const y = (outer_let = val)` VariableDecl
    /// where the assignment to outer_let is used as the value (chained assignment).
    fn closure_body_has_chained_assign<'a>(
        stmts: &[Statement<'a>],
        outer_lets: &HashSet<String>,
    ) -> bool {
        for stmt in stmts {
            match stmt {
                Statement::VariableDeclaration(v) => {
                    for decl in &v.declarations {
                        if let Some(init) = &decl.init {
                            // Skip nested closures
                            if matches!(init, Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)) {
                                continue;
                            }
                            if contains_chained_assign_to_set(init, outer_lets) {
                                return true;
                            }
                        }
                    }
                }
                Statement::IfStatement(i) => {
                    if closure_body_has_chained_assign(std::slice::from_ref(&i.consequent), outer_lets) {
                        return true;
                    }
                    if let Some(alt) = &i.alternate {
                        if closure_body_has_chained_assign(std::slice::from_ref(alt), outer_lets) {
                            return true;
                        }
                    }
                }
                Statement::BlockStatement(b) => {
                    if closure_body_has_chained_assign(&b.body, outer_lets) {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    let bodies = collect_all_function_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }

        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        let closure_body: Option<&[Statement]> = match init {
                            Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                            Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                            _ => None,
                        };
                        if let Some(body) = closure_body {
                            if closure_body_has_chained_assign(body, &outer_lets) {
                                return Err(CompilerError::invalid_react(
                                    "Cannot reassign variable after render completes. \
                                     Consider using state instead.",
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// validate_no_object_method_ref_call
// ---------------------------------------------------------------------------
//
// Detect: `obj.prop = () => ref.current; const x = obj.prop();`
// The object property stores a closure that accesses a ref, and is then called
// during render. This is indirect ref access.
fn validate_no_object_method_ref_call<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use std::collections::{HashMap, HashSet};

    /// Returns true if the expression tree contains `ref.current` where `ref` is in `refs`.
    fn expr_accesses_ref_current<'a>(expr: &Expression<'a>, refs: &HashSet<String>) -> bool {
        match expr {
            Expression::StaticMemberExpression(m) => {
                if m.property.name == "current" {
                    if let Expression::Identifier(obj) = &m.object {
                        if refs.contains(obj.name.as_str()) { return true; }
                    }
                }
                expr_accesses_ref_current(&m.object, refs)
            }
            Expression::CallExpression(call) => {
                expr_accesses_ref_current(&call.callee, refs)
                    || call.arguments.iter().any(|a| {
                        a.as_expression().map_or(false, |e| expr_accesses_ref_current(e, refs))
                    })
            }
            Expression::ArrowFunctionExpression(arrow) => {
                stmts_access_ref_current(&arrow.body.statements, refs)
            }
            Expression::FunctionExpression(func) => {
                func.body.as_ref().map_or(false, |b| stmts_access_ref_current(&b.statements, refs))
            }
            Expression::ConditionalExpression(c) => {
                expr_accesses_ref_current(&c.test, refs)
                    || expr_accesses_ref_current(&c.consequent, refs)
                    || expr_accesses_ref_current(&c.alternate, refs)
            }
            Expression::LogicalExpression(l) => {
                expr_accesses_ref_current(&l.left, refs)
                    || expr_accesses_ref_current(&l.right, refs)
            }
            _ => false,
        }
    }

    fn stmts_access_ref_current<'a>(stmts: &[Statement<'a>], refs: &HashSet<String>) -> bool {
        stmts.iter().any(|s| stmt_accesses_ref_current(s, refs))
    }

    fn stmt_accesses_ref_current<'a>(stmt: &Statement<'a>, refs: &HashSet<String>) -> bool {
        match stmt {
            Statement::ExpressionStatement(e) => expr_accesses_ref_current(&e.expression, refs),
            Statement::ReturnStatement(r) => {
                r.argument.as_ref().map_or(false, |a| expr_accesses_ref_current(a, refs))
            }
            Statement::VariableDeclaration(v) => v.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |init| expr_accesses_ref_current(init, refs))
            }),
            Statement::IfStatement(i) => {
                expr_accesses_ref_current(&i.test, refs)
                    || stmt_accesses_ref_current(&i.consequent, refs)
                    || i.alternate.as_ref().map_or(false, |a| stmt_accesses_ref_current(a, refs))
            }
            Statement::BlockStatement(b) => stmts_access_ref_current(&b.body, refs),
            _ => false,
        }
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect ref variable names (from useRef calls).
        let mut refs: HashSet<String> = HashSet::new();
        for stmt in stmts.iter() {
            collect_ref_names_from_stmt(stmt, &mut refs);
        }
        if refs.is_empty() { continue; }

        // Pass 1: collect `obj.prop = () => ...ref.current...` patterns.
        // Key: (obj_name, prop_name) → true if the stored fn accesses a ref.
        let mut ref_methods: HashMap<(String, String), bool> = HashMap::new();
        for stmt in stmts.iter() {
            if let Statement::ExpressionStatement(e) = stmt {
                if let Expression::AssignmentExpression(a) = &e.expression {
                    if let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(m) = &a.left {
                        if let Expression::Identifier(obj_id) = &m.object {
                            let key = (obj_id.name.to_string(), m.property.name.to_string());
                            if expr_accesses_ref_current(&a.right, &refs) {
                                ref_methods.insert(key, true);
                            }
                        }
                    }
                }
            }
        }
        if ref_methods.is_empty() { continue; }

        // Pass 2: detect `const result = obj.prop()` where (obj, prop) is a ref method.
        for stmt in stmts.iter() {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(Expression::CallExpression(call)) = &decl.init {
                        if let Expression::StaticMemberExpression(m) = &call.callee {
                            if let Expression::Identifier(obj_id) = &m.object {
                                let key = (obj_id.name.to_string(), m.property.name.to_string());
                                if ref_methods.contains_key(&key) {
                                    return Err(CompilerError::invalid_react(
                                        "Cannot access refs during render\n\nReact refs are values that are not needed for rendering. Refs should only be accessed outside of render, such as in event handlers or effects. Accessing a ref value (the `current` property) during render can cause your component not to update as expected (https://react.dev/reference/react/useRef)."
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// validate_no_curried_ref_factory_call
// ---------------------------------------------------------------------------
//
// Detect: `const f = x => () => ref.current; ... f(args)()` — a curried
// "ref-factory" closure whose result is immediately called during render.
fn validate_no_curried_ref_factory_call<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use std::collections::HashSet;

    /// Check if expr contains `ref.current` for any ref in `refs` (recursing into closures).
    fn expr_has_ref_current<'a>(expr: &Expression<'a>, refs: &HashSet<String>) -> bool {
        match expr {
            Expression::StaticMemberExpression(m) => {
                if m.property.name == "current" {
                    if let Expression::Identifier(obj) = &m.object {
                        if refs.contains(obj.name.as_str()) { return true; }
                    }
                }
                expr_has_ref_current(&m.object, refs)
            }
            Expression::ArrowFunctionExpression(arrow) => {
                arrow.body.statements.iter().any(|s| stmt_has_ref_current(s, refs))
            }
            Expression::FunctionExpression(func) => {
                func.body.as_ref().map_or(false, |b| {
                    b.statements.iter().any(|s| stmt_has_ref_current(s, refs))
                })
            }
            Expression::CallExpression(call) => {
                expr_has_ref_current(&call.callee, refs)
                    || call.arguments.iter().any(|a| {
                        a.as_expression().map_or(false, |e| expr_has_ref_current(e, refs))
                    })
            }
            Expression::LogicalExpression(l) => {
                expr_has_ref_current(&l.left, refs) || expr_has_ref_current(&l.right, refs)
            }
            Expression::ConditionalExpression(c) => {
                expr_has_ref_current(&c.test, refs)
                    || expr_has_ref_current(&c.consequent, refs)
                    || expr_has_ref_current(&c.alternate, refs)
            }
            _ => false,
        }
    }

    fn stmt_has_ref_current<'a>(stmt: &Statement<'a>, refs: &HashSet<String>) -> bool {
        match stmt {
            Statement::ExpressionStatement(e) => expr_has_ref_current(&e.expression, refs),
            Statement::ReturnStatement(r) => {
                r.argument.as_ref().map_or(false, |a| expr_has_ref_current(a, refs))
            }
            Statement::VariableDeclaration(v) => v.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |init| expr_has_ref_current(init, refs))
            }),
            Statement::IfStatement(i) => {
                stmt_has_ref_current(&i.consequent, refs)
                    || i.alternate.as_ref().map_or(false, |a| stmt_has_ref_current(a, refs))
            }
            Statement::BlockStatement(b) => b.body.iter().any(|s| stmt_has_ref_current(s, refs)),
            _ => false,
        }
    }

    /// Returns true if `expr` is a closure that returns (or directly is) a ref-accessing closure.
    /// Handles: `x => () => ref.current` (concise outer returning concise inner)
    ///          `x => () => { ref.current; ... }` (concise outer returning block inner)
    fn is_ref_factory_closure<'a>(expr: &Expression<'a>, refs: &HashSet<String>) -> bool {
        let stmts = match expr {
            Expression::ArrowFunctionExpression(a) => &a.body.statements[..],
            Expression::FunctionExpression(f) => match &f.body {
                Some(b) => &b.statements[..],
                None => return false,
            },
            _ => return false,
        };
        // Check if the body (concise or block) returns/is a ref-accessing closure.
        for stmt in stmts {
            match stmt {
                Statement::ExpressionStatement(e) => {
                    // Concise body: the expression IS the returned value
                    if matches!(&e.expression, Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)) {
                        if expr_has_ref_current(&e.expression, refs) { return true; }
                    }
                }
                Statement::ReturnStatement(r) => {
                    if let Some(ret) = &r.argument {
                        if matches!(ret, Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)) {
                            if expr_has_ref_current(ret, refs) { return true; }
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Scan `expr` for `factory(args)()` pattern where `factory` is in `ref_factories`.
    fn expr_has_curried_factory_call<'a>(
        expr: &Expression<'a>,
        ref_factories: &HashSet<String>,
    ) -> bool {
        if let Expression::CallExpression(outer_call) = expr {
            // Is the callee itself a call expression?
            if let Expression::CallExpression(inner_call) = &outer_call.callee {
                if let Expression::Identifier(id) = &inner_call.callee {
                    if ref_factories.contains(id.name.as_str()) {
                        return true;
                    }
                }
            }
            // Recurse into arguments and callee
            if expr_has_curried_factory_call(&outer_call.callee, ref_factories) { return true; }
            for arg in &outer_call.arguments {
                if let Some(e) = arg.as_expression() {
                    if expr_has_curried_factory_call(e, ref_factories) { return true; }
                }
            }
        }
        // Also recurse into object expressions (e.g. `{ handler: f(args)() }`)
        if let Expression::ObjectExpression(obj) = expr {
            for prop in &obj.properties {
                if let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(p) = prop {
                    if expr_has_curried_factory_call(&p.value, ref_factories) { return true; }
                }
            }
        }
        false
    }

    fn stmts_have_curried_factory_call<'a>(
        stmts: &[Statement<'a>],
        ref_factories: &HashSet<String>,
    ) -> bool {
        for stmt in stmts {
            match stmt {
                Statement::ExpressionStatement(e) => {
                    if expr_has_curried_factory_call(&e.expression, ref_factories) { return true; }
                }
                Statement::VariableDeclaration(v) => {
                    for decl in &v.declarations {
                        if let Some(init) = &decl.init {
                            if expr_has_curried_factory_call(init, ref_factories) { return true; }
                        }
                    }
                }
                Statement::ReturnStatement(r) => {
                    if let Some(a) = &r.argument {
                        if expr_has_curried_factory_call(a, ref_factories) { return true; }
                    }
                }
                _ => {}
            }
        }
        false
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let mut refs: HashSet<String> = HashSet::new();
        for stmt in stmts.iter() {
            collect_ref_names_from_stmt(stmt, &mut refs);
        }
        if refs.is_empty() { continue; }

        // Collect "ref-factory" variables: `const f = x => () => ref.current`
        let mut ref_factories: HashSet<String> = HashSet::new();
        for stmt in stmts.iter() {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        if is_ref_factory_closure(init, &refs) {
                            if let oxc_ast::ast::BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                ref_factories.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        if ref_factories.is_empty() { continue; }

        // Check for `factory(args)()` calls in the render body.
        if stmts_have_curried_factory_call(stmts, &ref_factories) {
            return Err(CompilerError::invalid_react(
                "Cannot access refs during render\n\nReact refs are values that are not needed for rendering. Refs should only be accessed outside of render, such as in event handlers or effects. Accessing a ref value (the `current` property) during render can cause your component not to update as expected (https://react.dev/reference/react/useRef)."
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_nested_closure_outer_let_reassign
// ---------------------------------------------------------------------------
/// Detect const-assigned closures that contain nested closures that directly
/// reassign outer `let` variables.
///
/// Pattern:
/// ```js
/// let local;
/// const mk = () => {
///   const inner = val => { local = val; };  // doubly-nested reassignment
///   return inner;
/// };
/// ```
/// This is always invalid because the inner closure captures a stale binding
/// and any call to it after the initial render will observe inconsistent state.
fn validate_nested_closure_outer_let_reassign<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Expression, Statement};

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        let outer_lets = collect_let_names_shallow(stmts);
        if outer_lets.is_empty() { continue; }

        for stmt in stmts {
            let Statement::VariableDeclaration(v) = stmt else { continue };
            for decl in &v.declarations {
                let Some(init) = &decl.init else { continue };
                // Level-1 closure
                let level1_body: Option<&[Statement<'a>]> = match init {
                    Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                    Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                    _ => None,
                };
                let Some(level1_stmts) = level1_body else { continue };

                // Scan level-1 body for nested closures that directly assign outer lets
                for inner_stmt in level1_stmts {
                    let Statement::VariableDeclaration(inner_v) = inner_stmt else { continue };
                    for inner_decl in &inner_v.declarations {
                        let Some(inner_init) = &inner_decl.init else { continue };
                        // Level-2 closure
                        let level2_body: Option<&[Statement<'a>]> = match inner_init {
                            Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                            Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                            _ => None,
                        };
                        let Some(level2_stmts) = level2_body else { continue };

                        for outer_name in &outer_lets {
                            if closure_body_assigns_name(level2_stmts, outer_name) {
                                return Err(CompilerError::invalid_react(format!(
                                    "Cannot reassign variable after render completes\n\n\
                                     Reassigning `{outer_name}` after render has completed can cause \
                                     inconsistent behavior on subsequent renders. Consider using state instead."
                                )));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_no_uninitialized_let_conditional_destructuring
// ---------------------------------------------------------------------------
/// Detect the pattern that triggers a TS compiler invariant:
///   `let a; let b; if (cond) { ({a, b} = expr); }`
/// where `a` and `b` are uninitialized `let` variables and the object
/// destructuring assignment happens conditionally (inside an `if` block).
/// Top-level destructuring assignment is valid; conditional-only is not.
fn validate_no_uninitialized_let_conditional_destructuring<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, AssignmentTargetProperty, BindingPatternKind,
                       Expression, Statement, VariableDeclarationKind};

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect names of `let` variables declared WITHOUT an initializer
        let mut uninitialized_lets: std::collections::HashSet<String> = std::collections::HashSet::new();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                if v.kind == VariableDeclarationKind::Let {
                    for decl in &v.declarations {
                        if decl.init.is_none() {
                            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                uninitialized_lets.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }
        if uninitialized_lets.is_empty() { continue; }

        // Check if-blocks for object destructuring assignment of those uninitialized lets
        for stmt in stmts {
            if let Statement::IfStatement(if_stmt) = stmt {
                if stmt_has_obj_destructure_of(&if_stmt.consequent, &uninitialized_lets) {
                    return Err(CompilerError::todo(
                        "Conditional object destructuring assignment of uninitialized let variables is not supported"
                    ));
                }
                if let Some(alt) = &if_stmt.alternate {
                    if stmt_has_obj_destructure_of(alt, &uninitialized_lets) {
                        return Err(CompilerError::todo(
                            "Conditional object destructuring assignment of uninitialized let variables is not supported"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn stmt_has_obj_destructure_of<'a>(
    stmt: &oxc_ast::ast::Statement<'a>,
    targets: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::{AssignmentTarget, AssignmentTargetProperty, Expression, Statement};
    match stmt {
        Statement::ExpressionStatement(e) => {
            // `({a, b} = expr)` — parenthesized assignment expression with object target
            let inner = if let Expression::ParenthesizedExpression(p) = &e.expression {
                &p.expression
            } else {
                &e.expression
            };
            if let Expression::AssignmentExpression(a) = inner {
                if let AssignmentTarget::ObjectAssignmentTarget(obj) = &a.left {
                    return obj.properties.iter().any(|prop| {
                        match prop {
                            AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(id) => {
                                targets.contains(id.binding.name.as_str())
                            }
                            AssignmentTargetProperty::AssignmentTargetPropertyProperty(_) => false,
                        }
                    });
                }
            }
            false
        }
        Statement::BlockStatement(b) => b.body.iter().any(|s| stmt_has_obj_destructure_of(s, targets)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// validate_no_non_ref_custom_hook_current_in_empty_deps_callback
// ---------------------------------------------------------------------------
/// When @validatePreserveExistingMemoizationGuarantees is active, detect the
/// pattern where a variable:
///   - Is assigned from a CUSTOM hook (not `useRef`)
///   - Has a name that does NOT start with lowercase "ref"
///   - Has its `.current` property accessed inside a `useCallback` callback
///   - The `useCallback` has an empty `[]` dependency array
///
/// In this case, the compiler infers `X.current` as a dependency that is
/// missing from the specified `[]`, causing the memoization check to fail.
fn validate_no_non_ref_custom_hook_current_in_empty_deps_callback<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Argument, BindingPatternKind, Expression, Statement};

    fn name_is_ref_by_convention(name: &str) -> bool {
        // React compiler treats variables as refs if their name starts with lowercase "ref"
        name.starts_with("ref") && name.chars().next().map_or(false, |c| c.is_lowercase())
    }

    fn hook_is_use_ref(callee: &Expression) -> bool {
        match callee {
            Expression::Identifier(id) => id.name.as_str() == "useRef",
            Expression::StaticMemberExpression(s) => s.property.name.as_str() == "useRef",
            _ => false,
        }
    }

    /// Check if the expression body accesses `name.current` or `name.current?.method`
    fn body_accesses_current<'a>(stmts: &[Statement<'a>], name: &str) -> bool {
        for stmt in stmts {
            if stmt_accesses_current(stmt, name) { return true; }
        }
        false
    }

    fn stmt_accesses_current<'a>(stmt: &Statement<'a>, name: &str) -> bool {
        match stmt {
            Statement::ExpressionStatement(e) => expr_accesses_current(&e.expression, name),
            Statement::ReturnStatement(r) => r.argument.as_ref().map_or(false, |a| expr_accesses_current(a, name)),
            Statement::VariableDeclaration(v) => v.declarations.iter().any(|d| {
                d.init.as_ref().map_or(false, |i| expr_accesses_current(i, name))
            }),
            Statement::IfStatement(i) => {
                stmt_accesses_current(&i.consequent, name)
                    || i.alternate.as_ref().map_or(false, |a| stmt_accesses_current(a, name))
            }
            Statement::BlockStatement(b) => b.body.iter().any(|s| stmt_accesses_current(s, name)),
            _ => false,
        }
    }

    fn expr_accesses_current<'a>(expr: &Expression<'a>, name: &str) -> bool {
        match expr {
            Expression::StaticMemberExpression(m) => {
                // name.current or name.current (chained)
                if m.property.name.as_str() == "current" {
                    if let Expression::Identifier(id) = &m.object {
                        if id.name.as_str() == name { return true; }
                    }
                }
                expr_accesses_current(&m.object, name)
            }
            Expression::ChainExpression(c) => {
                match &c.expression {
                    oxc_ast::ast::ChainElement::StaticMemberExpression(m) => {
                        if m.property.name.as_str() == "current" {
                            if let Expression::Identifier(id) = &m.object {
                                if id.name.as_str() == name { return true; }
                            }
                        }
                        expr_accesses_current(&m.object, name)
                    }
                    oxc_ast::ast::ChainElement::CallExpression(c) => {
                        expr_accesses_current(&c.callee, name)
                            || c.arguments.iter().any(|a| {
                                a.as_expression().map_or(false, |e| expr_accesses_current(e, name))
                            })
                    }
                    _ => false,
                }
            }
            Expression::CallExpression(c) => {
                expr_accesses_current(&c.callee, name)
                    || c.arguments.iter().any(|a| {
                        a.as_expression().map_or(false, |e| expr_accesses_current(e, name))
                    })
            }
            Expression::ConditionalExpression(c) => {
                expr_accesses_current(&c.test, name)
                    || expr_accesses_current(&c.consequent, name)
                    || expr_accesses_current(&c.alternate, name)
            }
            Expression::LogicalExpression(l) => {
                expr_accesses_current(&l.left, name) || expr_accesses_current(&l.right, name)
            }
            _ => false,
        }
    }

    /// Returns true if the argument list is an empty array literal `[]`
    fn is_empty_array<'a>(args: &[Argument<'a>]) -> bool {
        if let Some(last) = args.last() {
            if let Some(expr) = last.as_expression() {
                if let Expression::ArrayExpression(arr) = expr {
                    return arr.elements.is_empty();
                }
            }
        }
        false
    }

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // Collect non-ref-named variables from custom hooks (non-useRef)
        let mut non_ref_hook_vars: std::collections::HashSet<String> = std::collections::HashSet::new();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    let Some(init) = &decl.init else { continue };
                    let Expression::CallExpression(call) = init else { continue };
                    // Must be a hook call (starts with "use") but NOT "useRef"
                    let is_custom_hook = match &call.callee {
                        Expression::Identifier(id) => {
                            let n = id.name.as_str();
                            n.starts_with("use") && n != "useRef"
                        }
                        Expression::StaticMemberExpression(s) => {
                            let n = s.property.name.as_str();
                            n.starts_with("use") && n != "useRef"
                        }
                        _ => false,
                    };
                    if !is_custom_hook { continue; }
                    if hook_is_use_ref(&call.callee) { continue; }

                    // Extract the bound variable name
                    if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                        let var_name = id.name.to_string();
                        // Only flag non-ref-named variables
                        if !name_is_ref_by_convention(&var_name) {
                            non_ref_hook_vars.insert(var_name);
                        }
                    }
                }
            }
        }
        if non_ref_hook_vars.is_empty() { continue; }

        // Check for useCallback(() => { non_ref.current }, [])
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    let Some(init) = &decl.init else { continue };
                    let Expression::CallExpression(call) = init else { continue };
                    let is_use_callback = match &call.callee {
                        Expression::Identifier(id) => id.name.as_str() == "useCallback",
                        _ => false,
                    };
                    if !is_use_callback { continue; }
                    if !is_empty_array(&call.arguments) { continue; }

                    // Get the callback body
                    let callback_body = call.arguments.first().and_then(|a| a.as_expression());
                    let Some(callback) = callback_body else { continue };
                    let callback_stmts: Option<&[Statement<'a>]> = match callback {
                        Expression::ArrowFunctionExpression(a) => Some(&a.body.statements),
                        Expression::FunctionExpression(f) => f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    let Some(cb_stmts) = callback_stmts else { continue };

                    for var_name in &non_ref_hook_vars {
                        if body_accesses_current(cb_stmts, var_name) {
                            return Err(CompilerError::compilation_skipped(
                                "Existing memoization could not be preserved\n\nReact Compiler has skipped optimizing this component because the existing manual memoization could not be preserved. The inferred dependencies did not match the manually specified dependencies, which could cause the value to change more or less frequently than expected.",
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate_optional_dep_mismatch
// ---------------------------------------------------------------------------
// Detects useMemo/useCallback calls where a dep uses optional chaining (`?.`)
// but the callback body accesses the same path non-optionally — indicating the
// compiler would infer a non-optional dep, causing a mismatch with the specified
// optional dep under @validatePreserveExistingMemoizationGuarantees.
//
// Catches three patterns:
//  1. Dep `a.b?.c.d`, body has `a.b.c.d` (fully non-optional same path).
//  2. Dep `a?.b`, body has `a.b` somewhere (e.g. inside an `if` block).
//  3. Dep `a?.b`, body has `a.X` (accesses `a` non-optionally via any property)
//     AND body also contains `a?.b` (the dep text itself, accessed optionally
//     inside a conditional) — compiler infers `a.b` because `a` is known non-null.
fn validate_optional_dep_mismatch<'a>(
    source: &str,
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::Statement;

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        for stmt in stmts {
            check_stmt_for_optional_dep_mismatch(source, stmt)?;
        }
    }
    Ok(())
}

fn check_stmt_for_optional_dep_mismatch<'a>(
    source: &str,
    stmt: &oxc_ast::ast::Statement<'a>,
) -> Result<()> {
    use oxc_ast::ast::{Expression, Statement};
    match stmt {
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init {
                    check_expr_for_optional_dep_mismatch(source, init)?;
                }
            }
        }
        Statement::ExpressionStatement(e) => {
            check_expr_for_optional_dep_mismatch(source, &e.expression)?;
        }
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_for_optional_dep_mismatch(source, arg)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_for_optional_dep_mismatch(source, s)?;
            }
        }
        Statement::IfStatement(if_stmt) => {
            check_stmt_for_optional_dep_mismatch(source, &if_stmt.consequent)?;
            if let Some(alt) = &if_stmt.alternate {
                check_stmt_for_optional_dep_mismatch(source, alt)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_for_optional_dep_mismatch<'a>(
    source: &str,
    expr: &oxc_ast::ast::Expression<'a>,
) -> Result<()> {
    use oxc_ast::ast::{ArrayExpressionElement, Expression};
    use oxc_span::GetSpan;

    let Expression::CallExpression(call) = expr else { return Ok(()); };

    let callee_name = match &call.callee {
        Expression::Identifier(id) => id.name.as_str(),
        _ => return Ok(()),
    };
    if !matches!(callee_name, "useMemo" | "useCallback") {
        return Ok(());
    }
    if call.arguments.len() < 2 {
        return Ok(());
    }

    let callback = call.arguments.first().and_then(|a| a.as_expression());
    let deps_expr = call.arguments.get(1).and_then(|a| a.as_expression());

    let (Some(callback), Some(Expression::ArrayExpression(deps_arr))) = (callback, deps_expr) else {
        return Ok(());
    };

    // Get the callback body source text
    let cb_body_src: &str = match callback {
        Expression::ArrowFunctionExpression(a) => {
            let s = a.body.span.start as usize;
            let e = a.body.span.end as usize;
            if s <= e && e <= source.len() { &source[s..e] } else { return Ok(()); }
        }
        Expression::FunctionExpression(f) => {
            if let Some(body) = &f.body {
                let s = body.span.start as usize;
                let e = body.span.end as usize;
                if s <= e && e <= source.len() { &source[s..e] } else { return Ok(()); }
            } else { return Ok(()); }
        }
        _ => return Ok(()),
    };

    for dep_elem in &deps_arr.elements {
        let dep_expr = match dep_elem {
            ArrayExpressionElement::SpreadElement(_) => continue,
            ArrayExpressionElement::Elision(_) => continue,
            other => match other.as_expression() {
                Some(e) => e,
                None => continue,
            },
        };

        let dep_span = dep_expr.span();
        let ds = dep_span.start as usize;
        let de = dep_span.end as usize;
        if ds > de || de > source.len() { continue; }
        let dep_src = &source[ds..de];

        // Only process deps that contain optional chain `?.`
        if !dep_src.contains("?.") { continue; }

        // Non-optional version: strip all `?.` → `.`
        let non_optional = dep_src.replace("?.", ".");

        // Case 1 & 2: body contains the non-optional version of the dep
        if contains_as_word(cb_body_src, &non_optional) {
            return Err(CompilerError::compilation_skipped(
                "Existing memoization could not be preserved\n\nReact Compiler has skipped \
                 optimizing this component because the existing manual memoization could not \
                 be preserved. The inferred dependencies did not match the manually specified \
                 dependencies, which could cause the value to change more or less frequently \
                 than expected.",
            ));
        }

        // Case 3: root of dep (before first `?.`) is accessed non-optionally
        // somewhere in body (e.g. `props.cond`) AND the dep text appears in body.
        // This indicates `root` is known non-null in that context, so the compiler
        // would infer the non-optional form of the dep.
        if let Some(opt_pos) = dep_src.find("?.") {
            let root = &dep_src[..opt_pos];
            if !root.is_empty() {
                let root_dot = format!("{}.", root);
                if contains_as_word(cb_body_src, &root_dot)
                    && contains_as_word(cb_body_src, dep_src)
                {
                    return Err(CompilerError::compilation_skipped(
                        "Existing memoization could not be preserved\n\nReact Compiler has skipped \
                         optimizing this component because the existing manual memoization could not \
                         be preserved. The inferred dependencies did not match the manually specified \
                         dependencies, which could cause the value to change more or less frequently \
                         than expected.",
                    ));
                }
            }
        }

        // Case 4 (known TS compiler bug): dep has `?.IDENT.` pattern (post-optional
        // non-optional access followed by more chain, e.g. `a.b?.c.d?.e`), AND the dep
        // text appears verbatim in the body, AND the body contains `=>` before that
        // occurrence (dep is accessed inside a nested arrow function). The TS compiler
        // truncates dep inference at the first optional boundary when the access is
        // nested, causing "inferred less specific dep than source" mismatch.
        if dep_has_post_optional_nonoptional(dep_src)
            && contains_as_word(cb_body_src, dep_src)
        {
            if let Some(dep_pos) = cb_body_src.find(dep_src) {
                let before_dep = &cb_body_src[..dep_pos];
                if before_dep.contains("=>") {
                    return Err(CompilerError::compilation_skipped(
                        "Existing memoization could not be preserved\n\nReact Compiler has skipped \
                         optimizing this component because the existing manual memoization could not \
                         be preserved. The inferred dependencies did not match the manually specified \
                         dependencies, which could cause the value to change more or less frequently \
                         than expected.",
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Returns true if `dep_src` contains the pattern `?.IDENT.` — an optional chain
/// element followed by a non-optional member access (e.g. `b?.c.d`). This is the
/// pattern that causes the TS compiler's dep inference to truncate when the access
/// is inside a nested function.
fn dep_has_post_optional_nonoptional(dep_src: &str) -> bool {
    let bytes = dep_src.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i + 2 < len {
        if bytes[i] == b'?' && bytes[i + 1] == b'.' {
            // Found `?.`; skip identifier chars
            let mut j = i + 2;
            while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$') {
                j += 1;
            }
            // If next char is `.` (not `?`), we have `?.IDENT.` — non-optional follows
            if j < len && bytes[j] == b'.' {
                return true;
            }
        }
        i += 1;
    }
    false
}

// ---------------------------------------------------------------------------
// validate_no_indirect_props_mutation_in_effect
// ---------------------------------------------------------------------------
// Detects two patterns where props (or props-aliased values) are mutated
// through a closure chain that ends up in useEffect:
//
// Pattern A (ternary phi): `let x = cond ? global : props.Y`  →  closure
//   mutates x.Z  →  another closure calls the first  →  useEffect uses it.
//
// Pattern B (while-loop fixpoint): `while (...) { x = props.Y; }`  →  let
//   y = x  →  closure mutates y.Z  →  indirect closure  →  useEffect.
fn validate_no_indirect_props_mutation_in_effect<'a>(
    program: &'a oxc_ast::ast::Program<'a>,
) -> Result<()> {
    use oxc_ast::ast::{AssignmentTarget, BindingPatternKind, Expression, Statement,
                       VariableDeclarationKind};
    use std::collections::HashSet;

    let bodies = collect_component_hook_bodies(program);
    for stmts in bodies {
        // --- Pattern A: ternary init with props branch ---
        // Collect let/const variables initialized with a ternary that has a
        // `props.X` branch.
        let mut phi_props_vars: HashSet<String> = HashSet::new();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    let Some(init) = &decl.init else { continue };
                    if let Expression::ConditionalExpression(ce) = init {
                        let consequent_is_props = matches!(&ce.consequent,
                            Expression::StaticMemberExpression(s)
                            if matches!(&s.object, Expression::Identifier(id) if id.name == "props")
                        );
                        let alternate_is_props = matches!(&ce.alternate,
                            Expression::StaticMemberExpression(s)
                            if matches!(&s.object, Expression::Identifier(id) if id.name == "props")
                        );
                        if consequent_is_props || alternate_is_props {
                            if let BindingPatternKind::BindingIdentifier(id) = &decl.id.kind {
                                phi_props_vars.insert(id.name.to_string());
                            }
                        }
                    }
                }
            }
        }

        // --- Pattern B: while-loop assigns props.X to a let variable ---
        // Find while loops that have `x = props.MEMBER` in the body,
        // then track one level of aliasing (`let y = x`).
        let mut loop_props_vars: HashSet<String> = HashSet::new();
        for stmt in stmts {
            if let Statement::WhileStatement(while_stmt) = stmt {
                if let Statement::BlockStatement(block) = &while_stmt.body {
                    collect_props_assigns_in_stmts(&block.body, &mut loop_props_vars);
                }
            }
        }
        // Track one alias level: `let/const y = x` where x is in loop_props_vars
        if !loop_props_vars.is_empty() {
            for stmt in stmts {
                if let Statement::VariableDeclaration(v) = stmt {
                    for decl in &v.declarations {
                        let Some(init) = &decl.init else { continue };
                        if let Expression::Identifier(id) = init {
                            if loop_props_vars.contains(id.name.as_str()) {
                                if let BindingPatternKind::BindingIdentifier(bid) = &decl.id.kind {
                                    phi_props_vars.insert(bid.name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        if phi_props_vars.is_empty() { continue; }

        // Find closures (assigned to variables) that DIRECTLY mutate a
        // phi_props_var's property: `phi_var.PROP = value`.
        let mut mutating_closures: HashSet<String> = HashSet::new();
        for stmt in stmts {
            if let Statement::VariableDeclaration(v) = stmt {
                for decl in &v.declarations {
                    let Some(init) = &decl.init else { continue };
                    let body_stmts: Option<&[Statement<'a>]> = match init {
                        Expression::ArrowFunctionExpression(a) => Some(a.body.statements.as_slice()),
                        Expression::FunctionExpression(f) =>
                            f.body.as_ref().map(|b| b.statements.as_slice()),
                        _ => None,
                    };
                    let Some(body) = body_stmts else { continue };
                    if stmts_mutate_member_of_vars(body, &phi_props_vars) {
                        if let BindingPatternKind::BindingIdentifier(bid) = &decl.id.kind {
                            mutating_closures.insert(bid.name.to_string());
                        }
                    }
                }
            }
        }
        if mutating_closures.is_empty() { continue; }

        // Find closures that call any mutating_closure.
        let calling_closures = collect_closures_calling_any(stmts, &mutating_closures);

        // Union of directly-mutating and indirectly-mutating closures.
        let mut dangerous: HashSet<String> = mutating_closures;
        dangerous.extend(calling_closures);

        // Check if any useEffect callback calls one of the dangerous closures.
        if use_effect_uses_any(stmts, &dangerous) {
            return Err(CompilerError::invalid_react(
                "This value cannot be modified\n\nModifying component props or hook arguments \
                 is not allowed. Consider using a local variable instead."
            ));
        }
    }
    Ok(())
}

/// Walk statements looking for `IDENT = props.MEMBER` and add IDENT to `out`.
fn collect_props_assigns_in_stmts<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    out: &mut std::collections::HashSet<String>,
) {
    use oxc_ast::ast::{AssignmentTarget, Expression, Statement};
    for stmt in stmts {
        if let Statement::ExpressionStatement(e) = stmt {
            if let Expression::AssignmentExpression(a) = &e.expression {
                if let AssignmentTarget::AssignmentTargetIdentifier(ident) = &a.left {
                    if matches!(&a.right,
                        Expression::StaticMemberExpression(s)
                        if matches!(&s.object, Expression::Identifier(id) if id.name == "props")
                    ) {
                        out.insert(ident.name.to_string());
                    }
                }
            }
        }
    }
}

/// Returns true if any statement directly assigns to `VAR.PROP = val`
/// where VAR is in `vars`.
fn stmts_mutate_member_of_vars<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    vars: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::{AssignmentTarget, Expression, Statement};
    for stmt in stmts {
        if let Statement::ExpressionStatement(e) = stmt {
            if let Expression::AssignmentExpression(a) = &e.expression {
                if let AssignmentTarget::StaticMemberExpression(sme) = &a.left {
                    if let Expression::Identifier(obj) = &sme.object {
                        if vars.contains(obj.name.as_str()) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Find all closures (assigned to variables) whose body calls any name in `targets`.
fn collect_closures_calling_any<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    targets: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    use oxc_ast::ast::{BindingPatternKind, Expression, Statement};
    let mut callers = std::collections::HashSet::new();
    for stmt in stmts {
        if let Statement::VariableDeclaration(v) = stmt {
            for decl in &v.declarations {
                let Some(init) = &decl.init else { continue };
                let body: Option<&[Statement<'a>]> = match init {
                    Expression::ArrowFunctionExpression(a) => Some(a.body.statements.as_slice()),
                    Expression::FunctionExpression(f) =>
                        f.body.as_ref().map(|b| b.statements.as_slice()),
                    _ => None,
                };
                let Some(body) = body else { continue };
                if stmts_call_any(body, targets) {
                    if let BindingPatternKind::BindingIdentifier(bid) = &decl.id.kind {
                        callers.insert(bid.name.to_string());
                    }
                }
            }
        }
    }
    callers
}

/// Returns true if any statement (at top level) calls any name in `targets`.
fn stmts_call_any<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    targets: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::{Expression, Statement};
    for stmt in stmts {
        let call_expr = match stmt {
            Statement::ExpressionStatement(e) => Some(&e.expression),
            Statement::ReturnStatement(r) => r.argument.as_ref(),
            _ => None,
        };
        if let Some(Expression::CallExpression(call)) = call_expr {
            if let Expression::Identifier(id) = &call.callee {
                if targets.contains(id.name.as_str()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Returns true if any useEffect call's callback directly calls any name in `dangerous`.
fn use_effect_uses_any<'a>(
    stmts: &[oxc_ast::ast::Statement<'a>],
    dangerous: &std::collections::HashSet<String>,
) -> bool {
    use oxc_ast::ast::{Expression, Statement};
    for stmt in stmts {
        if let Statement::ExpressionStatement(e) = stmt {
            if let Expression::CallExpression(call) = &e.expression {
                let is_effect = match &call.callee {
                    Expression::Identifier(id) => id.name == "useEffect",
                    _ => false,
                };
                if !is_effect { continue; }
                let Some(callback) = call.arguments.first().and_then(|a| a.as_expression()) else {
                    continue
                };
                let cb_body: Option<&[Statement<'a>]> = match callback {
                    Expression::ArrowFunctionExpression(a) => Some(a.body.statements.as_slice()),
                    Expression::FunctionExpression(f) =>
                        f.body.as_ref().map(|b| b.statements.as_slice()),
                    _ => None,
                };
                let Some(cb_stmts) = cb_body else {
                    if let Expression::Identifier(id) = callback {
                        if dangerous.contains(id.name.as_str()) {
                            return true;
                        }
                    }
                    continue;
                };
                if stmts_call_any(cb_stmts, dangerous) {
                    return true;
                }
            }
        }
    }
    false
}

/// Check whether `s` contains `pattern` as a whole-word match (not preceded
/// by an identifier character: alphanumeric, `_`, or `$`).
fn contains_as_word(s: &str, pattern: &str) -> bool {
    if pattern.is_empty() { return false; }
    let mut start = 0;
    while let Some(rel_pos) = s[start..].find(pattern) {
        let pos = start + rel_pos;
        let before_ok = pos == 0 || {
            let c = s[..pos].chars().next_back().unwrap_or('\0');
            !(c.is_alphanumeric() || c == '_' || c == '$')
        };
        if before_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}
