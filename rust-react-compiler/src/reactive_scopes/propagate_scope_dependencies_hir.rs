use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    DependencyPathEntry, Effect, HIRFunction, IdentifierId, Instruction, InstructionId,
    InstructionValue, NonLocalBinding, Place, ReactiveScopeDependency,
};
use crate::hir::hir::Param;
use crate::hir::visitors::{each_dep_operand, each_instruction_value_operand, each_terminal_operand};

fn is_hook_name(name: &str) -> bool {
    name.starts_with("use") && name[3..].chars().next().map_or(false, |c| c.is_uppercase())
}

/// Trace the property access path used for `name` in the function source text.
///
/// Only applies to arrow functions. For regular function expressions, returns
/// empty vec (no narrowing).
///
/// Rules:
/// - Only narrows when the member access is at brace-depth ≤ 1 (not nested in if/else).
/// - Does not narrow when the member access is an assignment target (`name.prop =`).
/// - Removes the last segment when the access is a method call (`name.prop()`).
/// - Returns the agreed-upon path if all occurrences match; empty otherwise.
///
/// Examples (arrow functions):
/// - `() => foo.current` or `() => { console.log(foo.current); }` → `["current"]`
/// - `() => { mutator.user.hide(); }` → `["user"]` (method receiver)
/// - `() => (sharedVal.value = x)` → `[]` (assignment, no narrowing)
/// - `() => { if (c) { obj.prop } }` → `[]` (depth 2, conditional, no narrowing)
fn narrow_dep_path(source: &str, name: &str) -> Vec<String> {
    if name.is_empty() { return vec![]; }
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let name_bytes = name.as_bytes();
    let name_len = name.len();
    let bytes = source.as_bytes();
    let slen = bytes.len();

    let mut all_paths: Vec<Vec<String>> = Vec::new();
    let mut brace_depth: usize = 0;
    let mut i = 0;

    while i < slen {
        // Skip string literals and comments.
        if i + 1 < slen && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < slen && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        if i + 1 < slen && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < slen && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; }
            if i + 1 < slen { i += 2; }
            continue;
        }
        if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
            let q = bytes[i]; i += 1;
            while i < slen && bytes[i] != q { if bytes[i] == b'\\' { i += 1; } i += 1; }
            if i < slen { i += 1; }
            continue;
        }
        // Track brace depth.
        if bytes[i] == b'{' { brace_depth += 1; i += 1; continue; }
        if bytes[i] == b'}' { brace_depth = brace_depth.saturating_sub(1); i += 1; continue; }

        // Check for identifier preceded by '.' (property name, not variable).
        let is_prop = i > 0 && bytes[i - 1] == b'.';

        if !is_prop && i + name_len <= slen && bytes[i..].starts_with(name_bytes) {
            let before_ok = i == 0 || !is_id_char(bytes[i - 1]);
            let after_pos = i + name_len;
            let after_ok = after_pos >= slen || !is_id_char(bytes[after_pos]);

            if before_ok && after_ok {
                // Conditional access: inside nested braces — cannot narrow.
                if brace_depth > 1 {
                    all_paths.push(vec![]);
                    i = after_pos;
                    continue;
                }

                // Found an occurrence of `name`. Trace property chain.
                let mut path: Vec<String> = Vec::new();
                let mut j = after_pos;
                let mut is_assignment = false;
                while j < slen && bytes[j] == b' ' { j += 1; }

                loop {
                    if j >= slen || bytes[j] != b'.' { break; }
                    j += 1; // skip '.'
                    while j < slen && bytes[j] == b' ' { j += 1; }
                    if j >= slen || !is_id_char(bytes[j]) { break; }
                    let prop_start = j;
                    while j < slen && is_id_char(bytes[j]) { j += 1; }
                    let prop = std::str::from_utf8(&bytes[prop_start..j]).unwrap_or("").to_string();
                    while j < slen && bytes[j] == b' ' { j += 1; }
                    if j < slen && bytes[j] == b'(' {
                        // Method call — stop, don't add this segment.
                        break;
                    } else if j < slen && bytes[j] == b'='
                        && (j + 1 >= slen || (bytes[j + 1] != b'=' && bytes[j + 1] != b'>'))
                    {
                        // Assignment target (`name.prop = ...`) — no narrowing.
                        is_assignment = true;
                        break;
                    } else {
                        path.push(prop);
                        // Continue to check for further chaining.
                        while j < slen && bytes[j] == b' ' { j += 1; }
                    }
                }

                if is_assignment {
                    all_paths.push(vec![]);
                } else {
                    all_paths.push(path);
                }
                i = after_pos;
                continue;
            }
        }
        i += 1;
    }

    if all_paths.is_empty() { return vec![]; }
    let first = &all_paths[0];
    if first.is_empty() { return vec![]; }
    for path in &all_paths[1..] {
        if path != first { return vec![]; }
    }
    first.clone()
}

