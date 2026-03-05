#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::{HashMap, HashSet};
use crate::hir::hir::*;
use crate::ssa::eliminate_redundant_phi::eliminate_redundant_phi;

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

/// JS truthiness for constant values.
fn is_truthy(val: &PrimitiveValue) -> bool {
    match val {
        PrimitiveValue::Boolean(b) => *b,
        PrimitiveValue::Number(n) => *n != 0.0 && !n.is_nan(),
        PrimitiveValue::String(s) => !s.is_empty(),
        PrimitiveValue::Null | PrimitiveValue::Undefined => false,
    }
}

/// Constant propagation pass with lattice-based phi resolution and branch folding.
///
/// Follows the TS compiler's SCCP approach:
///   1. Lattice analysis + rewrite constants
///   2. Fold If/Branch terminals when test is a known constant
///   3. Remove unreachable blocks, prune dead phi operands, re-run phi elimination
///   4. Loop until no more branches can be folded
pub fn constant_propagation(hir: &mut HIRFunction) {
    loop {
        let branches_folded = constant_propagation_round(hir);
        if !branches_folded {
            break;
        }
        // Clean up graph after branch folding.
        cp_remove_unreachable_blocks(hir);
        cp_prune_dead_phi_operands(hir);
        eliminate_redundant_phi(hir);
    }
}

/// One round of constant propagation + branch folding.
/// Returns true if any branches were folded.
fn constant_propagation_round(hir: &mut HIRFunction) -> bool {
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
                        // Let/Reassign variables share the same pre-SSA identifier
                        // across multiple definitions, so propagating them would
                        // cause collisions in the lattice.
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

    // Branch folding: replace If/Branch terminals with Goto when test is constant.
    let mut branches_folded = false;
    for block in hir.body.blocks.values_mut() {
        let should_fold = match &block.terminal {
            // Only fold If terminals. Branch is used for loop tests and
            // ternaries — folding those corrupts loop/for codegen structure.
            Terminal::If { test, consequent, alternate, id, loc, .. } => {
                if let Some(LatticeValue::Constant(val)) = lattice.get(&test.identifier) {
                    let target = if is_truthy(val) { *consequent } else { *alternate };
                    Some(Terminal::Goto {
                        block: target,
                        variant: GotoVariant::Break,
                        id: *id,
                        loc: loc.clone(),
                    })
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(new_terminal) = should_fold {
            block.terminal = new_terminal;
            branches_folded = true;
        }
    }

    branches_folded
}

/// Remove blocks not reachable from the entry block.
fn cp_remove_unreachable_blocks(hir: &mut HIRFunction) {
    let mut reachable: HashSet<BlockId> = HashSet::new();
    let mut queue: Vec<BlockId> = vec![hir.body.entry];
    while let Some(block_id) = queue.pop() {
        if !reachable.insert(block_id) {
            continue;
        }
        if let Some(block) = hir.body.blocks.get(&block_id) {
            for succ in block.terminal.successors() {
                if !reachable.contains(&succ) {
                    queue.push(succ);
                }
            }
        }
    }
    hir.body.blocks.retain(|id, _| reachable.contains(id));
}

/// Remove phi operands whose predecessor block no longer exists.
fn cp_prune_dead_phi_operands(hir: &mut HIRFunction) {
    let existing: HashSet<BlockId> = hir.body.blocks.keys().cloned().collect();
    for block in hir.body.blocks.values_mut() {
        for phi in &mut block.phis {
            phi.operands.retain(|pred_id, _| existing.contains(pred_id));
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
