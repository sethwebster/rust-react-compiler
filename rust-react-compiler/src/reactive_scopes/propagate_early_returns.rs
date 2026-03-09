/// PropagateEarlyReturns: transform reactive scopes that contain early returns.
///
/// When a reactive scope has a `return` statement inside it, that return needs
/// special handling: we can't just cache the scope conditionally and then `return`
/// in the middle — the `return` would escape the cache block.
///
/// Instead, we:
/// 1. Create a sentinel temp variable `tN` (initialized to a sentinel symbol).
/// 2. Replace `return value;` inside the scope with `tN = value; break bbM;`
///    where `bbM` is a label around the entire scope body.
/// 3. Cache `tN` as a scope output.
/// 4. After the scope, emit `if (tN !== sentinel) { return tN; }`.
///
/// This mirrors the TS compiler's `PropagateEarlyReturns` pass, but runs on
/// the `hir.reactive_block` tree (built by `build_reactive_function`) rather
/// than on a separate `ReactiveFunction` type.
use crate::hir::environment::Environment;
use crate::hir::hir::{
    BlockId, Effect, HIRFunction, IdentifierName, InstructionKind,
    LValue, Place, ReactiveBlock, ReactiveScopeDeclaration, ReactiveStatement,
    ReactiveTerminal, ReactiveTerminalStatement, ReactiveTerminalTargetKind,
    ReactiveLabel, SourceLocation,
};
#[allow(unused_imports)]
use crate::hir::hir::IdentifierId;

pub fn run(hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    let Some(block) = hir.reactive_block.as_mut() else { return };
    transform_top_level_block(block, env, false);
}

/// Walk a block at a given nesting level, transforming outermost scopes with early returns.
/// `within_scope`: true if we're already inside a reactive scope (nested scopes are not
/// the outermost scope, so they don't get their own label — their returns are handled by
/// the enclosing scope's label).
fn transform_top_level_block(block: &mut ReactiveBlock, env: &mut Environment, within_scope: bool) {
    let len = block.len();
    for i in 0..len {
        let has_early_return = if let ReactiveStatement::Scope(sb) = &block[i] {
            !within_scope && block_has_early_return(&sb.instructions)
        } else {
            false
        };

        if has_early_return {
            let loc = if let ReactiveStatement::Scope(sb) = &block[i] {
                sb.scope.loc.clone()
            } else { SourceLocation::Generated };

            let sentinel_id = env.new_temporary(loc.clone());
            if let Some(ident) = env.get_identifier_mut(sentinel_id) {
                // Prefix with "$t_er" so rename_variables promotes it to a tN name.
                ident.name = Some(IdentifierName::Promoted(format!("$t_er{}", sentinel_id.0)));
            }
            let label_block_id = env.new_block_id();
            let instr_id = env.new_instruction_id();

            if let ReactiveStatement::Scope(scope_block) = &mut block[i] {
                let scope_id = scope_block.scope.id;

                // Add the sentinel to scope.declarations so it gets a cache slot.
                scope_block.scope.declarations.insert(sentinel_id, ReactiveScopeDeclaration {
                    identifier: sentinel_id,
                    scope: scope_id,
                });
                scope_block.scope.early_return_value = Some(sentinel_id);
                scope_block.scope.early_return_label_id = Some(label_block_id);

                // Mirror into env.scopes so codegen can see early_return_value/label_id.
                if let Some(env_scope) = env.scopes.get_mut(&scope_id) {
                    env_scope.early_return_value = Some(sentinel_id);
                    env_scope.early_return_label_id = Some(label_block_id);
                    if std::env::var("RC_DEBUG").is_ok() {
                        eprintln!("[propagate_er] scope {:?} sentinel={:?} label={:?}", scope_id.0, sentinel_id.0, label_block_id.0);
                    }
                } else if std::env::var("RC_DEBUG").is_ok() {
                    eprintln!("[propagate_er] scope {:?} NOT FOUND in env.scopes!", scope_id.0);
                }

                // Transform all Return terminals inside the scope body.
                transform_returns_in_block(
                    &mut scope_block.instructions,
                    sentinel_id,
                    label_block_id,
                    env,
                );

                // Wrap the scope body in a Label terminal so `break bbM;` has a target.
                let inner_instructions = std::mem::take(&mut scope_block.instructions);
                let label_terminal = ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Label {
                        block: inner_instructions,
                        id: instr_id,
                        loc: loc.clone(),
                    },
                    label: Some(ReactiveLabel {
                        id: label_block_id,
                        implicit: false,
                    }),
                });
                scope_block.instructions = vec![label_terminal];

                // Recurse into nested scopes (within_scope=true so they don't get own labels).
                let instrs = &mut scope_block.instructions;
                transform_top_level_block(instrs, env, true);
            }
        } else {
            match &mut block[i] {
                ReactiveStatement::Scope(scope_block) => {
                    transform_top_level_block(&mut scope_block.instructions, env, true);
                }
                ReactiveStatement::PrunedScope(sb) => {
                    transform_top_level_block(&mut sb.instructions, env, within_scope);
                }
                ReactiveStatement::Terminal(term) => {
                    transform_terminal_sub_blocks(&mut term.terminal, env, within_scope);
                }
                ReactiveStatement::Instruction(_) => {}
            }
        }
    }
}

