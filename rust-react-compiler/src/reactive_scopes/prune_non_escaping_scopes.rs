use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    ArrayElement, DeclarationId, HIRFunction, IdentifierId, InstructionValue, NonLocalBinding,
    ObjectPatternProperty, Pattern, ScopeId, Terminal,
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

fn join_levels(a: MemoLevel, b: MemoLevel) -> MemoLevel {
    match (a, b) {
        (MemoLevel::Memoized, _) | (_, MemoLevel::Memoized) => MemoLevel::Memoized,
        (MemoLevel::Conditional, _) | (_, MemoLevel::Conditional) => MemoLevel::Conditional,
        (MemoLevel::Unmemoized, _) | (_, MemoLevel::Unmemoized) => MemoLevel::Unmemoized,
        _ => MemoLevel::Never,
    }
}

fn is_hook_name(name: &str) -> bool {
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
        | InstructionValue::UnsupportedNode { .. } => MemoLevel::Unmemoized,

        // InlineJs (optional chaining) — Conditional so backward traversal propagates
        // through the synthetic dep added during node building.
        InstructionValue::InlineJs { .. } => MemoLevel::Conditional,

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

/// A node in the graph, keyed by DeclarationId (groups all SSA versions of the same var).
/// This mirrors the TS compiler's use of DeclarationId over IdentifierId.
struct Node {
    level: MemoLevel,
    deps: Vec<DeclarationId>,
    scopes: HashSet<ScopeId>,
    seen: bool,
    memoized: bool,
}

/// Scope node tracking its dependencies (for force-memoization).
/// At prune_non_escaping time, scope.dependencies may be empty (propagate_scope_dependencies
/// hasn't run yet), so we build our own dep graph from instruction operands.
struct ScopeNode {
    deps: Vec<DeclarationId>,
    seen: bool,
}

/// Prune reactive scopes whose outputs don't need memoization.
///
/// Port of the TS compiler's PruneNonEscapingScopes pass.
/// Uses DeclarationId (not IdentifierId) to correctly group SSA versions of the same
/// variable, matching the TS compiler's approach.
pub fn run_with_env(hir: &HIRFunction, env: &mut Environment) {
    if env.scopes.is_empty() {
        return;
    }

    // Build IdentifierId → DeclarationId map from the environment.
    let id_to_decl: HashMap<IdentifierId, DeclarationId> = env
        .identifiers
        .iter()
        .map(|(&iid, ident)| (iid, ident.declaration_id))
        .collect();

    let decl_for = |iid: IdentifierId| -> DeclarationId {
        id_to_decl.get(&iid).copied().unwrap_or(DeclarationId(iid.0))
    };

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

    // Build per-scope dependency lists from instructions.
    // We track which declarations are read as operands while inside a scope's range.
    // Keyed by ScopeId → set of decl_ids that are deps.
    // We approximate: for each instruction whose lvalue belongs to a scope, its operands
    // are deps of that scope.
    let mut scope_dep_decls: HashMap<ScopeId, HashSet<DeclarationId>> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let lv_decl = decl_for(instr.lvalue.identifier);
            // Look up this instruction's lvalue scope.
            let lv_scope = env
                .get_identifier(instr.lvalue.identifier)
                .and_then(|i| i.scope);

            if let Some(sid) = lv_scope {
                // Add all operand decl_ids as scope deps.
                let entry = scope_dep_decls.entry(sid).or_default();
                for op in each_instruction_value_operand(&instr.value) {
                    let op_decl = decl_for(op.identifier);
                    if op_decl != lv_decl {
                        entry.insert(op_decl);
                    }
                }
                // For StoreLocal, also add the stored value's decl.
                if let InstructionValue::StoreLocal { value, lvalue, .. } = &instr.value {
                    entry.insert(decl_for(value.identifier));
                    entry.insert(decl_for(lvalue.place.identifier));
                }
                // For Destructure, add the source value's decl.
                if let InstructionValue::Destructure { value, .. } = &instr.value {
                    entry.insert(decl_for(value.identifier));
                }
            }
        }
    }

    // Build the nodes graph, keyed by DeclarationId.
    // For each instruction, we add entries for:
    //  - The instruction's lvalue (keyed by its decl_id)
    //  - For StoreLocal: also the target variable's decl_id
    //  - For Destructure: also each pattern-bound variable's decl_id (Conditional)
    let mut nodes: HashMap<DeclarationId, Node> = HashMap::new();

    // Definitions: LoadLocal/LoadContext maps lvalue decl_id → source decl_id.
    // This lets us resolve indirections: `t = LoadLocal(x)` means visiting t traces to x.
    let mut definitions: HashMap<DeclarationId, DeclarationId> = HashMap::new();

    for (_, block) in &hir.body.blocks {
        for (instr_idx, instr) in block.instructions.iter().enumerate() {
            let lv = instr.lvalue.identifier;
            let lv_decl = decl_for(lv);
            let level = instruction_memo_level(&instr.value);
            let scope = env.get_identifier(lv).and_then(|i| i.scope);

            let deps: Vec<DeclarationId> = match level {
                MemoLevel::Never => {
                    // Never nodes have NO deps — barrier to backward traversal.
                    vec![]
                }
                _ => {
                    match &instr.value {
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => {
                            let src_decl = decl_for(place.identifier);
                            // Record this as a definition alias.
                            definitions.insert(lv_decl, src_decl);
                            vec![src_decl]
                        }
                        InstructionValue::StoreLocal { value, lvalue, .. } => {
                            let val_decl = decl_for(value.identifier);
                            let target_decl = decl_for(lvalue.place.identifier);
                            // The StoreLocal instruction itself is Conditional.
                            // Also ensure the target variable node exists and points to value.
                            let target_node = nodes.entry(target_decl).or_insert_with(|| Node {
                                level: MemoLevel::Conditional,
                                deps: vec![],
                                scopes: HashSet::new(),
                                seen: false,
                                memoized: false,
                            });
                            if !target_node.deps.contains(&val_decl) {
                                target_node.deps.push(val_decl);
                            }
                            if let Some(sid) = scope {
                                target_node.scopes.insert(sid);
                            }
                            vec![val_decl]
                        }
                        InstructionValue::StoreContext { value, lvalue, .. } => {
                            let val_decl = decl_for(value.identifier);
                            let target_decl = decl_for(lvalue.place.identifier);
                            let target_node = nodes.entry(target_decl).or_insert_with(|| Node {
                                level: MemoLevel::Conditional,
                                deps: vec![],
                                scopes: HashSet::new(),
                                seen: false,
                                memoized: false,
                            });
                            if !target_node.deps.contains(&val_decl) {
                                target_node.deps.push(val_decl);
                            }
                            if let Some(sid) = scope {
                                target_node.scopes.insert(sid);
                            }
                            vec![val_decl]
                        }
                        InstructionValue::Destructure { value, lvalue, .. } => {
                            let val_decl = decl_for(value.identifier);
                            // Register each pattern-bound variable as a Conditional node
                            // that depends on the source value.
                            let pattern_decls = collect_pattern_decls(&lvalue.pattern, &id_to_decl);
                            for pd in &pattern_decls {
                                let pnode = nodes.entry(*pd).or_insert_with(|| Node {
                                    level: MemoLevel::Conditional,
                                    deps: vec![],
                                    scopes: HashSet::new(),
                                    seen: false,
                                    memoized: false,
                                });
                                if !pnode.deps.contains(&val_decl) {
                                    pnode.deps.push(val_decl);
                                }
                                if let Some(sid) = scope {
                                    pnode.scopes.insert(sid);
                                }
                            }
                            vec![val_decl]
                        }
                        // InlineJs (e.g. optional chaining `x?.b`) has zero tracked
                        // operands, creating a dead end in the dep graph. Bridge the
                        // gap by linking back to the preceding instruction's lvalue,
                        // which is typically the LoadLocal/StoreLocal that produced the
                        // chain's root object.
                        InstructionValue::InlineJs { .. } => {
                            let mut inline_deps = Vec::new();
                            if instr_idx > 0 {
                                let prev = &block.instructions[instr_idx - 1];
                                let prev_decl = decl_for(prev.lvalue.identifier);
                                inline_deps.push(prev_decl);
                            }
                            inline_deps
                        }
                        _ => {
                            each_instruction_value_operand(&instr.value)
                                .into_iter()
                                .map(|op| decl_for(op.identifier))
                                .collect()
                        }
                    }
                }
            };

            // Insert or join node for this instruction's lvalue decl.
            let node = nodes.entry(lv_decl).or_insert_with(|| Node {
                level,
                deps: vec![],
                scopes: HashSet::new(),
                seen: false,
                memoized: false,
            });
            node.level = join_levels(node.level, level);
            for d in &deps {
                if !node.deps.contains(d) {
                    node.deps.push(*d);
                }
            }
            if let Some(sid) = scope {
                node.scopes.insert(sid);
            }
        }

        // Also process phi nodes: phi result depends on all operands.
        for phi in &block.phis {
            let phi_decl = decl_for(phi.place.identifier);
            let phi_scope = env.get_identifier(phi.place.identifier).and_then(|i| i.scope);
            let node = nodes.entry(phi_decl).or_insert_with(|| Node {
                level: MemoLevel::Conditional,
                deps: vec![],
                scopes: HashSet::new(),
                seen: false,
                memoized: false,
            });
            for (_, op_place) in &phi.operands {
                let op_decl = decl_for(op_place.identifier);
                if !node.deps.contains(&op_decl) {
                    node.deps.push(op_decl);
                }
            }
            if let Some(sid) = phi_scope {
                node.scopes.insert(sid);
            }
        }
    }

    // Build scope nodes from scope_dep_decls.
    let mut scope_nodes: HashMap<ScopeId, ScopeNode> = scope_dep_decls
        .into_iter()
        .map(|(sid, deps)| {
            (sid, ScopeNode { deps: deps.into_iter().collect(), seen: false })
        })
        .collect();

    // Collect escaping identifiers (as DeclarationIds).
    let mut escaping: Vec<DeclarationId> = Vec::new();
    for (_, block) in &hir.body.blocks {
        if let Terminal::Return { value, .. } = &block.terminal {
            escaping.push(decl_for(value.identifier));
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
                            CallArg::Place(p) => { escaping.push(decl_for(p.identifier)); }
                            CallArg::Spread(s) => { escaping.push(decl_for(s.place.identifier)); }
                        }
                    }
                }
            }
        }
    }

    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[prune_non_escaping] escaping decls: {:?}", escaping.iter().map(|id| id.0).collect::<Vec<_>>());
        eprintln!("[prune_non_escaping] definitions: {:?}", definitions.iter().map(|(k, v)| (k.0, v.0)).collect::<Vec<_>>());
    }

    // Visit each escaping decl and propagate memoization.
    for &decl in &escaping {
        visit(decl, false, &mut nodes, &mut scope_nodes, &definitions);
    }

    // Collect memoized decl ids.
    let memoized_decls: HashSet<DeclarationId> = nodes
        .iter()
        .filter(|(_, n)| n.memoized)
        .map(|(&d, _)| d)
        .collect();

    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[prune_non_escaping] memoized decls: {:?}", memoized_decls.iter().map(|id| id.0).collect::<Vec<_>>());
    }

    // Prune scopes where no declaration's decl_id is in memoized_decls.
    let scopes_to_prune: Vec<ScopeId> = env
        .scopes
        .iter()
        .filter_map(|(&sid, scope)| {
            let any_memoized = scope.declarations.keys().any(|iid| {
                let d = id_to_decl.get(iid).copied().unwrap_or(DeclarationId(iid.0));
                memoized_decls.contains(&d)
            });
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

/// Collect DeclarationIds from all pattern-bound variables in a Destructure pattern.
fn collect_pattern_decls(
    pattern: &Pattern,
    id_to_decl: &HashMap<IdentifierId, DeclarationId>,
) -> Vec<DeclarationId> {
    let mut out = Vec::new();
    match pattern {
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPatternProperty::Property(p) => {
                        let d = id_to_decl.get(&p.place.identifier)
                            .copied()
                            .unwrap_or(DeclarationId(p.place.identifier.0));
                        out.push(d);
                    }
                    ObjectPatternProperty::Spread(s) => {
                        let d = id_to_decl.get(&s.place.identifier)
                            .copied()
                            .unwrap_or(DeclarationId(s.place.identifier.0));
                        out.push(d);
                    }
                }
            }
        }
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) => {
                        let d = id_to_decl.get(&p.identifier)
                            .copied()
                            .unwrap_or(DeclarationId(p.identifier.0));
                        out.push(d);
                    }
                    ArrayElement::Spread(s) => {
                        let d = id_to_decl.get(&s.place.identifier)
                            .copied()
                            .unwrap_or(DeclarationId(s.place.identifier.0));
                        out.push(d);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
    }
    out
}