fn binding_name(binding: &NonLocalBinding) -> &str {
    match binding {
        NonLocalBinding::Global { name } => name.as_str(),
        NonLocalBinding::ImportSpecifier { name, .. } => name.as_str(),
        NonLocalBinding::ImportDefault { name, .. } => name.as_str(),
        NonLocalBinding::ImportNamespace { name, .. } => name.as_str(),
        NonLocalBinding::ModuleLocal { name } => name.as_str(),
    }
}

/// Compute the "root path" for a place: trace backwards through PropertyLoad
/// and LoadLocal chains to find the most specific external base + path.
///
/// Returns `Some((base_id, path))` if the place traces back to an external
/// reactive identifier (param or pre-scope), or `None` if it's fully internal.
fn resolve_dep_path(
    place_id: IdentifierId,
    def_at: &HashMap<IdentifierId, InstructionId>,
    instr_map: &HashMap<IdentifierId, &Instruction>,
    store_local_value: &HashMap<IdentifierId, IdentifierId>,
    phi_operands: &HashMap<IdentifierId, Vec<IdentifierId>>,
    range_start: InstructionId,
) -> Option<(IdentifierId, Vec<DependencyPathEntry>)> {
    let mut visited = HashSet::new();
    resolve_dep_path_inner(place_id, def_at, instr_map, store_local_value, phi_operands, range_start, 0, &mut visited)
}

