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

            // Constant folding for UnaryExpression where operand is known.
            if let InstructionValue::UnaryExpression { value, operator, loc } = &instr.value {
                let val = ssa_constants.get(&value.identifier)
                    .or_else(|| local_constants.get(&value.identifier))
                    .cloned();
                if let Some(val) = val {
                    let loc = loc.clone();
                    if let Some(result) = fold_unary(val, operator) {
                        instr.value = InstructionValue::Primitive { value: result.clone(), loc };
                        ssa_constants.insert(instr.lvalue.identifier, result);
                    }
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

fn fold_unary(val: PrimitiveValue, op: &UnaryOperator) -> Option<PrimitiveValue> {
    match op {
        UnaryOperator::Not => {
            let b = match &val {
                PrimitiveValue::Boolean(b) => !b,
                PrimitiveValue::Number(n) => *n == 0.0 || n.is_nan(),
                PrimitiveValue::String(s) => s.is_empty(),
                PrimitiveValue::Null | PrimitiveValue::Undefined => true,
            };
            Some(PrimitiveValue::Boolean(b))
        }
        UnaryOperator::Minus => match val {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(-n)),
            _ => None,
        },
        UnaryOperator::Plus => match val {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(n)),
            PrimitiveValue::Boolean(b) => Some(PrimitiveValue::Number(if b { 1.0 } else { 0.0 })),
            _ => None,
        },
        UnaryOperator::BitNot => match val {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(!(n as i32) as f64)),
            _ => None,
        },
        UnaryOperator::Typeof => {
            let t = match &val {
                PrimitiveValue::Number(_) => "number",
                PrimitiveValue::Boolean(_) => "boolean",
                PrimitiveValue::String(_) => "string",
                PrimitiveValue::Null => "object",
                PrimitiveValue::Undefined => "undefined",
            };
            Some(PrimitiveValue::String(t.to_string()))
        }
        UnaryOperator::Void => Some(PrimitiveValue::Undefined),
    }
}

fn fold_binary(left: PrimitiveValue, right: PrimitiveValue, op: BinaryOperator) -> Option<PrimitiveValue> {
    // Strict equality/inequality works across all primitive types
    match op {
        BinaryOperator::StrictEq => return Some(PrimitiveValue::Boolean(left == right)),
        BinaryOperator::StrictNEq => return Some(PrimitiveValue::Boolean(left != right)),
        _ => {}
    }

    match (left, right) {
        (PrimitiveValue::String(a), PrimitiveValue::String(b)) => {
            match op {
                BinaryOperator::Add => Some(PrimitiveValue::String(a + &b)),
                BinaryOperator::Lt => Some(PrimitiveValue::Boolean(a < b)),
                BinaryOperator::LtEq => Some(PrimitiveValue::Boolean(a <= b)),
                BinaryOperator::Gt => Some(PrimitiveValue::Boolean(a > b)),
                BinaryOperator::GtEq => Some(PrimitiveValue::Boolean(a >= b)),
                _ => None,
            }
        }
        (PrimitiveValue::Number(a), PrimitiveValue::Number(b)) => {
            // Comparison operators return boolean
            match op {
                BinaryOperator::Lt => return Some(PrimitiveValue::Boolean(a < b)),
                BinaryOperator::LtEq => return Some(PrimitiveValue::Boolean(a <= b)),
                BinaryOperator::Gt => return Some(PrimitiveValue::Boolean(a > b)),
                BinaryOperator::GtEq => return Some(PrimitiveValue::Boolean(a >= b)),
                BinaryOperator::Eq => return Some(PrimitiveValue::Boolean(a == b)),
                BinaryOperator::NEq => return Some(PrimitiveValue::Boolean(a != b)),
                _ => {}
            }
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
