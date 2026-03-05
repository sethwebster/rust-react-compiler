/// Merge adjacent reactive scopes that always invalidate together.
///
/// Port of MergeReactiveScopesThatInvalidateTogether.ts.
///
/// The algorithm is a single left-to-right pass over scopes (sorted by start):
///
/// 1. A scope is "eligible" if it has no deps (sentinel) OR its declarations are
///    always-invalidating (ObjectExpression, ArrayExpression, etc.).
///
/// 2. Between two consecutive scopes, "gap" instructions must all be "safe"
///    (LoadLocal, primitive, const StoreLocal, etc.) for a merge to be possible.
///    A `let` StoreLocal in the gap resets the merge candidate.
///
/// 3. Two scopes A and B can merge if:
///    Case 1: areEqualDependencies(A.deps, B.deps) — including both empty (sentinel)
///    Case 2: B.deps ⊆ A's always-invalidating outputs (non-empty B.deps)
///
///    AND: A's lvalues (+ gap lvalues) are all last-mutably-used within B's range.
///    "Last mutably used" is mutableRange.end (not max operand instruction id),
///    mirroring the TS compiler's `areLValuesLastUsedByScope`.
///
/// 4. After a merge, continue with the merged scope as the new "current" candidate,
///    but only if the merged scope is still eligible for further merging.
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    DeclarationId, HIRFunction, IdentifierId, InstructionKind, InstructionValue, ReactiveScopeDependency, ScopeId,
};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

