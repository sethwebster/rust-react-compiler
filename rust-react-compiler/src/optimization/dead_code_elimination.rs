#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::{HashSet, HashMap};
use crate::hir::hir::*;
use crate::hir::environment::Environment;

fn contains_as_word(s: &str, pattern: &str) -> bool {
    if pattern.is_empty() { return false; }
    let mut start = 0;
    while let Some(rel_pos) = s[start..].find(pattern) {
        let pos = start + rel_pos;
        let before_ok = pos == 0 || {
            let c = s[..pos].chars().next_back().unwrap_or('\0');
            !(c.is_alphanumeric() || c == '_' || c == '$')
        };
        if before_ok {
            return true;
        }
        start = pos + 1;
    }
    false
}

/// Dead code elimination pass.
///
/// Two sub-passes:
///   1. Unreachable block elimination — BFS from entry removes blocks with no
///      live predecessor path.
///   2. Dead instruction elimination — conservative liveness: an instruction's
///      lvalue must appear in the used-identifier set, OR the instruction has
///      observable side effects.
pub fn dead_code_elimination(hir: &mut HIRFunction) {
    dead_code_elimination_with_env(hir, None);
}

pub fn dead_code_elimination_with_env(hir: &mut HIRFunction, env: Option<&Environment>) {
    remove_unreachable_blocks(hir);
    // Iterate until convergence: removing dead phis/StoreLocals can make their
    // value-producing Primitives dead, requiring another pass.
    loop {
        let before_instrs: usize = hir.body.blocks.values().map(|b| b.instructions.len()).sum();
        let before_phis: usize = hir.body.blocks.values().map(|b| b.phis.len()).sum();
        remove_dead_phis(hir);
        eliminate_shadowed_stores(hir);
        remove_dead_instructions(hir, env);
        let after_instrs: usize = hir.body.blocks.values().map(|b| b.instructions.len()).sum();
        let after_phis: usize = hir.body.blocks.values().map(|b| b.phis.len()).sum();
        if after_instrs == before_instrs && after_phis == before_phis {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Pass: eliminate intra-block shadowed StoreLocals
// ---------------------------------------------------------------------------

/// Remove StoreLocal instructions that are immediately overwritten in the same
/// block before the variable is loaded.  Pattern: `x = <pure>; x = <any>` where
/// no LoadLocal of `x` occurs between the two stores.
///
/// Because DCE runs in a convergence loop, removing the first StoreLocal will
/// cause the pure expression instruction (e.g. `t0 = {}`) to also be pruned in
/// the following `remove_dead_instructions` pass.
fn eliminate_shadowed_stores(hir: &mut HIRFunction) {
    for block in hir.body.blocks.values_mut() {
        // Build a map: IdentifierId -> pure (bool) for each instruction in this block.
        // "pure" means the instruction has no side effects besides producing the value.
        let mut id_is_pure: HashMap<IdentifierId, bool> = HashMap::new();
        for instr in &block.instructions {
            let pure = !has_side_effects(&instr.value);
            id_is_pure.insert(instr.lvalue.identifier, pure);
        }

        // After SSA, StoreLocal.lvalue.place.identifier is the pre-SSA variable id
        // (same for all stores to `x`), while StoreLocal's outer lvalue (instr.lvalue)
        // is the SSA temp that the instruction result is assigned to.
        // LoadLocal.place.identifier is the SSA id of the variable being loaded —
        // it matches instr.lvalue.identifier of a prior StoreLocal, NOT lvalue.place.identifier.
        //
        // Build a map: SSA id (instr.lvalue.identifier) -> pre-SSA var id (lvalue.place.identifier)
        // so we can translate LoadLocal SSA ids back to the pre-SSA variable space.
        let mut ssa_to_var: HashMap<IdentifierId, IdentifierId> = HashMap::new();
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                ssa_to_var.insert(instr.lvalue.identifier, lvalue.place.identifier);
            }
        }

        // For each named variable (tracked by lvalue.place.identifier of StoreLocal),
        // record the index of the last StoreLocal that hasn't been "consumed" by a load yet.
        // If we see another StoreLocal before a load, the previous one may be a dead write.
        let mut last_store: HashMap<IdentifierId, usize> = HashMap::new(); // pre-SSA var_id -> instr index
        let mut dead_indices: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for (idx, instr) in block.instructions.iter().enumerate() {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let var_id = lvalue.place.identifier;
                    if let Some(prev_idx) = last_store.get(&var_id).copied() {
                        // There's a previous unread store to this variable.
                        // Only eliminate if:
                        //   1. The previous store is kind=Reassign (not the initial Let/Const).
                        //      The TS compiler preserves initial `let y = {}` assignments for
                        //      mutation analysis context, even when immediately overwritten.
                        //   2. The previous store's value is pure (no side effects).
                        let prev_instr = &block.instructions[prev_idx];
                        if let InstructionValue::StoreLocal { lvalue: prev_lvalue, value: prev_value, .. } = &prev_instr.value {
                            let is_reassign = prev_lvalue.kind == InstructionKind::Reassign;
                            let prev_val_pure = id_is_pure.get(&prev_value.identifier).copied().unwrap_or(false);
                            if is_reassign && prev_val_pure {
                                dead_indices.insert(prev_idx);
                            }
                        }
                    }
                    last_store.insert(var_id, idx);
                }
                InstructionValue::LoadLocal { place, .. } => {
                    // Translate SSA id to pre-SSA variable id to match last_store keys.
                    if let Some(&var_id) = ssa_to_var.get(&place.identifier) {
                        last_store.remove(&var_id);
                    }
                    // Also try direct lookup in case the identifier is already pre-SSA.
                    last_store.remove(&place.identifier);
                }
                InstructionValue::PostfixUpdate { value, .. }
                | InstructionValue::PrefixUpdate { value, .. } => {
                    // PostfixUpdate/PrefixUpdate implicitly reads the variable (value field).
                    // Clear any pending "unread store" for this variable.
                    if let Some(&var_id) = ssa_to_var.get(&value.identifier) {
                        last_store.remove(&var_id);
                    }
                    last_store.remove(&value.identifier);
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    // If a closure captures a variable, treat it as a read.
                    // The initial store establishes the variable for the closure
                    // and must be preserved (the closure may modify or use it).
                    for ctx in &lowered_func.func.context {
                        if let Some(&var_id) = ssa_to_var.get(&ctx.identifier) {
                            last_store.remove(&var_id);
                        }
                        last_store.remove(&ctx.identifier);
                    }
                }
                InstructionValue::CallExpression { .. }
                | InstructionValue::MethodCall { .. }
                | InstructionValue::NewExpression { .. } => {
                    // A call might observe or modify any variable through closures or
                    // aliasing. Conservatively clear all pending stores.
                    last_store.clear();
                }
                _ => {}
            }
        }

        if !dead_indices.is_empty() {
            if std::env::var("RC_DEBUG_SHADOW").is_ok() {
                for &di in &dead_indices {
                    let instr = &block.instructions[di];
                    eprintln!("[shadow] eliminating instr at idx={di}: {:?}", std::mem::discriminant(&instr.value));
                    if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                        eprintln!("  StoreLocal var_id={} value_id={}", lvalue.place.identifier.0, value.identifier.0);
                    }
                }
            }
            let mut i = 0;
            block.instructions.retain(|_| {
                let keep = !dead_indices.contains(&i);
                i += 1;
                keep
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 0: dead phi removal
// ---------------------------------------------------------------------------

/// Remove phis whose output identifier is never used by any live consumer.
/// Handles cyclic dead phis (phi A → phi B → phi A) via iterative analysis.
fn remove_dead_phis(hir: &mut HIRFunction) {
    // Step 1: Collect identifiers used by non-phi consumers (instructions, terminals, params).
    let mut non_phi_used: HashSet<IdentifierId> = HashSet::new();

    for param in &hir.params {
        match param {
            Param::Place(p) => { non_phi_used.insert(p.identifier); }
            Param::Spread(s) => { non_phi_used.insert(s.place.identifier); }
        }
    }
    for ctx in &hir.context {
        non_phi_used.insert(ctx.identifier);
    }

    for block in hir.body.blocks.values() {
        collect_terminal_uses(&block.terminal, &mut non_phi_used);
        for instr in &block.instructions {
            collect_instruction_uses(&instr.value, &mut non_phi_used);
        }
    }

    // Step 2: Iteratively mark phis as live.
    // A phi is live if its output is used by a non-phi consumer OR by a live phi.
    let mut live_phis: HashSet<IdentifierId> = HashSet::new();
    loop {
        let mut changed = false;
        for block in hir.body.blocks.values() {
            for phi in &block.phis {
                if live_phis.contains(&phi.place.identifier) {
                    continue;
                }
                if non_phi_used.contains(&phi.place.identifier) {
                    live_phis.insert(phi.place.identifier);
                    // Mark this phi's operands as non-phi-used so downstream phis see them.
                    for (_, operand) in &phi.operands {
                        non_phi_used.insert(operand.identifier);
                    }
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }

    // Step 3: Remove dead phis.
    // Liveness analysis above correctly propagates liveness through phi chains,
    // so phis in loop headers are removed only if their output is truly unused.
    for block in hir.body.blocks.values_mut() {
        block.phis.retain(|phi| live_phis.contains(&phi.place.identifier));
    }
}

// ---------------------------------------------------------------------------
// Pass 1: unreachable block removal
// ---------------------------------------------------------------------------

fn remove_unreachable_blocks(hir: &mut HIRFunction) {
    let mut reachable: HashSet<BlockId> = HashSet::new();
    let mut queue: Vec<BlockId> = vec![hir.body.entry];

    while let Some(block_id) = queue.pop() {
        if !reachable.insert(block_id) {
            continue;
        }
        if let Some(block) = hir.body.blocks.get(&block_id) {
            for succ in block.terminal.successors() {
                if !reachable.contains(&succ) {
                    queue.push(succ);
                }
            }
        }
    }

    hir.body.blocks.retain(|id, _| reachable.contains(id));
}

// ---------------------------------------------------------------------------
// Pass 2: dead instruction removal
// ---------------------------------------------------------------------------

fn remove_dead_instructions(hir: &mut HIRFunction, env: Option<&Environment>) {
    let mut used: HashSet<IdentifierId> = HashSet::new();

    // Parameters are always live.
    for param in &hir.params {
        match param {
            Param::Place(p) => { used.insert(p.identifier); }
            Param::Spread(s) => { used.insert(s.place.identifier); }
        }
    }

    // Context places are always live.
    for ctx in &hir.context {
        used.insert(ctx.identifier);
    }

    // Collect uses from terminals and instructions in all reachable blocks.
    // Also collect for-loop update block identifiers separately (below).
    let mut for_update_blocks: Vec<BlockId> = Vec::new();
    for block in hir.body.blocks.values() {
        collect_terminal_uses(&block.terminal, &mut used);
        // For-loop update blocks must survive DCE — the update expression
        // (e.g. `i = i + 1`) is semantically required even if the loop
        // variable doesn't escape.
        if let Terminal::For { update: Some(ubid), .. } = &block.terminal {
            for_update_blocks.push(*ubid);
        }
        for instr in &block.instructions {
            collect_instruction_uses(&instr.value, &mut used);
        }
        // Phi operands are uses.
        for phi in &block.phis {
            for (_, operand) in &phi.operands {
                used.insert(operand.identifier);
            }
        }
    }
    // Mark all identifiers in for-loop update blocks as used.
    for ubid in &for_update_blocks {
        if let Some(block) = hir.body.blocks.get(ubid) {
            for instr in &block.instructions {
                used.insert(instr.lvalue.identifier);
                collect_instruction_uses(&instr.value, &mut used);
            }
        }
    }

    // Build a set of named variables that are actually LoadLocal'd or LoadContext'd.
    let mut loaded_vars: HashSet<IdentifierId> = HashSet::new();
    // Build a set of named variables that are captured by FunctionExpressions.
    // Captured variables must not have their StoreLocals removed even if the outer
    // function never LoadLocals them — the closure reads them via LoadContext.
    let mut captured_vars: HashSet<IdentifierId> = HashSet::new();
    // Build a set of identifiers produced by NextPropertyOf / IteratorNext.
    // StoreLocals that bind such values are for-in/for-of loop variable declarations
    // and must be preserved for codegen even if the variable is never read.
    let mut loop_iter_results: HashSet<IdentifierId> = HashSet::new();
    // Collect InlineJs source strings — these reference variables by name but we
    // can't track individual operands. Build a combined string to scan against.
    let mut inline_js_sources: Vec<String> = Vec::new();
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if let InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } = &instr.value {
                loaded_vars.insert(place.identifier);
            }
            if let InstructionValue::FunctionExpression { lowered_func, .. } = &instr.value {
                for ctx_place in &lowered_func.func.context {
                    captured_vars.insert(ctx_place.identifier);
                }
            }
            if matches!(
                &instr.value,
                InstructionValue::NextPropertyOf { .. } | InstructionValue::IteratorNext { .. }
            ) {
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[DCE] IteratorNext/NextPropertyOf lv.id={}", instr.lvalue.identifier.0);
                }
                loop_iter_results.insert(instr.lvalue.identifier);
            }
            if let InstructionValue::InlineJs { source, .. } = &instr.value {
                inline_js_sources.push(source.clone());
            }
        }
    }
    // For InlineJs instructions: scan all named variables whose name appears in
    // any InlineJs source string and mark them as loaded. This prevents DCE from
    // removing StoreLocals for variables that InlineJs references by name.
    if !inline_js_sources.is_empty() {
        if let Some(env) = env {
            let combined = inline_js_sources.join(" ");
            // Collect all named identifiers in the HIR.
            for block in hir.body.blocks.values() {
                for instr in &block.instructions {
                    if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                        if let Some(name) = env.get_identifier(lvalue.place.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string())
                        {
                            // Check if name appears as a whole word in any InlineJs source.
                            if contains_as_word(&combined, &name) {
                                loaded_vars.insert(lvalue.place.identifier);
                            }
                        }
                    }
                }
            }
        }
    }

    // Build a set of declaration IDs that are loaded by any SSA version.
    // After SSA, Destructure pattern places create new SSA identifiers for named variables,
    // so DeclareLocal/StoreLocal (which keep the original pre-SSA identifier) may not
    // directly appear in `loaded_vars`. Using declaration_id groups all SSA versions of
    // the same variable, allowing liveness to propagate across SSA rename boundaries.
    let loaded_decl_ids: HashSet<DeclarationId> = if let Some(env) = env {
        loaded_vars.iter()
            .filter_map(|&id| env.get_identifier(id))
            .map(|i| i.declaration_id)
            .collect()
    } else {
        HashSet::new()
    };

    // Remove instructions whose lvalue is dead and that have no side effects.
    // Special case: StoreLocal whose named variable is never loaded, never
    // captured by a closure, AND never consumed as a phi operand is dead
    // (the write is truly unobservable).
    for block in hir.body.blocks.values_mut() {
        block.instructions.retain(|instr| {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                if !loaded_vars.contains(&lvalue.place.identifier)
                    && !captured_vars.contains(&lvalue.place.identifier)
                    && !used.contains(&lvalue.place.identifier)
                {
                    // Preserve for-in/for-of loop variable bindings even when the
                    // variable is never read — codegen needs the binding to emit
                    // `for (const y in ...) { ... }` with the correct variable name.
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[DCE] StoreLocal lv.place.id={} instr.lv.id={} value.id={} loop_iter_contains={} used_contains={}",
                            lvalue.place.identifier.0, instr.lvalue.identifier.0, value.identifier.0,
                            loop_iter_results.contains(&value.identifier),
                            used.contains(&instr.lvalue.identifier));
                    }
                    if loop_iter_results.contains(&value.identifier) {
                        return true;
                    }
                    if used.contains(&instr.lvalue.identifier) {
                        return true;
                    }
                    // SSA alias check: after a Destructure renames a variable (id→new_id),
                    // LoadLocals use new_id, not the original id in StoreLocal.lvalue.place.
                    // Check if any SSA alias (same declaration_id) of this variable is loaded.
                    if let Some(env) = env {
                        if let Some(decl_id) = env.get_identifier(lvalue.place.identifier)
                            .map(|i| i.declaration_id)
                        {
                            if loaded_decl_ids.contains(&decl_id) {
                                return true;
                            }
                        }
                    }
                    return false;
                }
            }
            // DeclareLocal for a variable that is never loaded, never captured,
            // and never used as a phi operand is truly dead — eliminate it.
            // This handles `let foo;` inside a loop where `foo` is never read.
            // SSA alias check: also preserve if any SSA alias (same declaration_id) is loaded.
            if let InstructionValue::DeclareLocal { lvalue, .. } = &instr.value {
                if !loaded_vars.contains(&lvalue.place.identifier)
                    && !captured_vars.contains(&lvalue.place.identifier)
                    && !used.contains(&lvalue.place.identifier)
                {
                    if let Some(env) = env {
                        if let Some(decl_id) = env.get_identifier(lvalue.place.identifier)
                            .map(|i| i.declaration_id)
                        {
                            if loaded_decl_ids.contains(&decl_id) {
                                // An SSA alias is loaded — fall through to has_side_effects (true).
                            } else {
                                return false;
                            }
                        } else {
                            return false;
                        }
                    } else {
                        return false;
                    }
                }
            }
            // PostfixUpdate/PrefixUpdate (e.g. i++, --i) are dead when:
            //   - the expression result (instr.lvalue) is unused, AND
            //   - the updated variable is never explicitly LoadLocal'd.
            //
            // IMPORTANT: in our SSA representation, PostfixUpdate.lvalue and
            // PostfixUpdate.value share the same identifier (both refer to the
            // pre-update place). This means `used.contains(update_lv.identifier)`
            // is always true (the PostfixUpdate's own `value` field adds it to `used`),
            // creating a circular false-liveness dependency. Using `loaded_vars`
            // instead avoids this: it only contains identifiers explicitly read via
            // LoadLocal/LoadContext instructions, excluding the PostfixUpdate's own
            // implicit read. If no LoadLocal reads the variable after the update,
            // the update is truly dead (e.g., `i++; i = props.i;` where the
            // increment is immediately overwritten).
            if let InstructionValue::PostfixUpdate { lvalue: update_lv, .. }
            | InstructionValue::PrefixUpdate { lvalue: update_lv, .. } = &instr.value
            {
                if used.contains(&instr.lvalue.identifier) {
                    return true;
                }
                // Use loaded_vars (not `used`) to avoid the circular liveness dependency:
                // PostfixUpdate.value references the same place as update_lv, so
                // `used.contains(&update_lv.identifier)` is always true.
                if loaded_vars.contains(&update_lv.identifier) {
                    return true;
                }
                // SSA alias check: a different SSA version of the same variable may be loaded.
                if let Some(env) = env {
                    if let Some(decl_id) = env.get_identifier(update_lv.identifier)
                        .map(|i| i.declaration_id)
                    {
                        if loaded_decl_ids.contains(&decl_id) {
                            return true;
                        }
                    }
                }
                return false;
            }
            used.contains(&instr.lvalue.identifier) || has_side_effects(&instr.value)
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn has_side_effects(value: &InstructionValue) -> bool {
    // Outlined FunctionExpressions (name_hint set by outline_functions) must survive DCE.
    if let InstructionValue::FunctionExpression { name_hint: Some(_), .. } = value {
        return true;
    }
    matches!(
        value,
        InstructionValue::CallExpression { .. }
            | InstructionValue::MethodCall { .. }
            | InstructionValue::NewExpression { .. }
            | InstructionValue::PropertyStore { .. }
            | InstructionValue::ComputedStore { .. }
            | InstructionValue::PropertyDelete { .. }
            | InstructionValue::ComputedDelete { .. }
            | InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::StoreGlobal { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Await { .. }
            | InstructionValue::UnsupportedNode { .. }
            | InstructionValue::InlineJs { .. }
            // JSX expressions are treated as side-effectful so that scope inference
            // can assign them a reactive scope before they are pruned by
            // prune_non_escaping_scopes. The pruned scope range then acts as a
            // merge barrier in merge_reactive_scopes_that_invalidate_together.
            | InstructionValue::JsxExpression { .. }
            | InstructionValue::JsxFragment { .. }
    )
}

/// Remove JSX instructions (JsxExpression, JsxFragment) whose lvalue is never
/// used as an operand anywhere in the function. This runs after
/// `prune_non_escaping_scopes` to clean up JSX statement expressions whose
/// reactive scopes were pruned (their results were never consumed).
///
/// We can't use the main DCE for this because JSX is kept alive by the main DCE
/// so that scope inference can assign it a reactive scope first. Once that scope
/// is pruned, this pass removes the now-unused JSX instruction.
pub fn prune_unused_jsx(hir: &mut HIRFunction) {
    use std::collections::HashSet;
    // Collect all identifiers used as operands (non-lvalue) anywhere.
    let mut used: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            collect_instruction_uses(&instr.value, &mut used);
        }
        collect_terminal_uses(&block.terminal, &mut used);
        for phi in &block.phis {
            for op in phi.operands.values() {
                used.insert(op.identifier);
            }
        }
    }
    // Remove JSX instructions whose result is unused.
    for (_, block) in hir.body.blocks.iter_mut() {
        block.instructions.retain(|instr| {
            let is_unused_jsx = matches!(
                &instr.value,
                InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
            ) && !used.contains(&instr.lvalue.identifier);
            !is_unused_jsx
        });
    }
}

fn collect_terminal_uses(terminal: &Terminal, used: &mut HashSet<IdentifierId>) {
    match terminal {
        Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
            used.insert(value.identifier);
        }
        Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
            used.insert(test.identifier);
        }
        Terminal::Switch { test, cases, .. } => {
            used.insert(test.identifier);
            for case in cases {
                if let Some(t) = &case.test {
                    used.insert(t.identifier);
                }
            }
        }
        Terminal::Try { handler_binding, .. } => {
            if let Some(binding) = handler_binding {
                used.insert(binding.identifier);
            }
        }
        // Most other terminals use only block IDs, not places.
        _ => {}
    }
}

