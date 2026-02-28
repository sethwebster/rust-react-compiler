#![allow(unused_imports, unused_variables, dead_code)]
use std::collections::HashSet;
use crate::hir::hir::*;

/// Dead code elimination pass.
///
/// Two sub-passes:
///   1. Unreachable block elimination — BFS from entry removes blocks with no
///      live predecessor path.
///   2. Dead instruction elimination — conservative liveness: an instruction's
///      lvalue must appear in the used-identifier set, OR the instruction has
///      observable side effects.
pub fn dead_code_elimination(hir: &mut HIRFunction) {
    remove_unreachable_blocks(hir);
    remove_dead_instructions(hir);
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

fn remove_dead_instructions(hir: &mut HIRFunction) {
    let mut used: HashSet<IdentifierId> = HashSet::new();

    // The return place is always live.
    used.insert(hir.returns.identifier);

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
    for block in hir.body.blocks.values() {
        collect_terminal_uses(&block.terminal, &mut used);
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

    // Remove instructions whose lvalue is dead and that have no side effects.
    for block in hir.body.blocks.values_mut() {
        block.instructions.retain(|instr| {
            used.contains(&instr.lvalue.identifier) || has_side_effects(&instr.value)
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn has_side_effects(value: &InstructionValue) -> bool {
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
            | InstructionValue::Debugger { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Await { .. }
            | InstructionValue::UnsupportedNode { .. }
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
        | InstructionValue::UnsupportedNode { .. } => {}
    }
}

fn mark_call_arg(arg: &CallArg, used: &mut HashSet<IdentifierId>) {
    match arg {
        CallArg::Place(p) => { used.insert(p.identifier); }
        CallArg::Spread(s) => { used.insert(s.place.identifier); }
    }
}
