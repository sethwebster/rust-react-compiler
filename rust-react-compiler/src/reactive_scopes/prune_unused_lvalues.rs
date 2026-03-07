use std::collections::HashSet;
use crate::hir::hir::{
    ArrayElement, HIRFunction, IdentifierId, InstructionValue, ObjectPatternProperty, Pattern,
};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

/// Remove unused lvalue bindings from destructuring patterns.
///
/// An element is "unused" if its lvalue identifier is never referenced as an operand
/// anywhere in the function (instructions, terminals, phi nodes, context captures,
/// scope dependencies). Removing unused elements matches TS compiler behavior and
/// produces cleaner output.
pub fn run(hir: &mut HIRFunction) {
    // Build the set of all "used" identifiers: identifiers that appear as VALUE operands
    // (reads) somewhere in the function.
    let mut used: HashSet<IdentifierId> = HashSet::new();

    // Scan instruction value operands (non-lvalue uses).
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            for place in each_instruction_value_operand(&instr.value) {
                used.insert(place.identifier);
            }
        }
        // Phi operands.
        for phi in &block.phis {
            for (_, operand) in &phi.operands {
                used.insert(operand.identifier);
            }
        }
        // Terminal operands.
        for place in each_terminal_operand(&block.terminal) {
            used.insert(place.identifier);
        }
    }

    // Function context captures.
    for ctx in &hir.context {
        used.insert(ctx.identifier);
    }

    // Params (always considered used).
    for param in &hir.params {
        match param {
            crate::hir::hir::Param::Place(p) => { used.insert(p.identifier); }
            crate::hir::hir::Param::Spread(s) => { used.insert(s.place.identifier); }
        }
    }

    // Returns place.
    used.insert(hir.returns.identifier);

    // Also scan FunctionExpression context captures recursively.
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::FunctionExpression { lowered_func, .. } = &instr.value {
                for ctx in &lowered_func.func.context {
                    used.insert(ctx.identifier);
                }
            }
        }
    }

    // Now prune unused elements from Destructure patterns.
    for block in hir.body.blocks.values_mut() {
        for instr in block.instructions.iter_mut() {
            let InstructionValue::Destructure { lvalue, .. } = &mut instr.value else {
                continue;
            };
            match &mut lvalue.pattern {
                Pattern::Object(obj) => {
                    obj.properties.retain(|prop| {
                        let id = match prop {
                            ObjectPatternProperty::Property(p) => p.place.identifier,
                            ObjectPatternProperty::Spread(s) => s.place.identifier,
                        };
                        used.contains(&id)
                    });
                }
                Pattern::Array(arr) => {
                    // Remove unused non-hole elements.
                    // Holes don't have identifiers; keep them for position purposes.
                    // But trailing holes after the last used element can be removed.
                    for elem in arr.items.iter_mut() {
                        if let ArrayElement::Place(p) = elem {
                            if !used.contains(&p.identifier) {
                                // Replace unused place with a Hole to preserve positions.
                                *elem = ArrayElement::Hole;
                            }
                        } else if let ArrayElement::Spread(s) = elem {
                            if !used.contains(&s.place.identifier) {
                                *elem = ArrayElement::Hole;
                            }
                        }
                    }
                    // Remove trailing holes.
                    while matches!(arr.items.last(), Some(ArrayElement::Hole)) {
                        arr.items.pop();
                    }
                }
            }
        }
    }
}
