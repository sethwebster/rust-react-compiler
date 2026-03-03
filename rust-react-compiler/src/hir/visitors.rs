/// HIR visitor helpers — collect operand Places from instructions and terminals.
///
/// These mirror the TypeScript `visitors.ts` helpers and are used by inference
/// passes that need to walk operands without caring about specific variants.
use crate::hir::hir::{
    ArrayElement, CallArg, InstructionValue, JsxAttribute, JsxTag,
    LValuePattern, ObjectExpressionProperty, Place, Terminal,
};

// ---------------------------------------------------------------------------
// Operand collectors (immutable)
// ---------------------------------------------------------------------------

/// Collect all input `Place` operands of an `InstructionValue`.
/// These are the Places the instruction *reads* — not the lvalue it writes to.
pub fn each_instruction_value_operand(value: &InstructionValue) -> Vec<&Place> {
    let mut out = Vec::new();
    collect_instruction_value_operands(value, &mut out);
    out
}

/// Like `each_instruction_value_operand` but for dep-propagation purposes.
/// For MethodCall, skips the `receiver` because the `property` operand already
/// captures the full dep chain (e.g., `props.render` subsumes `props`).
/// Processing receiver first would add `props` as dep, blocking `props.render`
/// via the ancestor check.
pub fn each_dep_operand(value: &InstructionValue) -> Vec<&Place> {
    match value {
        // For MethodCall, use the receiver as the dep (not the property accessor).
        // e.g. `a.at(b)` → deps are [a, b], not [a.at, b].
        // The receiver captures the full dependency on the base object.
        InstructionValue::MethodCall { receiver, args, .. } => {
            let mut out: Vec<&Place> = vec![receiver];
            for arg in args {
                match arg {
                    CallArg::Place(p) => out.push(p),
                    CallArg::Spread(s) => out.push(&s.place),
                }
            }
            out
        }
        _ => each_instruction_value_operand(value),
    }
}

fn collect_instruction_value_operands<'a>(
    value: &'a InstructionValue,
    out: &mut Vec<&'a Place>,
) {
    match value {
        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } => {
            out.push(place);
        }
        InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::InlineJs { .. }
        | InstructionValue::UnsupportedNode { .. }
        | InstructionValue::Debugger { .. } => {}

        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreGlobal { value, .. }
        | InstructionValue::TypeCastExpression { value, .. }
        | InstructionValue::UnaryExpression { value, .. }
        | InstructionValue::Await { value, .. }
        | InstructionValue::GetIterator { collection: value, .. }
        | InstructionValue::NextPropertyOf { value, .. } => {
            out.push(value);
        }

        InstructionValue::StoreContext { lvalue, value, .. } => {
            out.push(&lvalue.place);
            out.push(value);
        }

        InstructionValue::Destructure { value, .. } => {
            out.push(value);
        }

        InstructionValue::BinaryExpression { left, right, .. } => {
            out.push(left);
            out.push(right);
        }

        InstructionValue::CallExpression { callee, args, .. }
        | InstructionValue::NewExpression { callee, args, .. } => {
            out.push(callee);
            collect_call_args(args, out);
        }

        InstructionValue::MethodCall {
            receiver, property, args, ..
        } => {
            out.push(receiver);
            out.push(property);
            collect_call_args(args, out);
        }

        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::PropertyDelete { object, .. }
        | InstructionValue::PropertyStore { object, .. } => {
            out.push(object);
            if let InstructionValue::PropertyStore { value, .. } = value {
                out.push(value);
            }
        }

        InstructionValue::ComputedLoad { object, property, .. } => {
            out.push(object);
            out.push(property);
        }
        InstructionValue::ComputedDelete { object, property, .. } => {
            out.push(object);
            out.push(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            out.push(object);
            out.push(property);
            out.push(value);
        }

        InstructionValue::JsxExpression {
            tag, props, children, ..
        } => {
            if let JsxTag::Place(p) = tag {
                out.push(p);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => out.push(place),
                    JsxAttribute::Spread { argument } => out.push(argument),
                }
            }
            if let Some(ch) = children {
                out.extend(ch.iter());
            }
        }

        InstructionValue::JsxFragment { children, .. } => {
            out.extend(children.iter());
        }

        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectExpressionProperty::Property(p) => {
                        if let crate::hir::hir::ObjectPropertyKey::Computed(cp) = &p.key {
                            out.push(cp);
                        }
                        out.push(&p.place);
                    }
                    ObjectExpressionProperty::Spread(s) => out.push(&s.place),
                }
            }
        }

        InstructionValue::ObjectMethod { lowered_func, .. }
        | InstructionValue::FunctionExpression { lowered_func, .. } => {
            out.extend(lowered_func.func.context.iter());
        }

        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) => out.push(p),
                    ArrayElement::Spread(s) => out.push(&s.place),
                    ArrayElement::Hole => {}
                }
            }
        }

        InstructionValue::TemplateLiteral { subexprs, .. } => {
            out.extend(subexprs.iter());
        }

        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            out.push(tag);
        }

        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            out.push(iterator);
            out.push(collection);
        }

        InstructionValue::PrefixUpdate { value, .. }
        | InstructionValue::PostfixUpdate { value, .. } => {
            out.push(value);
        }

        InstructionValue::StartMemoize { .. } => {}
        InstructionValue::FinishMemoize { decl, .. } => {
            out.push(decl);
        }
    }
}

fn collect_call_args<'a>(args: &'a [CallArg], out: &mut Vec<&'a Place>) {
    for arg in args {
        match arg {
            CallArg::Place(p) => out.push(p),
            CallArg::Spread(s) => out.push(&s.place),
        }
    }
}