fn transform_terminal_sub_blocks(
    terminal: &mut ReactiveTerminal,
    env: &mut Environment,
    within_scope: bool,
) {
    match terminal {
        ReactiveTerminal::If { consequent, alternate, .. } => {
            transform_top_level_block(consequent, env, within_scope);
            if let Some(alt) = alternate {
                transform_top_level_block(alt, env, within_scope);
            }
        }
        ReactiveTerminal::While { loop_, .. }
        | ReactiveTerminal::DoWhile { loop_, .. }
        | ReactiveTerminal::For { loop_, .. }
        | ReactiveTerminal::ForOf { loop_, .. }
        | ReactiveTerminal::ForIn { loop_, .. } => {
            transform_top_level_block(loop_, env, within_scope);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_top_level_block(block, env, within_scope);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_top_level_block(block, env, within_scope);
            transform_top_level_block(handler, env, within_scope);
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(b) = &mut case.block {
                    transform_top_level_block(b, env, within_scope);
                }
            }
        }
        _ => {}
    }
}

/// Check recursively whether a block contains any Return terminal.
/// Does NOT recurse into nested Scope blocks.
fn block_has_early_return(block: &ReactiveBlock) -> bool {
    for stmt in block {
        match stmt {
            ReactiveStatement::Terminal(term) => {
                if matches!(term.terminal, ReactiveTerminal::Return { .. }) {
                    return true;
                }
                if terminal_sub_blocks_have_early_return(&term.terminal) {
                    return true;
                }
            }
            ReactiveStatement::Scope(_) => {
                // Don't recurse into nested scopes.
            }
            ReactiveStatement::PrunedScope(sb) => {
                if block_has_early_return(&sb.instructions) {
                    return true;
                }
            }
            ReactiveStatement::Instruction(_) => {}
        }
    }
    false
}

fn terminal_sub_blocks_have_early_return(terminal: &ReactiveTerminal) -> bool {
    match terminal {
        ReactiveTerminal::If { consequent, alternate, .. } => {
            block_has_early_return(consequent)
                || alternate.as_ref().map_or(false, |a| block_has_early_return(a))
        }
        ReactiveTerminal::While { loop_, .. }
        | ReactiveTerminal::DoWhile { loop_, .. }
        | ReactiveTerminal::For { loop_, .. }
        | ReactiveTerminal::ForOf { loop_, .. }
        | ReactiveTerminal::ForIn { loop_, .. } => block_has_early_return(loop_),
        ReactiveTerminal::Label { block, .. } => block_has_early_return(block),
        ReactiveTerminal::Try { block, handler, .. } => {
            block_has_early_return(block) || block_has_early_return(handler)
        }
        ReactiveTerminal::Switch { cases, .. } => cases
            .iter()
            .any(|c| c.block.as_ref().map_or(false, |b| block_has_early_return(b))),
        _ => false,
    }
}