fn resolve_dep_path_inner(
    place_id: IdentifierId,
    def_at: &HashMap<IdentifierId, InstructionId>,
    instr_map: &HashMap<IdentifierId, &Instruction>,
    store_local_value: &HashMap<IdentifierId, IdentifierId>,
    phi_operands: &HashMap<IdentifierId, Vec<IdentifierId>>,
    range_start: InstructionId,
    depth: u32,
    visited: &mut HashSet<IdentifierId>,
) -> Option<(IdentifierId, Vec<DependencyPathEntry>)> {
    if depth > 64 { return None; }
    if !visited.insert(place_id) { return None; } // cycle detected
    let def = def_at.get(&place_id);
    let is_external = match def {
        None => true, // param — external (no defining instruction)
        Some(def_id) => *def_id < range_start,
    };

    if is_external {
        // If this external place is itself a transparent instruction, trace through it.
        // This handles two cases:
        // 1. LoadLocal/LoadContext: e.g., t9 = LoadLocal(a) defined before scope start —
        //    we want `a` as the base, not `t9`.
        // 2. PropertyLoad: e.g., t2 = PropertyLoad(props, "render") defined before scope
        //    start — we want `props.render` as the dep, not just `props`.
        if let Some(instr) = instr_map.get(&place_id) {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    return resolve_dep_path_inner(place.identifier, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited);
                }
                InstructionValue::PropertyLoad { object, property, .. } => {
                    if let Some((base_id, mut path)) =
                        resolve_dep_path_inner(object.identifier, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited)
                    {
                        path.push(DependencyPathEntry {
                            property: property.clone(),
                            optional: false,
                        });
                        return Some((base_id, path));
                    }
                }
                // Hook function references (useCallback, useState, etc.) are stable globals.
                // They are never meaningful scope dependencies — skip them.
                InstructionValue::LoadGlobal { binding, .. } => {
                    if is_hook_name(binding_name(binding)) {
                        return None;
                    }
                }
                // Allocations (Object, Array) create new values every render.
                // Trace through to their first resolvable operand so the dep is
                // the reactive INPUT, not the allocation itself.
                // e.g., `{a: param}` → dep should be `param`, not the object.
                InstructionValue::ObjectExpression { properties, .. } => {
                    for prop in properties {
                        let val_id = match prop {
                            crate::hir::hir::ObjectExpressionProperty::Property(p) => p.place.identifier,
                            crate::hir::hir::ObjectExpressionProperty::Spread(s) => s.place.identifier,
                        };
                        if let Some(result) = resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                            return Some(result);
                        }
                    }
                    return None;
                }
                InstructionValue::ArrayExpression { elements, .. } => {
                    for elem in elements {
                        let val_id = match elem {
                            crate::hir::hir::ArrayElement::Place(p) => p.identifier,
                            crate::hir::hir::ArrayElement::Spread(s) => s.place.identifier,
                            crate::hir::hir::ArrayElement::Hole => continue,
                        };
                        if let Some(result) = resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                            return Some(result);
                        }
                    }
                    return None;
                }
                // NewExpression allocates — trace through args.
                InstructionValue::NewExpression { args, .. } => {
                    for arg in args {
                        let val_id = match arg {
                            crate::hir::hir::CallArg::Place(p) => p.identifier,
                            crate::hir::hir::CallArg::Spread(s) => s.place.identifier,
                        };
                        if let Some(result) = resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                            return Some(result);
                        }
                    }
                    return None;
                }
                _ => {}
            }
        }
        // Check if this is a phi node result with no def_at entry (phi block had no instructions).
        // Phi results are NOT external params — trace through their operands.
        // These phi nodes arise at logical-expression join points (&&/||/??).
        // We return only the base identifier (no path) because the phi value could be
        // EITHER the short-circuit operand OR the fully-evaluated operand, so the
        // most conservative and correct dep is the reactive base param itself.
        if def.is_none() {
            if let Some(ops) = phi_operands.get(&place_id) {
                let ops: Vec<IdentifierId> = ops.clone();
                for op_id in ops {
                    if op_id == place_id { continue; }
                    if let Some((base_id, _path)) = resolve_dep_path_inner(op_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                        // Strip path: the phi could yield either operand at runtime,
                        // so we only know the base reactive identifier is a dep.
                        return Some((base_id, vec![]));
                    }
                }
                return None;
            }
        }

        // Already an external base — return with empty path.
        if std::env::var("RC_DEBUG2").is_ok() {
            eprintln!("[resolve_ext] id={} depth={} def_at={:?} → Some({})", place_id.0, depth,
                def_at.get(&place_id).map(|id| id.0), place_id.0);
        }
        return Some((place_id, vec![]));
    }

    // Place is defined inside the scope — trace through PropertyLoad / LoadLocal chains.
    if let Some(instr) = instr_map.get(&place_id) {
        match &instr.value {
            InstructionValue::PropertyLoad { object, property, .. } => {
                if let Some((base_id, mut path)) =
                    resolve_dep_path_inner(object.identifier, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited)
                {
                    path.push(DependencyPathEntry {
                        property: property.clone(),
                        optional: false,
                    });
                    return Some((base_id, path));
                }
            }
            // LoadLocal / LoadContext are transparent — trace through to the source.
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => {
                return resolve_dep_path_inner(place.identifier, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited);
            }
            // ComputedLoad: trace through the object operand. We can't represent
            // computed property indices in the dep path, but tracking the base
            // object is better than no dep at all.
            InstructionValue::ComputedLoad { object, .. } => {
                return resolve_dep_path_inner(object.identifier, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited);
            }
            // Internal allocations: trace through to operands.
            InstructionValue::ObjectExpression { properties, .. } => {
                for prop in properties {
                    let val_id = match prop {
                        crate::hir::hir::ObjectExpressionProperty::Property(p) => p.place.identifier,
                        crate::hir::hir::ObjectExpressionProperty::Spread(s) => s.place.identifier,
                    };
                    if let Some(result) = resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                        return Some(result);
                    }
                }
                return None;
            }
            InstructionValue::ArrayExpression { elements, .. } => {
                for elem in elements {
                    let val_id = match elem {
                        crate::hir::hir::ArrayElement::Place(p) => p.identifier,
                        crate::hir::hir::ArrayElement::Spread(s) => s.place.identifier,
                        crate::hir::hir::ArrayElement::Hole => continue,
                    };
                    if let Some(result) = resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                        return Some(result);
                    }
                }
                return None;
            }
            InstructionValue::NewExpression { args, .. } => {
                for arg in args {
                    let val_id = match arg {
                        crate::hir::hir::CallArg::Place(p) => p.identifier,
                        crate::hir::hir::CallArg::Spread(s) => s.place.identifier,
                    };
                    if let Some(result) = resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                        return Some(result);
                    }
                }
                return None;
            }
            _ => {}
        }
    }

    // place_id is a StoreLocal binding target (named variable): trace to its assigned value.
    // e.g., `let cond = param_30` → StoreLocal { lvalue: 37, value: 30 }
    // instr_map doesn't have 37, but store_local_value[37] = 30.
    if let Some(&val_id) = store_local_value.get(&place_id) {
        return resolve_dep_path_inner(val_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited);
    }

    // place_id is a phi node result: trace through its operands to find an external base.
    // e.g., while-loop phi for `cond` (a param) → phi operands include the param id.
    if let Some(ops) = phi_operands.get(&place_id) {
        let ops: Vec<IdentifierId> = ops.clone();
        for op_id in ops {
            if op_id == place_id { continue; } // avoid trivial self-loop
            if let Some(result) = resolve_dep_path_inner(op_id, def_at, instr_map, store_local_value, phi_operands, range_start, depth + 1, visited) {
                return Some(result);
            }
        }
        return None;
    }

    if std::env::var("RC_DEBUG2").is_ok() {
        eprintln!("[resolve] id={} depth={} def_at={:?} is_external={} → None", place_id.0, depth,
            def_at.get(&place_id).map(|id| id.0), is_external);
    }
    None
}

