use std::collections::HashSet;
use crate::hir::environment::Environment;
use crate::hir::hir::{
    ArrayElement, DeclarationId, HIRFunction, IdentifierId, InstructionValue,
    ObjectPatternProperty, Pattern,
};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

/// Remove unused lvalue bindings from destructuring patterns.
///
/// An element is "unused" if its lvalue identifier is never referenced as an operand
/// anywhere in the function (instructions, terminals, phi nodes, context captures,
/// scope dependencies). Removing unused elements matches TS compiler behavior and
/// produces cleaner output.
///
/// Uses declaration_id-based tracking to handle SSA-renamed identifiers: if any
/// SSA version of a variable is used, the corresponding Destructure pattern element
/// is kept. This is necessary because SSA may create cyclic phi nodes for variables
/// captured in closures (e.g. after a while loop), and those phis share a
/// declaration_id with the Destructure-created identifier.
pub fn run(hir: &mut HIRFunction, env: Option<&Environment>) {
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
            crate::hir::hir::Param::Place(p) => {
                used.insert(p.identifier);
            }
            crate::hir::hir::Param::Spread(s) => {
                used.insert(s.place.identifier);
            }
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

    // Build declaration_id set for SSA-version-aware usage checks.
    // When SSA creates cyclic phi nodes for captured variables (e.g. in while loops),
    // the phi ids end up in `used` but the Destructure-created ids do not. Using
    // declaration_id allows us to match across all SSA versions of the same variable.
    let used_decl_ids: HashSet<DeclarationId> = if let Some(env) = env {
        used.iter()
            .filter_map(|&id| env.get_identifier(id))
            .map(|i| i.declaration_id)
            .collect()
    } else {
        HashSet::new()
    };

    let is_id_used = |id: IdentifierId| -> bool {
        if used.contains(&id) {
            return true;
        }
        if let Some(env) = env {
            if let Some(ident) = env.get_identifier(id) {
                return used_decl_ids.contains(&ident.declaration_id);
            }
        }
        false
    };

    // Now prune unused elements from Destructure patterns.
    for block in hir.body.blocks.values_mut() {
        for instr in block.instructions.iter_mut() {
            let InstructionValue::Destructure { lvalue, .. } = &mut instr.value else {
                continue;
            };
            match &mut lvalue.pattern {
                Pattern::Object(obj) => {
                    // If there's a USED Spread (rest) element, we MUST NOT remove non-spread
                    // properties even if they're unused. Removing `unused` from
                    // `{ unused, ...rest }` changes which properties end up in `rest`,
                    // changing semantics. When the spread itself is unused, remove everything
                    // unused normally.
                    let has_used_spread = obj.properties.iter().any(|p| {
                        if let ObjectPatternProperty::Spread(s) = p {
                            is_id_used(s.place.identifier)
                        } else {
                            false
                        }
                    });
                    if has_used_spread {
                        // Only remove the unused spread (if any) but keep non-spread properties.
                        obj.properties.retain(|prop| {
                            match prop {
                                ObjectPatternProperty::Property(_) => true, // keep all non-spread
                                ObjectPatternProperty::Spread(s) => is_id_used(s.place.identifier),
                            }
                        });
                    } else {
                        obj.properties.retain(|prop| {
                            let id = match prop {
                                ObjectPatternProperty::Property(p) => p.place.identifier,
                                ObjectPatternProperty::Spread(s) => s.place.identifier,
                            };
                            is_id_used(id)
                        });
                    }
                }
                Pattern::Array(arr) => {
                    // Remove unused non-hole elements.
                    // Holes don't have identifiers; keep them for position purposes.
                    // But trailing holes after the last used element can be removed.
                    for elem in arr.items.iter_mut() {
                        if let ArrayElement::Place(p) = elem {
                            if !is_id_used(p.identifier) {
                                // Replace unused place with a Hole to preserve positions.
                                *elem = ArrayElement::Hole;
                            }
                        } else if let ArrayElement::Spread(s) = elem {
                            if !is_id_used(s.place.identifier) {
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
