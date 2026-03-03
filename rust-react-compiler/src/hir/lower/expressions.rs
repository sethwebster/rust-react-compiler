#![allow(unused_imports, unused_variables, dead_code)]
use oxc_ast::ast::*;
use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use oxc_index::Idx;
use crate::hir::hir::{
    BinaryOperator as HirBinaryOp,
    UnaryOperator as HirUnaryOp,
    UpdateOperator as HirUpdateOp,
    SourceLocation, Place, InstructionValue, PrimitiveValue, NonLocalBinding,
    TemplateQuasi, InstructionKind, LValue,
};
use crate::error::{CompilerError, Result};
use super::LoweringContext;

// ---------------------------------------------------------------------------
// lower_literal
// ---------------------------------------------------------------------------

pub fn lower_literal<'a>(
    ctx: &mut LoweringContext,
    expr: &Expression<'a>,
) -> Result<Place> {
    let (value, loc) = match expr {
        Expression::NumericLiteral(lit) => (
            PrimitiveValue::Number(lit.value),
            SourceLocation::source(lit.span.start, lit.span.end),
        ),
        Expression::StringLiteral(lit) => (
            PrimitiveValue::String(lit.value.to_string()),
            SourceLocation::source(lit.span.start, lit.span.end),
        ),
        Expression::BooleanLiteral(lit) => (
            PrimitiveValue::Boolean(lit.value),
            SourceLocation::source(lit.span.start, lit.span.end),
        ),
        Expression::NullLiteral(lit) => (
            PrimitiveValue::Null,
            SourceLocation::source(lit.span.start, lit.span.end),
        ),
        _ => return Err(CompilerError::invariant("lower_literal called on non-literal expression")),
    };
    let place = ctx.push(InstructionValue::Primitive { value, loc: loc.clone() }, loc);
    Ok(place)
}

// ---------------------------------------------------------------------------
// lower_identifier
// ---------------------------------------------------------------------------

pub fn lower_identifier<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    ident: &IdentifierReference<'a>,
) -> Result<Place> {
    use oxc_semantic::SymbolFlags;
    let loc = SourceLocation::source(ident.span.start, ident.span.end);

    // In oxc 0.69, reference_id is a Cell<Option<ReferenceId>>, accessed via .get().
    let ref_id = ident.reference_id.get();
    let symbol_id = ref_id.and_then(|r| semantic.scoping().get_reference(r).symbol_id());

    match symbol_id {
        Some(sym_id) => {
            let flags = semantic.scoping().symbol_flags(sym_id);
            if flags.intersects(SymbolFlags::Import) {
                // Import bindings are module-level — emit LoadGlobal so downstream
                // passes (hook detection, outlining) can identify them by name.
                let binding = NonLocalBinding::Global { name: ident.name.to_string() };
                let load = InstructionValue::LoadGlobal { binding, loc: loc.clone() };
                Ok(ctx.push(load, loc))
            } else {
                let id = ctx.get_or_create_symbol(sym_id.index() as u32, Some(ident.name.as_str()), loc.clone());
                let load = InstructionValue::LoadLocal {
                    place: Place::new(id, loc.clone()),
                    loc: loc.clone(),
                };
                Ok(ctx.push(load, loc))
            }
        }
        None => {
            let binding = NonLocalBinding::Global { name: ident.name.to_string() };
            let load = InstructionValue::LoadGlobal {
                binding,
                loc: loc.clone(),
            };
            Ok(ctx.push(load, loc))
        }
    }
}

// ---------------------------------------------------------------------------
// lower_binary
// ---------------------------------------------------------------------------

pub fn lower_binary<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &BinaryExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let left = lower_expr(&expr.left, ctx)?;
    let right = lower_expr(&expr.right, ctx)?;

    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    let operator = map_binary_operator(expr.operator);
    let place = ctx.push(
        InstructionValue::BinaryExpression { operator, left, right, loc: loc.clone() },
        loc,
    );
    Ok(place)
}