fn resolve_dep_path_debug(
    place_id: IdentifierId,
    def_at: &HashMap<IdentifierId, InstructionId>,
    instr_map: &HashMap<IdentifierId, &Instruction>,
    store_local_value: &HashMap<IdentifierId, IdentifierId>,
    phi_operands: &HashMap<IdentifierId, Vec<IdentifierId>>,
    range_start: InstructionId,
) -> Option<(IdentifierId, Vec<DependencyPathEntry>)> {
    if std::env::var("RC_DEBUG2").is_ok() {
        eprintln!("[resolve_start] id={} def_at={:?} range_start={}", place_id.0,
            def_at.get(&place_id).map(|id| id.0), range_start.0);
    }
    let mut visited = HashSet::new();
    resolve_dep_path_inner(place_id, def_at, instr_map, store_local_value, phi_operands, range_start, 0, &mut visited)
}

/// Returns true for "transparent" instructions whose operands are subsumed by
/// the result when that result is used elsewhere.
///
/// - PropertyLoad / ComputedLoad: skip the object; the result's usage captures the path.
/// - LoadLocal / LoadContext: skip the source; the result's usage captures the path.
fn is_transparent_instruction(value: &InstructionValue) -> bool {
    matches!(
        value,
        InstructionValue::PropertyLoad { .. }
            | InstructionValue::ComputedLoad { .. }
            | InstructionValue::LoadLocal { .. }
            | InstructionValue::LoadContext { .. }
    )
}

