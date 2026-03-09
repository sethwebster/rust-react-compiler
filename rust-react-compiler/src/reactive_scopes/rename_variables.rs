use std::collections::HashMap;

use crate::hir::hir::{
    DeclarationId, HIRFunction, IdentifierName, Place, ReactiveBlock, ReactiveInstruction,
    ReactiveStatement, ReactiveTerminal, ReactiveValue,
};
use crate::hir::environment::Environment;

pub fn run(_hir: &mut HIRFunction) {}

/// Rename anonymous and `$t`/`$T` promoted temp variables to sequential `t0`, `t1`, `T0`, `T1`, ...
/// in tree definition order. Each unique DeclarationId gets the same name across all SSA copies.
///
/// This pass walks the `reactive_block` tree (built by `build_reactive_function`) and assigns
/// sequential names to unnamed temp identifiers, matching the TS compiler's `RenameVariables` pass.
///
/// Named user variables (IdentifierName::Named) are left unchanged. Only anonymous temps
/// (name: None) and promoted temps with `$t`/`$T` prefix are renamed.
///
/// NOTE: This pass is only active when `RC_TREE_CODEGEN` is enabled, because it mutates
/// the identifier names in the environment, which conflicts with the flat CFG codegen's
/// `ssa_value_to_name` and `scope_output_names` logic.
pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    // Clone the reactive_block to avoid borrow conflicts while mutating env.
    let reactive_block = match hir.reactive_block.clone() {
        Some(b) => b,
        None => return,
    };

    let mut seen: HashMap<DeclarationId, String> = HashMap::new();
    let mut counter: u32 = 0;
    let mut jsx_counter: u32 = 0;

    rename_block(&reactive_block, env, &mut seen, &mut counter, &mut jsx_counter);
}

fn rename_block(
    block: &ReactiveBlock,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    for stmt in block {
        rename_statement(stmt, env, seen, counter, jsx_counter);
    }
}

fn rename_statement(
    stmt: &ReactiveStatement,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            rename_instruction(instr, env, seen, counter, jsx_counter);
        }
        ReactiveStatement::Scope(scope_block) => {
            // Rename scope declarations first (they are the "definition" of these names).
            // Sort by id for deterministic ordering.
            let mut decl_ids: Vec<_> = scope_block.scope.declarations.keys().copied().collect();
            decl_ids.sort_by_key(|id| id.0);
            for id in decl_ids {
                rename_identifier_id(id, env, seen, counter, jsx_counter);
            }
            let mut reassign_ids = scope_block.scope.reassignments.clone();
            reassign_ids.sort_by_key(|id| id.0);
            for id in reassign_ids {
                rename_identifier_id(id, env, seen, counter, jsx_counter);
            }
            rename_block(&scope_block.instructions, env, seen, counter, jsx_counter);
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            rename_block(&scope_block.instructions, env, seen, counter, jsx_counter);
        }
        ReactiveStatement::Terminal(term_stmt) => {
            rename_terminal(&term_stmt.terminal, env, seen, counter, jsx_counter);
        }
    }
}

fn rename_instruction(
    instr: &ReactiveInstruction,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    // Process lvalue first (definition), then rvalue operands.
    if let Some(lvalue) = &instr.lvalue {
        rename_place(lvalue, env, seen, counter, jsx_counter);
    }
    rename_reactive_value(&instr.value, env, seen, counter, jsx_counter);
}

fn rename_reactive_value(
    value: &ReactiveValue,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    match value {
        ReactiveValue::Instruction(instr_val) => {
            // Walk operands of the instruction value.
            let operands = crate::hir::visitors::each_instruction_value_operand(instr_val);
            let ids: Vec<_> = operands.iter().map(|p| p.identifier).collect();
            for id in ids {
                let place = Place {
                    identifier: id,
                    effect: crate::hir::hir::Effect::Unknown,
                    reactive: false,
                    loc: crate::hir::hir::SourceLocation::Generated,
                };
                rename_place(&place, env, seen, counter, jsx_counter);
            }
        }
        ReactiveValue::Logical(logical) => {
            rename_reactive_value(&logical.left, env, seen, counter, jsx_counter);
            rename_reactive_value(&logical.right, env, seen, counter, jsx_counter);
        }
        ReactiveValue::Sequence(seq) => {
            for instr in &seq.instructions {
                rename_instruction(instr, env, seen, counter, jsx_counter);
            }
            rename_reactive_value(&seq.value, env, seen, counter, jsx_counter);
        }
        ReactiveValue::Ternary(ternary) => {
            rename_reactive_value(&ternary.test, env, seen, counter, jsx_counter);
            rename_reactive_value(&ternary.consequent, env, seen, counter, jsx_counter);
            rename_reactive_value(&ternary.alternate, env, seen, counter, jsx_counter);
        }
        ReactiveValue::OptionalCall(opt_call) => {
            rename_reactive_value(&opt_call.value, env, seen, counter, jsx_counter);
        }
    }
}

