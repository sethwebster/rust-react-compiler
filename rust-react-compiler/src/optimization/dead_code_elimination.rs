#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::HashSet;
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
    // Iterate until convergence: removing dead StoreLocals can make their
    // value-producing Primitives dead, requiring another pass.
    loop {
        let before: usize = hir.body.blocks.values().map(|b| b.instructions.len()).sum();
        remove_dead_instructions(hir, env);
        let after: usize = hir.body.blocks.values().map(|b| b.instructions.len()).sum();
        if after == before {
            break;
        }
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
                    return used.contains(&instr.lvalue.identifier);
                }
            }
            // DeclareLocal for a variable that is never loaded, never captured,
            // and never used as a phi operand is truly dead — eliminate it.
            // This handles `let foo;` inside a loop where `foo` is never read.
            if let InstructionValue::DeclareLocal { lvalue, .. } = &instr.value {
                if !loaded_vars.contains(&lvalue.place.identifier)
                    && !captured_vars.contains(&lvalue.place.identifier)
                    && !used.contains(&lvalue.place.identifier)
                {
                    return false;
                }
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
            // Update expressions (++/--) mutate a variable — always a side effect.
            | InstructionValue::PostfixUpdate { .. }
            | InstructionValue::PrefixUpdate { .. }
    )
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

        InstructionValue::PrefixUpdate { lvalue, value, .. }
        | InstructionValue::PostfixUpdate { lvalue, value, .. } => {
            used.insert(lvalue.identifier);
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
