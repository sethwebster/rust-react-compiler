/// Infer reactive scope variables.
///
/// Port of InferReactiveScopeVariables.ts / findDisjointMutableValues.
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    ArrayElement, DeclarationId, HIRFunction, IdentifierId, InstructionId,
    InstructionValue, MutableRange, NonLocalBinding, ObjectPatternProperty, Pattern,
    ReactiveScope, ReactiveScopeDeclaration, ScopeId, SourceLocation, Terminal,
};
use crate::hir::visitors::each_instruction_value_operand;
use crate::utils::disjoint_set::DisjointSet;

pub fn run(_hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    let canonical = {
        let mut ds = find_disjoint_mutable_values(hir, env);
        ds.canonicalize()
    };

    // Build ReactiveScope for each unique root.
    let mut scopes: HashMap<IdentifierId, ReactiveScope> = HashMap::new();

    for (&id, &root) in &canonical {
        let ident_range = env
            .get_identifier(id)
            .map(|i| i.mutable_range.clone())
            .unwrap_or_else(MutableRange::zero);

        let scope = scopes.entry(root).or_insert_with(|| {
            let scope_id = env.new_scope_id();
            ReactiveScope {
                id: scope_id,
                range: ident_range.clone(),
                dependencies: Vec::new(),
                declarations: HashMap::new(),
                reassignments: Vec::new(),
                merged_ranges: Vec::new(),
                early_returns: Vec::new(),
                early_return_value: None,
                early_return_label_id: None,
                loc: SourceLocation::Generated,
            }
        });

        if scope.range.start.0 == 0 {
            scope.range.start = ident_range.start;
        } else if ident_range.start.0 != 0 && ident_range.start < scope.range.start {
            scope.range.start = ident_range.start;
        }
        if ident_range.end > scope.range.end {
            scope.range.end = ident_range.end;
        }
    }

    let root_to_scope_id: HashMap<IdentifierId, ScopeId> =
        scopes.iter().map(|(&root, s)| (root, s.id)).collect();

    // Write scope IDs back to identifiers AND populate scope.declarations.
    for (&id, &root) in &canonical {
        if let Some(&scope_id) = root_to_scope_id.get(&root) {
            if let Some(ident) = env.get_identifier_mut(id) {
                ident.scope = Some(scope_id);
            }
            if let Some(scope) = scopes.get_mut(&root) {
                scope.declarations.insert(id, ReactiveScopeDeclaration { identifier: id, scope: scope_id });
            }
        }
    }

    // Register scopes in env.
    for (_root, scope) in scopes {
        let sid = scope.id;
        env.scopes.insert(sid, scope);
    }
}