fn collect_instruction_uses(value: &InstructionValue, used: &mut HashSet<IdentifierId>) {
    match value {
        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } => {
            used.insert(place.identifier);
        }

        InstructionValue::StoreLocal { lvalue: _, value, .. } => {
            used.insert(value.identifier);
        }

        InstructionValue::StoreContext { lvalue: _, value, .. } => {
            used.insert(value.identifier);
        }

        InstructionValue::StoreGlobal { value, .. } => {
            used.insert(value.identifier);
        }

        InstructionValue::Destructure { value, .. } => {
            used.insert(value.identifier);
        }

        InstructionValue::BinaryExpression { left, right, .. } => {
            used.insert(left.identifier);
            used.insert(right.identifier);
        }

        InstructionValue::TernaryExpression { test, consequent, alternate, .. } => {
            used.insert(test.identifier);
            used.insert(consequent.identifier);
            used.insert(alternate.identifier);
        }

        InstructionValue::UnaryExpression { value, .. }
        | InstructionValue::Await { value, .. }
        | InstructionValue::TypeCastExpression { value, .. }
        | InstructionValue::NextPropertyOf { value, .. } => {
            used.insert(value.identifier);
        }

        InstructionValue::CallExpression { callee, args, .. } => {
            used.insert(callee.identifier);
            for arg in args {
                mark_call_arg(arg, used);
            }
        }

        InstructionValue::MethodCall { receiver, property, args, .. } => {
            used.insert(receiver.identifier);
            used.insert(property.identifier);
            for arg in args {
                mark_call_arg(arg, used);
            }
        }

        InstructionValue::NewExpression { callee, args, .. } => {
            used.insert(callee.identifier);
            for arg in args {
                mark_call_arg(arg, used);
            }
        }

        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::PropertyDelete { object, .. } => {
            used.insert(object.identifier);
        }

        InstructionValue::PropertyStore { object, value, .. } => {
            used.insert(object.identifier);
            used.insert(value.identifier);
        }

        InstructionValue::ComputedLoad { object, property, .. }
        | InstructionValue::ComputedDelete { object, property, .. } => {
            used.insert(object.identifier);
            used.insert(property.identifier);
        }

        InstructionValue::ComputedStore { object, property, value, .. } => {
            used.insert(object.identifier);
            used.insert(property.identifier);
            used.insert(value.identifier);
        }

        InstructionValue::JsxExpression { tag, props, children, .. } => {
            if let JsxTag::Place(p) = tag {
                used.insert(p.identifier);
            }
            for prop in props {
                match prop {
                    JsxAttribute::Attribute { place, .. } => { used.insert(place.identifier); }
                    JsxAttribute::Spread { argument } => { used.insert(argument.identifier); }
                }
            }
            if let Some(children) = children {
                for c in children {
                    used.insert(c.identifier);
                }
            }
        }

        InstructionValue::JsxFragment { children, .. } => {
            for c in children {
                used.insert(c.identifier);
            }
        }

        InstructionValue::ArrayExpression { elements, .. } => {
            for el in elements {
                match el {
                    ArrayElement::Place(p) => { used.insert(p.identifier); }
                    ArrayElement::Spread(s) => { used.insert(s.place.identifier); }
                    ArrayElement::Hole => {}
                }
            }
        }

        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectExpressionProperty::Property(p) => {
                        used.insert(p.place.identifier);
                        if let ObjectPropertyKey::Computed(c) = &p.key {
                            used.insert(c.identifier);
                        }
                    }
                    ObjectExpressionProperty::Spread(s) => {
                        used.insert(s.place.identifier);
                    }
                }
            }
        }

        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs {
                used.insert(expr.identifier);
            }
        }

        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            used.insert(tag.identifier);
        }

        InstructionValue::GetIterator { collection, .. } => {
            used.insert(collection.identifier);
        }

        InstructionValue::IteratorNext { iterator, collection, .. } => {
            used.insert(iterator.identifier);
            used.insert(collection.identifier);
        }

        InstructionValue::PrefixUpdate { value, .. }
        | InstructionValue::PostfixUpdate { value, .. } => {
            // lvalue is the WRITE TARGET (output), not an input — only value is used.
            used.insert(value.identifier);
        }

        InstructionValue::FinishMemoize { decl, .. } => {
            used.insert(decl.identifier);
        }

        InstructionValue::StartMemoize { deps, .. } => {
            if let Some(deps) = deps {
                for dep in deps {
                    match &dep.root {
                        ManualMemoRoot::NamedLocal { place, .. } => {
                            used.insert(place.identifier);
                        }
                        ManualMemoRoot::Global { .. } => {}
                    }
                }
            }
        }

        // These carry no place operands that need tracking.
        InstructionValue::Primitive { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::InlineJs { .. }
        | InstructionValue::UnsupportedNode { .. } => {}
    }
}

fn mark_call_arg(arg: &CallArg, used: &mut HashSet<IdentifierId>) {
    match arg {
        CallArg::Place(p) => { used.insert(p.identifier); }
        CallArg::Spread(s) => { used.insert(s.place.identifier); }
    }
}
