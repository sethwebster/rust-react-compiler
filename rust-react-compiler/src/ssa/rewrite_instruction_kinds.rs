#![allow(unused_imports, unused_variables, dead_code)]

use std::collections::HashSet;
use crate::hir::hir::{HIRFunction, IdentifierId, InstructionKind, InstructionValue};

/// Rewrite `Let` declarations to `Const` for identifiers that are never
/// reassigned anywhere in the function.
///
/// A variable is considered "reassigned" if any `StoreLocal` instruction uses
/// `InstructionKind::Reassign` for that identifier's lvalue. If no reassignment
/// exists, `Let` and `HoistedLet` declarations can be tightened to `Const` /
/// `HoistedConst`, respectively. This mirrors RewriteInstructionKinds.ts.
pub fn rewrite_instruction_kinds_based_on_reassignment(hir: &mut HIRFunction) {
    // --- Pass 1: collect all identifiers that appear as a Reassign lvalue ---
    let mut reassigned: HashSet<IdentifierId> = HashSet::new();

    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        reassigned.insert(lvalue.place.identifier);
                    }
                }
                InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == crate::hir::hir::ContextStoreKind::Reassign {
                        reassigned.insert(lvalue.place.identifier);
                    }
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    // Update expressions are implicit reassignments.
                    reassigned.insert(lvalue.identifier);
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
                    tighten_instruction_kind(lvalue, &reassigned);
                }
                _ => {}
            }
        }
    }
}

/// Upgrade a `Let` → `Const` or `HoistedLet` → `HoistedConst` when the
/// identifier is never reassigned.
fn tighten_instruction_kind(
    lvalue: &mut crate::hir::hir::LValue,
    reassigned: &HashSet<IdentifierId>,
) {
    if reassigned.contains(&lvalue.place.identifier) {
        return;
    }
    lvalue.kind = match lvalue.kind {
        InstructionKind::Let => InstructionKind::Const,
        InstructionKind::HoistedLet => InstructionKind::HoistedConst,
        other => other,
    };
}