fn map_binary_operator(op: oxc_ast::ast::BinaryOperator) -> HirBinaryOp {
    match op {
        oxc_ast::ast::BinaryOperator::Addition              => HirBinaryOp::Add,
        oxc_ast::ast::BinaryOperator::Subtraction           => HirBinaryOp::Sub,
        oxc_ast::ast::BinaryOperator::Multiplication        => HirBinaryOp::Mul,
        oxc_ast::ast::BinaryOperator::Division              => HirBinaryOp::Div,
        oxc_ast::ast::BinaryOperator::Remainder             => HirBinaryOp::Mod,
        oxc_ast::ast::BinaryOperator::Exponential           => HirBinaryOp::Exp,
        oxc_ast::ast::BinaryOperator::BitwiseAnd            => HirBinaryOp::BitAnd,
        oxc_ast::ast::BinaryOperator::BitwiseOR             => HirBinaryOp::BitOr,
        oxc_ast::ast::BinaryOperator::BitwiseXOR            => HirBinaryOp::BitXor,
        oxc_ast::ast::BinaryOperator::ShiftLeft             => HirBinaryOp::Shl,
        oxc_ast::ast::BinaryOperator::ShiftRight            => HirBinaryOp::Shr,
        oxc_ast::ast::BinaryOperator::ShiftRightZeroFill    => HirBinaryOp::UShr,
        oxc_ast::ast::BinaryOperator::Equality              => HirBinaryOp::Eq,
        oxc_ast::ast::BinaryOperator::Inequality            => HirBinaryOp::NEq,
        oxc_ast::ast::BinaryOperator::StrictEquality        => HirBinaryOp::StrictEq,
        oxc_ast::ast::BinaryOperator::StrictInequality      => HirBinaryOp::StrictNEq,
        oxc_ast::ast::BinaryOperator::LessThan              => HirBinaryOp::Lt,
        oxc_ast::ast::BinaryOperator::LessEqualThan         => HirBinaryOp::LtEq,
        oxc_ast::ast::BinaryOperator::GreaterThan           => HirBinaryOp::Gt,
        oxc_ast::ast::BinaryOperator::GreaterEqualThan      => HirBinaryOp::GtEq,
        oxc_ast::ast::BinaryOperator::In                    => HirBinaryOp::In,
        oxc_ast::ast::BinaryOperator::Instanceof            => HirBinaryOp::Instanceof,
    }
}

// ---------------------------------------------------------------------------
// lower_unary
// ---------------------------------------------------------------------------

pub fn lower_unary<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &UnaryExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    // delete is special — emit PropertyDelete or ComputedDelete
    if expr.operator == oxc_ast::ast::UnaryOperator::Delete {
        match &expr.argument {
            Expression::StaticMemberExpression(m) => {
                let object = lower_expr(&m.object, ctx)?;
                let property = m.property.name.to_string();
                return Ok(ctx.push(
                    InstructionValue::PropertyDelete { object, property, loc: loc.clone() },
                    loc,
                ));
            }
            Expression::ComputedMemberExpression(m) => {
                let object = lower_expr(&m.object, ctx)?;
                let property = lower_expr(&m.expression, ctx)?;
                return Ok(ctx.push(
                    InstructionValue::ComputedDelete { object, property, loc: loc.clone() },
                    loc,
                ));
            }
            _ => {
                // delete on a bare identifier or other unsupported form
                return Ok(ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated));
            }
        }
    }

    let value = lower_expr(&expr.argument, ctx)?;
    let operator = map_unary_operator(expr.operator);
    let place = ctx.push(
        InstructionValue::UnaryExpression { operator, value, loc: loc.clone() },
        loc,
    );
    Ok(place)
}

fn map_unary_operator(op: oxc_ast::ast::UnaryOperator) -> HirUnaryOp {
    match op {
        oxc_ast::ast::UnaryOperator::UnaryPlus      => HirUnaryOp::Plus,
        oxc_ast::ast::UnaryOperator::UnaryNegation  => HirUnaryOp::Minus,
        oxc_ast::ast::UnaryOperator::LogicalNot     => HirUnaryOp::Not,
        oxc_ast::ast::UnaryOperator::BitwiseNot     => HirUnaryOp::BitNot,
        oxc_ast::ast::UnaryOperator::Typeof         => HirUnaryOp::Typeof,
        oxc_ast::ast::UnaryOperator::Void           => HirUnaryOp::Void,
        // Delete is handled before this call; this branch is unreachable.
        oxc_ast::ast::UnaryOperator::Delete         => unreachable!("delete handled above"),
    }
}

// ---------------------------------------------------------------------------
// lower_update
// ---------------------------------------------------------------------------

pub fn lower_update<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &UpdateExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    // `expr.argument` is a `SimpleAssignmentTarget`, not an `Expression`.
    // We must handle each variant directly rather than calling `lower_expr`.
    let arg_place = lower_update_target(ctx, semantic, &expr.argument, loc.clone(), lower_expr)?;

    let operation = match expr.operator {
        oxc_ast::ast::UpdateOperator::Increment => HirUpdateOp::Increment,
        oxc_ast::ast::UpdateOperator::Decrement => HirUpdateOp::Decrement,
    };

    let instr_value = if expr.prefix {
        InstructionValue::PrefixUpdate {
            lvalue: arg_place.clone(),
            operation,
            value: arg_place,
            loc: loc.clone(),
        }
    } else {
        InstructionValue::PostfixUpdate {
            lvalue: arg_place.clone(),
            operation,
            value: arg_place,
            loc: loc.clone(),
        }
    };

    Ok(ctx.push(instr_value, loc))
}

