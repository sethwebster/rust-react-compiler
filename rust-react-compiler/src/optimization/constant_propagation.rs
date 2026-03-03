#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::HashMap;
use crate::hir::hir::*;

/// Simple local constant propagation pass.
///
/// SSA temporaries (instruction lvalues) are defined exactly once and can
/// safely be propagated across blocks. Named variables (StoreLocal lvalue.place)
/// may be assigned in multiple branches (phi nodes), so we only propagate them
/// within the block where they are assigned.
pub fn constant_propagation(hir: &mut HIRFunction) {
    // Constants that are safe to use across all blocks: SSA temporaries that
    // came from Primitive instructions. These are defined exactly once (SSA).
    let mut ssa_constants: HashMap<IdentifierId, PrimitiveValue> = HashMap::new();

    // First pass: collect all SSA-temporary primitive constants (instr.lvalue.identifier)
    // that come from Primitive instructions only. These are always safe to propagate.
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                ssa_constants.insert(instr.lvalue.identifier, value.clone());
            }
        }
    }

    // Second pass: rewrite each block using the propagated constants.
    // For named-variable constants (from StoreLocal), we only allow propagation
    // within the same block to avoid incorrectly propagating values through phi nodes.
    for block in hir.body.blocks.values_mut() {
        // Per-block named-variable constants: only valid for the duration of this block.
        let mut local_constants: HashMap<IdentifierId, PrimitiveValue> = HashMap::new();

        for instr in &mut block.instructions {
            // Propagate LoadLocal of a known SSA constant or block-local constant.
            if let InstructionValue::LoadLocal { place, loc } = &instr.value {
                let val = ssa_constants.get(&place.identifier)
                    .or_else(|| local_constants.get(&place.identifier))
                    .cloned();
                if let Some(val) = val {
                    let loc = loc.clone();
                    instr.value = InstructionValue::Primitive { value: val.clone(), loc };
                    // This LoadLocal's lvalue is now also a known SSA constant.
                    ssa_constants.insert(instr.lvalue.identifier, val);
                }
            }

            // Record newly produced Primitive lvalues as SSA constants.
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                ssa_constants.insert(instr.lvalue.identifier, value.clone());
            }

            // StoreLocal: if the RHS is a known constant, record the named variable
            // as a block-local constant (NOT in ssa_constants, since named variables
            // may have phi nodes at block join points).
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                let val = ssa_constants.get(&value.identifier)
                    .or_else(|| local_constants.get(&value.identifier))
                    .cloned();
                if let Some(val) = val {
                    local_constants.insert(lvalue.place.identifier, val);
                } else {
                    // The named variable is no longer a known constant (reassigned to
                    // a non-constant). Remove it so stale constant values don't leak.
                    local_constants.remove(&lvalue.place.identifier);
                }
            }

            // Constant folding for BinaryExpression where both operands are known.
            if let InstructionValue::BinaryExpression { left, right, operator, loc } = &instr.value {
                let lv = ssa_constants.get(&left.identifier)
                    .or_else(|| local_constants.get(&left.identifier))
                    .cloned();
                let rv = ssa_constants.get(&right.identifier)
                    .or_else(|| local_constants.get(&right.identifier))
                    .cloned();
                if let (Some(lv), Some(rv)) = (lv, rv) {
                    let loc = loc.clone();
                    let op = operator.clone();
                    if let Some(result) = fold_binary(lv, rv, op) {
                        instr.value = InstructionValue::Primitive { value: result.clone(), loc };
                        ssa_constants.insert(instr.lvalue.identifier, result);
                    }
                }
            }
        }
    }
}

fn fold_binary(left: PrimitiveValue, right: PrimitiveValue, op: BinaryOperator) -> Option<PrimitiveValue> {
    match (left, right) {
        (PrimitiveValue::String(a), PrimitiveValue::String(b)) => {
            if op == BinaryOperator::Add {
                Some(PrimitiveValue::String(a + &b))
            } else {
                None
            }
        }
        (PrimitiveValue::Number(a), PrimitiveValue::Number(b)) => {
            let result = match op {
                BinaryOperator::Add => a + b,
                BinaryOperator::Sub => a - b,
                BinaryOperator::Mul => a * b,
                BinaryOperator::Div if b != 0.0 => a / b,
                BinaryOperator::Mod if b != 0.0 => a % b,
                BinaryOperator::Exp => a.powf(b),
                // Bitwise (convert to i32/u32)
                BinaryOperator::BitAnd => ((a as i32) & (b as i32)) as f64,
                BinaryOperator::BitOr  => ((a as i32) | (b as i32)) as f64,
                BinaryOperator::BitXor => ((a as i32) ^ (b as i32)) as f64,
                BinaryOperator::Shl    => ((a as i32).wrapping_shl(b as u32 & 31)) as f64,
                BinaryOperator::Shr    => ((a as i32).wrapping_shr(b as u32 & 31)) as f64,
                BinaryOperator::UShr   => ((a as u32).wrapping_shr(b as u32 & 31)) as f64,
                _ => return None,
            };
            Some(PrimitiveValue::Number(result))
        }
        _ => None,
    }
}