pub fn run(hir: &mut HIRFunction, env: &mut Environment) {
    // TEMP: fully disable to test correctness without this pass
    if std::env::var("RC_DISABLE_PROPDEP").is_ok() { return; }
    // Build def_at: identifier → InstructionId where it was defined.
    // Includes both instruction result temps AND StoreLocal binding targets,
    // so that variables defined inside a scope are not mistakenly treated as external deps.
    //
    // Also includes phi node results: phi.place.identifier → the ID of the first instruction
    // in that block (as a proxy for "defined in this block"). This prevents phi results from
    // being treated as external deps just because they don't appear in instruction lvalues.
    let mut def_at: HashMap<IdentifierId, InstructionId> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        // Register phi results using the block's first instruction ID as a proxy.
        // If the block has no instructions, we skip (phi block with no instructions is empty;
        // the range check will handle it correctly since range_start >= 0).
        let first_instr_id = block.instructions.first().map(|i| i.id);
        for phi in &block.phis {
            if let Some(fid) = first_instr_id {
                def_at.entry(phi.place.identifier).or_insert(fid);
            }
        }
        for instr in &block.instructions {
            def_at.insert(instr.lvalue.identifier, instr.id);
            // Also track StoreLocal binding targets so they're seen as scope-internal.
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                if std::env::var("RC_DEBUG2").is_ok() {
                    eprintln!("[def_at_store] named_var={} instr={}", lvalue.place.identifier.0, instr.id.0);
                }
                def_at.entry(lvalue.place.identifier).or_insert(instr.id);
            }
        }
    }

    // Build instr_map: identifier → Instruction (for chain tracing).
    let mut instr_map: HashMap<IdentifierId, &Instruction> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            instr_map.insert(instr.lvalue.identifier, instr);
        }
    }

    // Build phi_operands: phi result id → list of operand ids.
    // Used by resolve_dep_path to trace through phis to their sources.
    let mut phi_operands: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for phi in &block.phis {
            let ops: Vec<IdentifierId> = phi.operands.values().map(|p| p.identifier).collect();
            phi_operands.insert(phi.place.identifier, ops);
        }
    }

    // Build always-invalidating map and store-local-value map.
    // These let us include Object/Array/Function/JSX identifiers as deps even when
    // their reactive flag is false — matching TS's isAlwaysInvalidatingType behavior.
    let mut store_local_value: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    let mut is_always_invalidating: HashMap<IdentifierId, bool> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            // Outlined FunctionExpressions (name_hint set) are module-level stable functions —
            // they never change between renders, so they are NOT always-invalidating deps.
            let is_outlined = if let InstructionValue::FunctionExpression { name_hint, .. } = &instr.value {
                name_hint.is_some()
            } else {
                false
            };
            let always_inv = !is_outlined && matches!(
                &instr.value,
                InstructionValue::ObjectExpression { .. }
                    | InstructionValue::ArrayExpression { .. }
                    | InstructionValue::FunctionExpression { .. }
                    | InstructionValue::ObjectMethod { .. }
                    | InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. }
                    | InstructionValue::NewExpression { .. }
                    | InstructionValue::TaggedTemplateExpression { .. }
            );
            is_always_invalidating.insert(instr.lvalue.identifier, always_inv);
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                store_local_value.insert(lvalue.place.identifier, value.identifier);
            }
        }
    }

    let scope_ids: Vec<_> = env.scopes.keys().copied().collect();

    // Collect loop test block IDs: only these blocks' Branch terminals contribute to
    // scope deps. If-condition blocks are NOT loop test blocks and must not add their
    // test conditions as deps (control deps ≠ value deps).
    //
    // For compound conditions like `x.length && props.cond`, the logical-AND evaluation
    // spans MULTIPLE blocks (one per `&&`/`||` operand). We must add ALL blocks in the
    // test-evaluation subgraph, not just the direct test block, so that deps from inner
    // blocks (e.g., `props.cond` in the consequent of a Branch with logical_op=Some) are
    // also captured.
    use crate::hir::hir::{BlockId, Terminal};
    let mut loop_test_blocks: HashSet<BlockId> = HashSet::new();

    // Helper closure: recursively collect all blocks reachable from `start` that are
    // part of the test-evaluation subgraph (not the loop body or fallthrough).
    fn collect_test_subgraph(
        start: BlockId,
        loop_bid: BlockId,
        fallthrough_bid: BlockId,
        blocks: &indexmap::IndexMap<BlockId, crate::hir::hir::BasicBlock>,
        result: &mut HashSet<BlockId>,
    ) {
        if !result.insert(start) {
            return; // already visited
        }
        let Some(block) = blocks.get(&start) else { return };
        for succ in block.terminal.successors() {
            // Don't cross into the loop body or the exit — those aren't test blocks.
            if succ != loop_bid && succ != fallthrough_bid {
                collect_test_subgraph(succ, loop_bid, fallthrough_bid, blocks, result);
            }
        }
    }

    for (_, block) in &hir.body.blocks {
        match &block.terminal {
            Terminal::DoWhile { test, loop_, fallthrough, .. } => {
                collect_test_subgraph(*test, *loop_, *fallthrough, &hir.body.blocks, &mut loop_test_blocks);
            }
            Terminal::While { test, loop_, fallthrough, .. } => {
                collect_test_subgraph(*test, *loop_, *fallthrough, &hir.body.blocks, &mut loop_test_blocks);
            }
            Terminal::For { test, loop_, fallthrough, .. } => {
                collect_test_subgraph(*test, *loop_, *fallthrough, &hir.body.blocks, &mut loop_test_blocks);
            }
            Terminal::ForOf { test, loop_, fallthrough, .. } => {
                collect_test_subgraph(*test, *loop_, *fallthrough, &hir.body.blocks, &mut loop_test_blocks);
            }
            _ => {}
        }
    }

    // Build reactive_ids set: identifiers that are reactive (used for terminal dep filtering).
    let mut reactive_ids: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if instr.lvalue.reactive {
                reactive_ids.insert(instr.lvalue.identifier);
            }
        }
        // Phi results: if any operand is reactive, the phi is reactive.
        for phi in &block.phis {
            if phi.place.reactive {
                reactive_ids.insert(phi.place.identifier);
            }
        }
    }
    // Params are always reactive.
    for param in &hir.params {
        match param {
            Param::Place(p) => {
                reactive_ids.insert(p.identifier);
            }
            Param::Spread(s) => { reactive_ids.insert(s.place.identifier); }
        }
    }

    for scope_id in scope_ids {
        let (range_start, range_end) = {
            let scope = env.scopes.get(&scope_id).unwrap();
            (scope.range.start, scope.range.end)
        };

        // Track unique (base_id, path) pairs to avoid duplicate deps.
        let mut seen: HashSet<(u32, Vec<String>)> = HashSet::new();
        let mut dep_list: Vec<ReactiveScopeDependency> = Vec::new();

        // Build the set of declaration identifiers for this scope.
        // We only compute deps from instructions whose lvalue is a scope member
        // (or whose lvalue feeds directly into a scope member via a chain).
        // This prevents spurious deps: a `{}` scope spanning range [0,5] would
        // otherwise pick up `props.value` from instruction 4 inside the range,
        // even though the `{}` itself doesn't use `props.value`.
        let scope_decl_ids: HashSet<IdentifierId> = {
            let scope = env.scopes.get(&scope_id).unwrap();
            if std::env::var("RC_DEBUG").is_ok() {
                eprintln!("[prop_dep] scope {:?} range {:?}-{:?} decls: {:?}", scope_id.0, range_start.0, range_end.0,
                    scope.declarations.keys().map(|id| id.0).collect::<Vec<_>>());
            }
            scope.declarations.keys().copied().collect()
        };

        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if instr.id < range_start || instr.id >= range_end {
                    continue;
                }

                // Only compute deps for instructions that ARE scope members or
                // whose lvalue feeds directly into a scope member.  Instructions
                // within the range but producing unrelated allocations (e.g. `[]`
                // inside a `{}` scope's range because of mutable-lifetime overlap)
                // must not contribute deps to this scope.
                if !scope_decl_ids.contains(&instr.lvalue.identifier) {
                    // Not a direct member — check if it's a StoreLocal whose
                    // binding target IS a member (the SSA temp is not a member
                    // but the named variable it stores into is).
                    let is_store_into_member = if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                        scope_decl_ids.contains(&lvalue.place.identifier)
                    } else {
                        false
                    };

                    // Also include PropertyStore/ComputedStore/MethodCall where the
                    // object/receiver traces back (through LoadLocal) to a scope member.
                    // e.g. `obj.x = arg` mutates `obj` which is in scope — `arg` is a dep.
                    let is_mutation_of_member = match &instr.value {
                        InstructionValue::PropertyStore { object, .. }
                        | InstructionValue::ComputedStore { object, .. } => {
                            // Trace through LoadLocal chain to find the underlying named var.
                            let mut id = object.identifier;
                            loop {
                                if scope_decl_ids.contains(&id) { break true; }
                                if let Some(instr_def) = instr_map.get(&id) {
                                    match &instr_def.value {
                                        InstructionValue::LoadLocal { place, .. }
                                        | InstructionValue::LoadContext { place, .. } => {
                                            id = place.identifier;
                                        }
                                        _ => break false,
                                    }
                                } else {
                                    break false;
                                }
                            }
                        }
                        InstructionValue::MethodCall { receiver, .. } => {
                            let mut id = receiver.identifier;
                            loop {
                                if scope_decl_ids.contains(&id) { break true; }
                                if let Some(instr_def) = instr_map.get(&id) {
                                    match &instr_def.value {
                                        InstructionValue::LoadLocal { place, .. }
                                        | InstructionValue::LoadContext { place, .. } => {
                                            id = place.identifier;
                                        }
                                        _ => break false,
                                    }
                                } else {
                                    break false;
                                }
                            }
                        }
                        _ => false,
                    };

                    if !is_store_into_member && !is_mutation_of_member {
                        continue;
                    }
                }

                // Skip transparent instructions: their operands will be captured
                // via the result when that result is used in a non-transparent instruction.
                // This prevents double-counting: e.g., `props.value` inside a scope
                // would otherwise add both `props` (from LoadLocal) and `props.value`
                // (from the PropertyLoad result used in MethodCall).
                if is_transparent_instruction(&instr.value) {
                    continue;
                }

                // FunctionExpression: narrow deps to the specific member access paths
                // used on each captured variable inside the function body.
                // e.g. `() => foo.current` → dep is `foo.current`, not just `foo`.
                // Only applies to arrow functions; regular function expressions don't narrow.
                if let InstructionValue::FunctionExpression { lowered_func, fn_type, .. } = &instr.value {
                    use crate::hir::hir::FunctionExpressionType;
                    let is_arrow = *fn_type == FunctionExpressionType::Arrow;
                    let fn_source = &lowered_func.func.original_source;
                    for ctx_place in &lowered_func.func.context {
                        let Some((base_id, base_path)) =
                            resolve_dep_path(ctx_place.identifier, &def_at, &instr_map, &store_local_value, &phi_operands, range_start)
                        else {
                            continue;
                        };
                        if !ctx_place.reactive { continue; }
                        // Look up the name of this captured variable.
                        let cap_name = env.get_identifier(ctx_place.identifier)
                            .and_then(|id| id.name.as_ref())
                            .map(|n| n.value().to_string())
                            .unwrap_or_default();
                        // Try to narrow the dep to a more specific member path
                        // (only for arrow functions).
                        let narrowed = if is_arrow && !cap_name.is_empty() {
                            narrow_dep_path(fn_source, &cap_name)
                        } else {
                            vec![]
                        };
                        let final_path = if narrowed.is_empty() {
                            base_path
                        } else {
                            let mut p = base_path;
                            p.extend(narrowed.into_iter().map(|prop| DependencyPathEntry { property: prop, optional: false }));
                            p
                        };
                        let path_key: Vec<String> = final_path.iter().map(|e| e.property.clone()).collect();
                        let has_ancestor = !path_key.is_empty() && {
                            let mut found = false;
                            for prefix_len in 0..path_key.len() {
                                let prefix = path_key[..prefix_len].to_vec();
                                if seen.contains(&(base_id.0, prefix)) {
                                    found = true; break;
                                }
                            }
                            found
                        };
                        let key = (base_id.0, path_key);
                        if !has_ancestor && seen.insert(key) {
                            let base_place = if base_id == ctx_place.identifier {
                                ctx_place.clone()
                            } else {
                                Place {
                                    identifier: base_id,
                                    reactive: ctx_place.reactive,
                                    loc: ctx_place.loc.clone(),
                                    effect: Effect::Unknown,
                                }
                            };
                            dep_list.push(ReactiveScopeDependency { place: base_place, path: final_path });
                        }
                    }
                    continue; // Don't fall through to each_dep_operand for FunctionExpression.
                }

                for place in each_dep_operand(&instr.value) {
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[prop_dep] scope {:?} instr {} place {:?} reactive={}", scope_id.0, instr.id.0, place.identifier.0, place.reactive);
                    }
                    // Resolve dep path for all places — we need the base to check always-invalidating.
                    let resolved = resolve_dep_path(place.identifier, &def_at, &instr_map, &store_local_value, &phi_operands, range_start);
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[prop_dep]   def_at[{}]={:?}, range_start={:?}, resolved={:?}",
                            place.identifier.0,
                            def_at.get(&place.identifier).map(|id| id.0),
                            range_start.0,
                            resolved.as_ref().map(|(id, path)| (id.0, path.iter().map(|e| e.property.as_str()).collect::<Vec<_>>()))
                        );
                    }
                    let Some((base_id, path)) = resolved else {
                        continue;
                    };
                    // Determine if this dep is relevant:
                    // - Reactive deps always qualify (either the operand itself or the resolved base).
                    // - Non-reactive deps qualify if they are always-invalidating (Object/Array/Function/JSX)
                    //   with a direct (empty path) reference. This matches TS's isAlwaysInvalidatingType
                    //   which keeps such deps in pruneNonReactiveDependencies and enables canMergeScopes Case 2b.
                    let is_reactive = place.reactive || reactive_ids.contains(&base_id);
                    let relevant = is_reactive || (path.is_empty() && {
                        let val_id = store_local_value.get(&base_id).copied().unwrap_or(base_id);
                        is_always_invalidating.get(&val_id).copied().unwrap_or(false)
                            || is_always_invalidating.get(&base_id).copied().unwrap_or(false)
                    });
                    if !relevant {
                        continue;
                    }
                    let path_key: Vec<String> =
                        path.iter().map(|e| e.property.clone()).collect();
                    // Skip if an ancestor dep (same base, shorter/empty path) is already tracked.
                    // E.g., if `items` is already a dep, `items.map` is redundant.
                    let has_ancestor = !path_key.is_empty() && {
                        let mut found = false;
                        for prefix_len in 0..path_key.len() {
                            let prefix = path_key[..prefix_len].to_vec();
                            if seen.contains(&(base_id.0, prefix)) {
                                found = true;
                                break;
                            }
                        }
                        found
                    };
                    let key = (base_id.0, path_key);
                    if !has_ancestor && seen.insert(key) {
                        let base_place = if base_id == place.identifier {
                            place.clone()
                        } else {
                            Place {
                                identifier: base_id,
                                // Preserve reactive flag: non-reactive always-invalidating deps
                                // must remain reactive=false so prune_non_reactive_dependencies
                                // can remove them after scope merging.
                                reactive: place.reactive,
                                loc: place.loc.clone(),
                                effect: Effect::Unknown,
                            }
                        };
                        dep_list.push(ReactiveScopeDependency { place: base_place, path });
                    }
                }
            }
        }

        // Scan terminal operands (Branch/If tests) for reactive deps within the scope range.
        // We scan ALL blocks — both loop test blocks and if-condition blocks within the scope.
        // If-conditions inside the scope body are control deps that affect which value the
        // scope produces, so they ARE real deps (not just external control flow guards).
        // The range filter below ensures we only capture deps from within the scope body.
        for (_, block) in &hir.body.blocks {
            let term_id = block.terminal.id();
            if std::env::var("RC_DEBUG").is_ok() {
                eprintln!("[prop_dep] terminal scan: block {:?} term_id={} range=[{},{})", block.id.0, term_id.0, range_start.0, range_end.0);
            }
            if term_id < range_start || term_id >= range_end {
                continue;
            }
            for place in each_terminal_operand(&block.terminal) {
                if std::env::var("RC_DEBUG").is_ok() {
                    let resolved = resolve_dep_path(place.identifier, &def_at, &instr_map, &store_local_value, &phi_operands, range_start);
                    eprintln!("[prop_dep] terminal operand id={} reactive={} resolved={:?}",
                        place.identifier.0, reactive_ids.contains(&place.identifier),
                        resolved.as_ref().map(|(id, p)| (id.0, p.len())));
                }
                let Some((base_id, path)) = resolve_dep_path(place.identifier, &def_at, &instr_map, &store_local_value, &phi_operands, range_start) else {
                    continue;
                };
                if !reactive_ids.contains(&base_id) {
                    continue;
                }
                let path_key: Vec<String> = path.iter().map(|e| e.property.clone()).collect();
                let has_ancestor = !path_key.is_empty() && {
                    let mut found = false;
                    for prefix_len in 0..path_key.len() {
                        let prefix = path_key[..prefix_len].to_vec();
                        if seen.contains(&(base_id.0, prefix)) { found = true; break; }
                    }
                    found
                };
                let key = (base_id.0, path_key);
                if !has_ancestor && seen.insert(key) {
                    let base_place = if base_id == place.identifier {
                        place.clone()
                    } else {
                        Place {
                            identifier: base_id,
                            reactive: true,
                            loc: place.loc.clone(),
                            effect: Effect::Unknown,
                        }
                    };
                    dep_list.push(ReactiveScopeDependency { place: base_place, path });
                }
            }
        }

        // Sort deps alphabetically by "name.path" (mirrors TS compiler sort).
        dep_list.sort_by(|a, b| {
            let name_a = env.get_identifier(a.place.identifier)
                .and_then(|id| id.name.as_ref())
                .map(|n| n.value().to_string())
                .unwrap_or_default();
            let name_b = env.get_identifier(b.place.identifier)
                .and_then(|id| id.name.as_ref())
                .map(|n| n.value().to_string())
                .unwrap_or_default();
            let path_a: String = a.path.iter().map(|e| e.property.as_str()).collect::<Vec<_>>().join(".");
            let path_b: String = b.path.iter().map(|e| e.property.as_str()).collect::<Vec<_>>().join(".");
            let key_a = format!("{}.{}", name_a, path_a);
            let key_b = format!("{}.{}", name_b, path_b);
            key_a.cmp(&key_b)
        });

        if std::env::var("RC_DEBUG").is_ok() {
            eprintln!("[prop_dep] scope {:?} final deps: {:?}", scope_id.0,
                dep_list.iter().map(|d| d.place.identifier.0).collect::<Vec<_>>());
        }
        let scope = env.scopes.get_mut(&scope_id).unwrap();
        scope.dependencies = dep_list;
    }
}