/// Collect all `Place` operands from a terminal.
pub fn each_terminal_operand(terminal: &Terminal) -> Vec<&Place> {
    let mut out = Vec::new();
    match terminal {
        Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
            out.push(value);
        }
        Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
            out.push(test);
        }
        Terminal::Switch { test, cases, .. } => {
            out.push(test);
            for case in cases {
                if let Some(t) = &case.test {
                    out.push(t);
                }
            }
        }
        // Block-level terminals don't carry Place operands directly
        Terminal::Goto { .. }
        | Terminal::While { .. }
        | Terminal::DoWhile { .. }
        | Terminal::For { .. }
        | Terminal::ForOf { .. }
        | Terminal::ForIn { .. }
        | Terminal::Logical { .. }
        | Terminal::Ternary { .. }
        | Terminal::Optional { .. }
        | Terminal::Label { .. }
        | Terminal::Sequence { .. }
        | Terminal::MaybeThrow { .. }
        | Terminal::Try { .. }
        | Terminal::ReactiveScope { .. }
        | Terminal::PrunedScope { .. }
        | Terminal::Unsupported { .. }
        | Terminal::Unreachable { .. } => {}
    }
    out
}

// ---------------------------------------------------------------------------
// Mutable operand collectors
// ---------------------------------------------------------------------------

/// Collect all input `Place` operands of an `InstructionValue` (mutable).
pub fn each_instruction_value_operand_mut(value: &mut InstructionValue) -> Vec<&mut Place> {
    let mut out = Vec::new();
    collect_instruction_value_operands_mut(value, &mut out);
    out
}

fn collect_instruction_value_operands_mut<'a>(
    value: &'a mut InstructionValue,
    out: &mut Vec<&'a mut Place>,
) {
    match value {
        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } => {
            out.push(place);
        }
        InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::InlineJs { .. }
        | InstructionValue::UnsupportedNode { .. }
        | InstructionValue::Debugger { .. } => {}

        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreGlobal { value, .. }
        | InstructionValue::TypeCastExpression { value, .. }
        | InstructionValue::UnaryExpression { value, .. }
        | InstructionValue::Await { value, .. }
        | InstructionValue::GetIterator { collection: value, .. }
        | InstructionValue::NextPropertyOf { value, .. } => {
            out.push(value);
        }

        InstructionValue::StoreContext { lvalue, value, .. } => {
            out.push(&mut lvalue.place);
            out.push(value);
        }

        InstructionValue::Destructure { value, .. } => {
            out.push(value);
        }

        InstructionValue::BinaryExpression { left, right, .. } => {
            out.push(left);
            out.push(right);
        }

        InstructionValue::CallExpression { callee, args, .. }
        | InstructionValue::NewExpression { callee, args, .. } => {
            out.push(callee);
            for arg in args.iter_mut() {
                match arg {
                    CallArg::Place(p) => out.push(p),
                    CallArg::Spread(s) => out.push(&mut s.place),
                }
            }
        }

        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            out.push(receiver);
            out.push(property);
            for arg in args.iter_mut() {
                match arg {
                    CallArg::Place(p) => out.push(p),
                    CallArg::Spread(s) => out.push(&mut s.place),
                }
            }
        }

        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::PropertyDelete { object, .. } => {
            out.push(object);
        }

        InstructionValue::PropertyStore { object, value, .. } => {
            out.push(object);
            out.push(value);
        }

        InstructionValue::ComputedLoad { object, property, .. } => {
            out.push(object);
            out.push(property);
        }
        InstructionValue::ComputedDelete { object, property, .. } => {
            out.push(object);
            out.push(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            out.push(object);
            out.push(property);
            out.push(value);
        }

        InstructionValue::JsxExpression {
            tag, props, children, ..
        } => {
            if let JsxTag::Place(p) = tag {
                out.push(p);
            }
            for attr in props.iter_mut() {
                match attr {
                    JsxAttribute::Attribute { place, .. } => out.push(place),
                    JsxAttribute::Spread { argument } => out.push(argument),
                }
            }
            if let Some(ch) = children {
                out.extend(ch.iter_mut());
            }
        }

        InstructionValue::JsxFragment { children, .. } => {
            out.extend(children.iter_mut());
        }

        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties.iter_mut() {
                match prop {
                    ObjectExpressionProperty::Property(p) => {
                        if let crate::hir::hir::ObjectPropertyKey::Computed(cp) = &mut p.key {
                            out.push(cp);
                        }
                        out.push(&mut p.place);
                    }
                    ObjectExpressionProperty::Spread(s) => out.push(&mut s.place),
                }
            }
        }

        InstructionValue::ObjectMethod { lowered_func, .. }
        | InstructionValue::FunctionExpression { lowered_func, .. } => {
            out.extend(lowered_func.func.context.iter_mut());
        }

        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements.iter_mut() {
                match elem {
                    ArrayElement::Place(p) => out.push(p),
                    ArrayElement::Spread(s) => out.push(&mut s.place),
                    ArrayElement::Hole => {}
                }
            }
        }

        InstructionValue::TemplateLiteral { subexprs, .. } => {
            out.extend(subexprs.iter_mut());
        }

        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            out.push(tag);
        }

        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            out.push(iterator);
            out.push(collection);
        }

        InstructionValue::PrefixUpdate { value, .. }
        | InstructionValue::PostfixUpdate { value, .. } => {
            out.push(value);
        }

        InstructionValue::StartMemoize { .. } => {}
        InstructionValue::FinishMemoize { decl, .. } => {
            out.push(decl);
        }
    }
}
