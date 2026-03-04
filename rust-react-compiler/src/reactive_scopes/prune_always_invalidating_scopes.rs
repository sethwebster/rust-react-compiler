use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{HIRFunction, IdentifierId, InstructionId, InstructionValue, ScopeId};
use crate::hir::visitors::each_instruction_value_operand;

/// Prune reactive scopes whose dependencies always invalidate.
///
/// Some instructions always produce a new value (object/array/JSX literals,
/// `new` expressions). If such a value is created outside any reactive scope
/// (i.e. it is unmemoized), then any downstream scope that depends on it will
/// always re-execute — the dependency reference is always fresh.
///
/// This pass finds such "always invalidating" values and prunes scopes that
/// depend on them, since memoizing them is wasteful (the cached value would
/// never be reused).
///
/// Note: function calls are excluded because they *may* return primitives,
/// so we optimistically assume they do. Only guaranteed allocations
/// (ArrayExpression, ObjectExpression, JsxExpression, JsxFragment,
/// NewExpression) trigger pruning.
///
/// This is the Rust analog of the TypeScript compiler's
/// `pruneAlwaysInvalidatingScopes` pass.
pub fn run(hir: &mut HIRFunction, env: &mut Environment) {
    if env.scopes.is_empty() {
        return;
    }

    // Collect scope ranges for quick lookup.
    let scope_ranges: Vec<(ScopeId, u32, u32)> = env
        .scopes
        .values()
        .map(|s| (s.id, s.range.start.0, s.range.end.0))
        .collect();

    let instr_in_scope = |instr_id: u32| -> Option<ScopeId> {
        for &(sid, start, end) in &scope_ranges {
            if instr_id >= start && instr_id < end {
                return Some(sid);
            }
        }
        None
    };

    // Pre-collect all StoreLocal assignments grouped by target variable.
    // A variable is always-invalidating through StoreLocal only if ALL
    // StoreLocals to it produce always-invalidating values. This correctly
    // handles conditional assignments (e.g., `if (cond) { x = [] }` where
    // `x` might also be a primitive on the other branch).
    let mut store_local_sources: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                store_local_sources
                    .entry(lvalue.place.identifier)
                    .or_default()
                    .push(value.identifier);
            }
        }
    }

    // Walk all instructions and build two sets:
    //   - always_invalidating: identifiers that always produce fresh values
    //   - unmemoized: subset of always_invalidating not within any scope
    //
    // Also track the original definition instruction ID for each
    // always-invalidating identifier (used in case (b) below).
    let mut always_invalidating: HashSet<IdentifierId> = HashSet::new();
    let mut unmemoized: HashSet<IdentifierId> = HashSet::new();
    let mut invalidating_def_id: HashMap<IdentifierId, InstructionId> = HashMap::new();

    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if is_always_invalidating_value(&instr.value) {
                always_invalidating.insert(instr.lvalue.identifier);
                invalidating_def_id.insert(instr.lvalue.identifier, instr.id);
                if instr_in_scope(instr.id.0).is_none() {
                    unmemoized.insert(instr.lvalue.identifier);
                }
            }

            // Propagate through StoreLocal/LoadLocal chains.
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    if always_invalidating.contains(&value.identifier) {
                        // Only propagate if ALL stores to this variable are
                        // always-invalidating to avoid false positives from
                        // conditional assignments.
                        let all_stores_invalidating = store_local_sources
                            .get(&lvalue.place.identifier)
                            .map_or(false, |sources| {
                                sources.iter().all(|src| always_invalidating.contains(src))
                            });

                        if all_stores_invalidating {
                            always_invalidating.insert(lvalue.place.identifier);
                            if let Some(&def_id) = invalidating_def_id.get(&value.identifier) {
                                invalidating_def_id.insert(lvalue.place.identifier, def_id);
                            }
                        }
                    }
                    if unmemoized.contains(&value.identifier) {
                        let all_stores_unmemoized = store_local_sources
                            .get(&lvalue.place.identifier)
                            .map_or(false, |sources| {
                                sources.iter().all(|src| unmemoized.contains(src))
                            });

                        if all_stores_unmemoized {
                            unmemoized.insert(lvalue.place.identifier);
                        }
                    }
                }
                InstructionValue::LoadLocal { place, .. } => {
                    if always_invalidating.contains(&place.identifier) {
                        always_invalidating.insert(instr.lvalue.identifier);
                        if let Some(&def_id) = invalidating_def_id.get(&place.identifier) {
                            invalidating_def_id.insert(instr.lvalue.identifier, def_id);
                        }
                    }
                    if unmemoized.contains(&place.identifier) {
                        unmemoized.insert(instr.lvalue.identifier);
                    }
                }
                _ => {}
            }
        }
    }

    // Build a map of identifiers defined inside each scope (for case b).
    let mut scope_defined_ids: HashMap<ScopeId, HashSet<IdentifierId>> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let Some(sid) = instr_in_scope(instr.id.0) {
                scope_defined_ids
                    .entry(sid)
                    .or_default()
                    .insert(instr.lvalue.identifier);
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                    scope_defined_ids
                        .entry(sid)
                        .or_default()
                        .insert(lvalue.place.identifier);
                }
            }
        }
    }

    // Determine scopes to prune. A scope should be pruned if:
    //
    //   (a) ANY explicit dependency is an unmemoized always-invalidating value.
    //       This is the direct analog of the TS pass's logic.
    //
    //   (b) It's a sentinel scope (0 deps) and uses an unmemoized
    //       always-invalidating value from outside the scope. This handles
    //       cases where upstream passes (e.g., prune_non_reactive_dependencies)
    //       have already removed the deps, but the scope still depends on
    //       always-fresh values.
    //
    //       To avoid false positives from scope boundary alignment issues
    //       (where an allocation is just barely outside the scope range but
    //       is effectively part of the same computation), we check whether
    //       the original allocation's mutable range genuinely ends before
    //       the scope starts.
    let mut scopes_to_prune: Vec<ScopeId> = Vec::new();

    for (&sid, scope) in &env.scopes {
        // Case (a): explicit dependency is unmemoized always-invalidating
        let has_unmemoized_dep = scope
            .dependencies
            .iter()
            .any(|dep| unmemoized.contains(&dep.place.identifier));

        // Case (b): sentinel scope uses unmemoized always-invalidating from outside
        let sentinel_uses_unmemoized = if !has_unmemoized_dep && scope.dependencies.is_empty() {
            check_sentinel_scope_uses_unmemoized(
                sid,
                scope.range.start.0,
                &scope_defined_ids,
                &unmemoized,
                &invalidating_def_id,
                &instr_in_scope,
                hir,
                env,
            )
        } else {
            false
        };

        if has_unmemoized_dep || sentinel_uses_unmemoized {
            scopes_to_prune.push(sid);

            // Propagate: declarations/reassignments of pruned scope that were
            // always-invalidating become unmemoized (they lose memoization).
            for (&decl_id, _) in &scope.declarations {
                if always_invalidating.contains(&decl_id) {
                    unmemoized.insert(decl_id);
                }
            }
            for &reassign_id in &scope.reassignments {
                if always_invalidating.contains(&reassign_id) {
                    unmemoized.insert(reassign_id);
                }
            }
        }
    }

    // Remove pruned scopes and clear scope assignments on identifiers.
    for sid in &scopes_to_prune {
        env.scopes.remove(sid);
        for ident in env.identifiers.values_mut() {
            if ident.scope == Some(*sid) {
                ident.scope = None;
            }
        }
    }
}

