#![allow(unused_imports, unused_variables, dead_code)]
use oxc_ast::ast::*;
use oxc_index::Idx;
use oxc_semantic::Semantic;
use crate::hir::hir::{
    BinaryOperator as HirBinaryOp,
    SourceLocation, Place, InstructionValue, InstructionKind,
    LValue, NonLocalBinding,
};
use crate::error::{CompilerError, Result};
use super::LoweringContext;

// ---------------------------------------------------------------------------
// lower_member
//
// Lower a MemberExpression (load) into a PropertyLoad or ComputedLoad.
// ---------------------------------------------------------------------------

pub fn lower_member<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &MemberExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    match expr {
        MemberExpression::StaticMemberExpression(s) => {
            let loc = SourceLocation::source(s.span.start, s.span.end);
            let object = lower_expr(&s.object, ctx)?;
            let property = s.property.name.to_string();
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }
        MemberExpression::ComputedMemberExpression(c) => {
            let loc = SourceLocation::source(c.span.start, c.span.end);
            let object = lower_expr(&c.object, ctx)?;
            let property = lower_expr(&c.expression, ctx)?;
            Ok(ctx.push(
                InstructionValue::ComputedLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }
        MemberExpression::PrivateFieldExpression(p) => {
            let loc = SourceLocation::source(p.span.start, p.span.end);
            let object = lower_expr(&p.object, ctx)?;
            let property = format!("#{}", p.field.name);
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// lower_member_store
//
// Lower a MemberExpression as an assignment target, emitting a PropertyStore
// or ComputedStore.  Returns the stored value place.
// ---------------------------------------------------------------------------

pub fn lower_member_store<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    member: &MemberExpression<'a>,
    value: Place,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    match member {
        MemberExpression::StaticMemberExpression(s) => {
            let loc = SourceLocation::source(s.span.start, s.span.end);
            let object = lower_expr(&s.object, ctx)?;
            let property = s.property.name.to_string();
            ctx.push(
                InstructionValue::PropertyStore { object, property, value: value.clone(), loc: loc.clone() },
                loc,
            );
        }
        MemberExpression::ComputedMemberExpression(c) => {
            let loc = SourceLocation::source(c.span.start, c.span.end);
            let object = lower_expr(&c.object, ctx)?;
            let property = lower_expr(&c.expression, ctx)?;
            ctx.push(
                InstructionValue::ComputedStore { object, property, value: value.clone(), loc: loc.clone() },
                loc,
            );
        }
        MemberExpression::PrivateFieldExpression(p) => {
            let loc = SourceLocation::source(p.span.start, p.span.end);
            let object = lower_expr(&p.object, ctx)?;
            let property = format!("#{}", p.field.name);
            ctx.push(
                InstructionValue::PropertyStore { object, property, value: value.clone(), loc: loc.clone() },
                loc,
            );
        }
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// lower_assignment
//
// Lower an AssignmentExpression.  For simple `=` operator assignments to
// identifiers and member expressions this emits StoreLocal / StoreGlobal /
// PropertyStore / ComputedStore.  For compound operators (+=, -=, …) it
// loads the left-hand side, applies the binary operation, and stores back.
// ---------------------------------------------------------------------------

pub fn lower_assignment<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &AssignmentExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    // For compound assignments (+=, -=, etc.) we need to load the lhs first.
    // For plain = we skip the load.
    let is_plain = expr.operator == AssignmentOperator::Assign;

    // Lower the right-hand side.
    let rhs = lower_expr(&expr.right, ctx)?;

    // Compute the value to store: for compound ops, binary(load_lhs, rhs).
    let store_value = if is_plain {
        rhs.clone()
    } else {
        // Load the current value of the lhs.
        let lhs_load = lower_assignment_target_load(ctx, semantic, &expr.left, loc.clone(), lower_expr)?;
        let bin_op = compound_op_to_binary(expr.operator).ok_or_else(|| {
            CompilerError::todo("unsupported compound assignment operator")
        })?;
        ctx.push(
            InstructionValue::BinaryExpression {
                operator: bin_op,
                left: lhs_load,
                right: rhs.clone(),
                loc: loc.clone(),
            },
            loc.clone(),
        )
    };

    // Store the value into the lhs.
    store_to_assignment_target(ctx, semantic, &expr.left, store_value.clone(), loc, lower_expr)?;

    Ok(store_value)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Load from an assignment target (for compound assignments).
fn lower_assignment_target_load<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    target: &AssignmentTarget<'a>,
    loc: SourceLocation,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    match target {
        // In oxc 0.69, AssignmentTarget inherits SimpleAssignmentTarget variants
        // directly via inherit_variants!, so we match flat variants.
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let ref_id = ident.reference_id.get();
            let sym_id = ref_id.and_then(|r| semantic.scoping().get_reference(r).symbol_id());
            if let Some(sym) = sym_id {
                let id = ctx.get_or_create_symbol(sym.index() as u32, Some(ident.name.as_str()), loc.clone());
                Ok(ctx.push(
                    InstructionValue::LoadLocal { place: Place::new(id, loc.clone()), loc: loc.clone() },
                    loc,
                ))
            } else {
                Ok(ctx.push(
                    InstructionValue::LoadGlobal {
                        binding: NonLocalBinding::Global { name: ident.name.to_string() },
                        loc: loc.clone(),
                    },
                    loc,
                ))
            }
        }
        // Member expression variants inherited from MemberExpression via SimpleAssignmentTarget
        AssignmentTarget::StaticMemberExpression(s) => {
            let loc = SourceLocation::source(s.span.start, s.span.end);
            let object = lower_expr(&s.object, ctx)?;
            let property = s.property.name.to_string();
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }
        AssignmentTarget::ComputedMemberExpression(c) => {
            let loc = SourceLocation::source(c.span.start, c.span.end);
            let object = lower_expr(&c.object, ctx)?;
            let property = lower_expr(&c.expression, ctx)?;
            Ok(ctx.push(
                InstructionValue::ComputedLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }
        AssignmentTarget::PrivateFieldExpression(p) => {
            let loc = SourceLocation::source(p.span.start, p.span.end);
            let object = lower_expr(&p.object, ctx)?;
            let property = format!("#{}", p.field.name);
            Ok(ctx.push(
                InstructionValue::PropertyLoad { object, property, loc: loc.clone() },
                loc,
            ))
        }
        _ => {
            Ok(ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated))
        }
    }
}

/// Store a value into an assignment target.
fn store_to_assignment_target<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    target: &AssignmentTarget<'a>,
    value: Place,
    loc: SourceLocation,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    match target {
        // In oxc 0.69, AssignmentTarget inherits SimpleAssignmentTarget variants
        // directly via inherit_variants!, so we match flat variants.
        AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let ref_id = ident.reference_id.get();
            let sym_id = ref_id.and_then(|r| semantic.scoping().get_reference(r).symbol_id());
            if let Some(sym) = sym_id {
                let id = ctx.get_or_create_symbol(sym.index() as u32, Some(ident.name.as_str()), loc.clone());
                let lvalue = LValue {
                    place: Place::new(id, loc.clone()),
                    kind: InstructionKind::Reassign,
                };
                ctx.push(
                    InstructionValue::StoreLocal {
                        lvalue,
                        value,
                        type_annotation: None,
                        loc: loc.clone(),
                    },
                    loc,
                );
            } else {
                ctx.push(
                    InstructionValue::StoreGlobal {
                        name: ident.name.to_string(),
                        value,
                        loc: loc.clone(),
                    },
                    loc,
                );
            }
        }
        // Member expression variants inherited from MemberExpression via SimpleAssignmentTarget
        AssignmentTarget::StaticMemberExpression(s) => {
            let loc = SourceLocation::source(s.span.start, s.span.end);
            let object = lower_expr(&s.object, ctx)?;
            let property = s.property.name.to_string();
            ctx.push(
                InstructionValue::PropertyStore { object, property, value: value.clone(), loc: loc.clone() },
                loc,
            );
        }
        AssignmentTarget::ComputedMemberExpression(c) => {
            let loc = SourceLocation::source(c.span.start, c.span.end);
            let object = lower_expr(&c.object, ctx)?;
            let property = lower_expr(&c.expression, ctx)?;
            ctx.push(
                InstructionValue::ComputedStore { object, property, value: value.clone(), loc: loc.clone() },
                loc,
            );
        }
        AssignmentTarget::PrivateFieldExpression(p) => {
            let loc = SourceLocation::source(p.span.start, p.span.end);
            let object = lower_expr(&p.object, ctx)?;
            let property = format!("#{}", p.field.name);
            ctx.push(
                InstructionValue::PropertyStore { object, property, value: value.clone(), loc: loc.clone() },
                loc,
            );
        }
        _ => {
            ctx.push(InstructionValue::UnsupportedNode { loc }, SourceLocation::Generated);
        }
    }
    Ok(())
}

/// Map a compound assignment operator to its corresponding HIR BinaryOperator.
/// Returns None for operators that don't have a simple binary equivalent
/// (logical assignment operators &&=, ||=, ??=).
fn compound_op_to_binary(op: AssignmentOperator) -> Option<HirBinaryOp> {
    match op {
        AssignmentOperator::Addition              => Some(HirBinaryOp::Add),
        AssignmentOperator::Subtraction           => Some(HirBinaryOp::Sub),
        AssignmentOperator::Multiplication        => Some(HirBinaryOp::Mul),
        AssignmentOperator::Division              => Some(HirBinaryOp::Div),
        AssignmentOperator::Remainder             => Some(HirBinaryOp::Mod),
        AssignmentOperator::Exponential           => Some(HirBinaryOp::Exp),
        AssignmentOperator::BitwiseAnd            => Some(HirBinaryOp::BitAnd),
        AssignmentOperator::BitwiseOR             => Some(HirBinaryOp::BitOr),
        AssignmentOperator::BitwiseXOR            => Some(HirBinaryOp::BitXor),
        AssignmentOperator::ShiftLeft             => Some(HirBinaryOp::Shl),
        AssignmentOperator::ShiftRight            => Some(HirBinaryOp::Shr),
        AssignmentOperator::ShiftRightZeroFill    => Some(HirBinaryOp::UShr),
        // Logical assignment operators (&&=, ||=, ??=) and plain = are not
        // compound binary ops.
        _ => None,
    }
}
