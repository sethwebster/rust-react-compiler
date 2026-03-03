use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    HIRFunction, IdentifierId, InstructionValue, NonLocalBinding, ScopeId, Terminal,
};
use crate::hir::visitors::each_instruction_value_operand;

/// Memoization levels mirror the TS compiler's MemoizationLevel enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoLevel {
    /// Must be memoized if reachable from escaping values (Object, Array, JSX, Call, etc.)
    Memoized,
    /// Memoized only if any dependency is memoized (PropertyLoad, ComputedLoad, LoadLocal, etc.)
    Conditional,
    /// Not memoized by default, but CAN be memoized if forced. Rvalues ARE walked.
    Unmemoized,
    /// Never memoized AND does not propagate deps (BinaryExpression, Primitive, etc.)
    /// This acts as a barrier to backward memoization traversal.
    Never,
}

fn is_hook_name(name: &str) -> bool {
    // Strip at most one leading _ (matches TS behavior).
    let n = name.strip_prefix('_').unwrap_or(name);
    if let Some(rest) = n.strip_prefix("use") {
        rest.starts_with(|c: char| c.is_uppercase())
    } else {
        false
    }
}

fn instruction_memo_level(value: &InstructionValue) -> MemoLevel {
    match value {
        // Allocating expressions — always need memoization if reachable.
        InstructionValue::ObjectExpression { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::TaggedTemplateExpression { .. }
        | InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::CallExpression { .. }
        | InstructionValue::MethodCall { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::PropertyStore { .. } => MemoLevel::Memoized,

        // Transparent reads — propagate memoization (Conditional).
        InstructionValue::PropertyLoad { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::ComputedStore { .. }
        | InstructionValue::LoadLocal { .. }
        | InstructionValue::LoadContext { .. }
        | InstructionValue::StoreLocal { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::Destructure { .. }
        | InstructionValue::PostfixUpdate { .. }
        | InstructionValue::PrefixUpdate { .. }
        | InstructionValue::TypeCastExpression { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::GetIterator { .. }
        | InstructionValue::IteratorNext { .. } => MemoLevel::Conditional,

        // Unmemoized: not memoized by default, but deps ARE walked and CAN be forced.
        InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StoreGlobal { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::NextPropertyOf { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. }
        | InstructionValue::UnsupportedNode { .. }
        | InstructionValue::InlineJs { .. } => MemoLevel::Unmemoized,

        // Never: barrier to backward propagation (no rvalues = empty deps).
        InstructionValue::BinaryExpression { .. }
        | InstructionValue::UnaryExpression { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. } => MemoLevel::Never,
    }
}

/// Prune reactive scopes whose outputs don't need memoization.
///
/// Port of the TS compiler's PruneNonEscapingScopes pass.
/// Key insight: `Never` level nodes (BinaryExpression, Primitive, etc.) have
/// no deps — they stop backward traversal. So `bar(props)` result won't be
/// discovered through a BinaryExpression barrier.
pub fn run_with_env(hir: &HIRFunction, env: &mut Environment) {
    if env.scopes.is_empty() {
        return;
    }

    // Build global name map for hook detection.
    let mut id_to_global_name: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                let name = match binding {
                    NonLocalBinding::Global { name } => name.clone(),
                    NonLocalBinding::ModuleLocal { name } => name.clone(),
                    NonLocalBinding::ImportDefault { name, .. }
                    | NonLocalBinding::ImportNamespace { name, .. }
                    | NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                };
                id_to_global_name.insert(instr.lvalue.identifier, name);
            }
        }
    }

    // Build memo nodes for every instruction.
    // For `Never` level nodes, deps are EMPTY (no rvalues) — they're barriers.
    // For `Unmemoized` and above, deps are populated from operands.
    let mut nodes: HashMap<IdentifierId, (MemoLevel, Vec<IdentifierId>, Option<ScopeId>)> = HashMap::new();
    let mut store_targets: HashMap<IdentifierId, IdentifierId> = HashMap::new();

    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let lv = instr.lvalue.identifier;
            let level = instruction_memo_level(&instr.value);
            let scope = env.get_identifier(lv).and_then(|i| i.scope);

            let deps = match level {
                MemoLevel::Never => {
                    // Never nodes have NO deps — barrier to backward traversal.
                    vec![]
                }
                _ => {
                    // Memoized, Conditional, and Unmemoized nodes propagate through operands.
                    match &instr.value {
                        InstructionValue::StoreLocal { value, .. } => {
                            vec![value.identifier]
                        }
                        InstructionValue::StoreContext { value, .. } => {
                            vec![value.identifier]
                        }
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => {
                            vec![place.identifier]
                        }
                        _ => {
                            each_instruction_value_operand(&instr.value)
                                .into_iter()
                                .map(|op| op.identifier)
                                .collect()
                        }
                    }
                }
            };

            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                store_targets.insert(lvalue.place.identifier, value.identifier);
            }

            nodes.insert(lv, (level, deps, scope));
        }
    }

    // Collect escaping identifiers.
    let mut escaping: Vec<IdentifierId> = Vec::new();
    for (_, block) in &hir.body.blocks {
        if let Terminal::Return { value, .. } = &block.terminal {
            escaping.push(value.identifier);
        }
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
                            CallArg::Place(p) => { escaping.push(p.identifier); }
                            CallArg::Spread(s) => { escaping.push(s.place.identifier); }
                        }
                    }
                }
            }
        }
    }

    // Visit each escaping identifier and propagate memoization.
    let mut memoized_set: HashSet<IdentifierId> = HashSet::new();
    let mut visited: HashSet<IdentifierId> = HashSet::new();

    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[prune_non_escaping] escaping ids: {:?}", escaping.iter().map(|id| id.0).collect::<Vec<_>>());
        eprintln!("[prune_non_escaping] store_targets: {:?}", store_targets.iter().map(|(k, v)| (k.0, v.0)).collect::<Vec<_>>());
    }
    for &id in &escaping {
        visit(id, false, &nodes, &store_targets, &mut memoized_set, &mut visited, env);
    }
    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[prune_non_escaping] memoized: {:?}", memoized_set.iter().map(|id| id.0).collect::<Vec<_>>());
    }

    // Prune scopes where no declaration is memoized.
    let scopes_to_prune: Vec<ScopeId> = env
        .scopes
        .iter()
        .filter_map(|(&sid, scope)| {
            let any_memoized = scope.declarations.keys().any(|id| memoized_set.contains(id));
            if !any_memoized {
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!(
                        "[prune_non_escaping] scope {:?} pruned: no memoized members ({:?})",
                        sid.0,
                        scope.declarations.keys().map(|id| id.0).collect::<Vec<_>>()
                    );
                }
                Some(sid)
            } else {
                None
            }
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

fn visit(
    id: IdentifierId,
    force: bool,
    nodes: &HashMap<IdentifierId, (MemoLevel, Vec<IdentifierId>, Option<ScopeId>)>,
    store_targets: &HashMap<IdentifierId, IdentifierId>,
    memoized_set: &mut HashSet<IdentifierId>,
    visited: &mut HashSet<IdentifierId>,
    env: &Environment,
) -> bool {
    if !visited.insert(id) {
        return memoized_set.contains(&id);
    }

    let (level, deps, scope) = if let Some(node) = nodes.get(&id) {
        (node.0, node.1.clone(), node.2)
    } else if let Some(&val_id) = store_targets.get(&id) {
        // Named variable (StoreLocal target) — trace to stored value.
        // If the value is memoized, also add this id to memoized_set
        // so scope declarations using this var id are preserved.
        let result = visit(val_id, force, nodes, store_targets, memoized_set, visited, env);
        if result {
            memoized_set.insert(id);
        }
        return result;
    } else {
        return false;
    };

    // Visit ALL dependencies (don't short-circuit — need to visit every path).
    let mut has_memoized_dep = false;
    for &dep_id in &deps {
        if visit(dep_id, false, nodes, store_targets, memoized_set, visited, env) {
            has_memoized_dep = true;
        }
    }

    let should_memoize = match level {
        MemoLevel::Memoized => true,
        MemoLevel::Conditional => has_memoized_dep || force,
        MemoLevel::Unmemoized => force,
        MemoLevel::Never => force,
    };

    if should_memoize {
        memoized_set.insert(id);

        // Force-memoize all scope dependencies.
        if let Some(sid) = scope {
            if let Some(scope_data) = env.scopes.get(&sid) {
                for dep in &scope_data.dependencies {
                    visit(dep.place.identifier, true, nodes, store_targets, memoized_set, visited, env);
                }
            }
        }
    }

    should_memoize
}