/// Resolve a `SimpleAssignmentTarget` to a `Place` that can be used as both
/// the lvalue and current-value operand of a prefix/postfix update.
fn lower_update_target<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    target: &SimpleAssignmentTarget<'a>,
    loc: SourceLocation,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    match target {
        SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let ref_id = ident.reference_id.get();
            let symbol_id = ref_id.and_then(|r| semantic.scoping().get_reference(r).symbol_id());
            match symbol_id {
                Some(sym_id) => {
                    let id = ctx.get_or_create_symbol(
                        sym_id.index() as u32,
                        Some(ident.name.as_str()),
                        loc.clone(),
                    );
                    Ok(Place::new(id, loc))
                }
                None => {
                    // Global variable — load it into a temporary place.
                    let binding = NonLocalBinding::Global { name: ident.name.to_string() };
                    Ok(ctx.push(
                        InstructionValue::LoadGlobal { binding, loc: loc.clone() },
                        loc,
                    ))
                }
            }
        }
        // MemberExpression variants inherited into SimpleAssignmentTarget via macro
        SimpleAssignmentTarget::StaticMemberExpression(s) => {
            let member_loc = SourceLocation::source(s.span.start, s.span.end);
            let object = lower_expr(&s.object, ctx)?;
            let property = s.property.name.to_string();
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: member_loc.clone() },
                member_loc,
            ))
        }
        SimpleAssignmentTarget::ComputedMemberExpression(c) => {
            let member_loc = SourceLocation::source(c.span.start, c.span.end);
            let object = lower_expr(&c.object, ctx)?;
            let property = lower_expr(&c.expression, ctx)?;
            Ok(ctx.push(
                InstructionValue::ComputedLoad { object, property, loc: member_loc.clone() },
                member_loc,
            ))
        }
        SimpleAssignmentTarget::PrivateFieldExpression(p) => {
            let member_loc = SourceLocation::source(p.span.start, p.span.end);
            let object = lower_expr(&p.object, ctx)?;
            let property = format!("#{}", p.field.name);
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: member_loc.clone() },
                member_loc,
            ))
        }
        // TS wrappers and other unsupported forms
        _ => {
            Ok(ctx.push(
                InstructionValue::UnsupportedNode { loc },
                SourceLocation::Generated,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// lower_await
// ---------------------------------------------------------------------------

pub fn lower_await<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &AwaitExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);
    let value = lower_expr(&expr.argument, ctx)?;
    Ok(ctx.push(InstructionValue::Await { value, loc: loc.clone() }, loc))
}

// ---------------------------------------------------------------------------
// lower_template
// ---------------------------------------------------------------------------

pub fn lower_template<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &TemplateLiteral<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    let mut subexprs = Vec::with_capacity(expr.expressions.len());
    for e in &expr.expressions {
        let place = lower_expr(e, ctx)?;
        subexprs.push(place);
    }

    let quasis: Vec<TemplateQuasi> = expr
        .quasis
        .iter()
        .map(|q| TemplateQuasi {
            raw: q.value.raw.to_string(),
            cooked: q.value.cooked.as_ref().map(|s| s.to_string()),
        })
        .collect();

    Ok(ctx.push(
        InstructionValue::TemplateLiteral { subexprs, quasis, loc: loc.clone() },
        loc,
    ))
}

// ---------------------------------------------------------------------------
// lower_tagged_template
// ---------------------------------------------------------------------------

pub fn lower_tagged_template<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &TaggedTemplateExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    let tag = lower_expr(&expr.tag, ctx)?;

    // Collect the full quasi information from the quasi field (TemplateLiteral).
    // For the tagged template HIR node we store only a single representative quasi
    // (the first raw string), matching the BuildHIR.ts approach of capturing the
    // cooked/raw pair for the first element.
    let quasi = expr
        .quasi
        .quasis
        .first()
        .map(|q| TemplateQuasi {
            raw: q.value.raw.to_string(),
            cooked: q.value.cooked.as_ref().map(|s| s.to_string()),
        })
        .unwrap_or_else(|| TemplateQuasi { raw: String::new(), cooked: None });

    Ok(ctx.push(
        InstructionValue::TaggedTemplateExpression { tag, quasi, loc: loc.clone() },
        loc,
    ))
}