/// Check if a sentinel scope (0 deps) uses unmemoized always-invalidating
/// values from outside the scope, accounting for scope boundary alignment.
fn check_sentinel_scope_uses_unmemoized(
    sid: ScopeId,
    scope_start: u32,
    scope_defined_ids: &HashMap<ScopeId, HashSet<IdentifierId>>,
    unmemoized: &HashSet<IdentifierId>,
    invalidating_def_id: &HashMap<IdentifierId, InstructionId>,
    instr_in_scope: &dyn Fn(u32) -> Option<ScopeId>,
    hir: &HIRFunction,
    env: &Environment,
) -> bool {
    let defined_in_scope = scope_defined_ids.get(&sid);

    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if instr_in_scope(instr.id.0) != Some(sid) {
                continue;
            }
            for operand in each_instruction_value_operand(&instr.value) {
                let op_id = operand.identifier;
                let is_defined_inside =
                    defined_in_scope.map_or(false, |s| s.contains(&op_id));
                if !is_defined_inside && unmemoized.contains(&op_id) {
                    // Find the original allocation's mutable range end.
                    let orig_def = invalidating_def_id.get(&op_id);
                    let mut orig_mutable_end = 0u32;
                    if let Some(&def_id) = orig_def {
                        for (_, b) in &hir.body.blocks {
                            for i in &b.instructions {
                                if i.id == def_id && is_always_invalidating_value(&i.value) {
                                    if let Some(ident) =
                                        env.identifiers.get(&i.lvalue.identifier)
                                    {
                                        orig_mutable_end = ident.mutable_range.end.0;
                                    }
                                }
                            }
                        }
                    }

                    // Mutable range is [start, end) — end is exclusive.
                    //
                    // If the original allocation's mutable range extends to or past
                    // the scope start, the allocation is likely part of the same
                    // computation (scope boundary misalignment). Exception: if the
                    // range is long (> 2 instructions) and ends exactly at the scope
                    // boundary, it was likely separated by a hook call, so we DO prune.
                    if orig_mutable_end > 0 && orig_mutable_end >= scope_start {
                        let orig_start = orig_def.map(|d| d.0).unwrap_or(0);
                        let range_len = orig_mutable_end - orig_start;
                        if orig_mutable_end == scope_start && range_len > 2 {
                            return true; // Hook-separated: prunable
                        }
                        continue; // Boundary alignment: skip
                    }

                    // Also check the operand's own mutable range (in case
                    // it was propagated through LoadLocal/StoreLocal).
                    if let Some(ident) = env.identifiers.get(&op_id) {
                        if ident.mutable_range.end.0 > scope_start {
                            continue; // Overlaps into scope: skip
                        }
                    }

                    return true; // Genuine separation: prunable
                }
            }
        }
    }

    false
}

/// Returns true if this instruction value always produces a fresh
/// heap-allocated value.
fn is_always_invalidating_value(value: &InstructionValue) -> bool {
    matches!(
        value,
        InstructionValue::ArrayExpression { .. }
            | InstructionValue::ObjectExpression { .. }
            | InstructionValue::JsxExpression { .. }
            | InstructionValue::JsxFragment { .. }
            | InstructionValue::NewExpression { .. }
    )
}
