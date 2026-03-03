use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    ArrayElement, HIRFunction, IdentifierId, InstructionValue, LValuePattern,
    ObjectPatternProperty, Pattern, ScopeId, Terminal,
};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

/// Collect all bound identifier places from a destructuring LValuePattern.
/// These are the variables that receive values from the Destructure instruction.
fn collect_pattern_places(pattern: &LValuePattern, out: &mut Vec<IdentifierId>) {
    match &pattern.pattern {
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPatternProperty::Property(p) => out.push(p.place.identifier),
                    ObjectPatternProperty::Spread(s) => out.push(s.place.identifier),
                }
            }
        }
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) => out.push(p.identifier),
                    ArrayElement::Spread(s) => out.push(s.place.identifier),
                    ArrayElement::Hole => {}
                }
            }
        }
    }
}

pub fn run(_hir: &mut HIRFunction) {}

/// Remove scopes that have zero dependencies AND whose only members are
/// LoadLocal / LoadContext SSA temps.  These arise when a named variable is
/// read at the end of a function body (`return x;` produces `t = LoadLocal(x)`)
/// and the liveness range of `t` satisfies the `lvalue_mutable` check, causing
/// it to form a spurious singleton scope with 0 deps.
///
/// Also prunes scopes where ALL member identifiers flow only into dead loads
/// (LoadLocal/LoadContext instructions whose own results are unused). This handles
/// statement expressions like `<dif>{x}</dif>;` whose JSX result is never returned.
///
/// Also prunes scopes with no allocating members (pure reads/primitives/binary ops
/// don't need memoization).
///
/// Also prunes scopes where no member escapes (not reachable from Return terminals
/// or hook call arguments via backward data-flow). This is a simplified
/// PruneNonEscapingScopes pass.
pub fn run_with_env(hir: &HIRFunction, env: &mut Environment) {
    if env.scopes.is_empty() {
        return;
    }

    // Build set of identifiers produced by allocating instructions.
    // Also propagate through StoreLocal/LoadLocal/LoadContext chains so that named
    // variables that hold allocating values are also considered allocating.
    // E.g., `ret = []` means the named binding `ret` should be treated as allocating
    // even though the StoreLocal's lvalue.identifier is a phantom temp, not an array.
    let mut allocating_ids: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if is_allocating_instruction(&instr.value) {
                allocating_ids.insert(instr.lvalue.identifier);
            }
        }
    }
    // Propagate through StoreLocal: if `value` is allocating, mark the `lvalue.place`
    // (the named binding) and the instruction's phantom lvalue as allocating too.
    // Propagate through LoadLocal: if the source is allocating, the load result is too.
    // Iterate to fixpoint for chains.
    let mut changed = true;
    while changed {
        changed = false;
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        if allocating_ids.contains(&value.identifier) {
                            if allocating_ids.insert(lvalue.place.identifier) { changed = true; }
                            if allocating_ids.insert(instr.lvalue.identifier) { changed = true; }
                        }
                    }
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        if allocating_ids.contains(&place.identifier) {
                            if allocating_ids.insert(instr.lvalue.identifier) { changed = true; }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Build maps over all instructions:
    // - is_load: whether an identifier is the result of LoadLocal/LoadContext
    // - use_count: how many times each identifier appears as an operand
    // - loads_of: for each source identifier, the set of LoadLocal/LoadContext
    //   lvalue identifiers that load it (so we can do transitive dead-load checks)
    let mut is_load: HashMap<IdentifierId, bool> = HashMap::new();
    let mut use_count: HashMap<IdentifierId, u32> = HashMap::new();
    // Maps source id → set of LoadLocal lvalue ids that consume it.
    let mut load_consumers: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();

    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let load = matches!(
                &instr.value,
                InstructionValue::LoadLocal { .. } | InstructionValue::LoadContext { .. }
            );
            is_load.insert(instr.lvalue.identifier, load);

            // Track LoadLocal/LoadContext consumers for transitive analysis.
            if let InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } = &instr.value
            {
                load_consumers
                    .entry(place.identifier)
                    .or_default()
                    .push(instr.lvalue.identifier);
            }

            // Count each operand use.
            for place in each_instruction_value_operand(&instr.value) {
                *use_count.entry(place.identifier).or_insert(0) += 1;
            }
        }
        // Also count terminal operands.
        for place in each_terminal_operand(&block.terminal) {
            *use_count.entry(place.identifier).or_insert(0) += 1;
        }
    }

    // Returns true if `id` is only consumed by LoadLocal/LoadContext instructions
    // whose own results are unused (use_count=0). This handles statement expressions
    // like `<dif>{x}</dif>;` where the JSX temp feeds only into a dead load.
    let is_only_consumed_by_dead_loads = |id: IdentifierId| -> bool {
        let direct = use_count.get(&id).copied().unwrap_or(0);
        if direct == 0 {
            return true; // Not consumed at all.
        }
        // Check if ALL consumers are LoadLocal/LoadContext with their own use_count=0.
        let consumers = load_consumers.get(&id);
        let load_uses = consumers.map_or(0, |v| v.len() as u32);
        if load_uses != direct {
            return false; // Has non-load consumers.
        }
        // All consumers are loads; check that each load result is itself dead.
        consumers.map_or(false, |v| {
            v.iter()
                .all(|load_id| use_count.get(load_id).copied().unwrap_or(0) == 0)
        })
    };

    // -----------------------------------------------------------------------
    // Criterion 4: backward liveness analysis (simplified PruneNonEscapingScopes)
    //
    // Build a map: id → Vec<id> of "what must be live when id is live"
    // (backward dependency map).  For each instruction:
    //   - For temps: lvalue depends on all read operands
    //   - For StoreLocal/StoreContext: the target variable also depends on
    //     the stored value (so that if the variable is later loaded, we trace
    //     back to what was stored into it)
    //
    // Then seed live_ids from:
    //   (a) Return terminal operands
    //   (b) Arguments to hook calls (useXxx pattern)
    // and propagate backward to find all transitively live identifiers.
    // Scopes with NO live members are pruned.
    // -----------------------------------------------------------------------

    // Detect LoadGlobal names so we can identify hook callees.
    let mut id_to_global_name: HashMap<IdentifierId, String> = HashMap::new();
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
                    id_to_global_name.insert(instr.lvalue.identifier, n);
                }
            }
        }
    }

    // Build backward dependency map.
    let mut produces: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let instr_lv = instr.lvalue.identifier;
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    // Instruction result depends on the stored value.
                    produces.entry(instr_lv).or_default().push(value.identifier);
                    // The target variable also depends on the stored value:
                    // if var is live, what was stored into it must be live.
                    produces
                        .entry(lvalue.place.identifier)
                        .or_default()
                        .push(value.identifier);
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    produces.entry(instr_lv).or_default().push(value.identifier);
                    produces
                        .entry(lvalue.place.identifier)
                        .or_default()
                        .push(value.identifier);
                }
                InstructionValue::Destructure { lvalue, value, .. } => {
                    // Track instr_lv → source value.
                    produces.entry(instr_lv).or_default().push(value.identifier);
                    // Track each pattern-bound identifier → source value.
                    // This ensures liveness propagates correctly: if x is used and came from
                    // `const { x } = obj`, then `obj` must be live too.
                    let mut pattern_ids = Vec::new();
                    collect_pattern_places(lvalue, &mut pattern_ids);
                    for pid in pattern_ids {
                        produces.entry(pid).or_default().push(value.identifier);
                    }
                }
                _ => {
                    for op in each_instruction_value_operand(&instr.value) {
                        produces.entry(instr_lv).or_default().push(op.identifier);
                    }
                }
            }
        }
    }

    // Seed live_ids from Return terminals + hook call arguments.
    let mut live_ids: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        // Return terminal value.
        if let Terminal::Return { value, .. } = &block.terminal {
            live_ids.insert(value.identifier);
        }
        // Arguments to hook calls.
        for instr in &block.instructions {
            if let InstructionValue::CallExpression { callee, args, .. } = &instr.value {
                if id_to_global_name
                    .get(&callee.identifier)
                    .map(|n| is_hook_name(n))
                    .unwrap_or(false)
                {
                    for arg in args {
                        use crate::hir::hir::CallArg;
                        match arg {
                            CallArg::Place(p) => {
                                live_ids.insert(p.identifier);
                            }
                            CallArg::Spread(s) => {
                                live_ids.insert(s.place.identifier);
                            }
                        }
                    }
                }
            }
        }
    }

    // Propagate backward.
    let mut worklist: Vec<IdentifierId> = live_ids.iter().copied().collect();
    while let Some(id) = worklist.pop() {
        if let Some(deps) = produces.get(&id) {
            for &dep in deps {
                if live_ids.insert(dep) {
                    worklist.push(dep);
                }
            }
        }
    }

    let scopes_to_prune: Vec<ScopeId> = env
        .scopes
        .iter()
        .filter_map(|(&sid, scope)| {
            // Collect all idents belonging to this scope.
            let members: Vec<IdentifierId> = env
                .identifiers
                .iter()
                .filter(|(_, ident)| ident.scope == Some(sid))
                .map(|(&id, _)| id)
                .collect();

            if members.is_empty() {
                return None;
            }

            // Criterion 1: 0 deps AND all-load members (spurious LoadLocal scopes).
            if scope.dependencies.is_empty() {
                let all_loads = members
                    .iter()
                    .all(|id| is_load.get(id).copied().unwrap_or(false));
                if all_loads {
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[prune_scope] scope {:?} pruned: criterion 1 (all-load)", sid.0);
                    }
                    return Some(sid);
                }
            }

            // Criterion 2: ALL member identifiers flow only into dead loads
            // (or are not consumed at all). Handles discarded statement expressions.
            let all_dead = members
                .iter()
                .all(|id| is_only_consumed_by_dead_loads(*id));
            if all_dead {
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[prune_scope] scope {:?} pruned: criterion 2 (all-dead)", sid.0);
                }
                return Some(sid);
            }

            // Criterion 3: None of the scope's member identifiers are produced by
            // allocating instructions (Object/Array/Function/JSX/Call/New). Memoizing
            // purely non-allocating computations (param reads, primitive ops, property
            // loads) wastes a cache slot and is never emitted by the TS compiler.
            let any_allocating = members.iter().any(|id| allocating_ids.contains(id));
            if !any_allocating {
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[prune_scope] scope {:?} pruned: criterion 3 (no allocating members={:?})", sid.0, members.iter().map(|id| id.0).collect::<Vec<_>>());
                }
                return Some(sid);
            }

            // Criterion 4: Sentinel scope (0 deps) whose members are not transitively
            // reachable from the function's return value or hook call arguments.
            // Only applies to sentinel scopes — scopes with deps are needed for their
            // side effects even if their outputs don't directly escape to the return.
            if scope.dependencies.is_empty() {
                let any_live = members.iter().any(|id| live_ids.contains(id));
                if !any_live {
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[prune_scope] scope {:?} pruned: criterion 4 (no live members={:?} live_ids.len={})", sid.0, members.iter().map(|id| id.0).collect::<Vec<_>>(), live_ids.len());
                    }
                    return Some(sid);
                }
            }

            if std::env::var("RC_DEBUG").is_ok() {
                eprintln!("[prune_scope] scope {:?} KEPT (ndeps={}, members={:?})", sid.0, scope.dependencies.len(), members.iter().map(|id| id.0).collect::<Vec<_>>());
            }

            None
        })
        .collect();

    for sid in scopes_to_prune {
        env.scopes.remove(&sid);
        for ident in env.identifiers.values_mut() {
            if ident.scope == Some(sid) {
                ident.scope = None;
            }
        }
    }
}

/// Returns true if a callee name matches the React hook naming convention:
/// starts with "use" followed by an uppercase letter (e.g., useState, useEffect).
fn is_hook_name(name: &str) -> bool {
    // Strip leading sigils ($, _) used in some contexts.
    let n = name.trim_start_matches('$').trim_start_matches('_');
    if let Some(rest) = n.strip_prefix("use") {
        rest.starts_with(|c: char| c.is_uppercase())
    } else {
        false
    }
}

/// Returns true if this instruction creates a new heap-allocated value that
/// is worth caching in a reactive scope.
/// Outlined FunctionExpressions (name_hint set) are replaced by a stable module-level
/// stub reference and are NOT worth caching — they would produce empty sentinel scopes.
fn is_allocating_instruction(value: &InstructionValue) -> bool {
    if let InstructionValue::FunctionExpression { name_hint, .. } = value {
        if name_hint.is_some() {
            return false; // Outlined — stable, not worth caching.
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
            | InstructionValue::CallExpression { .. }
            | InstructionValue::MethodCall { .. }
    )
}
