/// Infer which Places are "reactive" — their values may change across renders.
///
/// Algorithm (simplified port of InferReactivePlaces.ts):
///
/// 1. Mark all function params as reactive.
/// 2. Walk blocks; for each instruction:
///    - If any input operand is reactive, mark the lvalue reactive.
///    - Hook globals (useXxx) are sources of reactivity.
/// 3. Phi nodes: if any operand is reactive, mark the phi place reactive.
/// 4. Repeat until fixpoint (handles back-edges and aliases).
use std::collections::HashSet;

use crate::hir::hir::{HIRFunction, IdentifierId, InstructionValue, NonLocalBinding, Param};
use crate::hir::visitors::{each_instruction_value_operand, each_instruction_value_operand_mut};

pub fn infer_reactive_places(hir: &mut HIRFunction) {
    let mut reactive: HashSet<IdentifierId> = HashSet::new();

    // Seed: all params are reactive.
    for param in &hir.params {
        match param {
            Param::Place(p) => { reactive.insert(p.identifier); }
            Param::Spread(s) => { reactive.insert(s.place.identifier); }
        }
    }

    // Fixpoint iteration.
    loop {
        let prev = reactive.len();

        for (_, block) in &hir.body.blocks {
            // Phi nodes: if any incoming operand is reactive → phi is reactive.
            for phi in &block.phis {
                if phi.operands.values().any(|op| reactive.contains(&op.identifier)) {
                    reactive.insert(phi.place.identifier);
                }
            }

            for instr in &block.instructions {
                let has_reactive = each_instruction_value_operand(&instr.value)
                    .iter()
                    .any(|p| reactive.contains(&p.identifier));
                let is_hook = value_is_hook_source(&instr.value);
                if has_reactive || is_hook {
                    reactive.insert(instr.lvalue.identifier);
                }
            }
        }

        if reactive.len() == prev {
            break;
        }
    }

    // Write back: mark Place.reactive flags.
    for (_, block) in &mut hir.body.blocks {
        for phi in &mut block.phis {
            if reactive.contains(&phi.place.identifier) {
                phi.place.reactive = true;
            }
            for op in phi.operands.values_mut() {
                if reactive.contains(&op.identifier) {
                    op.reactive = true;
                }
            }
        }
        for instr in &mut block.instructions {
            if reactive.contains(&instr.lvalue.identifier) {
                instr.lvalue.reactive = true;
            }
            for place in each_instruction_value_operand_mut(&mut instr.value) {
                if reactive.contains(&place.identifier) {
                    place.reactive = true;
                }
            }
        }
    }
}

/// A LoadGlobal of a hook name is a source of reactivity.
fn value_is_hook_source(value: &InstructionValue) -> bool {
    if let InstructionValue::LoadGlobal { binding: NonLocalBinding::Global { name }, .. } = value {
        is_hook_name(name)
    } else {
        false
    }
}

fn is_hook_name(name: &str) -> bool {
    name.starts_with("use") && name[3..].chars().next().map_or(false, |c| c.is_uppercase())
}
