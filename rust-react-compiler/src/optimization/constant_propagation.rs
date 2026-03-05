#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::{HashMap, HashSet};
use crate::hir::hir::*;

/// Lattice value for constant propagation.
/// Top = not yet analyzed, Constant = known value, Bottom = not constant.
#[derive(Debug, Clone, PartialEq)]
enum LatticeValue {
    Top,
    Constant(PrimitiveValue),
    Bottom,
}

impl LatticeValue {
    fn meet(&self, other: &LatticeValue) -> LatticeValue {
        match (self, other) {
            (LatticeValue::Top, v) | (v, LatticeValue::Top) => v.clone(),
            (LatticeValue::Bottom, _) | (_, LatticeValue::Bottom) => LatticeValue::Bottom,
            (LatticeValue::Constant(a), LatticeValue::Constant(b)) => {
                if a == b {
                    LatticeValue::Constant(a.clone())
                } else {
                    LatticeValue::Bottom
                }
            }
        }
    }
}

/// Constant propagation pass with lattice-based phi resolution.
///
/// Propagation rules:
///   - SSA temporaries from Primitive instructions: constant
///   - StoreLocal instruction lvalue (SSA temp): inherits value from stored constant
///   - StoreLocal named variable target (Const kind only): inherits value cross-block
///   - StoreLocal named variable target (Let/Reassign): block-local only in rewrite pass
///   - Phi nodes: lattice meet of all operands (handles cycles via convergence)
///   - LoadLocal: inherits value from source identifier
///   - BinaryExpression/UnaryExpression with known operands: fold to constant
pub fn constant_propagation(hir: &mut HIRFunction) {
    let mut lattice: HashMap<IdentifierId, LatticeValue> = HashMap::new();

    // Initialize: all Primitive instruction results are constants.
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                lattice.insert(instr.lvalue.identifier, LatticeValue::Constant(value.clone()));
            }
        }
    }

    // Iterate until convergence using lattice-based analysis.
    // This handles cyclic phis from loops correctly.
    loop {
        let mut changed = false;

        // Process phi nodes first.
        for block in hir.body.blocks.values() {
            for phi in &block.phis {
                let mut result = LatticeValue::Top;
                for (_, operand) in &phi.operands {
                    let op_val = lattice.get(&operand.identifier)
                        .cloned()
                        .unwrap_or(LatticeValue::Top);
                    result = result.meet(&op_val);
                }
                let prev = lattice.get(&phi.place.identifier).cloned().unwrap_or(LatticeValue::Top);
                if result != prev {
                    lattice.insert(phi.place.identifier, result);
                    changed = true;
                }
            }
        }

        // Process instructions.
        for block in hir.body.blocks.values() {
            for instr in &block.instructions {
                let new_val = match &instr.value {
                    InstructionValue::Primitive { value, .. } => {
                        Some(LatticeValue::Constant(value.clone()))
                    }
                    InstructionValue::LoadLocal { place, .. } => {
                        lattice.get(&place.identifier).cloned()
                    }
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        let is_const = lvalue.kind == InstructionKind::Const
                            || lvalue.kind == InstructionKind::HoistedConst;
                        let val = lattice.get(&value.identifier).cloned();
                        // Only propagate const declarations cross-block.
                        // Let/Reassign variables may be mutated by closures (StoreContext)
                        // or have multiple SSA definitions making propagation unsafe.
                        if is_const {
                            if let Some(v) = &val {
                                let prev = lattice.get(&lvalue.place.identifier)
                                    .cloned().unwrap_or(LatticeValue::Top);
                                let new = prev.meet(v);
                                if new != prev {
                                    lattice.insert(lvalue.place.identifier, new);
                                    changed = true;
                                }
                            }
                            // The instruction lvalue (SSA temp) feeds into phis.
                            val
                        } else {
                            // Non-const — don't track cross-block.
                            None
                        }
                    }
                    InstructionValue::UnaryExpression { value, operator, .. } => {
                        match lattice.get(&value.identifier) {
                            Some(LatticeValue::Constant(v)) => {
                                match fold_unary(v.clone(), operator) {
                                    Some(r) => Some(LatticeValue::Constant(r)),
                                    None => Some(LatticeValue::Bottom),
                                }
                            }
                            Some(LatticeValue::Bottom) => Some(LatticeValue::Bottom),
                            _ => None,
                        }
                    }
                    InstructionValue::BinaryExpression { left, right, operator, .. } => {
                        match (lattice.get(&left.identifier), lattice.get(&right.identifier)) {
                            (Some(LatticeValue::Constant(lv)), Some(LatticeValue::Constant(rv))) => {
                                match fold_binary(lv.clone(), rv.clone(), operator.clone()) {
                                    Some(r) => Some(LatticeValue::Constant(r)),
                                    None => Some(LatticeValue::Bottom),
                                }
                            }
                            (Some(LatticeValue::Bottom), _) | (_, Some(LatticeValue::Bottom)) => {
                                Some(LatticeValue::Bottom)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };

                if let Some(new_val) = new_val {
                    let id = instr.lvalue.identifier;
                    let prev = lattice.get(&id).cloned().unwrap_or(LatticeValue::Top);
                    let merged = prev.meet(&new_val);
                    if merged != prev {
                        lattice.insert(id, merged);
                        changed = true;
                    }
                }
            }
        }

        if !changed {
            break;
        }
    }


    // Build the final constant map from lattice values.
    let mut ssa_constants: HashMap<IdentifierId, PrimitiveValue> = lattice.iter()
        .filter_map(|(id, v)| {
            if let LatticeValue::Constant(val) = v {
                Some((*id, val.clone()))
            } else {
                None
            }
        })
        .collect();

    // Rewrite pass: substitute constants into instructions.
    // Uses ssa_constants (cross-block) + local_constants (per-block, for let vars).
    // Must update ssa_constants as we replace instructions, so that downstream
    // instructions in the same block can see the newly produced constants.
    for block in hir.body.blocks.values_mut() {
        let mut local_constants: HashMap<IdentifierId, PrimitiveValue> = HashMap::new();

        for instr in &mut block.instructions {
            // Replace LoadLocal with Primitive if the source is a known constant.
            if let InstructionValue::LoadLocal { place, loc } = &instr.value {
                let val = ssa_constants.get(&place.identifier)
                    .or_else(|| local_constants.get(&place.identifier))
                    .cloned();
                if let Some(val) = val {
                    let loc = loc.clone();
                    instr.value = InstructionValue::Primitive { value: val.clone(), loc };
                    ssa_constants.insert(instr.lvalue.identifier, val);
                }
            }

            // Record any Primitive lvalue as a constant (including newly created ones).
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                ssa_constants.insert(instr.lvalue.identifier, value.clone());
            }

            // Track non-const StoreLocals as block-local constants.
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                if lvalue.kind != InstructionKind::Const && lvalue.kind != InstructionKind::HoistedConst {
                    let val = ssa_constants.get(&value.identifier)
                        .or_else(|| local_constants.get(&value.identifier))
                        .cloned();
                    if let Some(val) = val {
                        local_constants.insert(lvalue.place.identifier, val);
                    } else {
                        local_constants.remove(&lvalue.place.identifier);
                    }
                }
            }

            // Fold UnaryExpression.
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

            // Fold BinaryExpression.
            if let InstructionValue::BinaryExpression { left, right, operator, loc } = &instr.value {
                let lv = ssa_constants.get(&left.identifier)
                    .or_else(|| local_constants.get(&left.identifier))
                    .cloned();
                let rv = ssa_constants.get(&right.identifier)
                    .or_else(|| local_constants.get(&right.identifier))
                    .cloned();
                if let (Some(lv), Some(rv)) = (lv, rv) {
                    let loc = loc.clone();
                    if let Some(result) = fold_binary(lv, rv, operator.clone()) {
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
