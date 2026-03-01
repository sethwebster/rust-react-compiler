/// Infer reactive scope variables.
///
/// Port of InferReactiveScopeVariables.ts / findDisjointMutableValues.
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{
    ArrayElement, DeclarationId, HIRFunction, IdentifierId, InstructionId,
    InstructionValue, MutableRange, ObjectPatternProperty, Pattern,
    ReactiveScope, ScopeId, SourceLocation,
};
use crate::hir::visitors::each_instruction_value_operand;
use crate::utils::disjoint_set::DisjointSet;

pub fn run(_hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    let canonical = {
        let mut ds = find_disjoint_mutable_values(hir, env);
        ds.canonicalize()
    };

    // Build ReactiveScope for each unique root.
    let mut scopes: HashMap<IdentifierId, ReactiveScope> = HashMap::new();

    for (&id, &root) in &canonical {
        let ident_range = env
            .get_identifier(id)
            .map(|i| i.mutable_range.clone())
            .unwrap_or_else(MutableRange::zero);

        let scope = scopes.entry(root).or_insert_with(|| {
            let scope_id = env.new_scope_id();
            ReactiveScope {
                id: scope_id,
                range: ident_range.clone(),
                dependencies: Vec::new(),
                declarations: HashMap::new(),
                reassignments: Vec::new(),
                merged_ranges: Vec::new(),
                early_returns: Vec::new(),
                loc: SourceLocation::Generated,
            }
        });

        if scope.range.start.0 == 0 {
            scope.range.start = ident_range.start;
        } else if ident_range.start.0 != 0 && ident_range.start < scope.range.start {
            scope.range.start = ident_range.start;
        }
        if ident_range.end > scope.range.end {
            scope.range.end = ident_range.end;
        }
    }

    let root_to_scope_id: HashMap<IdentifierId, ScopeId> =
        scopes.iter().map(|(&root, s)| (root, s.id)).collect();

    // Write scope IDs back to identifiers.
    for (&id, &root) in &canonical {
        if let Some(&scope_id) = root_to_scope_id.get(&root) {
            if let Some(ident) = env.get_identifier_mut(id) {
                ident.scope = Some(scope_id);
            }
        }
    }

    // Register scopes in env.
    for (_root, scope) in scopes {
        let sid = scope.id;
        env.scopes.insert(sid, scope);
    }
}

