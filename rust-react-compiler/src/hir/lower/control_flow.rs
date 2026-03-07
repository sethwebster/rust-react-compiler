#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::HashMap;
use oxc_ast::ast::*;
use oxc_semantic::Semantic;
use crate::hir::hir::*;
use crate::error::{CompilerError, Result};
use super::{LoweringContext, Scope};

// ---------------------------------------------------------------------------
// lower_if_statement
//
// Lowers:
//   if (test) { consequent } else { alternate }
//
// CFG shape:
//   current  → If { test, consequent: consq_id, alternate: alt_id,
//                   fallthrough: fall_id }
//   consq_id → ... → Goto(fall_id)
//   alt_id   → ... → Goto(fall_id)   (only when else branch exists)
//   fall_id  → (continues)
//
// When there is no else branch, alt_id == fall_id, so the If terminal's
// alternate and fallthrough point at the same block (a no-op branch arm).
// ---------------------------------------------------------------------------

pub fn lower_if_statement<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &IfStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let test_place = lower_expr(&stmt.test, ctx)?;

    let consq_id = ctx.reserve(BlockKind::Block);
    let fall_id  = ctx.reserve(BlockKind::Block);
    let alt_id   = if stmt.alternate.is_some() {
        ctx.reserve(BlockKind::Block)
    } else {
        fall_id
    };

    let id  = ctx.next_instruction_id();
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    ctx.terminate(Terminal::If {
        test: test_place,
        consequent: consq_id,
        alternate: alt_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // --- Consequent arm ---
    ctx.switch_to(consq_id, BlockKind::Block);
    lower_stmt(&stmt.consequent, ctx)?;
    // Emit a goto to the fallthrough if the current block is still alive.
    // Note: lowering the consequent may have changed the current block (e.g.
    // via a logical expression `??`/`&&`/`||` that creates new blocks). We
    // must seal whatever the current block is, not just `consq_id`.
    if !ctx.current_dead {
        let goto_id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Goto {
            block: fall_id,
            variant: GotoVariant::Break,
            id: goto_id,
            loc: loc.clone(),
        });
    }

    // --- Alternate arm (only when an else branch was supplied) ---
    if let Some(alternate) = &stmt.alternate {
        ctx.switch_to(alt_id, BlockKind::Block);
        lower_stmt(alternate, ctx)?;
        if !ctx.current_dead {
            let goto_id = ctx.next_instruction_id();
            ctx.terminate(Terminal::Goto {
                block: fall_id,
                variant: GotoVariant::Break,
                id: goto_id,
                loc: loc.clone(),
            });
        }
    }

    // --- Fallthrough ---
    ctx.switch_to(fall_id, BlockKind::Block);

    Ok(())
}

// ---------------------------------------------------------------------------
// lower_switch_statement
//
// Lowers:
//   switch (discriminant) { case a: ...; case b: ...; default: ... }
//
// CFG shape:
//   current      → Switch { test, cases: [...], fallthrough: fall_id }
//   case_block_i → ... → Goto(case_block_{i+1})  (implicit fallthrough)
//   case_block_i → ... → Goto(fall_id)             (explicit break)
//   fall_id      → (continues)
// ---------------------------------------------------------------------------