fn rename_terminal(
    terminal: &ReactiveTerminal,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    match terminal {
        ReactiveTerminal::Return { value, .. } => {
            rename_place(value, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::Throw { value, .. } => {
            rename_place(value, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::If { test, consequent, alternate, .. } => {
            rename_place(test, env, seen, counter, jsx_counter);
            rename_block(consequent, env, seen, counter, jsx_counter);
            if let Some(alt) = alternate {
                rename_block(alt, env, seen, counter, jsx_counter);
            }
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            rename_place(test, env, seen, counter, jsx_counter);
            for case in cases {
                if let Some(test_place) = &case.test {
                    rename_place(test_place, env, seen, counter, jsx_counter);
                }
                if let Some(block) = &case.block {
                    rename_block(block, env, seen, counter, jsx_counter);
                }
            }
        }
        ReactiveTerminal::While { test, loop_, .. } => {
            rename_reactive_value(test, env, seen, counter, jsx_counter);
            rename_block(loop_, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::DoWhile { loop_, test, .. } => {
            rename_block(loop_, env, seen, counter, jsx_counter);
            rename_reactive_value(test, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::For { init, test, update, loop_, .. } => {
            rename_reactive_value(init, env, seen, counter, jsx_counter);
            rename_reactive_value(test, env, seen, counter, jsx_counter);
            if let Some(upd) = update {
                rename_reactive_value(upd, env, seen, counter, jsx_counter);
            }
            rename_block(loop_, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::ForOf { iterable, loop_, .. } => {
            rename_reactive_value(iterable, env, seen, counter, jsx_counter);
            rename_block(loop_, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::ForIn { object, loop_, .. } => {
            rename_reactive_value(object, env, seen, counter, jsx_counter);
            rename_block(loop_, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::Label { block, .. } => {
            rename_block(block, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::Try { block, handler_binding, handler, .. } => {
            rename_block(block, env, seen, counter, jsx_counter);
            if let Some(binding) = handler_binding {
                rename_place(binding, env, seen, counter, jsx_counter);
            }
            rename_block(handler, env, seen, counter, jsx_counter);
        }
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
    }
}

/// Rename an identifier by IdentifierId (used for scope declarations/reassignments).
fn rename_identifier_id(
    id: crate::hir::hir::IdentifierId,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    let place = Place {
        identifier: id,
        effect: crate::hir::hir::Effect::Unknown,
        reactive: false,
        loc: crate::hir::hir::SourceLocation::Generated,
    };
    rename_place(&place, env, seen, counter, jsx_counter);
}

fn rename_place(
    place: &Place,
    env: &mut Environment,
    seen: &mut HashMap<DeclarationId, String>,
    counter: &mut u32,
    jsx_counter: &mut u32,
) {
    let ident = match env.get_identifier(place.identifier) {
        Some(i) => i.clone(),
        None => return,
    };
    // Determine if this identifier should be renamed:
    // - `name: None` (anonymous temp) → rename to t{counter}
    // - `name: Some(Promoted("$t..."))` or `name: Some(Promoted("$T..."))` → rename to t{counter}/T{counter}
    // Named user variables (Named("a"), Named("x"), etc.) are NOT renamed.
    // Only rename promoted temps that have the $t/$T prefix.
    // Anonymous temps (name: None) are left unnamed so hir_codegen inlines them.
    // Named user variables (Named("x")) are never touched.
    let needs_rename = match &ident.name {
        None => false,
        Some(n) => matches!(n, IdentifierName::Promoted(_))
            && (n.value().starts_with("$t") || n.value().starts_with("$T")),
    };
    if !needs_rename {
        return;
    }
    let is_jsx = ident.name.as_ref().map(|n| n.value().starts_with("$T")).unwrap_or(false);
    let decl_id = ident.declaration_id;
    let new_name = seen.entry(decl_id).or_insert_with(|| {
        if is_jsx {
            let n = format!("T{}", *jsx_counter);
            *jsx_counter += 1;
            n
        } else {
            let n = format!("t{}", *counter);
            *counter += 1;
            n
        }
    }).clone();
    if let Some(ident_mut) = env.get_identifier_mut(place.identifier) {
        ident_mut.name = Some(IdentifierName::Promoted(new_name));
    }
}