fn find_disjoint_mutable_values(
    hir: &HIRFunction,
    env: &Environment,
) -> DisjointSet<IdentifierId> {
    let mut set = DisjointSet::new();
    let mut declarations: HashMap<DeclarationId, IdentifierId> = HashMap::new();

    for (_, block) in &hir.body.blocks {
        for phi in &block.phis {
            let phi_range = env
                .get_identifier(phi.place.identifier)
                .map(|i| i.mutable_range.clone())
                .unwrap_or_else(MutableRange::zero);
            let block_start = block
                .instructions
                .first()
                .map(|i| i.id)
                .unwrap_or(block.terminal.id());
            let mutated_after = phi_range.start.0 + 1 != phi_range.end.0
                && phi_range.end > block_start;
            if mutated_after {
                let mut operands = vec![phi.place.identifier];
                let phi_decl = env
                    .get_identifier(phi.place.identifier)
                    .map(|i| i.declaration_id);
                if let Some(decl_id) = phi_decl {
                    if let Some(&did) = declarations.get(&decl_id) {
                        operands.push(did);
                    }
                }
                for op in phi.operands.values() {
                    operands.push(op.identifier);
                }
                set.union(&operands);
            }
        }

        for instr in &block.instructions {
            let lvalue_id = instr.lvalue.identifier;
            let lvalue_range = env
                .get_identifier(lvalue_id)
                .map(|i| i.mutable_range.clone())
                .unwrap_or_else(MutableRange::zero);
            let lvalue_mutable = lvalue_range.end.0 > lvalue_range.start.0 + 1;

            let mut operands: Vec<IdentifierId> = Vec::new();
            if lvalue_mutable || may_allocate(&instr.value) {
                operands.push(lvalue_id);
            }

            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                }
                InstructionValue::DeclareContext { lvalue, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                    let store_range = env.get_identifier(pid)
                        .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                    if store_range.end.0 > store_range.start.0 + 1 {
                        operands.push(pid);
                    }
                    let val_range = env.get_identifier(value.identifier)
                        .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                    if is_mutable_at(instr.id, &val_range) && val_range.start.0 > 0 {
                        operands.push(value.identifier);
                    }
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    let pid = lvalue.place.identifier;
                    let decl_id = env.get_identifier(pid).map(|i| i.declaration_id)
                        .unwrap_or(DeclarationId(pid.0));
                    declarations.entry(decl_id).or_insert(pid);
                    let store_range = env.get_identifier(pid)
                        .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                    if store_range.end.0 > store_range.start.0 + 1 {
                        operands.push(pid);
                    }
                    let val_range = env.get_identifier(value.identifier)
                        .map(|i| i.mutable_range.clone()).unwrap_or_else(MutableRange::zero);
                    if is_mutable_at(instr.id, &val_range) && val_range.start.0 > 0 {
                        operands.push(value.identifier);
                    }
                }
                InstructionValue::Destructure { lvalue, value, .. } => {
                    for place_id in pattern_places(&lvalue.pattern) {
                        let decl_id = env
                            .get_identifier(place_id)
                            .map(|i| i.declaration_id)
                            .unwrap_or(DeclarationId(place_id.0));
                        declarations.entry(decl_id).or_insert(place_id);
                        let pr = env
                            .get_identifier(place_id)
                            .map(|i| i.mutable_range.clone())
                            .unwrap_or_else(MutableRange::zero);
                        if pr.end.0 > pr.start.0 + 1 {
                            operands.push(place_id);
                        }
                    }
                    let val_range = env
                        .get_identifier(value.identifier)
                        .map(|i| i.mutable_range.clone())
                        .unwrap_or_else(MutableRange::zero);
                    if is_mutable_at(instr.id, &val_range) && val_range.start.0 > 0 {
                        operands.push(value.identifier);
                    }
                }
                _ => {
                    for op in each_instruction_value_operand(&instr.value) {
                        let op_range = env
                            .get_identifier(op.identifier)
                            .map(|i| i.mutable_range.clone())
                            .unwrap_or_else(MutableRange::zero);
                        if is_mutable_at(instr.id, &op_range) && op_range.start.0 > 0 {
                            operands.push(op.identifier);
                        }
                    }
                }
            }

            if !operands.is_empty() {
                let mut seen = HashSet::new();
                let dedup: Vec<_> =
                    operands.into_iter().filter(|&id| seen.insert(id)).collect();
                set.union(&dedup);
            }
        }
    }
    set
}

fn is_mutable_at(instr_id: InstructionId, range: &MutableRange) -> bool {
    instr_id >= range.start && instr_id < range.end
}

fn pattern_places(pattern: &Pattern) -> Vec<IdentifierId> {
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

fn may_allocate(value: &InstructionValue) -> bool {
    match value {
        InstructionValue::PostfixUpdate { .. }
        | InstructionValue::PrefixUpdate { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StoreLocal { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::TypeCastExpression { .. }
        | InstructionValue::LoadLocal { .. }
        | InstructionValue::LoadContext { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::JsxText { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::GetIterator { .. }
        | InstructionValue::IteratorNext { .. }
        | InstructionValue::NextPropertyOf { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. }
        | InstructionValue::UnaryExpression { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::StoreGlobal { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::UnsupportedNode { .. }
        | InstructionValue::PropertyStore { .. }
        | InstructionValue::ComputedStore { .. } => false,

        InstructionValue::ObjectExpression { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::TaggedTemplateExpression { .. }
        | InstructionValue::CallExpression { .. }
        | InstructionValue::MethodCall { .. } => true,

        InstructionValue::Destructure { lvalue, .. } => match &lvalue.pattern {
            Pattern::Array(ap) => ap.items.iter().any(|e| matches!(e, ArrayElement::Spread(_))),
            Pattern::Object(op) => op.properties.iter().any(|p| matches!(p, ObjectPatternProperty::Spread(_))),
        },
    }
}
