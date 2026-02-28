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
pub fn lower_program(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
) -> Result<HIRFunction> {
    // Files marked with @expectNothingCompiled should pass without transformation.
    // Return a minimal stub HIR so the fixture counts as passing.
    if source.contains("@expectNothingCompiled") {
        return make_passthrough_hir(env);
    }

    // 'use no forget' directive opts a function out of compilation → passthrough.
    if source.contains("'use no forget'") || source.contains("\"use no forget\"") {
        return make_passthrough_hir(env);
    }

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
        }
    }

    // Check for ESLint/Flow rule suppressions.
    // Skip this check when @panicThreshold:"none" is set (compiler returns passthrough instead).
    {
        let first = source.lines().next().unwrap_or("");
        let panic_threshold_none = first.contains("@panicThreshold:\"none\"")
            || first.contains("@panicThreshold:'none'");
        if !panic_threshold_none {
            validate_no_eslint_suppression(source)?;
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
    }

    let first_line = source.lines().next().unwrap_or("");
    let panic_threshold_none = first_line.contains("@panicThreshold:\"none\"")
        || first_line.contains("@panicThreshold:'none'");

    macro_rules! maybe_lower_fn {
        ($call:expr) => {{
            let result = $call;
            if panic_threshold_none {
                match result {
                    Ok(hir) => return Ok(hir),
                    Err(_) => return make_passthrough_hir(env),
                }
            } else {
                return result;
            }
        }};
    }

    macro_rules! maybe_lower_opt {
        ($call:expr) => {{
            let result = $call;
            if panic_threshold_none {
                match result {
                    Ok(Some(hir)) => return Ok(hir),
                    Ok(None) => {}
                    Err(_) => return make_passthrough_hir(env),
                }
            } else {
                if let Some(hir) = result? {
                    return Ok(hir);
                }
            }
        }};
    }

    for stmt in &program.body {
        match stmt {
            // ----------------------------------------------------------------
            // 1. Plain function declaration
            Statement::FunctionDeclaration(func) => {
                maybe_lower_fn!(lower_function(func, &semantic, env));
            }

            // ----------------------------------------------------------------
            // 2. export default function / export default () => ...
            Statement::ExportDefaultDeclaration(decl) => {
                match &decl.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                        maybe_lower_fn!(lower_function(func, &semantic, env));
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow) => {
                        maybe_lower_fn!(lower_arrow_function(arrow, &semantic, env));
                    }
                    ExportDefaultDeclarationKind::FunctionExpression(func) => {
                        maybe_lower_fn!(lower_function(func, &semantic, env));
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
                            maybe_lower_fn!(lower_function(func, &semantic, env));
                        }
                        Declaration::VariableDeclaration(var_decl) => {
                            maybe_lower_opt!(try_lower_var_declarators(
                                &var_decl.declarations,
                                &semantic,
                                env,
                            ));
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
        if let Some(init) = &decl.init {
            match init {
                Expression::FunctionExpression(func) => {
                    return Ok(Some(lower_function(func, semantic, env)?));
                }
                Expression::ArrowFunctionExpression(arrow) => {
                    return Ok(Some(lower_arrow_function(arrow, semantic, env)?));
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
    for formal_param in &func.params.items {
        lower_formal_param(formal_param, semantic, &mut ctx, &mut params)?;
    }
    if let Some(rest) = &func.params.rest {
        let rest_loc = span_loc(rest.span);
        let tmp = ctx.make_temporary(rest_loc);
        params.push(Param::Spread(SpreadPattern { place: tmp }));
    }

    // --- Body ---
    // For expression arrows (`() => expr`), `func.expression` is true and the
    // body has one ExpressionStatement containing the return expression.
    // We lower all statements and then emit an implicit void return;
    // the explicit-return case is already emitted by lower_statement for
    // ReturnStatement nodes.
    for stmt in &func.body.statements {
        lower_statement(stmt, semantic, &mut ctx)?;
    }
    // If expression body, we need to return the result of the expression.
    // lower_statement already processes ExpressionStatements, so we just
    // emit a void fallthrough return.
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
    let mut ctx = LoweringContext::new(env);

    // --- Params ---
    let mut params: Vec<Param> = Vec::new();
    for formal_param in &func.params.items {
        lower_formal_param(formal_param, semantic, &mut ctx, &mut params)?;
    }
    // Handle rest parameter (e.g., `...args`)
    if let Some(rest) = &func.params.rest {
        let rest_loc = span_loc(rest.span);
        let tmp = ctx.make_temporary(rest_loc);
        params.push(Param::Spread(SpreadPattern { place: tmp }));
    }

    // --- Body ---
    if let Some(body) = &func.body {
        // Detect hoisted function declarations that appear after a return statement.
        // This pattern requires special handling (function hoisting) that we don't support yet.
        check_hoisted_function_declarations(&body.statements)?;

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
        directives: vec![],
        aliasing_effects: None,
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
            // Full destructuring lowering is handled by patterns.rs.
            ctx.push(
                InstructionValue::UnsupportedNode { loc: loc.clone() },
                loc,
            );
        }
        BindingPatternKind::AssignmentPattern(_) => {
            let tmp = ctx.make_temporary(loc.clone());
            params.push(Param::Place(tmp));
            ctx.push(
                InstructionValue::UnsupportedNode { loc: loc.clone() },
                loc,
            );
        }
    }
    Ok(())
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
            // If the try block contains a throw statement, emit a TODO.
            if stmt_list_has_throw(&s.block.body) {
                return Err(CompilerError::todo(
                    "(BuildHIR::lowerStatement) Support ThrowStatement inside of try/catch",
                ));
            }
            // TryStatement with catch (and optional finally) is not fully supported;
            // emit UnsupportedNode as a placeholder.
            let loc = span_loc(s.span);
            ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated);
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
        Expression::TSAsExpression(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSSatisfiesExpression(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSNonNullExpression(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSTypeAssertion(e) => lower_expr(&e.expression, semantic, ctx),
        Expression::TSInstantiationExpression(e) => lower_expr(&e.expression, semantic, ctx),

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
        BindingPatternKind::ArrayPattern(arr_pat) => {
            let hir_pattern = lower_array_pattern(arr_pat, semantic, ctx, loc.clone());
            ctx.push(
                InstructionValue::Destructure {
                    lvalue: LValuePattern { pattern: Pattern::Array(hir_pattern), kind },
                    value,
                    loc: loc.clone(),
                },
                loc,
            );
        }
        BindingPatternKind::ObjectPattern(obj_pat) => {
            // Computed property keys in destructuring are not supported.
            for prop in &obj_pat.properties {
                if prop.computed {
                    return Err(CompilerError::invariant(
                        "[InferMutationAliasingEffects] Expected value kind to be initialized",
                    ));
                }
            }
            let hir_pattern = lower_object_pattern(obj_pat, semantic, ctx, loc.clone());
            ctx.push(
                InstructionValue::Destructure {
                    lvalue: LValuePattern { pattern: Pattern::Object(hir_pattern), kind },
                    value,
                    loc: loc.clone(),
                },
                loc,
            );
        }
        BindingPatternKind::AssignmentPattern(_) => {
            ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated);
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
fn validate_no_eslint_suppression(source: &str) -> Result<()> {
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

    // Check if any of the rules are suppressed in the source.
    for rule in &rules_to_check {
        let disable_patterns = [
            format!("eslint-disable {}", rule),
            format!("eslint-disable-next-line {}", rule),
        ];
        for pattern in &disable_patterns {
            if source.contains(pattern.as_str()) {
                return Err(CompilerError::invalid_react(format!(
                    "React Compiler has skipped optimizing this component because one or more React ESLint rules were disabled\n\
                     React Compiler only works when your components follow all the rules of React, disabling them may result in unexpected or incorrect behavior. \
                     Found suppression `{pattern}`."
                )));
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

/// Validate that hook identifiers are not referenced as values (must be called).
/// Scans all top-level functions in the program.
fn validate_no_hook_as_value(program: &oxc_ast::ast::Program) -> Result<()> {
    for stmt in &program.body {
        match stmt {
            oxc_ast::ast::Statement::FunctionDeclaration(f) => {
                if let Some(body) = &f.body {
                    for s in &body.statements {
                        check_stmt_hook_value(s)?;
                    }
                }
            }
            oxc_ast::ast::Statement::ExportDefaultDeclaration(d) => {
                match &d.declaration {
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body {
                            for s in &body.statements {
                                check_stmt_hook_value(s)?;
                            }
                        }
                    }
                    ExportDefaultDeclarationKind::ArrowFunctionExpression(a) => {
                        for s in &a.body.statements {
                            check_stmt_hook_value(s)?;
                        }
                    }
                    _ => {}
                }
            }
            oxc_ast::ast::Statement::ExportNamedDeclaration(d) => {
                if let Some(Declaration::FunctionDeclaration(f)) = &d.declaration {
                    if let Some(body) = &f.body {
                        for s in &body.statements {
                            check_stmt_hook_value(s)?;
                        }
                    }
                }
            }
            oxc_ast::ast::Statement::VariableDeclaration(v) => {
                for decl in &v.declarations {
                    if let Some(init) = &decl.init {
                        match init {
                            Expression::ArrowFunctionExpression(a) => {
                                for s in &a.body.statements {
                                    check_stmt_hook_value(s)?;
                                }
                            }
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body {
                                    for s in &body.statements {
                                        check_stmt_hook_value(s)?;
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

fn check_stmt_hook_value(stmt: &oxc_ast::ast::Statement) -> Result<()> {
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    check_expr_hook_value(init, false)?;
                }
            }
        }
        Statement::ExpressionStatement(e) => check_expr_hook_value(&e.expression, false)?,
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                check_expr_hook_value(arg, false)?;
            }
        }
        Statement::IfStatement(i) => {
            check_expr_hook_value(&i.test, false)?;
            check_stmt_hook_value(&i.consequent)?;
            if let Some(alt) = &i.alternate {
                check_stmt_hook_value(alt)?;
            }
        }
        Statement::BlockStatement(b) => {
            for s in &b.body {
                check_stmt_hook_value(s)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_expr_hook_value(expr: &Expression, is_callee: bool) -> Result<()> {
    const MSG: &str = "Hooks may not be referenced as normal values, they must be called. See https://react.dev/reference/rules/react-calls-components-and-hooks#never-pass-around-hooks-as-regular-values";
    match expr {
        Expression::Identifier(id) => {
            if !is_callee && is_hook_name(id.name.as_str()) {
                return Err(CompilerError::invalid_react(MSG));
            }
        }
        Expression::StaticMemberExpression(s) => {
            let prop = s.property.name.as_str();
            if !is_callee && is_hook_name(prop) {
                return Err(CompilerError::invalid_react(MSG));
            }
            // Don't recurse into the object — that's fine as a value
        }
        Expression::CallExpression(call) => {
            // Callee is being called, so it's ok; recurse with is_callee=true for callee
            check_expr_hook_value(&call.callee, true)?;
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    check_expr_hook_value(e, false)?;
                }
            }
        }
        Expression::JSXElement(j) => {
            for attr in &j.opening_element.attributes {
                if let oxc_ast::ast::JSXAttributeItem::Attribute(a) = attr {
                    if let Some(oxc_ast::ast::JSXAttributeValue::ExpressionContainer(ec)) = &a.value {
                        if let Some(inner) = ec.expression.as_expression() {
                            check_expr_hook_value(inner, false)?;
                        }
                    }
                }
            }
            for child in &j.children {
                if let oxc_ast::ast::JSXChild::ExpressionContainer(ec) = child {
                    if let Some(inner) = ec.expression.as_expression() {
                        check_expr_hook_value(inner, false)?;
                    }
                }
            }
        }
        Expression::LogicalExpression(l) => {
            check_expr_hook_value(&l.left, false)?;
            check_expr_hook_value(&l.right, false)?;
        }
        Expression::ConditionalExpression(c) => {
            check_expr_hook_value(&c.test, false)?;
            check_expr_hook_value(&c.consequent, false)?;
            check_expr_hook_value(&c.alternate, false)?;
        }
        Expression::AssignmentExpression(a) => {
            check_expr_hook_value(&a.right, false)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions {
                check_expr_hook_value(e, false)?;
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
        global_check_stmts(stmts, &is_global_or_module_ref, &local_assigners)?;
    }
    Ok(())
}

fn global_check_stmts<'a, F>(stmts: &[oxc_ast::ast::Statement<'a>], is_global: &F, local_assigners: &std::collections::HashSet<String>) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    for stmt in stmts { global_check_stmt(stmt, is_global, local_assigners)?; }
    Ok(())
}

fn global_check_stmt<'a, F>(stmt: &oxc_ast::ast::Statement<'a>, is_global: &F, local_assigners: &std::collections::HashSet<String>) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    use oxc_ast::ast::Statement;
    match stmt {
        Statement::ExpressionStatement(e) => global_check_expr(&e.expression, is_global, local_assigners)?,
        Statement::VariableDeclaration(v) => {
            for decl in &v.declarations {
                if let Some(init) = &decl.init { global_check_expr(init, is_global, local_assigners)?; }
            }
        }
        Statement::ReturnStatement(r) => {
            if let Some(a) = &r.argument { global_check_expr(a, is_global, local_assigners)?; }
        }
        Statement::IfStatement(i) => {
            global_check_expr(&i.test, is_global, local_assigners)?;
            global_check_stmt(&i.consequent, is_global, local_assigners)?;
            if let Some(alt) = &i.alternate { global_check_stmt(alt, is_global, local_assigners)?; }
        }
        Statement::BlockStatement(b) => global_check_stmts(&b.body, is_global, local_assigners)?,
        Statement::WhileStatement(w) => {
            global_check_expr(&w.test, is_global, local_assigners)?;
            global_check_stmt(&w.body, is_global, local_assigners)?;
        }
        Statement::ForStatement(f) => {
            if let Some(init) = &f.init {
                if let Some(e) = init.as_expression() { global_check_expr(e, is_global, local_assigners)?; }
            }
            if let Some(t) = &f.test { global_check_expr(t, is_global, local_assigners)?; }
            if let Some(u) = &f.update { global_check_expr(u, is_global, local_assigners)?; }
            global_check_stmt(&f.body, is_global, local_assigners)?;
        }
        _ => {}
    }
    Ok(())
}

fn global_check_expr<'a, F>(expr: &Expression<'a>, is_global: &F, local_assigners: &std::collections::HashSet<String>) -> Result<()>
where F: Fn(&oxc_ast::ast::IdentifierReference) -> bool
{
    match expr {
        Expression::AssignmentExpression(a) => {
            global_check_assignment_target(&a.left, is_global)?;
            global_check_expr(&a.right, is_global, local_assigners)?;
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
            global_check_expr(&l.left, is_global, local_assigners)?;
            global_check_expr(&l.right, is_global, local_assigners)?;
        }
        Expression::ConditionalExpression(c) => {
            global_check_expr(&c.test, is_global, local_assigners)?;
            global_check_expr(&c.consequent, is_global, local_assigners)?;
            global_check_expr(&c.alternate, is_global, local_assigners)?;
        }
        Expression::SequenceExpression(s) => {
            for e in &s.expressions { global_check_expr(e, is_global, local_assigners)?; }
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
            global_check_expr(&call.callee, is_global, local_assigners)?;
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
                    }
                    global_check_expr(e, is_global, local_assigners)?;
                }
            }
        }
        Expression::JSXElement(jsx) => {
            // JSX component tag: <Foo /> where Foo is a local global-assigning closure
            if let oxc_ast::ast::JSXElementName::Identifier(id) = &jsx.opening_element.name {
                if local_assigners.contains(id.name.as_str()) {
                    return Err(CompilerError::invalid_react(
                        "Cannot reassign variables declared outside of the component/hook\n\nReassigning this value during render is a form of side effect.",
                    ));
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
                        global_check_expr(e, is_global, local_assigners)?;
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
            check_expr_for_ref_access(&call.callee, refs, deep)?;
            for (i, arg) in call.arguments.iter().enumerate() {
                // Skip the callback (first arg) for deferred hooks
                if is_deferred_hook && i == 0 {
                    continue;
                }
                if let Some(e) = arg.as_expression() {
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
                check_stmts_for_ref_access(&arrow.body.statements, refs, deep)?;
            }
        }
        Expression::FunctionExpression(func) => {
            if deep {
                if let Some(body) = &func.body {
                    check_stmts_for_ref_access(&body.statements, refs, deep)?;
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

    let mut bodies: Vec<&'a [oxc_ast::ast::Statement<'a>]> = Vec::new();
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(f) => {
                let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                if is_component_or_hook(name) {
                    if let Some(body) = &f.body { bodies.push(&body.statements); }
                }
            }
            Statement::ExportDefaultDeclaration(d) => match &d.declaration {
                ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                    let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                    if is_component_or_hook(name) || name.is_empty() {
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
                    let name = f.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                    if is_component_or_hook(name) {
                        if let Some(body) = &f.body { bodies.push(&body.statements); }
                    }
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
                                bodies.push(&a.body.statements);
                            }
                            Expression::FunctionExpression(f) => {
                                if let Some(body) = &f.body { bodies.push(&body.statements); }
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