fn find_disjoint_mutable_values(
    hir: &HIRFunction,
    env: &Environment,
) -> DisjointSet<IdentifierId> {
    let mut set = DisjointSet::new();
    let mut declarations: HashMap<DeclarationId, IdentifierId> = HashMap::new();

    // Build a map: identifier id → binding name, for detecting hook calls.
    // Covers all NonLocalBinding variants (Global, ImportSpecifier, etc.).
    let mut global_names: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                let name = match binding {
                    NonLocalBinding::Global { name } => name.clone(),
                    NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                    NonLocalBinding::ImportDefault { name, .. } => name.clone(),
                    NonLocalBinding::ImportNamespace { name, .. } => name.clone(),
                    NonLocalBinding::ModuleLocal { name } => name.clone(),
                };
                global_names.insert(instr.lvalue.identifier, name);
            }
        }
    }

    // Build a set of identifiers that are results of hook calls.
    // These must NOT be grouped into memoized scopes.
    let mut hook_results: HashSet<IdentifierId> = HashSet::new();
    // Track named variables (not temps) that hold hook results directly.
    // When a variable is the target of StoreLocal(hook_result), loading it
    // must not link it into a reactive scope (e.g. `ref2 = useRef(null);`).
    let mut hook_result_vars: HashSet<IdentifierId> = HashSet::new();
    // Track the SSA temps that are results of outlined FunctionExpressions.
    // These are module-level stable stubs (e.g. `_temp`) — their holders must
    // not be co-located with reactive scopes via the FunctionExpression capture union.
    let mut outlined_fn_ids: HashSet<IdentifierId> = HashSet::new();
    // Track named variables that hold outlined function references.
    // e.g. `const setGlobal = _temp` → setGlobal should not be unioned via captures.
    let mut outlined_fn_holder_vars: HashSet<IdentifierId> = HashSet::new();
    // Build maps: identifier → may_allocate and identifier → reactive.
    // Used to filter StoreLocal value operands: only link values that allocate or are reactive.
    let mut ident_allocates: HashSet<IdentifierId> = HashSet::new();
    // Track identifiers that DIRECTLY allocate (via may_allocate, not propagated).
    // Used to distinguish ArrayExpression/ObjectExpression from TernaryExpression results.
    let mut ident_direct_allocates: HashSet<IdentifierId> = HashSet::new();
    // Track TernaryExpression result identifiers specifically.
    // Used to limit "propagated allocates join" in StoreLocal to only ternary-carried arrays.
    let mut ident_is_ternary: HashSet<IdentifierId> = HashSet::new();
    let mut ident_reactive: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                if global_names.get(&callee.identifier).map_or(false, |n| is_hook_name(n)) {
                    hook_results.insert(instr.lvalue.identifier);
                }
            }
            // Track outlined FunctionExpression results (name_hint is set by outline_functions).
            if let InstructionValue::FunctionExpression { name_hint, .. } = &instr.value {
                if name_hint.is_some() {
                    outlined_fn_ids.insert(instr.lvalue.identifier);
                }
            }
            // Track named variables that hold hook results (e.g. `const ref2 = useRef(null)`)
            // or outlined function references (e.g. `const setGlobal = _temp`).
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                if hook_results.contains(&value.identifier) {
                    hook_result_vars.insert(lvalue.place.identifier);
                }
                if outlined_fn_ids.contains(&value.identifier) {
                    outlined_fn_holder_vars.insert(lvalue.place.identifier);
                }
            }
            if may_allocate(&instr.value, &global_names) {
                ident_allocates.insert(instr.lvalue.identifier);
                ident_direct_allocates.insert(instr.lvalue.identifier);
            }
            if matches!(&instr.value, InstructionValue::TernaryExpression { .. }) {
                ident_is_ternary.insert(instr.lvalue.identifier);
            }
            if instr.lvalue.reactive {
                ident_reactive.insert(instr.lvalue.identifier);
            }
        }
    }

    // Collect phi places that are results of logical expressions (&&, ||, ??).
    // These are phi nodes in the fallthrough block of Logical/Branch(logical_op) terminals.
    // Logical expression results select between existing memoized values — they do NOT
    // create new allocations. Memoizing them adds no value and creates spurious scopes.
    let mut logical_phi_ids: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        let fallthrough_of_logical = match &block.terminal {
            Terminal::Logical { fallthrough, .. } => Some(*fallthrough),
            Terminal::Branch { logical_op: Some(_), fallthrough, .. } => Some(*fallthrough),
            _ => None,
        };
        if let Some(ft_block_id) = fallthrough_of_logical {
            if let Some(ft_block) = hir.body.blocks.get(&ft_block_id) {
                for phi in &ft_block.phis {
                    logical_phi_ids.insert(phi.place.identifier);
                }
            }
        }
    }

    // Propagate ident_allocates through LoadLocal/LoadContext chains and phi nodes.
    // When `t = LoadLocal(arr)` and `arr` allocates, `t` should also be
    // considered allocating. This matters for default parameter patterns where
    // `arr = [-1, 1]` (allocates) → `t = LoadLocal(arr)` → `StoreLocal(x, t)`.
    // Similarly, if a phi node `x = phi(arr, _T0)` where `arr` allocates,
    // then `x` should be considered allocating.
    // Exception: phi nodes from logical expressions are NOT propagated — they
    // select between existing values, not create new allocations.
    {
        let mut changed = true;
        while changed {
            changed = false;
            for (_, block) in &hir.body.blocks {
                for instr in &block.instructions {
                    if ident_allocates.contains(&instr.lvalue.identifier) {
                        continue;
                    }
                    let propagates = match &instr.value {
                        // LoadLocal/LoadContext: propagate from source
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => {
                            ident_allocates.contains(&place.identifier)
                        }
                        // TernaryExpression: propagates if either branch allocates
                        InstructionValue::TernaryExpression { consequent, alternate, .. } => {
                            ident_allocates.contains(&consequent.identifier)
                                || ident_allocates.contains(&alternate.identifier)
                        }
                        _ => false,
                    };
                    if propagates {
                        ident_allocates.insert(instr.lvalue.identifier);
                        changed = true;
                    }
                }
                for phi in &block.phis {
                    // Skip logical expression phi nodes — they don't allocate.
                    if logical_phi_ids.contains(&phi.place.identifier) {
                        continue;
                    }
                    if !ident_allocates.contains(&phi.place.identifier) {
                        let any_alloc = phi.operands.values()
                            .any(|op| ident_allocates.contains(&op.identifier));
                        if any_alloc {
                            ident_allocates.insert(phi.place.identifier);
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    for (_, block) in &hir.body.blocks {
        for phi in &block.phis {
            // Logical expression phi nodes (&&, ||, ??) do not create new allocations —
            // they select between existing memoized values. Skip them entirely.
            if logical_phi_ids.contains(&phi.place.identifier) {
                continue;
            }
            let phi_range = env
                .get_identifier(phi.place.identifier)
                .map(|i| i.mutable_range.clone())
                .unwrap_or_else(MutableRange::zero);
            let block_start = block
                .instructions
                .first()
                .map(|i| i.id)
                .unwrap_or(block.terminal.id());
            let mutated_after = phi_range.start.0 + 1 != phi_range.end.0
                && phi_range.end > block_start;
            // Also union when a phi operand allocates (e.g., default param with array literal).
            // This ensures phi results that carry allocating values get memoized scopes.
            let any_operand_allocates = phi.operands.values()
                .any(|op| ident_allocates.contains(&op.identifier));
            if mutated_after || any_operand_allocates {
                let mut operands = vec![phi.place.identifier];
                let phi_decl = env
                    .get_identifier(phi.place.identifier)
                    .map(|i| i.declaration_id);
                if let Some(decl_id) = phi_decl {
                    if let Some(&did) = declarations.get(&decl_id) {
                        operands.push(did);
                    }
                }
                for op in phi.operands.values() {
                    operands.push(op.identifier);
                }
                set.union(&operands);
            }
        }

        for instr in &block.instructions {
            let lvalue_id = instr.lvalue.identifier;
            let lvalue_range = env
                .get_identifier(lvalue_id)
                .map(|i| i.mutable_range.clone())
                .unwrap_or_else(MutableRange::zero);
            // `lvalue_reactive`: the place's reactive flag (set by infer_reactive_places).
            // Non-reactive values (e.g. LoadGlobal of a non-hook) must not be added
            // to the disjoint set unless they may_allocate — this prevents spurious
            // scopes for transparent loads.
            let lvalue_reactive = instr.lvalue.reactive;
            let lvalue_mutable = lvalue_range.end.0 > lvalue_range.start.0 + 1;

            let mut operands: Vec<IdentifierId> = Vec::new();
            // Transparent instructions (LoadLocal, LoadContext, PropertyLoad, LoadGlobal) are
            // pure reads. Their SSA results should NOT get their own disjoint-set node just
            // because of liveness range. They're handled by their match arms below if mutable.
            let is_transparent_read = matches!(
                &instr.value,
                InstructionValue::LoadLocal { .. }
                    | InstructionValue::LoadContext { .. }
                    | InstructionValue::PropertyLoad { .. }
                    | InstructionValue::ComputedLoad { .. }
                    | InstructionValue::LoadGlobal { .. }
            );
            // Also check propagated ident_allocates for non-transparent instructions:
            // TernaryExpression results that carry an allocating value should be treated
            // as may_alloc=true. Exclude transparent reads (LoadLocal, etc.) which must
            // not be added to the disjoint set via this path.
            let may_alloc = may_allocate(&instr.value, &global_names)
                || (!is_transparent_read && ident_allocates.contains(&lvalue_id));
            // Hook calls (useState, useEffect, useMemo, useCallback, etc.) must run
            // unconditionally on every render (React's rules of hooks). Their lvalues
            // must NOT be added to the disjoint set (no memoized scope created directly).
            let is_hook_call = matches!(
                &instr.value,
                InstructionValue::CallExpression { callee, .. }
                    if global_names.get(&callee.identifier).map_or(false, |n| is_hook_name(n))
            );
            if !is_hook_call && (may_alloc || (lvalue_mutable && lvalue_reactive && !is_transparent_read)) {
                operands.push(lvalue_id);
            }

            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                }
                InstructionValue::DeclareContext { lvalue, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                    // If storing a hook result, skip the target variable too —
                    // variables holding hook results must not be memoized.
                    if !hook_results.contains(&value.identifier) {
                        // Only create a scope for the stored variable when the value ALLOCATES.
                        // Non-allocating reactive values (property accesses, binary ops, etc.)
                        // do not need memoized scopes — they're cheap to recompute and serve
                        // only as scope dependencies, not scope outputs.
                        let val_allocates = ident_allocates.contains(&value.identifier);
                        if val_allocates {
                            let val_is_ternary = ident_is_ternary.contains(&value.identifier);
                            if val_is_ternary && !ident_direct_allocates.contains(&value.identifier) {
                                // Value is a TernaryExpression result that carries an allocating
                                // branch (e.g., `cond ? [-1,1] : param`): always join dest with value.
                                // This handles default param patterns.
                                operands.push(pid);
                                operands.push(value.identifier);
                            } else {
                                // Value directly allocates (ArrayExpression, FunctionExpression, etc.)
                                // or is a non-ternary propagated value: use original mutable_range checks.
                                let store_range = env.get_identifier(pid)
                                    .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                                if store_range.end.0 > store_range.start.0 + 1 {
                                    operands.push(pid);
                                }
                                let val_range = env.get_identifier(value.identifier)
                                    .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                                if is_mutable_at(instr.id, &val_range) && val_range.start.0 > 0 {
                                    operands.push(value.identifier);
                                }
                            }
                        }
                    }
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                    // If storing a hook result, skip the target variable too.
                    if !hook_results.contains(&value.identifier) {
                        let val_allocates = ident_allocates.contains(&value.identifier);
                        if val_allocates {
                            let store_range = env.get_identifier(pid)
                                .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                            if store_range.end.0 > store_range.start.0 + 1 {
                                operands.push(pid);
                            }
                            let val_range = env.get_identifier(value.identifier)
                                .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                            if is_mutable_at(instr.id, &val_range) && val_range.start.0 > 0 {
                                operands.push(value.identifier);
                            }
                        }
                    }
                }
                InstructionValue::Destructure { lvalue, value, .. } => {
                    // When destructuring a hook result (useState, useReducer, etc.), the pattern
                    // variables must NOT be placed in memoized reactive scopes — hooks run
                    // unconditionally on every render. Register declarations but skip the disjoint set.
                    let is_hook_result = hook_results.contains(&value.identifier);
                    for place_id in pattern_places(&lvalue.pattern) {
                        let decl_id = env
                            .get_identifier(place_id)
                            .map(|i| i.declaration_id)
                            .unwrap_or(DeclarationId(place_id.0));
                        declarations.entry(decl_id).or_insert(place_id);
                        if !is_hook_result {
                            let pr = env
                                .get_identifier(place_id)
                                .map(|i| i.mutable_range.clone())
                                .unwrap_or_else(MutableRange::zero);
                            if pr.end.0 > pr.start.0 + 1 {
                                operands.push(place_id);
                            }
                        }
                    }
                    // Don't group hook results into memoized scopes.
                    if !is_hook_result {
                        let val_allocates = ident_allocates.contains(&value.identifier);
                        let val_reactive = ident_reactive.contains(&value.identifier);
                        if val_allocates || val_reactive {
                            let val_range = env
                                .get_identifier(value.identifier)
                                .map(|i| i.mutable_range.clone())
                                .unwrap_or_else(MutableRange::zero);
                            if is_mutable_at(instr.id, &val_range) && val_range.start.0 > 0 {
                                operands.push(value.identifier);
                            }
                        }
                    }
                }
                // LoadLocal / LoadContext: link the SSA result with the source variable
                // when the source is mutable. This mirrors the TS compiler's behavior
                // where union(lvalue, operand) is called for every mutable operand,
                // regardless of whether the lvalue itself is reactive.
                // Without this, `t = LoadLocal(y)` won't link `t` with `y`, breaking
                // downstream unions like PropertyStore(t, ...) that need to trace back
                // through `t` to `y`.
                // EXCEPT: if the source is a hook result variable (e.g. `ref2 = useRef(null)`),
                // do NOT link it — hook results are stable and must not pull the loaded
                // SSA temp into a reactive scope.
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if hook_result_vars.contains(&place.identifier) {
                        // Hook result variable — skip, no scope membership propagation.
                    } else {
                        let src_range = env
                            .get_identifier(place.identifier)
                            .map(|i| i.mutable_range.clone())
                            .unwrap_or_else(MutableRange::zero);
                        if is_mutable_at(instr.id, &src_range) && src_range.start.0 > 0 {
                            operands.push(place.identifier);
                            // Also include the lvalue (SSA result) even if not reactive,
                            // so mutations via this handle trace back to the source variable.
                            if lvalue_mutable {
                                operands.push(lvalue_id);
                            }
                        }
                    }
                }

                // Hook calls (useRef, useState, useEffect, etc.) must run unconditionally
                // and must NOT be grouped into memoized scopes.
                // For hook calls, skip operand processing so their arguments don't
                // accidentally pull in scope membership.
                InstructionValue::CallExpression { callee, .. } => {
                    let is_hook = global_names
                        .get(&callee.identifier)
                        .map_or(false, |n| is_hook_name(n));
                    if !is_hook {
                        for op in each_instruction_value_operand(&instr.value) {
                            let op_range = env
                                .get_identifier(op.identifier)
                                .map(|i| i.mutable_range.clone())
                                .unwrap_or_else(MutableRange::zero);
                            if is_mutable_at(instr.id, &op_range) && op_range.start.0 > 0 {
                                operands.push(op.identifier);
                            }
                        }
                    }
                    // For hook calls: no operand processing, no scope grouping.
                }
                // For MethodCall: only link the receiver (which is mutated), NOT the
                // arguments (which are merely read). Linking args would incorrectly merge
                // the arg's scope with the receiver's scope.
                InstructionValue::MethodCall { receiver, property, .. } => {
                    for id in [receiver.identifier, property.identifier] {
                        let op_range = env
                            .get_identifier(id)
                            .map(|i| i.mutable_range.clone())
                            .unwrap_or_else(MutableRange::zero);
                        if is_mutable_at(instr.id, &op_range) && op_range.start.0 > 0 {
                            operands.push(id);
                        }
                    }
                }
                // FunctionExpression: merge captured context variables into the same
                // scope unconditionally. Captures may be declared after the function
                // (due to JS hoisting) so the range-based check would miss them.
                //
                // EXCEPTION: skip captures of outlined function holder variables.
                // An outlined FunctionExpression (name_hint set) becomes a stable
                // module-level stub reference — its holder variable (e.g. `setGlobal`)
                // does not need co-location with the capturing FunctionExpression's scope.
                InstructionValue::FunctionExpression { lowered_func, name_hint, .. } => {
                    if name_hint.is_none() {
                        // Non-outlined function: union ALL non-parameter captures with the function.
                        // This mirrors the TS compiler behavior: even immutable captures (const vars
                        // that are only read inside the closure) are co-located in the same reactive
                        // scope. This allows capturing-alias patterns like:
                        //   const x = {foo};
                        //   const f = () => { x.something = value; };
                        // to produce a single merged scope with deps=[foo, ...] rather than
                        // two separate scopes for x and f.
                        // Parameters are excluded (cap_range.start=0) since they're stable deps,
                        // not allocating values needing co-location.
                        for ctx_place in &lowered_func.func.context {
                            // Skip captures of outlined function holder vars: they're stable
                            // module-level refs, not reactive values needing co-location.
                            if outlined_fn_holder_vars.contains(&ctx_place.identifier) {
                                continue;
                            }
                            // Skip hook result variables — they are stable across renders.
                            if hook_result_vars.contains(&ctx_place.identifier) {
                                continue;
                            }
                            let cap_range = env
                                .get_identifier(ctx_place.identifier)
                                .map(|i| i.mutable_range.clone())
                                .unwrap_or_else(MutableRange::zero);
                            // Exclude parameters (mutable_range.start=0) — they are reactive
                            // deps, not allocating values to co-locate.
                            // Also exclude non-allocating captures with no mutable range.
                            if is_mutable_at(instr.id, &cap_range) && cap_range.start.0 > 0 {
                                operands.push(ctx_place.identifier);
                            }
                        }
                    }
                    // Outlined FunctionExpressions: no operand processing at all.
                    // They become stable module-level stubs and should not create scopes.
                }
                // PropertyLoad / ComputedLoad are transparent reads.
                // Their object operand is a dependency (captured by propagate_scope_deps),
                // NOT a co-location requirement. Do not union the object into the same scope
                // as the result — this prevents spurious scopes for parameter-copy temps.
                //
                // PropertyStore / ComputedStore are property mutations.
                // The scope that owns the object already encompasses the mutation via range
                // extension. We must NOT union the value/property operand temps into the
                // owning scope — that would incorrectly add non-allocating temps (like
                // LoadLocal(arg)) to the scope's declarations, preventing always-inv detection
                // and blocking inner scope merging.
                InstructionValue::PropertyLoad { .. }
                | InstructionValue::ComputedLoad { .. }
                | InstructionValue::PropertyStore { .. }
                | InstructionValue::ComputedStore { .. } => {
                    // No operand unioning for property reads/writes.
                }
                // TernaryExpression: join the result with any allocating branches.
                // This handles default parameter patterns like `x = cond ? [arr] : param`
                // where the ternary result should be memoized along with the array literal.
                InstructionValue::TernaryExpression { consequent, alternate, .. } => {
                    for branch_id in [consequent.identifier, alternate.identifier] {
                        if ident_allocates.contains(&branch_id) {
                            // Join the branch's value with the ternary result
                            operands.push(branch_id);
                        }
                    }
                }
                _ => {
                    for op in each_instruction_value_operand(&instr.value) {
                        // Skip inner always-allocating operands (e.g. `{}` or `[]` passed as
                        // elements of an outer array/object). They form their own separate
                        // memoized scopes and must not be merged into the containing scope
                        // via SSA liveness overlap.
                        if ident_allocates.contains(&op.identifier) {
                            continue;
                        }
                        let op_range = env
                            .get_identifier(op.identifier)
                            .map(|i| i.mutable_range.clone())
                            .unwrap_or_else(MutableRange::zero);
                        if is_mutable_at(instr.id, &op_range) && op_range.start.0 > 0 {
                            operands.push(op.identifier);
                        }
                    }
                }
            }

            if !operands.is_empty() {
                let mut seen = HashSet::new();
                let dedup: Vec<_> =
                    operands.into_iter().filter(|&id| seen.insert(id)).collect();
                if std::env::var("RC_DEBUG2").is_ok() {
                    eprintln!("[infer_scope] instr[{}] {:?} dedup={:?}", instr.id.0,
                        std::mem::discriminant(&instr.value),
                        dedup.iter().map(|id| {
                            let r = env.get_identifier(*id).map(|i| i.mutable_range.clone()).unwrap_or_else(crate::hir::hir::MutableRange::zero);
                            format!("{}[{},{})", id.0, r.start.0, r.end.0)
                        }).collect::<Vec<_>>());
                }
                set.union(&dedup);
            }
        }
    }
    set
}

fn is_mutable_at(instr_id: InstructionId, range: &MutableRange) -> bool {
    instr_id >= range.start && instr_id < range.end
}

pub fn pattern_places_pub(pattern: &Pattern) -> Vec<IdentifierId> {
    pattern_places(pattern)
}

fn pattern_places(pattern: &Pattern) -> Vec<IdentifierId> {
    let mut out = Vec::new();
    match pattern {
        Pattern::Array(ap) => {
            for elem in &ap.items {
                match elem {
                    ArrayElement::Place(p) => out.push(p.identifier),
                    ArrayElement::Spread(s) => out.push(s.place.identifier),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(op) => {
            for prop in &op.properties {
                match prop {
                    ObjectPatternProperty::Property(p) => out.push(p.place.identifier),
                    ObjectPatternProperty::Spread(s) => out.push(s.place.identifier),
                }
            }
        }
    }
    out
}

fn may_allocate(value: &InstructionValue, global_names: &HashMap<IdentifierId, String>) -> bool {
    match value {
        InstructionValue::PostfixUpdate { .. }
        | InstructionValue::PrefixUpdate { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StoreLocal { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::TypeCastExpression { .. }
        | InstructionValue::LoadLocal { .. }
        | InstructionValue::LoadContext { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::GetIterator { .. }
        | InstructionValue::IteratorNext { .. }
        | InstructionValue::NextPropertyOf { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. }
        | InstructionValue::UnaryExpression { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::TernaryExpression { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::StoreGlobal { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::UnsupportedNode { .. }
        | InstructionValue::PropertyStore { .. }
        | InstructionValue::ComputedStore { .. } => false,

        // InlineJs: used for optional chains. Optional CALLS (containing `?.(`) may allocate
        // arbitrary values and need memoized scopes. Optional PROPERTY ACCESS (just `?.property`)
        // is equivalent to a conditional PropertyLoad — no allocation, no scope needed.
        InstructionValue::InlineJs { source, .. } => source.contains("?.("),

        InstructionValue::ObjectExpression { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::TaggedTemplateExpression { .. } => true,

        // Outlined function expressions have name_hint set by outline_functions.
        // They're equivalent to stable module-level references (like LoadGlobal),
        // so they do NOT allocate and should not be placed in memoized scopes.
        InstructionValue::FunctionExpression { name_hint, .. } => name_hint.is_none(),

        // Hook calls (e.g. useRef, useEffect, useState) must run unconditionally
        // (React's rules of hooks) — they must NOT be placed inside a memoized scope.
        // Known primitive-returning builtins (String, Number, Boolean, etc.) return
        // primitives that compare by value — no need to memoize them.
        // Non-hook, non-primitive calls may allocate new values and should be memoized.
        InstructionValue::CallExpression { callee, .. } => {
            let callee_name = global_names.get(&callee.identifier).map(|s| s.as_str());
            match callee_name {
                Some(name) if is_hook_name(name) => false,
                Some(name) if is_primitive_returning_builtin(name) => false,
                _ => true,
            }
        }

        InstructionValue::MethodCall { .. } => true,

        InstructionValue::Destructure { lvalue, .. } => match &lvalue.pattern {
            Pattern::Array(ap) => ap.items.iter().any(|e| matches!(e, ArrayElement::Spread(_))),
            Pattern::Object(op) => op.properties.iter().any(|p| matches!(p, ObjectPatternProperty::Spread(_))),
        },
    }
}

fn is_hook_name(name: &str) -> bool {
    name.starts_with("use") && name[3..].chars().next().map_or(false, |c| c.is_uppercase())
}

/// Returns true for global functions that always return a primitive (string/number/boolean).
/// These don't heap-allocate so they don't need memoization scopes.
fn is_primitive_returning_builtin(name: &str) -> bool {
    matches!(
        name,
        "String" | "Number" | "Boolean"
        | "parseInt" | "parseFloat"
        | "isNaN" | "isFinite"
        | "encodeURI" | "encodeURIComponent"
        | "decodeURI" | "decodeURIComponent"
    )
}

/// Hooks whose results must NOT be placed in memoized scopes:
/// - React-managed state (useState, useReducer, useContext)
/// - Effect hooks that return void (useEffect, etc.)
/// - Stable hooks (useRef, useId, etc.)
/// Excluded: memoization hooks (useMemo, useCallback) whose results CAN be cached.
fn is_non_memoizable_hook(name: &str) -> bool {
    matches!(
        name,
        "useState"
            | "useReducer"
            | "useContext"
            | "useEffect"
            | "useLayoutEffect"
            | "useInsertionEffect"
            | "useRef"
            | "useId"
            | "useImperativeHandle"
            | "useDebugValue"
    )
}
