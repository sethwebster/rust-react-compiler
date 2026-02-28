#![allow(unused_imports, unused_variables, dead_code)]
use oxc_ast::ast::*;
use oxc_semantic::Semantic;
use crate::hir::hir::*;
use crate::error::{CompilerError, Result};
use super::LoweringContext;

/// Lower a `CallExpression` into HIR.
///
/// Detects the method-call pattern (`obj.method(args)` and `obj[expr](args)`)
/// and emits `InstructionValue::MethodCall`; all other callees go through
/// `InstructionValue::CallExpression`.
pub fn lower_call<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &CallExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    // eval() is not supported.
    if let Expression::Identifier(ident) = &expr.callee {
        if ident.name == "eval" {
            return Err(CompilerError::compilation_skipped(
                "The 'eval' function is not supported",
            ));
        }
        // Hook calls (callee starts with 'use') with spread arguments are not yet supported.
        let name = ident.name.as_str();
        let first = name.chars().next();
        let is_hook = name.starts_with("use") && first.map_or(false, |c| c.is_lowercase());
        if is_hook && expr.arguments.iter().any(|a| matches!(a, Argument::SpreadElement(_))) {
            return Err(CompilerError::todo(
                "Support spread syntax for hook arguments",
            ));
        }

        // useMemo/useCallback specific validations on the callback argument.
        if matches!(name, "useMemo" | "useCallback") {
            validate_memo_callback(expr)?;
        }
    }

    // React.useMemo / React.useCallback via static member expression.
    if let Expression::StaticMemberExpression(s) = &expr.callee {
        if let Expression::Identifier(obj) = &s.object {
            if obj.name == "React" && matches!(s.property.name.as_str(), "useMemo" | "useCallback") {
                validate_memo_callback(expr)?;
            }
        }
    }

    // Static member expression: obj.method(args)
    if let Expression::StaticMemberExpression(s) = &expr.callee {
        let receiver = lower_expr(&s.object, ctx)?;
        let member_loc = SourceLocation::source(s.span.start, s.span.end);
        // Emit a PropertyLoad to represent the method reference
        let property = ctx.push(
            InstructionValue::PropertyLoad {
                object: receiver.clone(),
                property: s.property.name.to_string(),
                loc: member_loc.clone(),
            },
            member_loc,
        );
        let args = lower_args(ctx, semantic, &expr.arguments, lower_expr)?;
        let call_loc = SourceLocation::source(expr.span.start, expr.span.end);
        return Ok(ctx.push(
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                loc: call_loc.clone(),
            },
            call_loc,
        ));
    }

    // Computed member expression: obj[expr](args)
    if let Expression::ComputedMemberExpression(c) = &expr.callee {
        let receiver = lower_expr(&c.object, ctx)?;
        let member_loc = SourceLocation::source(c.span.start, c.span.end);
        let prop_place = lower_expr(&c.expression, ctx)?;
        let property = ctx.push(
            InstructionValue::ComputedLoad {
                object: receiver.clone(),
                property: prop_place,
                loc: member_loc.clone(),
            },
            member_loc,
        );
        let args = lower_args(ctx, semantic, &expr.arguments, lower_expr)?;
        let call_loc = SourceLocation::source(expr.span.start, expr.span.end);
        return Ok(ctx.push(
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                loc: call_loc.clone(),
            },
            call_loc,
        ));
    }

    // Regular (non-method) call
    let callee = lower_expr(&expr.callee, ctx)?;
    let args = lower_args(ctx, semantic, &expr.arguments, lower_expr)?;
    let call_loc = SourceLocation::source(expr.span.start, expr.span.end);
    Ok(ctx.push(
        InstructionValue::CallExpression {
            callee,
            args,
            loc: call_loc.clone(),
        },
        call_loc,
    ))
}

/// Lower a `NewExpression` into HIR.
pub fn lower_new<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &NewExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let callee = lower_expr(&expr.callee, ctx)?;
    let args = lower_args(ctx, semantic, &expr.arguments, lower_expr)?;
    let loc = SourceLocation::source(expr.span.start, expr.span.end);
    Ok(ctx.push(
        InstructionValue::NewExpression {
            callee,
            args,
            loc: loc.clone(),
        },
        loc,
    ))
}

