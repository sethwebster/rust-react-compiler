/// Merge overlapping reactive scopes.
///
/// After `infer_reactive_scope_variables`, some scopes may have overlapping
/// instruction ranges. This happens when two identifiers are in separate
/// disjoint groups but their mutable ranges overlap.
///
/// Two overlapping scopes are merged if:
/// 1. They share at least one DeclarationId (SSA splits of the same named variable), OR
/// 2. Both scopes are "always-invalidating" — all their declarations come from
///    always-allocating instructions (ObjectExpression, ArrayExpression, etc.).
///    These represent values that are recreated every render, so merging their
///    sentinel scopes is always safe and matches the TS compiler's behavior.
///    Note: deps are not yet computed at this stage, so we infer "sentinel" from
///    instruction types rather than from scope.dependencies.
///
/// Adjacent scopes (B.start == A.end) are handled by
/// `merge_reactive_scopes_that_invalidate_together` AFTER
/// `propagate_scope_dependencies_hir` computes dependency sets.
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    ArrayElement, CallArg, DeclarationId, HIRFunction, IdentifierId, InstructionValue,
    JsxAttribute, JsxTag, ObjectExpressionProperty, ScopeId,
};

pub fn run(_hir: &mut HIRFunction) {}

/// Collect all Place identifier IDs that an instruction reads as inputs (not outputs).
/// Used to determine if one scope is dependent on another's declarations.
fn instruction_place_reads(value: &InstructionValue) -> Vec<IdentifierId> {
    let mut reads = Vec::new();
    match value {
        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } => {
            reads.push(place.identifier);
        }
        InstructionValue::StoreLocal { value, .. } => {
            reads.push(value.identifier);
        }
        InstructionValue::StoreContext { value, .. } => {
            reads.push(value.identifier);
        }
        InstructionValue::StoreGlobal { value, .. } => {
            reads.push(value.identifier);
        }
        InstructionValue::Destructure { value, .. } => {
            reads.push(value.identifier);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            reads.push(left.identifier);
            reads.push(right.identifier);
        }
        InstructionValue::TernaryExpression { test, consequent, alternate, .. } => {
            reads.push(test.identifier);
            reads.push(consequent.identifier);
            reads.push(alternate.identifier);
        }
        InstructionValue::UnaryExpression { value, .. }
        | InstructionValue::TypeCastExpression { value, .. }
        | InstructionValue::Await { value, .. }
        | InstructionValue::NextPropertyOf { value, .. } => {
            reads.push(value.identifier);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            reads.push(callee.identifier);
            for arg in args {
                match arg {
                    CallArg::Place(p) => reads.push(p.identifier),
                    CallArg::Spread(s) => reads.push(s.place.identifier),
                }
            }
        }
        InstructionValue::MethodCall { receiver, property, args, .. } => {
            reads.push(receiver.identifier);
            reads.push(property.identifier);
            for arg in args {
                match arg {
                    CallArg::Place(p) => reads.push(p.identifier),
                    CallArg::Spread(s) => reads.push(s.place.identifier),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            reads.push(callee.identifier);
            for arg in args {
                match arg {
                    CallArg::Place(p) => reads.push(p.identifier),
                    CallArg::Spread(s) => reads.push(s.place.identifier),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectExpressionProperty::Property(p) => {
                        reads.push(p.place.identifier);
                        if let crate::hir::hir::ObjectPropertyKey::Computed(key_place) = &p.key {
                            reads.push(key_place.identifier);
                        }
                    }
                    ObjectExpressionProperty::Spread(s) => reads.push(s.place.identifier),
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) => reads.push(p.identifier),
                    ArrayElement::Spread(s) => reads.push(s.place.identifier),
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::PropertyDelete { object, .. } => {
            reads.push(object.identifier);
        }
        InstructionValue::PropertyStore { object, value, .. } => {
            reads.push(object.identifier);
            reads.push(value.identifier);
        }
        InstructionValue::ComputedLoad { object, property, .. }
        | InstructionValue::ComputedDelete { object, property, .. } => {
            reads.push(object.identifier);
            reads.push(property.identifier);
        }
        InstructionValue::ComputedStore { object, property, value, .. } => {
            reads.push(object.identifier);
            reads.push(property.identifier);
            reads.push(value.identifier);
        }
        InstructionValue::JsxExpression { tag, props, children, .. } => {
            if let JsxTag::Place(p) = tag {
                reads.push(p.identifier);
            }
            for prop in props {
                match prop {
                    JsxAttribute::Spread { argument } => reads.push(argument.identifier),
                    JsxAttribute::Attribute { place, .. } => reads.push(place.identifier),
                }
            }
            if let Some(children) = children {
                for child in children {
                    reads.push(child.identifier);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                reads.push(child.identifier);
            }
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            for ctx in &lowered_func.func.context {
                reads.push(ctx.identifier);
            }
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for s in subexprs {
                reads.push(s.identifier);
            }
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            reads.push(tag.identifier);
        }
        InstructionValue::GetIterator { collection, .. } => {
            reads.push(collection.identifier);
        }
        InstructionValue::IteratorNext { iterator, collection, .. } => {
            reads.push(iterator.identifier);
            reads.push(collection.identifier);
        }
        InstructionValue::PrefixUpdate { lvalue, value, .. }
        | InstructionValue::PostfixUpdate { lvalue, value, .. } => {
            reads.push(lvalue.identifier);
            reads.push(value.identifier);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            reads.push(decl.identifier);
        }
        // No inputs: LoadGlobal, DeclareLocal, DeclareContext, Primitive, JsxText,
        // RegExpLiteral, MetaProperty, Debugger, StartMemoize, InlineJs, etc.
        _ => {}
    }
    reads
}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    if env.scopes.len() <= 1 {
        return;
    }

    // Build map of IdentifierId → global name for hook detection.
    let mut global_names: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                use crate::hir::hir::NonLocalBinding;
                let name = match binding {
                    NonLocalBinding::Global { name } => Some(name.clone()),
                    NonLocalBinding::ModuleLocal { name } => Some(name.clone()),
                    NonLocalBinding::ImportDefault { name, .. }
                    | NonLocalBinding::ImportNamespace { name, .. }
                    | NonLocalBinding::ImportSpecifier { name, .. } => Some(name.clone()),
                };
                if let Some(n) = name {
                    global_names.insert(instr.lvalue.identifier, n);
                }
            }
        }
    }
    let is_hook_name = |name: &str| -> bool {
        name.starts_with("use") && name[3..].chars().next().map_or(false, |c| c.is_uppercase())
    };

    // Precompute which identifiers are produced by always-invalidating instructions
    // (ObjectExpression, ArrayExpression, etc.). These represent values that are
    // always freshly allocated — scopes that ONLY declare such values are effectively
    // sentinel scopes (always-invalidating, no external deps), even though
    // scope.dependencies hasn't been computed yet at this pipeline stage.
    //
    // CallExpression and MethodCall (non-hook) results are also always-invalidating:
    // unknown functions may return different values each render. This allows overlapping
    // scopes where a CallExpression result is used inside an ObjectExpression/ArrayExpression
    // to merge correctly (e.g., `{session_id: getNumber()}` merges with getNumber() scope).
    let mut ident_is_always_inv: HashMap<IdentifierId, bool> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            // Outlined FunctionExpressions (name_hint set) are module-level stable stubs —
            // they never change between renders, so they should NOT be treated as always-inv.
            let is_outlined_fn = if let InstructionValue::FunctionExpression { name_hint, .. } = &instr.value {
                name_hint.is_some()
            } else {
                false
            };
            let always_inv = !is_outlined_fn && matches!(&instr.value,
                InstructionValue::ObjectExpression { .. }
                | InstructionValue::ArrayExpression { .. }
                | InstructionValue::FunctionExpression { .. }
                | InstructionValue::ObjectMethod { .. }
                | InstructionValue::JsxExpression { .. }
                | InstructionValue::JsxFragment { .. }
                | InstructionValue::NewExpression { .. }
                | InstructionValue::TaggedTemplateExpression { .. }
                | InstructionValue::MethodCall { .. }
            ) || matches!(&instr.value,
                // Non-hook CallExpressions produce new values every render.
                InstructionValue::CallExpression { callee, .. }
                    if !global_names.get(&callee.identifier).map_or(false, |n| is_hook_name(n))
            ) || (
                // A LoadLocal/LoadGlobal/LoadContext of a non-reactive place is "always-inv"
                // for scope-merging purposes: it reads a stable reference (e.g., a module-level
                // global or non-reactive binding) that doesn't need independent memoization.
                // This allows scopes like `[someGlobal]` (ArrayExpression + LoadLocal(someGlobal))
                // to be treated as sentinel scopes even though they contain a LoadLocal.
                matches!(&instr.value,
                    InstructionValue::LoadLocal { .. }
                    | InstructionValue::LoadGlobal { .. }
                    | InstructionValue::LoadContext { .. }
                ) && !instr.lvalue.reactive
            ) || matches!(&instr.value,
                // Primitives (numbers, strings, booleans) are stack values — no allocation.
                // They should not block "all-always-inv" detection on a parent scope.
                InstructionValue::Primitive { .. }
                    | InstructionValue::BinaryExpression { .. }
                    | InstructionValue::TernaryExpression { .. }
                    | InstructionValue::UnaryExpression { .. }
            ) || (
                // PropertyLoad/ComputedLoad are always-inv only when their result is NOT reactive.
                // A reactive property load like `props.b` (where props is reactive) IS reactive
                // and should not be treated as always-inv (it's a real dep for memoization).
                matches!(&instr.value,
                    InstructionValue::PropertyLoad { .. }
                    | InstructionValue::ComputedLoad { .. }
                ) && !instr.lvalue.reactive
            );
            ident_is_always_inv.insert(instr.lvalue.identifier, always_inv);
        }
    }
    // Second pass: propagate always-inv transitively through StoreLocal and LoadLocal.
    // If `object = {}` (StoreLocal where value is always-inv), the binding is always-inv.
    // If `t29 = object` (LoadLocal where source is always-inv), the result is always-inv.
    // Iterate to fixpoint for chains.
    let mut changed = true;
    while changed {
        changed = false;
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        let val_always_inv = ident_is_always_inv.get(&value.identifier).copied().unwrap_or(false);
                        if val_always_inv {
                            // Propagate to binding target
                            let e = ident_is_always_inv.entry(lvalue.place.identifier).or_insert(false);
                            if !*e { *e = true; changed = true; }
                            // Propagate to instruction's phantom lvalue
                            let e2 = ident_is_always_inv.entry(instr.lvalue.identifier).or_insert(false);
                            if !*e2 { *e2 = true; changed = true; }
                        }
                    }
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        let src_always_inv = ident_is_always_inv.get(&place.identifier).copied().unwrap_or(false);
                        if src_always_inv {
                            let e = ident_is_always_inv.entry(instr.lvalue.identifier).or_insert(false);
                            if !*e { *e = true; changed = true; }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Single-pass demotion: after fixpoint, mark any identifier that is ever assigned
    // a non-always-inv value (via StoreLocal) as NOT always-inv. This handles cases like
    // `y = {}; y = x[0]` where y is first marked always-inv but then overwritten.
    // This is safe (no oscillation) because we only set false, never true.
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                let val_always_inv = ident_is_always_inv.get(&value.identifier).copied().unwrap_or(false);
                if !val_always_inv {
                    ident_is_always_inv.insert(lvalue.place.identifier, false);
                    ident_is_always_inv.insert(instr.lvalue.identifier, false);
                }
            }
        }
    }

    // A scope is "always-invalidating sentinel" if it has at least one declaration
    // and ALL declarations are produced by always-invalidating instructions.
    // We look up the CURRENT env.scopes (updated by merges) each time.
    let scope_is_always_inv = |sid: ScopeId, env: &Environment| -> bool {
        let scope = match env.scopes.get(&sid) {
            Some(s) => s,
            None => return false,
        };
        !scope.declarations.is_empty()
            && scope.declarations.keys().all(|id|
                ident_is_always_inv.get(id).copied().unwrap_or(false)
            )
    };

    // Build a map: ScopeId → set of DeclarationIds in that scope.
    // Two scopes may only merge if they share at least one DeclarationId.
    let mut scope_decl_ids: HashMap<ScopeId, HashSet<DeclarationId>> = HashMap::new();
    for (&sid, scope) in &env.scopes {
        let decl_ids: HashSet<DeclarationId> = scope
            .declarations
            .keys()
            .filter_map(|id| env.identifiers.get(id).map(|i| i.declaration_id))
            .collect();
        scope_decl_ids.insert(sid, decl_ids);
    }

    // Collect scopes sorted by range start.
    let mut scope_list: Vec<ScopeId> = env.scopes.keys().copied().collect();
    scope_list.sort_by(|a, b| {
        let sa = &env.scopes[a];
        let sb = &env.scopes[b];
        sa.range.start.cmp(&sb.range.start)
            .then(sa.range.end.cmp(&sb.range.end))
    });

    // Build mapping: old scope id → survivor scope id (after merging).
    let mut merged_to: HashMap<ScopeId, ScopeId> = HashMap::new();

    // Process sorted scopes; merge only strictly overlapping pairs that share
    // at least one DeclarationId.
    // Use a running list of (survivor_id, effective_end).
    let mut active: Vec<(ScopeId, u32)> = Vec::new(); // (scope_id, end_exclusive)

    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[merge] {} scopes to process", scope_list.len());
        for &sid in &scope_list {
            let sc = &env.scopes[&sid];
            eprintln!("[merge] scope {:?} range=[{},{}]", sid.0, sc.range.start.0, sc.range.end.0);
        }
    }

    for &sid in &scope_list {
        let scope = &env.scopes[&sid];
        let s = scope.range.start.0;
        let e = scope.range.end.0;

        let mut best_merge: Option<usize> = None;

        for (i, &(survivor, survivor_end)) in active.iter().enumerate() {
            let ss = env.scopes[&survivor].range.start.0;
            let se = survivor_end;
            // Strict overlap: [ss, se) overlaps [s, e) iff ss < e && s < se.
            if ss < e && s < se {
                // Both scopes are always-invalidating: safe to merge into one sentinel block.
                // This is detected before deps are computed by checking instruction types.
                let survivor_always_inv = scope_is_always_inv(survivor, env);
                let new_always_inv = scope_is_always_inv(sid, env);
                // Only merge if NEITHER scope reads reactive external identifiers.
                // If either scope reads reactive identifiers (props/state/etc.), it
                // has real deps that may differ from the other scope's deps — merging
                // would over-invalidate the combined scope. Such scopes should remain
                // separate until propagate_scope_dependencies_hir computes their deps,
                // then merge_reactive_scopes_that_invalidate_together can merge them
                // correctly (e.g., via Case 2 for always-inv output deps).
                if survivor_always_inv && new_always_inv {
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!(
                            "[merge] merging {:?} into {:?}: both always-invalidating",
                            sid.0, survivor.0
                        );
                    }
                    best_merge = Some(i);
                    break;
                }
                // Two scopes with the SAME start that overlap — they were aligned together
                // (e.g., both expanded to start at a loop entry block's first instruction).
                // These will be emitted in the same block region anyway, so merge them.
                if ss == s {
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!(
                            "[merge] merging {:?} into {:?}: same start (co-aligned)",
                            sid.0, survivor.0
                        );
                    }
                    best_merge = Some(i);
                    break;
                }
                // If new scope is fully contained within an always-invalidating survivor, merge it.
                // This handles hoisting cases where a sub-expression scope (e.g., ObjectExpression
                // containing a Primitive) is nested inside an always-invalidating parent scope.
                if survivor_always_inv && s >= ss && e <= se {
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!(
                            "[merge] merging {:?} into {:?}: new scope fully contained in always-inv survivor",
                            sid.0, survivor.0
                        );
                    }
                    best_merge = Some(i);
                    break;
                }
                // Otherwise: only merge if the scopes share at least one DeclarationId.
                // This prevents merging independent allocations with a dep scope.
                let survivor_decls = scope_decl_ids.get(&survivor);
                let new_decls = scope_decl_ids.get(&sid);
                let shares_decls = match (survivor_decls, new_decls) {
                    (Some(a), Some(b)) => a.iter().any(|d| b.contains(d)),
                    _ => false,
                };
                if shares_decls {
                    best_merge = Some(i);
                    break;
                }
                // No shared declaration and not both always-invalidating: treat as independent.
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!(
                        "[merge] NOT merging scope {:?} into {:?}: no shared declarations, survivor_always_inv={}, new_always_inv={}",
                        sid.0, survivor.0, survivor_always_inv, new_always_inv,
                    );
                }
            }
            // Adjacent scopes (se == s) are NOT merged here. They are handled by
            // `merge_reactive_scopes_that_invalidate_together` after deps are computed.
        }

        if let Some(i) = best_merge {
            let (survivor, _) = active[i];
            if std::env::var("RC_DEBUG").is_ok() {
                eprintln!("[merge] merging scope {:?} into {:?}", sid.0, survivor.0);
            }
            merged_to.insert(sid, survivor);
            if e > active[i].1 {
                active[i].1 = e;
            }
            // Update the survivor's decl set to include the merged scope's decls.
            if let Some(new_decls) = scope_decl_ids.remove(&sid) {
                scope_decl_ids.entry(survivor).or_default().extend(new_decls);
            }
        } else {
            if std::env::var("RC_DEBUG").is_ok() {
                eprintln!("[merge] scope {:?} range=[{},{}] stays independent", sid.0, s, e);
            }
            active.push((sid, e));
            merged_to.insert(sid, sid);
        }
    }

    // Update scope ranges for survivors.
    for &(sid, new_end) in &active {
        if let Some(scope) = env.scopes.get_mut(&sid) {
            scope.range.end.0 = new_end;
        }
    }

    // Transfer declarations and dependencies from merged scopes into their survivors.
    // merged_to maps every scope (including survivors, which map to themselves) to its survivor.
    let survivors: std::collections::HashSet<ScopeId> = active.iter().map(|(s, _)| *s).collect();
    let merged_pairs: Vec<(ScopeId, ScopeId)> = merged_to
        .iter()
        .filter(|(k, v)| k != v)
        .map(|(&k, &v)| (k, v))
        .collect();
    for (merged_sid, survivor_sid) in merged_pairs {
        let declarations = env.scopes.get(&merged_sid).map(|s| s.declarations.clone()).unwrap_or_default();
        let dependencies = env.scopes.get(&merged_sid).map(|s| s.dependencies.clone()).unwrap_or_default();
        if let Some(survivor_scope) = env.scopes.get_mut(&survivor_sid) {
            survivor_scope.declarations.extend(declarations);
            let existing: std::collections::HashSet<IdentifierId> =
                survivor_scope.dependencies.iter().map(|d| d.place.identifier).collect();
            for dep in dependencies {
                if !existing.contains(&dep.place.identifier) {
                    survivor_scope.dependencies.push(dep);
                }
            }
        }
    }

    // Remove merged-away scopes from env.
    env.scopes.retain(|sid, _| survivors.contains(sid));

    // Re-assign ident.scope: if an ident pointed to a merged scope, point to survivor.
    for ident in env.identifiers.values_mut() {
        if let Some(old_sid) = ident.scope {
            if let Some(&new_sid) = merged_to.get(&old_sid) {
                ident.scope = Some(new_sid);
            }
        }
    }
}