pub fn lower_switch_statement<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &SwitchStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let test_place = lower_expr(&stmt.discriminant, ctx)?;

    let fall_id = ctx.reserve(BlockKind::Block);

    ctx.push_scope(Scope::Switch { label: None, break_block: fall_id });

    // Reserve a block for every case up-front so we can refer to the "next"
    // case block when emitting implicit fallthrough gotos.
    let case_ids: Vec<BlockId> = stmt.cases.iter()
        .map(|_| ctx.reserve(BlockKind::Block))
        .collect();

    let id  = ctx.next_instruction_id();
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    // Build Case descriptors with proper `?` error propagation.
    // For `case expr:` we lower the test expression in the current (header)
    // block before emitting the Switch terminal.  For `default:` test is None.
    let mut cases: Vec<Case> = Vec::with_capacity(stmt.cases.len());
    for (case, &block_id) in stmt.cases.iter().zip(case_ids.iter()) {
        let test = if let Some(test_expr) = &case.test {
            Some(lower_expr(test_expr, ctx)?)
        } else {
            None
        };
        cases.push(Case { test, block: block_id });
    }

    ctx.terminate(Terminal::Switch {
        test: test_place,
        cases,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // Lower each case body in order.
    for (i, (case, &case_block_id)) in stmt.cases.iter().zip(case_ids.iter()).enumerate() {
        ctx.switch_to(case_block_id, BlockKind::Block);

        for consequent_stmt in &case.consequent {
            lower_stmt(consequent_stmt, ctx)?;
            // Stop only when control flow is truly dead (break/return/throw).
            // Inner if/while/etc. change current_block_id but are NOT dead ends.
            if ctx.is_current_dead() {
                break;
            }
        }

        // If the block is still open, emit implicit fallthrough to the next
        // case block or to the switch fallthrough.
        // Use GotoVariant::Try (natural flow) — NOT Break — so codegen can
        // distinguish implicit fallthrough from an explicit `break;` statement.
        if !ctx.is_current_dead() {
            let next_block = case_ids.get(i + 1).copied().unwrap_or(fall_id);
            let goto_id = ctx.next_instruction_id();
            ctx.terminate(Terminal::Goto {
                block: next_block,
                variant: GotoVariant::Try,
                id: goto_id,
                loc: loc.clone(),
            });
        }
    }

    ctx.pop_scope();

    ctx.switch_to(fall_id, BlockKind::Block);

    Ok(())
}

// ---------------------------------------------------------------------------
// lower_conditional  (ternary: test ? consequent : alternate)
//
// CFG shape:
//   current  → Branch { test, consequent: consq_id, alternate: alt_id,
//                       fallthrough: fall_id }
//   consq_id (Value) → lower consequent → Goto(fall_id)
//   alt_id   (Value) → lower alternate  → Goto(fall_id)
//   fall_id  (Value) → Phi { result ← {consq_id: consq_place,
//                                       alt_id:   alt_place} }
//                    → (result returned to caller)
// ---------------------------------------------------------------------------

pub fn lower_conditional<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &ConditionalExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let test_place = lower_expr(&expr.test, ctx)?;

    let consq_id = ctx.reserve(BlockKind::Value);
    let alt_id   = ctx.reserve(BlockKind::Value);
    let fall_id  = ctx.reserve(BlockKind::Value);

    let id  = ctx.next_instruction_id();
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    ctx.terminate(Terminal::Branch {
        test: test_place,
        consequent: consq_id,
        alternate: alt_id,
        fallthrough: fall_id,
        logical_op: None,
        id,
        loc: loc.clone(),
    });

    // --- Consequent arm ---
    ctx.switch_to(consq_id, BlockKind::Value);
    let consq_place = lower_expr(&expr.consequent, ctx)?;
    {
        let goto_id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Goto {
            block: fall_id,
            variant: GotoVariant::Break,
            id: goto_id,
            loc: loc.clone(),
        });
    }

    // --- Alternate arm ---
    ctx.switch_to(alt_id, BlockKind::Value);
    let alt_place = lower_expr(&expr.alternate, ctx)?;
    {
        let goto_id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Goto {
            block: fall_id,
            variant: GotoVariant::Break,
            id: goto_id,
            loc: loc.clone(),
        });
    }

    // --- Fallthrough / join ---
    ctx.switch_to(fall_id, BlockKind::Value);

    let result = ctx.make_temporary(loc.clone());

    let mut operands = HashMap::new();
    operands.insert(consq_id, consq_place);
    operands.insert(alt_id, alt_place);

    ctx.current.phis.push(Phi {
        place: result.clone(),
        operands,
    });

    Ok(result)
}

// ---------------------------------------------------------------------------
// lower_logical  (a && b, a || b, a ?? b)
//
// CFG shape for AND (a && b):
//   current  → Branch { test: a,
//                       consequent: right_id,  ← evaluate b only if a truthy
//                       alternate:  fall_id,   ← short-circuit; result = a
//                       fallthrough: fall_id }
//   right_id (Value) → lower b → Goto(fall_id)
//   fall_id  (Value) → Phi { result ← {left_block: a, right_id: b} }
//
// CFG shape for OR / NullishCoalescing (a || b, a ?? b):
//   current  → Branch { test: a,
//                       consequent: fall_id,   ← short-circuit; result = a
//                       alternate:  right_id,  ← evaluate b only if a falsy
//                       fallthrough: fall_id }
//   right_id (Value) → lower b → Goto(fall_id)
//   fall_id  (Value) → Phi { result ← {left_block: a, right_id: b} }
// ---------------------------------------------------------------------------

pub fn lower_logical<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &LogicalExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let left_place = lower_expr(&expr.left, ctx)?;
    let left_block = ctx.current_block_id();

    let right_id = ctx.reserve(BlockKind::Value);
    let fall_id  = ctx.reserve(BlockKind::Value);

    // AND: truthy  → evaluate right; falsy  → short-circuit to fall.
    // OR / ??:      truthy → short-circuit to fall; falsy → evaluate right.
    let (consequent, alternate) = match expr.operator {
        oxc_ast::ast::LogicalOperator::And      => (right_id, fall_id),
        oxc_ast::ast::LogicalOperator::Or       => (fall_id,  right_id),
        oxc_ast::ast::LogicalOperator::Coalesce => (fall_id,  right_id),
    };

    let id  = ctx.next_instruction_id();
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    ctx.terminate(Terminal::Branch {
        test: left_place.clone(),
        consequent,
        alternate,
        fallthrough: fall_id,
        logical_op: Some(match expr.operator {
            oxc_ast::ast::LogicalOperator::And => crate::hir::hir::LogicalOperator::And,
            oxc_ast::ast::LogicalOperator::Or => crate::hir::hir::LogicalOperator::Or,
            oxc_ast::ast::LogicalOperator::Coalesce => crate::hir::hir::LogicalOperator::NullishCoalescing,
        }),
        id,
        loc: loc.clone(),
    });

    // --- Right arm ---
    ctx.switch_to(right_id, BlockKind::Value);
    let right_place = lower_expr(&expr.right, ctx)?;
    {
        let goto_id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Goto {
            block: fall_id,
            variant: GotoVariant::Break,
            id: goto_id,
            loc: loc.clone(),
        });
    }

    // --- Fallthrough / join ---
    ctx.switch_to(fall_id, BlockKind::Value);

    let result = ctx.make_temporary(loc.clone());

    let mut operands = HashMap::new();
    operands.insert(left_block, left_place);
    operands.insert(right_id, right_place);

    ctx.current.phis.push(Phi {
        place: result.clone(),
        operands,
    });

    Ok(result)
}
