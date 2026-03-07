#![allow(unused_imports, unused_variables, dead_code)]

use std::collections::HashSet;
use crate::hir::hir::{HIRFunction, IdentifierId};

/// Remove phi nodes that are trivially redundant:
/// - A phi with zero operands is a placeholder from an unreachable join point.
/// - A phi whose every operand carries the same SSA id is equivalent to a
///   direct assignment and can be replaced by that id everywhere.
///
/// This mirrors EliminateRedundantPhi in the TS compiler.
pub fn eliminate_redundant_phi(hir: &mut HIRFunction) {
    // Iterate to a fixed point: each round may expose new trivial phis after
    // substituting previously-removed ones.
    loop {
        // Collect (phi_result_id -> replacement_id) for all trivial phis found
        // in this round.
        let mut replacements: std::collections::HashMap<IdentifierId, IdentifierId> =
            std::collections::HashMap::new();

        for block in hir.body.blocks.values() {
            for phi in &block.phis {
                if phi.operands.is_empty() {
                    // Placeholder phi from unreachable predecessor — trivially remove.
                    // Map its result to itself (will be dropped below, no use-sites
                    // should reference it, but guard against it).
                    // We'll just mark it for removal; no replacement needed.
                    continue;
                }

                let unique_ids: HashSet<IdentifierId> = phi.operands
                    .values()
                    .map(|p| {
                        // Chase any already-collected replacement chain.
                        let mut id = p.identifier;
                        while let Some(&r) = replacements.get(&id) {
                            if r == id { break; }
                            id = r;
                        }
                        id
                    })
                    .collect();

                // A phi is trivial if all operands resolve to the same SSA id,
                // OR if all operands are the phi's own result (self-loop).
                let phi_id = phi.place.identifier;
                let non_self: HashSet<IdentifierId> = unique_ids
                    .iter()
                    .copied()
                    .filter(|&id| id != phi_id)
                    .collect();

                match non_self.len() {
                    0 => {
                        // Pure self-loop — all operands point back to this phi.
                        // This means the variable is never actually assigned from
                        // outside; treat as undefined (keep the id, drop the phi).
                        // Map to self so the retain filter drops this phi.
                        replacements.insert(phi_id, phi_id);
                    }
                    1 => {
                        // All operands agree on a single non-self value.
                        let replacement = *non_self.iter().next().unwrap();
                        if std::env::var("RC_DEBUG_SSA").is_ok() {
                            eprintln!("[ssa] ELIMINATE: $t{} -> $t{}", phi_id.0, replacement.0);
                        }
                        replacements.insert(phi_id, replacement);
                    }
                    _ => {
                        // Genuinely distinct operands — phi is needed.
                    }
                }
            }
        }

        if replacements.is_empty() {
            break;
        }

        // Apply replacements: rewrite every Place in the function.
        apply_replacements(hir, &replacements);

        // Drop now-trivial phi nodes.
        for block in hir.body.blocks.values_mut() {
            block.phis.retain(|phi| {
                if phi.operands.is_empty() {
                    return false;
                }
                !replacements.contains_key(&phi.place.identifier)
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Apply a replacement map to all Places in the HIR.
// ---------------------------------------------------------------------------

fn apply_replacements(
    hir: &mut HIRFunction,
    replacements: &std::collections::HashMap<IdentifierId, IdentifierId>,
) {
    let resolve = |id: IdentifierId| -> IdentifierId {
        let mut current = id;
        // Chase replacement chains (e.g. a -> b -> c).
        for _ in 0..replacements.len() + 1 {
            match replacements.get(&current) {
                Some(&next) if next != current => current = next,
                _ => break,
            }
        }
        current
    };

    let rp = |id: IdentifierId| -> IdentifierId { resolve(id) };

    for block in hir.body.blocks.values_mut() {
        // Phi places: do NOT rewrite — a phi's result identifier is its definition,
        // not a use.  Rewriting it would corrupt the retain check that uses the
        // pre-replacement identifier to decide which phis to drop.
        // Phi operands: DO rewrite, because they are uses of other identifiers.
        for phi in &mut block.phis {
            for operand in phi.operands.values_mut() {
                operand.identifier = rp(operand.identifier);
            }
        }

        // Instructions.
        for instr in &mut block.instructions {
            instr.lvalue.identifier = rp(instr.lvalue.identifier);
            rewrite_value_identifiers(&mut instr.value, rp);
        }

        // Terminal.
        rewrite_terminal_identifiers(&mut block.terminal, rp);
    }

    // Params.
    for param in &mut hir.params {
        match param {
            crate::hir::hir::Param::Place(p) => p.identifier = rp(p.identifier),
            crate::hir::hir::Param::Spread(s) => s.place.identifier = rp(s.place.identifier),
        }
    }

    // Context.
    for ctx in &mut hir.context {
        ctx.identifier = rp(ctx.identifier);
    }

    // Returns place.
    hir.returns.identifier = rp(hir.returns.identifier);
}

fn rewrite_value_identifiers(
    val: &mut crate::hir::hir::InstructionValue,
    rp: impl Fn(IdentifierId) -> IdentifierId,
) {
    use crate::hir::hir::InstructionValue::*;
    match val {
        LoadLocal { place, .. } | LoadContext { place, .. } => place.identifier = rp(place.identifier),
        DeclareLocal { lvalue, .. } => lvalue.place.identifier = rp(lvalue.place.identifier),
        DeclareContext { lvalue, .. } => lvalue.place.identifier = rp(lvalue.place.identifier),
        StoreLocal { lvalue, value, .. } => {
            lvalue.place.identifier = rp(lvalue.place.identifier);
            value.identifier = rp(value.identifier);
        }
        StoreContext { lvalue, value, .. } => {
            lvalue.place.identifier = rp(lvalue.place.identifier);
            value.identifier = rp(value.identifier);
        }
        StoreGlobal { value, .. } => value.identifier = rp(value.identifier),
        Destructure { value, .. } => value.identifier = rp(value.identifier),
        BinaryExpression { left, right, .. } => {
            left.identifier = rp(left.identifier);
            right.identifier = rp(right.identifier);
        }
        TernaryExpression { test, consequent, alternate, .. } => {
            test.identifier = rp(test.identifier);
            consequent.identifier = rp(consequent.identifier);
            alternate.identifier = rp(alternate.identifier);
        }
        UnaryExpression { value, .. } => value.identifier = rp(value.identifier),
        TypeCastExpression { value, .. } => value.identifier = rp(value.identifier),
        CallExpression { callee, args, .. } => {
            callee.identifier = rp(callee.identifier);
            for arg in args.iter_mut() {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => p.identifier = rp(p.identifier),
                    crate::hir::hir::CallArg::Spread(s) => s.place.identifier = rp(s.place.identifier),
                }
            }
        }
        MethodCall { receiver, property, args, .. } => {
            receiver.identifier = rp(receiver.identifier);
            property.identifier = rp(property.identifier);
            for arg in args.iter_mut() {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => p.identifier = rp(p.identifier),
                    crate::hir::hir::CallArg::Spread(s) => s.place.identifier = rp(s.place.identifier),
                }
            }
        }
        NewExpression { callee, args, .. } => {
            callee.identifier = rp(callee.identifier);
            for arg in args.iter_mut() {
                match arg {
                    crate::hir::hir::CallArg::Place(p) => p.identifier = rp(p.identifier),
                    crate::hir::hir::CallArg::Spread(s) => s.place.identifier = rp(s.place.identifier),
                }
            }
        }
        ObjectExpression { properties, .. } => {
            for prop in properties.iter_mut() {
                match prop {
                    crate::hir::hir::ObjectExpressionProperty::Property(p) => {
                        p.place.identifier = rp(p.place.identifier);
                        if let crate::hir::hir::ObjectPropertyKey::Computed(k) = &mut p.key {
                            k.identifier = rp(k.identifier);
                        }
                    }
                    crate::hir::hir::ObjectExpressionProperty::Spread(s) => {
                        s.place.identifier = rp(s.place.identifier)
                    }
                }
            }
        }
        ArrayExpression { elements, .. } => {
            for el in elements.iter_mut() {
                match el {
                    crate::hir::hir::ArrayElement::Place(p) => p.identifier = rp(p.identifier),
                    crate::hir::hir::ArrayElement::Spread(s) => s.place.identifier = rp(s.place.identifier),
                    crate::hir::hir::ArrayElement::Hole => {}
                }
            }
        }
        PropertyLoad { object, .. } => object.identifier = rp(object.identifier),
        PropertyStore { object, value, .. } => {
            object.identifier = rp(object.identifier);
            value.identifier = rp(value.identifier);
        }
        PropertyDelete { object, .. } => object.identifier = rp(object.identifier),
        ComputedLoad { object, property, .. } => {
            object.identifier = rp(object.identifier);
            property.identifier = rp(property.identifier);
        }
        ComputedStore { object, property, value, .. } => {
            object.identifier = rp(object.identifier);
            property.identifier = rp(property.identifier);
            value.identifier = rp(value.identifier);
        }
        ComputedDelete { object, property, .. } => {
            object.identifier = rp(object.identifier);
            property.identifier = rp(property.identifier);
        }
        JsxExpression { tag, props, children, .. } => {
            if let crate::hir::hir::JsxTag::Place(p) = tag {
                p.identifier = rp(p.identifier);
            }
            for attr in props.iter_mut() {
                match attr {
                    crate::hir::hir::JsxAttribute::Attribute { place, .. } => {
                        place.identifier = rp(place.identifier)
                    }
                    crate::hir::hir::JsxAttribute::Spread { argument } => {
                        argument.identifier = rp(argument.identifier)
                    }
                }
            }
            if let Some(ch) = children.as_mut() {
                for c in ch.iter_mut() {
                    c.identifier = rp(c.identifier);
                }
            }
        }
        JsxFragment { children, .. } => {
            for c in children.iter_mut() {
                c.identifier = rp(c.identifier);
            }
        }
        TemplateLiteral { subexprs, .. } => {
            for e in subexprs.iter_mut() {
                e.identifier = rp(e.identifier);
            }
        }
        TaggedTemplateExpression { tag, .. } => tag.identifier = rp(tag.identifier),
        Await { value, .. } => value.identifier = rp(value.identifier),
        GetIterator { collection, .. } => collection.identifier = rp(collection.identifier),
        IteratorNext { iterator, collection, .. } => {
            iterator.identifier = rp(iterator.identifier);
            collection.identifier = rp(collection.identifier);
        }
        NextPropertyOf { value, .. } => value.identifier = rp(value.identifier),
        PrefixUpdate { lvalue, value, .. } => {
            lvalue.identifier = rp(lvalue.identifier);
            value.identifier = rp(value.identifier);
        }
        PostfixUpdate { lvalue, value, .. } => {
            lvalue.identifier = rp(lvalue.identifier);
            value.identifier = rp(value.identifier);
        }
        FinishMemoize { decl, .. } => decl.identifier = rp(decl.identifier),
        // No place operands:
        LoadGlobal { .. }
        | Primitive { .. }
        | JsxText { .. }
        | RegExpLiteral { .. }
        | MetaProperty { .. }
        | Debugger { .. }
        | StartMemoize { .. }
        | UnsupportedNode { .. }
        | InlineJs { .. }
        | ObjectMethod { .. }
        | FunctionExpression { .. } => {}
    }
}

fn rewrite_terminal_identifiers(
    terminal: &mut crate::hir::hir::Terminal,
    rp: impl Fn(IdentifierId) -> IdentifierId,
) {
    use crate::hir::hir::Terminal::*;
    match terminal {
        Return { value, .. } => value.identifier = rp(value.identifier),
        Throw { value, .. } => value.identifier = rp(value.identifier),
        If { test, .. } | Branch { test, .. } => test.identifier = rp(test.identifier),
        Switch { test, cases, .. } => {
            test.identifier = rp(test.identifier);
            for case in cases.iter_mut() {
                if let Some(t) = case.test.as_mut() {
                    t.identifier = rp(t.identifier);
                }
            }
        }
        Try { handler_binding, .. } => {
            if let Some(p) = handler_binding.as_mut() {
                p.identifier = rp(p.identifier);
            }
        }
        Unsupported { .. }
        | Unreachable { .. }
        | Goto { .. }
        | DoWhile { .. }
        | While { .. }
        | For { .. }
        | ForOf { .. }
        | ForIn { .. }
        | Logical { .. }
        | Ternary { .. }
        | Optional { .. }
        | Label { .. }
        | Sequence { .. }
        | MaybeThrow { .. }
        | ReactiveScope { .. }
        | PrunedScope { .. } => {}
    }
}