fn visit(
    id: DeclarationId,
    force: bool,
    nodes: &mut HashMap<DeclarationId, Node>,
    scope_nodes: &mut HashMap<ScopeId, ScopeNode>,
    definitions: &HashMap<DeclarationId, DeclarationId>,
) -> bool {
    // Resolve definition aliases (LoadLocal indirections).
    let resolved = definitions.get(&id).copied().unwrap_or(id);

    let node = match nodes.get(&resolved) {
        Some(_) => resolved,
        None => {
            // Unknown id (e.g., params not tracked). Return false.
            return false;
        }
    };

    // If already seen, return cached result.
    // Note: we use seen=true to prevent infinite loops.
    if nodes[&node].seen {
        return nodes[&node].memoized;
    }
    nodes.get_mut(&node).unwrap().seen = true;
    nodes.get_mut(&node).unwrap().memoized = false; // Temporary

    // Clone deps and scopes to avoid borrow conflicts.
    let deps: Vec<DeclarationId> = nodes[&node].deps.clone();
    let scopes: Vec<ScopeId> = nodes[&node].scopes.iter().copied().collect();
    let level = nodes[&node].level;

    // Visit all dependencies.
    let mut has_memoized_dep = false;
    for dep_id in &deps {
        if visit(*dep_id, false, nodes, scope_nodes, definitions) {
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
        nodes.get_mut(&node).unwrap().memoized = true;

        // Force-memoize all scope dependencies.
        for sid in &scopes {
            force_memoize_scope(*sid, nodes, scope_nodes, definitions);
        }
    }

    should_memoize
}

fn force_memoize_scope(
    sid: ScopeId,
    nodes: &mut HashMap<DeclarationId, Node>,
    scope_nodes: &mut HashMap<ScopeId, ScopeNode>,
    definitions: &HashMap<DeclarationId, DeclarationId>,
) {
    let already_seen = match scope_nodes.get_mut(&sid) {
        Some(sn) => {
            if sn.seen {
                return;
            }
            sn.seen = true;
            false
        }
        None => return,
    };
    let _ = already_seen;

    let deps: Vec<DeclarationId> = scope_nodes[&sid].deps.clone();
    for dep in deps {
        visit(dep, true, nodes, scope_nodes, definitions);
    }
}
