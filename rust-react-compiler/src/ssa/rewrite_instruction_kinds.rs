#![allow(unused_imports, unused_variables, dead_code)]

use std::collections::HashSet;
use crate::hir::hir::{DeclarationId, HIRFunction, IdentifierId, InstructionKind, InstructionValue};
use crate::hir::environment::Environment;

/// Rewrite `Let` declarations to `Const` for identifiers that are never
/// reassigned anywhere in the function.
///
/// A variable is considered "reassigned" if any `StoreLocal` instruction uses
/// `InstructionKind::Reassign` for that identifier's lvalue, OR if it appears
/// as the lvalue of a `PrefixUpdate`/`PostfixUpdate`. After SSA, the named
/// variable identifiers in `StoreLocal.lvalue.place` retain their pre-SSA ids,
/// while `PostfixUpdate.lvalue` holds the SSA-renamed id. We use
/// `declaration_id` (preserved across SSA) to match them correctly.
pub fn rewrite_instruction_kinds_based_on_reassignment(hir: &mut HIRFunction, env: &Environment) {
    // --- Pass 1: collect declaration_ids that are reassigned ---
    // declaration_id is preserved across SSA renaming, so all SSA versions of
    // the same source variable share the same declaration_id.
    let mut reassigned_decls: HashSet<DeclarationId> = HashSet::new();

    // Helper: look up declaration_id for an IdentifierId
    let decl_id = |id: IdentifierId| -> DeclarationId {
        env.get_identifier(id)
            .map(|i| i.declaration_id)
            .unwrap_or_else(|| DeclarationId(id.0))
    };

    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        reassigned_decls.insert(decl_id(lvalue.place.identifier));
                    }
                }
                InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == crate::hir::hir::ContextStoreKind::Reassign {
                        reassigned_decls.insert(decl_id(lvalue.place.identifier));
                    }
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    // Update expressions are implicit reassignments.
                    // After SSA, lvalue.identifier is the SSA-renamed version;
                    // use declaration_id to match the original named variable.
                    reassigned_decls.insert(decl_id(lvalue.identifier));
                }
                _ => {}
            }
        }
    }

    // --- Pass 2: tighten declaration kinds for non-reassigned variables ---
    for block in hir.body.blocks.values_mut() {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. } => {
                    tighten_instruction_kind(lvalue, &reassigned_decls, env);
                }
                _ => {}
            }
        }
    }
}

/// Upgrade a `Let` → `Const` or `HoistedLet` → `HoistedConst` when the
/// identifier is never reassigned (matched by declaration_id).
fn tighten_instruction_kind(
    lvalue: &mut crate::hir::hir::LValue,
    reassigned_decls: &HashSet<DeclarationId>,
    env: &Environment,
) {
    let decl = env.get_identifier(lvalue.place.identifier)
        .map(|i| i.declaration_id)
        .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
    if reassigned_decls.contains(&decl) {
        return;
    }
    lvalue.kind = match lvalue.kind {
        InstructionKind::Let => InstructionKind::Const,
        InstructionKind::HoistedLet => InstructionKind::HoistedConst,
        other => other,
    };
}