pub fn run(_hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    if env.scopes.len() <= 1 {
        return;
    }

    if std::env::var("RC_DEBUG").is_ok() {
        let mut scope_list: Vec<_> = env.scopes.iter().collect();
        scope_list.sort_by_key(|(_, s)| s.range.start.0);
        eprintln!("[merge_adjacent_start] {} scopes:", scope_list.len());
        for (sid, s) in &scope_list {
            eprintln!(
                "  scope {:?} range=[{},{}] ndeps={}",
                sid.0,
                s.range.start.0,
                s.range.end.0,
                s.dependencies.len()
            );
        }
    }

    // Compute the actual last-use instruction ID for each identifier.
    // The TS `areLValuesLastUsedByScope` tracks the last instruction where each
    // Declaration is READ anywhere (including terminals/returns). We must use last-READ
    // rather than mutableRange.end, because mutableRange.end only tracks mutations
    // (stores/mutations) not reads. For example, if `a = someObj()` has mutableRange=[3,4)
    // but `a` is read in the return statement at instruction 14, we must use 14, not 4.
    //
    // We track by DeclarationId (grouping all SSA versions of the same variable).
    let mut last_use_by_ident: HashMap<IdentifierId, u32> = HashMap::new();
    let mut decl_id_of: HashMap<IdentifierId, DeclarationId> = HashMap::new();
    for (&id, ident) in &env.identifiers {
        decl_id_of.insert(id, ident.declaration_id);
    }
    // Scan all instructions and terminals for operand uses.
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let iid = instr.id.0;
            for op in each_instruction_value_operand(&instr.value) {
                let entry = last_use_by_ident.entry(op.identifier).or_insert(0);
                if iid > *entry { *entry = iid; }
            }
            // Also record lvalue appearances (for StoreLocal etc.)
            let entry = last_use_by_ident.entry(instr.lvalue.identifier).or_insert(0);
            if iid > *entry { *entry = iid; }
        }
        // Include terminal operands (return values, if conditions, etc.)
        let tid = block.terminal.id().0;
        for op in each_terminal_operand(&block.terminal) {
            let entry = last_use_by_ident.entry(op.identifier).or_insert(0);
            if tid > *entry { *entry = tid; }
        }
    }
    // Now aggregate by DeclarationId: last_use_by_decl[decl_id] = max last_use across all idents.
    let mut last_use_by_decl: HashMap<DeclarationId, u32> = HashMap::new();
    for (&id, &last_use) in &last_use_by_ident {
        if let Some(&decl_id) = decl_id_of.get(&id) {
            let entry = last_use_by_decl.entry(decl_id).or_insert(0);
            if last_use > *entry { *entry = last_use; }
        }
    }
    // Helper: last use of an identifier (max across ident and its declaration group).
    let last_use = |id: IdentifierId| -> u32 {
        let by_ident = last_use_by_ident.get(&id).copied().unwrap_or(0);
        let by_decl = decl_id_of.get(&id)
            .and_then(|d| last_use_by_decl.get(d))
            .copied().unwrap_or(0);
        by_ident.max(by_decl)
    };

    let mut store_local_value: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    let mut is_always_invalidating: HashMap<IdentifierId, bool> = HashMap::new();

    // Build a map of IdentifierId → global name, to detect hook calls.
    // A hook call is a CallExpression whose callee was loaded from a global
    // with a name matching `use[A-Z]...` (React's rules of hooks naming).
    let mut global_name_of: HashMap<IdentifierId, String> = HashMap::new();
    // Instruction IDs of hook calls (useXxx(...)) — these must not be placed
    // inside memoization blocks and must block scope merging.
    let mut hook_call_iids: HashSet<u32> = HashSet::new();

    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let always_inv = always_invalidating_instruction(&instr.value);
            is_always_invalidating.insert(instr.lvalue.identifier, always_inv);

            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                store_local_value.insert(lvalue.place.identifier, value.identifier);
            }

            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                let name = match binding {
                    crate::hir::hir::NonLocalBinding::Global { name } => name.clone(),
                    crate::hir::hir::NonLocalBinding::ModuleLocal { name } => name.clone(),
                    _ => String::new(),
                };
                if !name.is_empty() {
                    global_name_of.insert(instr.lvalue.identifier, name);
                }
            }
        }
    }
    // Second pass: identify hook call instructions.
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                if let Some(name) = global_name_of.get(&callee.identifier) {
                    if is_hook_name(name) {
                        hook_call_iids.insert(instr.id.0);
                    }
                }
            }
        }
    }

    // Single left-to-right pass (mirrors TS MergeReactiveScopesThatInvalidateTogether).
    // When two adjacent scopes A and B merge, the merged B becomes the new current
    // candidate and can chain-merge with the next scope C in the same pass.
    // A failed candidate is permanently discarded — it does NOT get a second chance.
    // (A fixpoint loop would incorrectly allow previously-failed candidates to retry
    // against expanded successors, producing too-large merged scopes.)
    struct Current {
        scope_id: ScopeId,
        /// All lvalue identifiers accumulated from scope A's instructions + gap instructions.
        /// Includes both SSA temps (instr.lvalue.identifier) and binding targets
        /// (StoreLocal lvalue.place.identifier).
        lvalues: HashSet<IdentifierId>,
    }

    {
        let mut scope_list: Vec<ScopeId> = env.scopes.keys().copied().collect();
        scope_list.sort_by_key(|sid| env.scopes[sid].range.start.0);

        let mut current: Option<Current> = None;

        for idx in 0..scope_list.len() {
            let sid = scope_list[idx];
            if !env.scopes.contains_key(&sid) {
                continue;
            }

            let b_start = env.scopes[&sid].range.start.0;
            let b_end = env.scopes[&sid].range.end.0;

            let merged = if let Some(ref mut cur) = current {
                let a_end = env.scopes[&cur.scope_id].range.end.0;

                // Overlapping scopes can't merge this way.
                if a_end > b_start {
                    false
                } else {
                    // Block merge if scope A's own range contains a hook call.
                    // Hook calls must run unconditionally (React rules of hooks) and
                    // must never end up inside a memoization `if (changed)` block.
                    let a_start = env.scopes[&cur.scope_id].range.start.0;
                    let hook_in_scope_a = hook_call_iids.iter().any(|&iid| iid >= a_start && iid < a_end);
                    if hook_in_scope_a {
                        if std::env::var("RC_DEBUG").is_ok() {
                            eprintln!("[merge_adjacent] BLOCKED: hook call in scope A range [{},{})", a_start, a_end);
                        }
                        false
                    } else {
                        // Check gap instructions in [a_end, b_start).
                        let mut gap_safe = true;
                        if a_end < b_start {
                            'gap: for (_, block) in &hir.body.blocks {
                                for instr in &block.instructions {
                                    let iid = instr.id.0;
                                    if iid >= a_end && iid < b_start {
                                        let safe = is_safe_gap_instruction(&instr.value);
                                        if std::env::var("RC_DEBUG").is_ok() {
                                            let kind_str = if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                                format!("{:?}", lvalue.kind)
                                            } else { String::new() };
                                            eprintln!("[gap_check] iid={} discriminant={:?} kind={} safe={}", iid, std::mem::discriminant(&instr.value), kind_str, safe);
                                        }
                                        if safe {
                                            // Accumulate gap lvalues (SSA temp + binding targets).
                                            cur.lvalues.insert(instr.lvalue.identifier);
                                            if let InstructionValue::StoreLocal { lvalue, .. } =
                                                &instr.value
                                            {
                                                cur.lvalues.insert(lvalue.place.identifier);
                                            }
                                        } else {
                                            gap_safe = false;
                                            break 'gap;
                                        }
                                    }
                                }
                            }
                        }
                        if std::env::var("RC_DEBUG").is_ok() {
                            eprintln!("[gap_check] a_end={} b_start={} gap_safe={}", a_end, b_start, gap_safe);
                        }

                        if !gap_safe {
                            false
                        } else {
                            let cur_scope = &env.scopes[&cur.scope_id];
                            let next_scope = &env.scopes[&sid];
                            let can_merge = can_merge_scopes(
                                cur_scope,
                                next_scope,
                                &store_local_value,
                                &is_always_invalidating,
                            );

                            // Case 3 (TS Case 2): B's dependencies are all direct (no property
                            // path), always-invalidating values (Array/Object/Function/JSX)
                            // that are declared by scope A.  When A creates always-invalidating
                            // outputs and ALL of B's deps are those same outputs, A and B always
                            // invalidate together — so merging is correct.
                            //
                            // This is a port of the TS check:
                            //   next.scope.dependencies.every(dep =>
                            //     dep.path.length === 0 &&
                            //     isAlwaysInvalidatingType(dep.identifier.type) &&
                            //     current.scope.declarations.some(decl =>
                            //       decl.identifier.declarationId === dep.identifier.declarationId))
                            //
                            // NOTE: We do NOT use "sentinel A, all A-decls last-used within B"
                            // (the old Case 3) — that was overly aggressive and incorrectly merged
                            // `{}` / `[]` into a reactive dep scope like `[t0, t1, props.value]`.
                            let can_merge_sentinel_into_reactive = if !can_merge {
                                let a_decl_ids: HashSet<IdentifierId> =
                                    cur_scope.declarations.keys().copied().collect();
                                // B must have at least one dep (otherwise use Case 1 / regular merge).
                                !a_decl_ids.is_empty() && !next_scope.dependencies.is_empty() && next_scope.dependencies.iter().all(|dep| {
                                    // Dep must be a direct identifier (no property path like props.value).
                                    if !dep.path.is_empty() { return false; }
                                    // Dep must be declared by A or its StoreLocal value declared by A.
                                    let declared_by_a = a_decl_ids.contains(&dep.place.identifier)
                                        || store_local_value.get(&dep.place.identifier)
                                            .map(|&val_id| a_decl_ids.contains(&val_id))
                                            .unwrap_or(false);
                                    if !declared_by_a { return false; }
                                    // Dep must be of always-invalidating type (Array/Object/Function/JSX).
                                    let direct_inv = is_always_invalidating
                                        .get(&dep.place.identifier)
                                        .copied()
                                        .unwrap_or(false);
                                    let via_store = store_local_value.get(&dep.place.identifier)
                                        .and_then(|&val_id| is_always_invalidating.get(&val_id))
                                        .copied()
                                        .unwrap_or(false);
                                    direct_inv || via_store
                                })
                            } else {
                                false
                            };
                            if std::env::var("RC_DEBUG").is_ok() && can_merge_sentinel_into_reactive {
                                eprintln!("[merge_adjacent] Case3 (always-inv outputs of A are all deps of B)");
                            }

                            if can_merge || can_merge_sentinel_into_reactive {
                                // areLValuesLastUsedByScope: all accumulated lvalues (gap
                                // instructions between A and B) must be last-used BEFORE B.range.end.
                                // Uses actual last-read instruction ID (same as TS's lastUsage map),
                                // not mutableRange.end. This prevents merging when a gap lvalue is
                                // read in the return statement or after scope B ends.
                                let mut all_ok = true;
                                if std::env::var("RC_DEBUG").is_ok() {
                                    eprintln!("[merge_adjacent] checking {} lvalues vs b_end={}", cur.lvalues.len(), b_end);
                                }
                                for &lv in &cur.lvalues {
                                    let lu = last_use(lv);
                                    if std::env::var("RC_DEBUG").is_ok() {
                                        eprintln!("  lv={} last_use={} b_end={}", lv.0, lu, b_end);
                                    }
                                    if lu >= b_end {
                                        all_ok = false;
                                    }
                                }

                                if std::env::var("RC_DEBUG").is_ok() && !all_ok {
                                    eprintln!("[merge_adjacent] BLOCKED by lvalue check");
                                }

                                all_ok
                            } else {
                                false
                            }
                        }
                    }
                }
            } else {
                false
            };

            if merged {
                let cur_sid = current.as_ref().unwrap().scope_id;
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!(
                        "[merge_adjacent] merging scope {:?} into {:?} (immediate)",
                        cur_sid.0, sid.0
                    );
                }
                // Apply immediately so chain merges see the updated declarations.
                merge_scopes_a_into_b(env, cur_sid, sid, &store_local_value);

                // After merging A into B, the new current candidate is B (if eligible).
                // Clear lvalues — gap instructions between B and the NEXT scope will accumulate.
                let eligible = scope_is_eligible_for_merging(&env.scopes[&sid], &is_always_invalidating, &store_local_value);
                if eligible {
                    if let Some(ref mut cur) = current {
                        cur.scope_id = sid;
                        cur.lvalues.clear();
                    }
                } else {
                    current = None;
                }
            } else {
                // No merge: reset current, set this scope as new candidate.
                // Start with EMPTY lvalues — gap instruction lvalues between A and B
                // are accumulated as we process the gap. (Scope A's own declarations
                // are NOT included: they are scope outputs and handled separately by
                // codegen as `let x; if (changed) { x = ...; }`, so they remain
                // accessible outside the scope even when A merges into B. Only gap
                // instruction lvalues — const declarations computed inline between A
                // and B — need to be checked, as those would be hidden inside B if merged.)
                // This matches TS's MergeReactiveScopesThatInvalidateTogether exactly.
                current = None;
                current = Some(Current { scope_id: sid, lvalues: HashSet::new() });
            }
        }
    }

    if std::env::var("RC_DEBUG").is_ok() {
        let mut scope_list: Vec<_> = env.scopes.iter().collect();
        scope_list.sort_by_key(|(_, s)| s.range.start.0);
        eprintln!("  [after merge_adjacent] {} scopes:", scope_list.len());
        for (sid, s) in &scope_list {
            eprintln!(
                "  scope {:?} range=[{},{}] ndeps={}",
                sid.0,
                s.range.start.0,
                s.range.end.0,
                s.dependencies.len()
            );
        }
    }

}

