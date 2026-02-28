#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::HashMap;
use crate::hir::hir::*;

/// Simple local constant propagation pass.
///
/// For every `Primitive { value }` instruction we record the constant in a map.
/// Any subsequent `LoadLocal` that reads a known-constant identifier is rewritten
/// in-place to `Primitive { value }`, eliminating the indirection.
///
/// This is a single-pass, intra-block propagation; a full dataflow fixpoint is
/// left for a later phase.
pub fn constant_propagation(hir: &mut HIRFunction) {
    let mut constants: HashMap<IdentifierId, PrimitiveValue> = HashMap::new();

    for block in hir.body.blocks.values_mut() {
        for instr in &mut block.instructions {
            // First: if this instruction *defines* a primitive constant, record it.
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                constants.insert(instr.lvalue.identifier, value.clone());
            }

            // Second: if this instruction *loads* a known constant, replace it.
            if let InstructionValue::LoadLocal { place, loc } = &instr.value {
                if let Some(val) = constants.get(&place.identifier) {
                    let val = val.clone();
                    let loc = loc.clone();
                    instr.value = InstructionValue::Primitive { value: val, loc };
                    // The lvalue is now itself a constant.
                    if let InstructionValue::Primitive { value, .. } = &instr.value {
                        constants.insert(instr.lvalue.identifier, value.clone());
                    }
                }
            }
        }
    }
}
