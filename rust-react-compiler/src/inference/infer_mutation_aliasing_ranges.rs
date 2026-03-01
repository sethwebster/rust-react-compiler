/// Simplified mutable-range inference.
///
/// Sets `Identifier.mutable_range` for each identifier in env.identifiers:
///   - range.start = instruction ID where the identifier is first defined (lvalue)
///   - range.end   = last instruction ID where it appears as an operand + 1
///
/// This is a simplified liveness analysis that does not model aliasing.
/// Full aliasing is deferred to a later implementation phase.
use std::collections::HashMap;

use crate::hir::environment::Environment;
use crate::hir::hir::{HIRFunction, IdentifierId, InstructionId, MutableRange, Param};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

pub struct InferMutationAliasingRangesOptions {
    pub is_function_expression: bool,
}

pub fn infer_mutation_aliasing_ranges(
    hir: &mut HIRFunction,
    env: &mut Environment,
    _options: InferMutationAliasingRangesOptions,
) {
    let zero = InstructionId(0);

    // Map: identifier id → (def instruction id, last-use instruction id)
    let mut defs: HashMap<IdentifierId, InstructionId> = HashMap::new();
    let mut last_uses: HashMap<IdentifierId, InstructionId> = HashMap::new();

    // Params are defined at instruction 0.
    for param in &hir.params {
        match param {
            Param::Place(p) => { defs.entry(p.identifier).or_insert(zero); }
            Param::Spread(s) => { defs.entry(s.place.identifier).or_insert(zero); }
        }
    }

    // Walk all blocks.
    for (_, block) in &hir.body.blocks {
        let block_start = block.instructions.first().map(|i| i.id).unwrap_or(zero);

        // Phi nodes.
        for phi in &block.phis {
            defs.entry(phi.place.identifier).or_insert(block_start);
            for op in phi.operands.values() {
                use_at(&mut last_uses, op.identifier, block_start);
            }
        }

        // Instructions.
        for instr in &block.instructions {
            let iid = instr.id;
            defs.entry(instr.lvalue.identifier).or_insert(iid);
            for op in each_instruction_value_operand(&instr.value) {
                use_at(&mut last_uses, op.identifier, iid);
            }
        }

        // Terminal operands.
        let tid = block.terminal.id();
        for op in each_terminal_operand(&block.terminal) {
            use_at(&mut last_uses, op.identifier, tid);
        }
    }

    // Write back ranges to env.identifiers.
    for (&id, &start) in &defs {
        let end_last_use = last_uses.get(&id).copied().unwrap_or(start);
        // Range is [start, end+1) — "end" is exclusive.
        let end = InstructionId(end_last_use.0 + 1);
        let range = MutableRange {
            start,
            end: if end > start { end } else { InstructionId(start.0 + 1) },
        };
        if let Some(ident) = env.get_identifier_mut(id) {
            ident.mutable_range = range;
        }
    }
}

fn use_at(last_uses: &mut HashMap<IdentifierId, InstructionId>, id: IdentifierId, iid: InstructionId) {
    last_uses.entry(id).and_modify(|e| { if iid > *e { *e = iid; } }).or_insert(iid);
}