/// Can scope A (current) merge with scope B (next)?
fn can_merge_scopes(
    scope_a: &crate::hir::hir::ReactiveScope,
    scope_b: &crate::hir::hir::ReactiveScope,
    store_local_value: &HashMap<IdentifierId, IdentifierId>,
    is_always_invalidating: &HashMap<IdentifierId, bool>,
) -> bool {
    // Case 1: Equal deps (including both empty → sentinel-sentinel merge).
    if deps_are_equal(scope_a, scope_b) {
        return true;
    }

    // Case 2: B's deps are a non-empty subset of A's always-invalidating outputs.
    // i.e., every dep of B is (a) always-invalidating AND (b) declared by scope A.
    // TS requires dep.path.length === 0 — dep must be a DIRECT reference, not a
    // property access like `x.map`. Only direct refs can be produced by scope A.
    if !scope_b.dependencies.is_empty()
        && scope_b.dependencies.iter().all(|dep| {
            // Path must be empty (TS: dep.path.length === 0).
            if !dep.path.is_empty() {
                if std::env::var("RC_DEBUG").is_ok() { eprintln!("[can_merge] Case2 fail: path non-empty"); }
                return false;
            }
            let dep_id = dep.place.identifier;
            let val_id = store_local_value.get(&dep_id).copied().unwrap_or(dep_id);
            let always_inv = is_always_invalidating.get(&val_id).copied().unwrap_or(false)
                || is_always_invalidating.get(&dep_id).copied().unwrap_or(false);
            if !always_inv {
                return false;
            }
            // The dep must be produced within scope A (declared by A).
            scope_a.declarations.contains_key(&dep_id)
                || scope_a.declarations.contains_key(&val_id)
        })
    {
        return true;
    }

    false
}

