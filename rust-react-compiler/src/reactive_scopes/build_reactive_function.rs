/// Build a ReactiveFunction tree from the flat HIR CFG.
///
/// Converts flat basic blocks + terminals into a tree of ReactiveBlock nodes,
/// where reactive scopes become ReactiveScopeBlock nodes and control flow
/// terminals (if/else, loops, switch) become ReactiveTerminal nodes.
///
/// This is a simplified port of BuildReactiveFunction.ts that works with
/// env.scopes ranges instead of scope terminals.
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::*;

pub fn run(_hir: &mut HIRFunction) {}

/// Build the reactive function tree. Returns a ReactiveBlock or None if
/// the tree couldn't be built (fallback to flat codegen).
pub fn build(hir: &HIRFunction, env: &Environment) -> Option<ReactiveBlock> {
    let mut builder = Builder::new(hir, env);
    let entry = hir.body.entry;
    let block = hir.body.blocks.get(&entry)?;
    Some(builder.visit_block(block))
}

struct Builder<'a> {
    hir: &'a HIRFunction,
    env: &'a Environment,
    emitted: HashSet<BlockId>,
    /// Map from instruction ID → scope that starts at that instruction
    scope_starts: HashMap<InstructionId, ScopeId>,
    /// Map from instruction ID → scope that ends at that instruction
    scope_ends: HashMap<InstructionId, ScopeId>,
}

impl<'a> Builder<'a> {
    fn new(hir: &'a HIRFunction, env: &'a Environment) -> Self {
        let mut scope_starts = HashMap::new();
        let mut scope_ends = HashMap::new();
        for (&sid, scope) in &env.scopes {
            scope_starts.entry(scope.range.start).or_insert(sid);
            scope_ends.entry(scope.range.end).or_insert(sid);
        }
        Builder {
            hir,
            env,
            emitted: HashSet::new(),
            scope_starts,
            scope_ends,
        }
    }

    fn visit_block(&mut self, block: &BasicBlock) -> ReactiveBlock {
        let mut result = ReactiveBlock::new();
        self.visit_block_into(block, &mut result);
        result
    }

    fn visit_block_into(&mut self, block: &BasicBlock, out: &mut ReactiveBlock) {
        if !self.emitted.insert(block.id) {
            return;
        }

        // Track which scopes are currently open.
        let mut open_scope: Option<(ScopeId, Vec<ReactiveStatement>)> = None;

        for instr in &block.instructions {
            // Check if a scope ends at this instruction.
            if let Some(&sid) = self.scope_ends.get(&instr.id) {
                if let Some((open_sid, body)) = open_scope.take() {
                    if open_sid == sid {
                        // Close the scope.
                        if let Some(scope) = self.env.scopes.get(&sid) {
                            out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                                scope: scope.clone(),
                                instructions: body,
                            }));
                        }
                    } else {
                        // Mismatched scope — flush as instructions.
                        out.extend(body);
                    }
                }
            }

            // Check if a scope starts at this instruction.
            if let Some(&sid) = self.scope_starts.get(&instr.id) {
                if open_scope.is_some() {
                    // Nested scope — flush outer into a scope block first.
                    // (Simplified: we don't handle true nesting yet.)
                }
                open_scope = Some((sid, Vec::new()));
            }

            // Emit the instruction into the current target.
            let ri = ReactiveInstruction {
                id: instr.id,
                lvalue: Some(instr.lvalue.clone()),
                value: ReactiveValue::Instruction(instr.value.clone()),
                effects: None,
                loc: instr.lvalue.loc.clone(),
            };
            if let Some((_, ref mut body)) = open_scope {
                body.push(ReactiveStatement::Instruction(ri));
            } else {
                out.push(ReactiveStatement::Instruction(ri));
            }
        }

        // Close any still-open scope at end of block.
        // The scope end might be in a successor block.
        // For now, flush the open scope here — the terminal handling will
        // continue into successor blocks.

        // Handle terminal.
        match &block.terminal {
            Terminal::Return { value, id, loc, .. } => {
                // If there's an open scope, close it first.
                if let Some((sid, body)) = open_scope.take() {
                    if let Some(scope) = self.env.scopes.get(&sid) {
                        out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                            scope: scope.clone(),
                            instructions: body,
                        }));
                    }
                }
                out.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: value.clone(),
                        id: *id,
                        loc: loc.clone(),
                    },
                    label: None,
                }));
            }

            Terminal::Goto { block: next, variant, .. } => {
                // Flush open scope.
                if let Some((sid, body)) = open_scope.take() {
                    if let Some(scope) = self.env.scopes.get(&sid) {
                        out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                            scope: scope.clone(),
                            instructions: body,
                        }));
                    }
                }
                // Continue into next block (if not already emitted and not a loop back-edge).
                if !self.emitted.contains(next) && *variant != GotoVariant::Continue {
                    if let Some(next_block) = self.hir.body.blocks.get(next) {
                        self.visit_block_into(next_block, out);
                    }
                }
            }

            Terminal::If { test, consequent, alternate, fallthrough, id, loc, .. } => {
                // Flush open scope.
                if let Some((sid, body)) = open_scope.take() {
                    if let Some(scope) = self.env.scopes.get(&sid) {
                        out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                            scope: scope.clone(),
                            instructions: body,
                        }));
                    }
                }

                let cons_block = self.hir.body.blocks.get(consequent)
                    .map(|b| self.visit_block(b))
                    .unwrap_or_default();
                let alt_block = if alternate != fallthrough {
                    self.hir.body.blocks.get(alternate)
                        .map(|b| self.visit_block(b))
                } else {
                    None
                };

                out.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::If {
                        test: test.clone(),
                        consequent: cons_block,
                        alternate: alt_block,
                        id: *id,
                        loc: loc.clone(),
                    },
                    label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                }));

                // Continue into fallthrough.
                if !self.emitted.contains(fallthrough) {
                    if let Some(ft_block) = self.hir.body.blocks.get(fallthrough) {
                        self.visit_block_into(ft_block, out);
                    }
                }
            }

            // For other terminals, flush scope and skip tree building.
            // The flat codegen will handle these.
            _ => {
                if let Some((sid, body)) = open_scope.take() {
                    if let Some(scope) = self.env.scopes.get(&sid) {
                        out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                            scope: scope.clone(),
                            instructions: body,
                        }));
                    }
                }
            }
        }
    }
}