/// Replace all `Return(value)` in a block with StoreLocal(sentinel) + Break(label).
/// Does NOT recurse into nested Scope blocks.
fn transform_returns_in_block(
    block: &mut ReactiveBlock,
    sentinel_id: crate::hir::hir::IdentifierId,
    label_block_id: BlockId,
    env: &mut Environment,
) {
    let mut i = 0;
    while i < block.len() {
        let is_return = matches!(&block[i], ReactiveStatement::Terminal(t)
            if matches!(t.terminal, ReactiveTerminal::Return { .. }));

        if is_return {
            let (value, loc) = if let ReactiveStatement::Terminal(t) = &block[i] {
                if let ReactiveTerminal::Return { value, loc, .. } = &t.terminal {
                    (value.clone(), loc.clone())
                } else { unreachable!() }
            } else { unreachable!() };

            let sentinel_place = Place {
                identifier: sentinel_id,
                effect: Effect::ConditionallyMutate,
                reactive: true,
                loc: loc.clone(),
            };

            let store_instr = crate::hir::hir::ReactiveInstruction {
                id: env.new_instruction_id(),
                lvalue: None,
                value: crate::hir::hir::ReactiveValue::Instruction(
                    crate::hir::hir::InstructionValue::StoreLocal {
                        lvalue: LValue {
                            place: sentinel_place,
                            kind: InstructionKind::Reassign,
                        },
                        value,
                        type_annotation: None,
                        loc: loc.clone(),
                    }
                ),
                effects: None,
                loc: loc.clone(),
            };

            let break_term = ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Break {
                    target: label_block_id,
                    id: env.new_instruction_id(),
                    target_kind: ReactiveTerminalTargetKind::Labeled,
                    loc: loc.clone(),
                },
                label: None,
            };

            block[i] = ReactiveStatement::Instruction(store_instr);
            block.insert(i + 1, ReactiveStatement::Terminal(break_term));
            i += 2;
            continue;
        }

        // Recurse into sub-blocks of non-Return terminals.
        if let ReactiveStatement::Terminal(term) = &mut block[i] {
            if !matches!(term.terminal, ReactiveTerminal::Return { .. }) {
                transform_returns_in_terminal(&mut term.terminal, sentinel_id, label_block_id, env);
            }
        }
        if let ReactiveStatement::PrunedScope(sb) = &mut block[i] {
            transform_returns_in_block(&mut sb.instructions, sentinel_id, label_block_id, env);
        }
        // Don't recurse into Scope blocks.

        i += 1;
    }
}

fn transform_returns_in_terminal(
    terminal: &mut ReactiveTerminal,
    sentinel_id: crate::hir::hir::IdentifierId,
    label_block_id: BlockId,
    env: &mut Environment,
) {
    match terminal {
        ReactiveTerminal::If { consequent, alternate, .. } => {
            transform_returns_in_block(consequent, sentinel_id, label_block_id, env);
            if let Some(alt) = alternate {
                transform_returns_in_block(alt, sentinel_id, label_block_id, env);
            }
        }
        ReactiveTerminal::While { loop_, .. }
        | ReactiveTerminal::DoWhile { loop_, .. }
        | ReactiveTerminal::For { loop_, .. }
        | ReactiveTerminal::ForOf { loop_, .. }
        | ReactiveTerminal::ForIn { loop_, .. } => {
            transform_returns_in_block(loop_, sentinel_id, label_block_id, env);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_returns_in_block(block, sentinel_id, label_block_id, env);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_returns_in_block(block, sentinel_id, label_block_id, env);
            transform_returns_in_block(handler, sentinel_id, label_block_id, env);
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(b) = &mut case.block {
                    transform_returns_in_block(b, sentinel_id, label_block_id, env);
                }
            }
        }
        _ => {}
    }
}