/// Is this scope eligible to be a merge candidate?
///
/// Eligible if:
/// - No dependencies (sentinel scope), OR
/// - Any declaration is always-invalidating (mirrors TS `.some()` check).
fn scope_is_eligible_for_merging(
    scope: &crate::hir::hir::ReactiveScope,
    is_always_invalidating: &HashMap<IdentifierId, bool>,
    store_local_value: &HashMap<IdentifierId, IdentifierId>,
) -> bool {
    // Sentinel scopes (no deps) are always eligible.
    if scope.dependencies.is_empty() {
        return true;
    }
    // Scopes that always invalidate are eligible as merge candidates for Case 2.
    if scope.declarations.is_empty() {
        return false;
    }
    // TS uses .some(): eligible if ANY declaration is always-invalidating.
    scope.declarations.keys().any(|&decl_id| {
        let val_id = store_local_value.get(&decl_id).copied().unwrap_or(decl_id);
        is_always_invalidating.get(&val_id).copied().unwrap_or(false)
            || is_always_invalidating.get(&decl_id).copied().unwrap_or(false)
    })
}

/// Is this instruction safe in a gap between two scope blocks?
fn is_safe_gap_instruction(value: &InstructionValue) -> bool {
    match value {
        InstructionValue::LoadLocal { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::TernaryExpression { .. }
        | InstructionValue::UnaryExpression { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::TemplateLiteral { .. } => true,
        // Outlined function expressions (name_hint is set) are module-level stable stubs.
        // They are hoisted outside the component and never change between renders, so they
        // are safe to skip over during scope gap analysis — they do not affect memoization.
        InstructionValue::FunctionExpression { name_hint, .. } => name_hint.is_some(),
        InstructionValue::StoreLocal { lvalue, .. } => {
            // Only Const StoreLocals are safe in gaps; Let resets the merge candidate.
            matches!(lvalue.kind, InstructionKind::Const | InstructionKind::HoistedConst)
        }
        _ => false,
    }
}

/// Returns true for instruction types whose result always allocates a new value.
/// Outlined FunctionExpressions (name_hint set) are module-level stable stubs
/// that never change between renders — NOT always-invalidating.
fn always_invalidating_instruction(value: &InstructionValue) -> bool {
    if let InstructionValue::FunctionExpression { name_hint, .. } = value {
        if name_hint.is_some() {
            return false; // Outlined — stable module-level function.
        }
    }
    matches!(
        value,
        InstructionValue::ObjectExpression { .. }
            | InstructionValue::ArrayExpression { .. }
            | InstructionValue::FunctionExpression { .. }
            | InstructionValue::ObjectMethod { .. }
            | InstructionValue::JsxExpression { .. }
            | InstructionValue::JsxFragment { .. }
            | InstructionValue::NewExpression { .. }
            | InstructionValue::TaggedTemplateExpression { .. }
            | InstructionValue::MethodCall { .. }
    )
}

fn merge_scopes_a_into_b(
    env: &mut Environment,
    sid_a: ScopeId,
    sid_b: ScopeId,
    store_local_value: &HashMap<IdentifierId, IdentifierId>,
) {
    let a_range_start = env.scopes[&sid_a].range.start;
    let a_declarations = env.scopes[&sid_a].declarations.clone();
    let a_deps = env.scopes[&sid_a].dependencies.clone();

    {
        let scope_b = env.scopes.get_mut(&sid_b).unwrap();
        scope_b.range.start = a_range_start;
        scope_b.declarations.extend(a_declarations);

        // After merging A's declarations into B, remove any of B's existing deps
        // that are now declared within B (they are internal, not external deps).
        // Also filter deps whose StoreLocal value is now internal to the merged scope.
        let all_declared: HashSet<IdentifierId> = scope_b.declarations.keys().copied().collect();
        let dep_is_internal = |dep: &ReactiveScopeDependency| -> bool {
            if all_declared.contains(&dep.place.identifier) { return true; }
            if let Some(&val_id) = store_local_value.get(&dep.place.identifier) {
                if all_declared.contains(&val_id) { return true; }
            }
            false
        };
        scope_b.dependencies.retain(|dep| !dep_is_internal(dep));

        let existing: HashSet<IdentifierId> = scope_b
            .dependencies
            .iter()
            .map(|d| d.place.identifier)
            .collect();
        for dep in a_deps {
            if !existing.contains(&dep.place.identifier) && !dep_is_internal(&dep) {
                scope_b.dependencies.push(dep);
            }
        }
    }

    env.scopes.remove(&sid_a);

    for ident in env.identifiers.values_mut() {
        if ident.scope == Some(sid_a) {
            ident.scope = Some(sid_b);
        }
    }
}

fn deps_are_equal(
    scope_a: &crate::hir::hir::ReactiveScope,
    scope_b: &crate::hir::hir::ReactiveScope,
) -> bool {
    let deps_a = &scope_a.dependencies;
    let deps_b = &scope_b.dependencies;
    if deps_a.len() != deps_b.len() {
        return false;
    }
    // Both empty → equal (sentinel-sentinel merging allowed by TS).
    if deps_a.is_empty() {
        return true;
    }
    deps_a.iter().zip(deps_b.iter()).all(|(a, b)| {
        a.place.identifier == b.place.identifier
            && a.path.len() == b.path.len()
            && a.path
                .iter()
                .zip(b.path.iter())
                .all(|(pa, pb)| pa.property == pb.property)
    })
}

/// Returns true if `name` looks like a React hook name: `use` followed by an uppercase letter.
fn is_hook_name(name: &str) -> bool {
    name.starts_with("use")
        && name.len() > 3
        && name[3..].starts_with(|c: char| c.is_uppercase())
}
