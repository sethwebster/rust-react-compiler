/// Infer which Places are "reactive" — their values may change across renders.
///
/// Algorithm (simplified port of InferReactivePlaces.ts):
///
/// 1. Mark all function params as reactive.
/// 2. Walk blocks; for each instruction:
///    - If any input operand is reactive, mark the lvalue reactive.
///    - Hook globals (useXxx) are sources of reactivity.
/// 3. Phi nodes: if any operand is reactive, mark the phi place reactive.
/// 4. Repeat until fixpoint (handles back-edges and aliases).
use std::collections::{HashSet, HashMap};

use indexmap::IndexMap;

use crate::hir::hir::{
    ArrayElement, BasicBlock, BlockId, HIRFunction, IdentifierId, InstructionValue, NonLocalBinding, ObjectPatternProperty, Param, Pattern, Terminal,
};
use crate::hir::visitors::{each_instruction_value_operand, each_instruction_value_operand_mut};

pub fn infer_reactive_places(hir: &mut HIRFunction) {
    let mut reactive: HashSet<IdentifierId> = HashSet::new();

    // Pre-scan: collect identifiers that hold stable-hook references.
    // When these are used as a CallExpression callee, the call result is NOT reactive
    // even if the arguments are reactive (stable hooks always return the same object).
    let mut stable_hook_refs: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if is_stable_hook_load(&instr.value) {
                stable_hook_refs.insert(instr.lvalue.identifier);
                // Also track the stored var if it's immediately StoreLocal'd
            }
        }
    }

    // Pre-scan: identify stable dispatcher identifiers from hooks that return
    // [value, dispatch] pairs (useState, useReducer, useActionState).
    // The second element of the destructure is the stable dispatcher/setter.
    // We preemptively add these to a "never reactive" set so they are never
    // treated as reactive deps.
    let mut stable_dispatchers: HashSet<IdentifierId> = HashSet::new();
    {
        // Step 1: find call results that are from dispatch-returning hooks.
        let mut dispatch_hook_results: HashSet<IdentifierId> = HashSet::new();
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                    // Check if callee is a LoadGlobal for useState/useReducer/useActionState
                    // by finding the LoadGlobal instruction for this callee identifier.
                    if is_dispatch_hook_ref(callee.identifier, &hir.body.blocks) {
                        dispatch_hook_results.insert(instr.lvalue.identifier);
                    }
                }
            }
        }
        // Step 2: for Destructure instructions on these results, mark the second+ elements stable.
        for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::Destructure { value, lvalue, .. } = &instr.value {
                    if dispatch_hook_results.contains(&value.identifier) {
                        if let Pattern::Array(ap) = &lvalue.pattern {
                            // Elements at index 1+ are stable dispatchers (setState, dispatch).
                            for (i, elem) in ap.items.iter().enumerate() {
                                if i >= 1 {
                                    match elem {
                                        ArrayElement::Place(p) => { stable_dispatchers.insert(p.identifier); }
                                        ArrayElement::Spread(s) => { stable_dispatchers.insert(s.place.identifier); }
                                        ArrayElement::Hole => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Seed: all params are reactive.
    for param in &hir.params {
        match param {
            Param::Place(p) => {
                if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[reactive_places] seeding param id={}", p.identifier.0);
                }
                reactive.insert(p.identifier);
            }
            Param::Spread(s) => { reactive.insert(s.place.identifier); }
        }
    }

    // Fixpoint iteration.
    loop {
        let prev = reactive.len();

        // Control dependency pass.
        //
        // Strategy A: Phi-based (for If/Branch/Switch with Place test).
        // If a block has a reactive conditional terminal, phis in the fallthrough
        // that are non-trivial (phi operands differ across branches) are reactive.
        // Note: Our SSA only renames instruction lvalues, not named-variable StoreLocal
        // targets, so named-variable phis are always trivial. We handle that in Strategy B.
        //
        // Strategy B: StoreLocal-based control dep.
        // When a named variable is assigned (StoreLocal) inside a branch controlled by a
        // reactive conditional, mark that named variable as reactive. This covers cases
        // where `if (cond) { x = 1; } else { x = 2; }` makes `x` reactive.
        let mut control_reactive_blocks: HashSet<BlockId> = HashSet::new();
        for (_, block) in &hir.body.blocks {
            match &block.terminal {
                Terminal::If { test, consequent, alternate, fallthrough, .. }
                | Terminal::Branch { test, consequent, alternate, fallthrough, .. } => {
                    if reactive.contains(&test.identifier) {
                        control_reactive_blocks.insert(*fallthrough);
                        // Strategy B: scan then/else blocks for StoreLocal to named vars.
                        // Any named variable stored inside will become reactive.
                        for branch_id in [consequent, alternate] {
                            // Walk the branch block and its successors (up to fallthrough).
                            let mut visited_branch: HashSet<BlockId> = HashSet::new();
                            let mut work = vec![*branch_id];
                            while let Some(bk) = work.pop() {
                                if bk == *fallthrough || !visited_branch.insert(bk) { continue; }
                                if let Some(branch_block) = hir.body.blocks.get(&bk) {
                                    for instr in &branch_block.instructions {
                                        if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                            // Named variable assigned in branch → reactive.
                                            reactive.insert(lvalue.place.identifier);
                                        }
                                        if let InstructionValue::StoreContext { lvalue, .. } = &instr.value {
                                            reactive.insert(lvalue.place.identifier);
                                        }
                                    }
                                    for &succ in branch_block.terminal.successors().iter() {
                                        work.push(succ);
                                    }
                                }
                            }
                        }
                    }
                }
                Terminal::Switch { test, cases, fallthrough, .. } => {
                    if reactive.contains(&test.identifier) {
                        control_reactive_blocks.insert(*fallthrough);
                        // Strategy B for switch cases.
                        for case in cases {
                            let mut visited_branch: HashSet<BlockId> = HashSet::new();
                            let mut work = vec![case.block];
                            while let Some(bk) = work.pop() {
                                if bk == *fallthrough || !visited_branch.insert(bk) { continue; }
                                if let Some(branch_block) = hir.body.blocks.get(&bk) {
                                    for instr in &branch_block.instructions {
                                        if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                            reactive.insert(lvalue.place.identifier);
                                        }
                                    }
                                    for &succ in branch_block.terminal.successors().iter() {
                                        work.push(succ);
                                    }
                                }
                            }
                        }
                    }
                }
                Terminal::While { test: test_block_id, loop_: loop_block_id, fallthrough, .. }
                | Terminal::DoWhile { test: test_block_id, loop_: loop_block_id, fallthrough, .. } => {
                    // Check if the test block produces a reactive terminal value.
                    if let Some(test_block) = hir.body.blocks.get(test_block_id) {
                        let test_reactive = test_block.instructions.last()
                            .map_or(false, |i| reactive.contains(&i.lvalue.identifier));
                        if test_reactive {
                            control_reactive_blocks.insert(*fallthrough);
                            // Strategy B: vars assigned in loop body are reactive.
                            let mut visited_branch: HashSet<BlockId> = HashSet::new();
                            let mut work = vec![*loop_block_id];
                            while let Some(bk) = work.pop() {
                                if bk == *fallthrough || !visited_branch.insert(bk) { continue; }
                                if let Some(branch_block) = hir.body.blocks.get(&bk) {
                                    for instr in &branch_block.instructions {
                                        if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                            reactive.insert(lvalue.place.identifier);
                                        }
                                    }
                                    for &succ in branch_block.terminal.successors().iter() {
                                        work.push(succ);
                                    }
                                }
                            }
                        }
                    }
                }
                Terminal::For { test: test_block_id, loop_: loop_block_id, fallthrough, .. } => {
                    if let Some(test_block) = hir.body.blocks.get(test_block_id) {
                        let test_reactive = test_block.instructions.last()
                            .map_or(false, |i| reactive.contains(&i.lvalue.identifier));
                        if test_reactive {
                            control_reactive_blocks.insert(*fallthrough);
                            let mut visited_branch: HashSet<BlockId> = HashSet::new();
                            let mut work = vec![*loop_block_id];
                            while let Some(bk) = work.pop() {
                                if bk == *fallthrough || !visited_branch.insert(bk) { continue; }
                                if let Some(branch_block) = hir.body.blocks.get(&bk) {
                                    for instr in &branch_block.instructions {
                                        if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                                            reactive.insert(lvalue.place.identifier);
                                        }
                                    }
                                    for &succ in branch_block.terminal.successors().iter() {
                                        work.push(succ);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        for (bid, block) in &hir.body.blocks {
            // Phi nodes: if any incoming operand is reactive → phi is reactive.
            // ALSO: if this block is control-reactive AND the phi is non-trivial
            // (different operands from different branches), it is reactive.
            // Trivial phis (all operands are the same SSA id) are not control-dependent.
            let is_ctrl_reactive = control_reactive_blocks.contains(bid);
            for phi in &block.phis {
                let data_reactive = phi.operands.values().any(|op| reactive.contains(&op.identifier));
                let ctrl_reactive = if is_ctrl_reactive {
                    let ids: std::collections::HashSet<u32> = phi.operands.values().map(|op| op.identifier.0).collect();
                    ids.len() > 1
                } else {
                    false
                };
                if data_reactive || ctrl_reactive {
                    reactive.insert(phi.place.identifier);
                }
            }

            for instr in &block.instructions {
                // Stable hook calls (useRef, useEffect, etc.) never produce reactive values
                // even if their arguments are reactive.
                if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                    if stable_hook_refs.contains(&callee.identifier) {
                        continue; // result is stable — skip reactivity propagation
                    }
                }
                if let InstructionValue::MethodCall { receiver, property, .. } = &instr.value {
                    // Method calls on stable hook refs are also stable
                    // (e.g., ref.method() — hooks don't have reactive methods)
                }

                // Never mark stable dispatchers (setState, dispatch) as reactive.
                if stable_dispatchers.contains(&instr.lvalue.identifier) {
                    continue;
                }

                let has_reactive = each_instruction_value_operand(&instr.value)
                    .iter()
                    .any(|p| reactive.contains(&p.identifier));
                let is_hook = value_is_hook_source(&instr.value);
                if has_reactive || is_hook {
                    reactive.insert(instr.lvalue.identifier);
                    // For StoreLocal: the stored variable (lvalue.place) also becomes
                    // reactive when the assigned value is reactive.
                    if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                        if !stable_dispatchers.contains(&lvalue.place.identifier) {
                            reactive.insert(lvalue.place.identifier);
                        }
                    }
                    // For Destructure instructions, also mark pattern variables as reactive,
                    // but skip stable dispatchers.
                    if let InstructionValue::Destructure { value, lvalue, .. } = &instr.value {
                        if reactive.contains(&value.identifier) {
                            for pid in destructure_pattern_ids(&lvalue.pattern) {
                                if !stable_dispatchers.contains(&pid) {
                                    reactive.insert(pid);
                                }
                            }
                        }
                    }
                }
            }
        }

        if reactive.len() == prev {
            break;
        }
    }

    if std::env::var("RC_DEBUG").is_ok() {
        eprintln!("[reactive_places] reactive set size: {}", reactive.len());
        let mut rids: Vec<u32> = reactive.iter().map(|r| r.0).collect();
        rids.sort();
        eprintln!("[reactive_places] reactive ids: {:?}", &rids[..rids.len().min(20)]);
    }
    // Write back: mark Place.reactive flags.
    for (_, block) in &mut hir.body.blocks {
        for phi in &mut block.phis {
            if reactive.contains(&phi.place.identifier) {
                phi.place.reactive = true;
            }
            for op in phi.operands.values_mut() {
                if reactive.contains(&op.identifier) {
                    op.reactive = true;
                }
            }
        }
        for instr in &mut block.instructions {
            if reactive.contains(&instr.lvalue.identifier) {
                instr.lvalue.reactive = true;
            }
            for place in each_instruction_value_operand_mut(&mut instr.value) {
                if reactive.contains(&place.identifier) {
                    place.reactive = true;
                }
            }
            // Mark Destructure pattern places reactive in the write-back phase.
            if let InstructionValue::Destructure { lvalue, .. } = &mut instr.value {
                mark_pattern_places_reactive(&mut lvalue.pattern, &reactive);
            }
            // Mark StoreLocal's target variable reactive in write-back.
            if let InstructionValue::StoreLocal { lvalue, .. } = &mut instr.value {
                if reactive.contains(&lvalue.place.identifier) {
                    lvalue.place.reactive = true;
                }
            }
        }
    }
}

/// A LoadGlobal of a hook name is a source of reactivity —
/// but stable hooks (whose return value never changes) are excluded.
fn value_is_hook_source(value: &InstructionValue) -> bool {
    if let InstructionValue::LoadGlobal { binding, .. } = value {
        let name = match binding {
            NonLocalBinding::Global { name } => name.as_str(),
            NonLocalBinding::ImportSpecifier { name, .. } => name.as_str(),
            NonLocalBinding::ImportDefault { name, .. } => name.as_str(),
            NonLocalBinding::ImportNamespace { name, .. } => name.as_str(),
            NonLocalBinding::ModuleLocal { name } => name.as_str(),
        };
        is_hook_name(name) && !is_stable_hook(name)
    } else {
        false
    }
}

/// Returns true if this instruction loads a stable hook (useRef, useEffect, etc.)
/// whose call results should never be marked reactive.
fn is_stable_hook_load(value: &InstructionValue) -> bool {
    if let InstructionValue::LoadGlobal { binding, .. } = value {
        let name = match binding {
            NonLocalBinding::Global { name } => name.as_str(),
            NonLocalBinding::ImportSpecifier { name, .. } => name.as_str(),
            NonLocalBinding::ImportDefault { name, .. } => name.as_str(),
            NonLocalBinding::ImportNamespace { name, .. } => name.as_str(),
            NonLocalBinding::ModuleLocal { name } => name.as_str(),
        };
        is_stable_hook(name)
    } else {
        false
    }
}

fn is_hook_name(name: &str) -> bool {
    name.starts_with("use") && name[3..].chars().next().map_or(false, |c| c.is_uppercase())
}

/// Returns true if the given identifier refers to a hook that returns
/// a [value, dispatch/setState] pair. Used to detect stable dispatchers.
fn is_dispatch_hook_ref(id: IdentifierId, blocks: &IndexMap<BlockId, BasicBlock>) -> bool {
    for (_, block) in blocks {
        for instr in &block.instructions {
            if instr.lvalue.identifier == id {
                if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                    let name = match binding {
                        NonLocalBinding::Global { name } => name.as_str(),
                        NonLocalBinding::ImportSpecifier { name, .. } => name.as_str(),
                        NonLocalBinding::ImportDefault { name, .. } => name.as_str(),
                        NonLocalBinding::ImportNamespace { name, .. } => name.as_str(),
                        NonLocalBinding::ModuleLocal { name } => name.as_str(),
                    };
                    return is_dispatch_hook(name);
                }
                return false;
            }
        }
    }
    false
}

/// Hooks whose return value includes a stable dispatcher at index 1+.
fn is_dispatch_hook(name: &str) -> bool {
    matches!(name, "useState" | "useReducer" | "useActionState" | "useFormState")
}

/// Hooks whose return value is always the same object across renders.
/// These are NOT sources of reactivity.
fn is_stable_hook(name: &str) -> bool {
    matches!(
        name,
        "useRef"
            | "useId"
            | "useImperativeHandle"
            | "useDebugValue"
            | "useEffect"
            | "useLayoutEffect"
            | "useInsertionEffect"
    )
}

/// Mark all places in a destructuring pattern as reactive if they are in the reactive set.
fn mark_pattern_places_reactive(pattern: &mut Pattern, reactive: &HashSet<IdentifierId>) {
    match pattern {
        Pattern::Array(ap) => {
            for elem in ap.items.iter_mut() {
                match elem {
                    ArrayElement::Place(p) => {
                        if reactive.contains(&p.identifier) { p.reactive = true; }
                    }
                    ArrayElement::Spread(s) => {
                        if reactive.contains(&s.place.identifier) { s.place.reactive = true; }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(op) => {
            for prop in op.properties.iter_mut() {
                match prop {
                    ObjectPatternProperty::Property(p) => {
                        if reactive.contains(&p.place.identifier) { p.place.reactive = true; }
                    }
                    ObjectPatternProperty::Spread(s) => {
                        if reactive.contains(&s.place.identifier) { s.place.reactive = true; }
                    }
                }
            }
        }
    }
}

/// Collect all IdentifierIds that are bound by a destructuring pattern.
fn destructure_pattern_ids(pattern: &Pattern) -> Vec<IdentifierId> {
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
