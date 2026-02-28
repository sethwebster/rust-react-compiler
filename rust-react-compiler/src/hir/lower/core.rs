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
    BindingPatternKind, Expression, FormalParameter, Function, Statement,
    VariableDeclarationKind,
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

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse `source`, locate the first function declaration or function
/// expression, lower it to HIR and return the result.
pub fn lower_program(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
) -> Result<HIRFunction> {
    let allocator = Allocator::default();
    let parser_return = oxc_parser::Parser::new(&allocator, source, source_type).parse();

    if !parser_return.errors.is_empty() {
        let msgs: Vec<_> = parser_return.errors.iter().map(|e| e.to_string()).collect();
        return Err(CompilerError::invalid_js(format!(
            "Parse errors:\n{}",
            msgs.join("\n")
        )));
    }

    let program = parser_return.program;
    let semantic_ret = SemanticBuilder::new().build(&program);
    let semantic = semantic_ret.semantic;

    // Find the first FunctionDeclaration or ExpressionStatement with
    // FunctionExpression in program.body.
    for stmt in &program.body {
        match stmt {
            Statement::FunctionDeclaration(func) => {
                return lower_function(func, &semantic, env);
            }
            Statement::ExpressionStatement(expr_stmt) => {
                if let Expression::FunctionExpression(func) = &expr_stmt.expression {
                    return lower_function(func, &semantic, env);
                }
            }
            _ => {}
        }
    }

    Err(CompilerError::invalid_js(
        "No function declaration or expression found at top level",
    ))
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
            // TryStatement is not fully supported yet; emit UnsupportedNode.
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
            let loc = span_loc(e.span);
            if let Some(arg) = &e.argument {
                lower_expr(arg, semantic, ctx)?;
            }
            Ok(ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated))
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
        VariableDeclarationKind::Var => InstructionKind::Let,
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