/// Lower a slice of call/new arguments into `Vec<CallArg>`.
///
/// In oxc 0.69 `Argument<'a>` inherits from `Expression<'a>` via the
/// `inherit_variants!` macro. The only non-expression variant is
/// `Argument::SpreadElement`. All other arms can be reached through the callee
/// of `Argument::to_expression()` (generated by `ast_macros`).
fn lower_args<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    args: &[Argument<'a>],
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Vec<CallArg>> {
    let mut result = Vec::with_capacity(args.len());
    for arg in args {
        let call_arg = lower_arg(ctx, semantic, arg, lower_expr)?;
        result.push(call_arg);
    }
    Ok(result)
}

fn lower_arg<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    arg: &Argument<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<CallArg> {
    match arg {
        Argument::SpreadElement(spread) => {
            let place = lower_expr(&spread.argument, ctx)?;
            Ok(CallArg::Spread(SpreadPattern { place }))
        }
        // Every non-SpreadElement variant of Argument is an Expression variant.
        // In oxc 0.69 the `inherit_variants!` macro generates `to_expression()`
        // which re-tags the inner data as an `Expression`. We explicitly match
        // the concrete arms below so the compiler can verify exhaustiveness.
        // Variants are forwarded by casting — we convert via `as_expression()`
        // which returns `Option<&Expression<'a>>` on non-spread arms.
        _ => {
            // `as_expression()` is generated by oxc's ast_macros for the
            // inherited Expression variants. It returns None only for
            // SpreadElement, already handled above.
            let expr_ref = arg.as_expression().ok_or_else(|| {
                CompilerError::todo("Unhandled non-expression Argument variant")
            })?;
            let place = lower_expr(expr_ref, ctx)?;
            Ok(CallArg::Place(place))
        }
    }
}

/// Validate that a useMemo/useCallback callback argument follows React rules:
/// - Must not be async or a generator function
/// - (useMemo only) must not accept parameters
/// - (useMemo) dependency list must be an array literal
fn validate_memo_callback<'a>(expr: &CallExpression<'a>) -> Result<()> {
    let callee_name = match &expr.callee {
        Expression::Identifier(i) => i.name.as_str(),
        Expression::StaticMemberExpression(s) => s.property.name.as_str(),
        _ => return Ok(()),
    };

    let callback_arg = expr.arguments.first().and_then(|a| a.as_expression());

    if let Some(callback) = callback_arg {
        match callback {
            Expression::ArrowFunctionExpression(arrow) => {
                if arrow.r#async {
                    return Err(CompilerError::invalid_react(format!(
                        "{callee_name}() callbacks may not be async or generator functions\n\n\
                         {callee_name}() callbacks are called once and must synchronously return a value."
                    )));
                }
                if callee_name == "useMemo" && !arrow.params.items.is_empty() {
                    return Err(CompilerError::invalid_react(format!(
                        "{callee_name}() callbacks may not accept parameters"
                    )));
                }
            }
            Expression::FunctionExpression(func) => {
                if func.r#async || func.generator {
                    return Err(CompilerError::invalid_react(format!(
                        "{callee_name}() callbacks may not be async or generator functions\n\n\
                         {callee_name}() callbacks are called once and must synchronously return a value."
                    )));
                }
                if callee_name == "useMemo" && !func.params.items.is_empty() {
                    return Err(CompilerError::invalid_react(format!(
                        "{callee_name}() callbacks may not accept parameters"
                    )));
                }
            }
            _ => {}
        }
    }

    // Validate dep list (second argument) for useMemo must be an array literal.
    if callee_name == "useMemo" {
        if let Some(dep_arg) = expr.arguments.get(1).and_then(|a| a.as_expression()) {
            if !matches!(dep_arg, Expression::ArrayExpression(_)) {
                return Err(CompilerError::invalid_react(
                    "Expected the dependency list for useMemo to be an array literal"
                ));
            }
        }
    }

    Ok(())
}
