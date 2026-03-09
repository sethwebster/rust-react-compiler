/// Build a ReactiveFunction tree from the flat HIR CFG.
///
/// Converts flat basic blocks + terminals into a tree of ReactiveBlock nodes.
/// Reactive scopes become ReactiveScopeBlock nodes wrapping their instructions.
/// Control flow terminals (if/else, loops, switch) become ReactiveTerminal nodes
/// with nested ReactiveBlock children.
///
/// Works directly with `env.scopes` ranges (no scope terminals required).
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::*;

pub fn run(hir: &mut HIRFunction, env: &Environment) {
    hir.reactive_block = build(hir, env);
}

/// Build the reactive function tree. Returns None on failure (fallback to flat codegen).
pub fn build(hir: &HIRFunction, env: &Environment) -> Option<ReactiveBlock> {
    let mut ctx = Context::new(hir, env);
    let entry = hir.body.entry;
    let block = hir.body.blocks.get(&entry)?;
    let mut result = Vec::new();
    ctx.visit_block(block, &mut result);
    Some(result)
}

struct Context<'a> {
    hir: &'a HIRFunction,
    env: &'a Environment,
    emitted: HashSet<BlockId>,
    /// Blocks that are "scheduled" as fallthroughs — should not be visited
    /// until the current terminal's children are done.
    scheduled: HashSet<BlockId>,
    /// Blocks that require an explicit `break` to reach (loop/label fallthroughs).
    /// A `goto fallthrough (Break)` to a break_target emits a JS `break;`.
    /// A `goto join (Break)` to a non-break-target scheduled block is a natural
    /// if/else fallthrough and is suppressed.
    break_targets: HashSet<BlockId>,
    /// Scope lookup: instruction ID → scope that starts/ends here.
    scope_starts: HashMap<InstructionId, ScopeId>,
    scope_ends: HashMap<InstructionId, ScopeId>,
}

impl<'a> Context<'a> {
    fn new(hir: &'a HIRFunction, env: &'a Environment) -> Self {
        let mut scope_starts = HashMap::new();
        let mut scope_ends = HashMap::new();
        for (&sid, scope) in &env.scopes {
            scope_starts.entry(scope.range.start).or_insert(sid);
            scope_ends.entry(scope.range.end).or_insert(sid);
        }
        Context {
            hir,
            env,
            emitted: HashSet::new(),
            scheduled: HashSet::new(),
            break_targets: HashSet::new(),
            scope_starts,
            scope_ends,
        }
    }

    fn traverse_block(&mut self, block: &BasicBlock) -> ReactiveBlock {
        let mut result = Vec::new();
        self.visit_block(block, &mut result);
        result
    }

    /// Thin wrapper: creates a fresh scope_body, calls visit_block_inner, then closes any
    /// scope that was still open at the end.
    fn visit_block(&mut self, block: &BasicBlock, out: &mut ReactiveBlock) {
        let mut scope_body = None;
        self.visit_block_inner(block, out, &mut scope_body);
        self.close_scope(&mut scope_body, out);
    }

    /// Push a statement into scope_body if a scope is currently open, otherwise into out.
    fn push_stmt_or_scope(
        scope_body: &mut Option<(ScopeId, Vec<ReactiveStatement>)>,
        out: &mut Vec<ReactiveStatement>,
        stmt: ReactiveStatement,
    ) {
        if let Some((_, ref mut body)) = scope_body {
            body.push(stmt);
        } else {
            out.push(stmt);
        }
    }

    fn visit_block_inner(
        &mut self,
        block: &BasicBlock,
        out: &mut ReactiveBlock,
        scope_body: &mut Option<(ScopeId, Vec<ReactiveStatement>)>,
    ) {
        if !self.emitted.insert(block.id) {
            return;
        }

        // Emit instructions, wrapping scope ranges in ReactiveScopeBlock.
        for instr in &block.instructions {
            // Check scope end BEFORE scope start (a scope can end and another
            // start at the same instruction ID).
            if let Some(&sid) = self.scope_ends.get(&instr.id) {
                if let Some((open_sid, body)) = scope_body.take() {
                    if open_sid == sid {
                        if let Some(scope) = self.env.scopes.get(&sid) {
                            out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                                scope: scope.clone(),
                                instructions: body,
                            }));
                        }
                    } else {
                        // Mismatched — flush as raw instructions.
                        out.extend(body);
                    }
                }
            }

            // Check scope start.
            if let Some(&sid) = self.scope_starts.get(&instr.id) {
                // If there's already an open scope, close it first.
                if let Some((old_sid, body)) = scope_body.take() {
                    if let Some(scope) = self.env.scopes.get(&old_sid) {
                        out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                            scope: scope.clone(),
                            instructions: body,
                        }));
                    }
                }
                *scope_body = Some((sid, Vec::new()));
            }

            // Skip iteration machinery instructions — they are part of ForOf/ForIn
            // terminal construction and should not appear as standalone statements.
            if matches!(&instr.value,
                InstructionValue::GetIterator { .. } |
                InstructionValue::IteratorNext { .. } |
                InstructionValue::NextPropertyOf { .. }
            ) {
                continue;
            }

            let stmt = ReactiveStatement::Instruction(ReactiveInstruction {
                id: instr.id,
                lvalue: Some(instr.lvalue.clone()),
                value: ReactiveValue::Instruction(instr.value.clone()),
                effects: None,
                loc: instr.lvalue.loc.clone(),
            });

            if let Some((_, ref mut body)) = scope_body {
                body.push(stmt);
            } else {
                out.push(stmt);
            }
        }

        // Handle terminal.
        // For terminals that have fallthroughs (non-terminating control flow), we do NOT
        // close the scope first — instead we thread scope_body through to the fallthrough block.
        // For Return/Throw/Unreachable we DO close the scope (control flow ends here).
        // For ReactiveScope/PrunedScope we also close first (they represent new scope markers).
        let terminal_target = match &block.terminal {
            Terminal::Return { .. } | Terminal::Throw { .. } | Terminal::Unreachable { .. } => {
                self.close_scope(scope_body, out);
                self.emit_terminal(&block.terminal, out);
                None
            }

            Terminal::Goto { block: next, variant, .. } => {
                match variant {
                    GotoVariant::Break => {
                        if !self.break_targets.contains(next) {
                            // Natural fallthrough to an if/else merge block (or constant-folded branch).
                            // No explicit break needed — continue to the target block.
                            Some(*next)
                        } else {
                            // Break out of a loop or labeled block.
                            Self::push_stmt_or_scope(
                                scope_body,
                                out,
                                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                                    terminal: ReactiveTerminal::Break {
                                        target: *next,
                                        id: block.terminal.id(),
                                        target_kind: ReactiveTerminalTargetKind::Implicit,
                                        loc: block.terminal.loc().clone(),
                                    },
                                    label: None,
                                }),
                            );
                            None
                        }
                    }
                    GotoVariant::Continue => {
                        // Always emit explicit continue; in tree codegen.
                        // End-of-loop-body continues (redundant but harmless) get normalized away.
                        // Explicit continues inside nested blocks must be emitted.
                        Self::push_stmt_or_scope(
                            scope_body,
                            out,
                            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                                terminal: ReactiveTerminal::Continue {
                                    target: *next,
                                    id: block.terminal.id(),
                                    target_kind: ReactiveTerminalTargetKind::Implicit,
                                    loc: block.terminal.loc().clone(),
                                },
                                label: None,
                            }),
                        );
                        None
                    }
                    _ => Some(*next),
                }
            }

            Terminal::If { test, consequent, alternate, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);

                let cons = self.hir.body.blocks.get(consequent)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();
                let alt = if alternate != fallthrough {
                    self.hir.body.blocks.get(alternate)
                        .map(|b| self.traverse_block(b))
                } else {
                    None
                };

                self.scheduled.remove(fallthrough);
                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::If {
                            test: test.clone(),
                            consequent: cons,
                            alternate: alt,
                            id: *id,
                            loc: loc.clone(),
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::While { test, loop_, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);
                self.scheduled.insert(*test);
                self.scheduled.insert(*loop_);
                self.break_targets.insert(*fallthrough);

                // Test is a value block — extract the test expression.
                let test_val = self.extract_test_value(*test);

                let body = self.hir.body.blocks.get(loop_)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                self.scheduled.remove(test);
                self.scheduled.remove(loop_);
                self.break_targets.remove(fallthrough);

                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::While {
                            test: Box::new(test_val),
                            loop_: body,
                            id: *id,
                            loc: loc.clone(),
                            test_bid: *test,
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::DoWhile { loop_, test, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);
                self.scheduled.insert(*test);
                self.break_targets.insert(*fallthrough);

                let body = self.hir.body.blocks.get(loop_)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();
                let test_val = self.extract_test_value(*test);

                self.scheduled.remove(fallthrough);
                self.scheduled.remove(test);
                self.break_targets.remove(fallthrough);

                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::DoWhile {
                            loop_: body,
                            test: Box::new(test_val),
                            id: *id,
                            loc: loc.clone(),
                            test_bid: *test,
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::For { init, test, update, loop_, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);
                self.scheduled.insert(*test);
                if let Some(u) = update { self.scheduled.insert(*u); }
                self.scheduled.insert(*loop_);
                self.break_targets.insert(*fallthrough);

                let init_val = self.extract_init_value(*init);
                let test_val = self.extract_test_value(*test);
                let update_val = update.map(|u| {
                    Box::new(self.extract_block_value(u))
                });

                let body = self.hir.body.blocks.get(loop_)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                self.scheduled.remove(test);
                if let Some(u) = update { self.scheduled.remove(&u); }
                self.scheduled.remove(loop_);
                self.break_targets.remove(fallthrough);

                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::For {
                            init: Box::new(init_val),
                            test: Box::new(test_val),
                            update: update_val,
                            loop_: body,
                            id: *id,
                            loc: loc.clone(),
                            init_bid: *init,
                            test_bid: *test,
                            update_bid: *update,
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::ForOf { init, test, loop_, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);
                self.scheduled.insert(*test);
                self.scheduled.insert(*init);
                self.scheduled.insert(*loop_);
                self.break_targets.insert(*fallthrough);

                // Extract the iterable from GetIterator in init block.
                let iterable_val = if let Some(block) = self.hir.body.blocks.get(init) {
                    self.emitted.insert(*init);
                    block.instructions.iter().find_map(|instr| {
                        if let InstructionValue::GetIterator { collection, .. } = &instr.value {
                            Some(ReactiveValue::Instruction(InstructionValue::LoadLocal {
                                place: collection.clone(),
                                loc: instr.lvalue.loc.clone(),
                            }))
                        } else {
                            None
                        }
                    }).unwrap_or(ReactiveValue::Instruction(InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    }))
                } else {
                    ReactiveValue::Instruction(InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    })
                };

                // Extract the loop variable name from first StoreLocal in loop block.
                let loop_var = if let Some(block) = self.hir.body.blocks.get(loop_) {
                    block.instructions.iter().find_map(|instr| {
                        if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                            self.env.get_identifier(lvalue.place.identifier)
                                .and_then(|i| i.name.as_ref())
                                .map(|n| n.value().to_string())
                        } else {
                            None
                        }
                    }).unwrap_or_else(|| "_item".to_string())
                } else {
                    "_item".to_string()
                };

                let body = self.hir.body.blocks.get(loop_)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                self.scheduled.remove(test);
                self.scheduled.remove(init);
                self.scheduled.remove(loop_);
                self.break_targets.remove(fallthrough);

                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::ForOf {
                            loop_var,
                            iterable: Box::new(iterable_val),
                            loop_: body,
                            id: *id,
                            loc: loc.clone(),
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::ForIn { init, loop_, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);
                self.scheduled.insert(*init);
                self.scheduled.insert(*loop_);
                self.break_targets.insert(*fallthrough);

                // Extract the object being iterated (from NextPropertyOf in init block).
                let object_val = if let Some(block) = self.hir.body.blocks.get(init) {
                    self.emitted.insert(*init);
                    block.instructions.iter().find_map(|instr| {
                        if let InstructionValue::NextPropertyOf { value, .. } = &instr.value {
                            Some(ReactiveValue::Instruction(InstructionValue::LoadLocal {
                                place: value.clone(),
                                loc: instr.lvalue.loc.clone(),
                            }))
                        } else {
                            None
                        }
                    }).unwrap_or(ReactiveValue::Instruction(InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    }))
                } else {
                    ReactiveValue::Instruction(InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    })
                };

                // Extract the loop variable name (from first StoreLocal in loop block).
                let loop_var = if let Some(block) = self.hir.body.blocks.get(loop_) {
                    block.instructions.iter().find_map(|instr| {
                        if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                            self.env.get_identifier(lvalue.place.identifier)
                                .and_then(|i| i.name.as_ref())
                                .map(|n| n.value().to_string())
                        } else {
                            None
                        }
                    }).unwrap_or_else(|| "_key".to_string())
                } else {
                    "_key".to_string()
                };

                let body = self.hir.body.blocks.get(loop_)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                self.scheduled.remove(init);
                self.scheduled.remove(loop_);
                self.break_targets.remove(fallthrough);

                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::ForIn {
                            loop_var,
                            object: Box::new(object_val),
                            loop_: body,
                            id: *id,
                            loc: loc.clone(),
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::Switch { test, cases, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);
                self.break_targets.insert(*fallthrough);

                let mut reactive_cases = Vec::new();
                for case in cases.iter().rev() {
                    if self.emitted.contains(&case.block) || self.scheduled.contains(&case.block) {
                        continue;
                    }
                    let case_body = self.hir.body.blocks.get(&case.block)
                        .map(|b| self.traverse_block(b)).unwrap_or_default();
                    reactive_cases.push(ReactiveSwitchCase {
                        test: case.test.clone(),
                        block: Some(case_body),
                    });
                    self.scheduled.insert(case.block);
                }
                reactive_cases.reverse();
                // Unschedule case blocks.
                for case in cases {
                    self.scheduled.remove(&case.block);
                }

                self.scheduled.remove(fallthrough);
                self.break_targets.remove(fallthrough);
                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::Switch {
                            test: test.clone(),
                            cases: reactive_cases,
                            id: *id,
                            loc: loc.clone(),
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::Try { block: try_block, handler, handler_binding, fallthrough, id, loc, .. } => {
                self.scheduled.insert(*fallthrough);

                let try_body = self.hir.body.blocks.get(try_block)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();
                let catch_body = self.hir.body.blocks.get(handler)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                Self::push_stmt_or_scope(
                    scope_body,
                    out,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::Try {
                            block: try_body,
                            handler: catch_body,
                            handler_binding: handler_binding.clone(),
                            id: *id,
                            loc: loc.clone(),
                        },
                        label: Some(ReactiveLabel { id: *fallthrough, implicit: false }),
                    }),
                );
                Some(*fallthrough)
            }

            Terminal::ReactiveScope { scope, block: scope_block, fallthrough, .. } => {
                // Close any currently open scope first — ReactiveScope is a new scope marker.
                self.close_scope(scope_body, out);
                self.scheduled.insert(*fallthrough);

                let body = self.hir.body.blocks.get(scope_block)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: scope.clone(),
                    instructions: body,
                }));
                Some(*fallthrough)
            }

            Terminal::PrunedScope { scope, block: scope_block, fallthrough, .. } => {
                // Close any currently open scope first — PrunedScope is a new scope marker.
                self.close_scope(scope_body, out);
                self.scheduled.insert(*fallthrough);

                let body = self.hir.body.blocks.get(scope_block)
                    .map(|b| self.traverse_block(b)).unwrap_or_default();

                self.scheduled.remove(fallthrough);
                out.push(ReactiveStatement::PrunedScope(PrunedReactiveScopeBlock {
                    scope: scope.clone(),
                    instructions: body,
                }));
                Some(*fallthrough)
            }

            // Terminals handled as inner blocks or lowered away.
            _ => {
                block.terminal.fallthrough()
            }
        };

        // Continue into the next block, threading scope_body through the fallthrough.
        if let Some(next_bid) = terminal_target {
            if !self.emitted.contains(&next_bid) && !self.scheduled.contains(&next_bid) {
                if let Some(next_block) = self.hir.body.blocks.get(&next_bid) {
                    self.visit_block_inner(next_block, out, scope_body);
                }
            }
        }
    }

    fn close_scope(&self, scope_body: &mut Option<(ScopeId, Vec<ReactiveStatement>)>, out: &mut ReactiveBlock) {
        if let Some((sid, body)) = scope_body.take() {
            if let Some(scope) = self.env.scopes.get(&sid) {
                out.push(ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: scope.clone(),
                    instructions: body,
                }));
            } else {
                out.extend(body);
            }
        }
    }

    fn emit_terminal(&self, terminal: &Terminal, out: &mut ReactiveBlock) {
        match terminal {
            Terminal::Return { value, id, loc, .. } => {
                out.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: value.clone(),
                        id: *id,
                        loc: loc.clone(),
                    },
                    label: None,
                }));
            }
            Terminal::Throw { value, id, loc, .. } => {
                out.push(ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Throw {
                        value: value.clone(),
                        id: *id,
                        loc: loc.clone(),
                    },
                    label: None,
                }));
            }
            _ => {}
        }
    }

    /// Extract a test value from a test/branch block.
    fn extract_test_value(&mut self, test_bid: BlockId) -> ReactiveValue {
        self.emitted.insert(test_bid);
        if let Some(block) = self.hir.body.blocks.get(&test_bid) {
            if let Terminal::Branch { test, .. } = &block.terminal {
                // The test block's last instruction produces the test value.
                // Wrap all instructions + the test place as a Sequence.
                if block.instructions.is_empty() {
                    return ReactiveValue::Instruction(InstructionValue::LoadLocal {
                        place: test.clone(),
                        loc: test.loc.clone(),
                    });
                }
                let instrs: Vec<ReactiveInstruction> = block.instructions.iter().map(|i| {
                    ReactiveInstruction {
                        id: i.id,
                        lvalue: Some(i.lvalue.clone()),
                        value: ReactiveValue::Instruction(i.value.clone()),
                        effects: None,
                        loc: i.lvalue.loc.clone(),
                    }
                }).collect();
                // For all non-empty cases: resolve via the last instruction's lvalue place,
                // so reactive_value_expr can resolve it through inlined_exprs.
                // (e.g., $t_cond = BinaryExpression(x, 10) → inlined_exprs[$t_cond] = "x < 10")
                // Prefer Branch.test place as it is the canonical resolved reference.
                return ReactiveValue::Instruction(InstructionValue::LoadLocal {
                    place: test.clone(),
                    loc: test.loc.clone(),
                });
            }
        }
        ReactiveValue::Instruction(InstructionValue::Primitive {
            value: PrimitiveValue::Boolean(true),
            loc: SourceLocation::Generated,
        })
    }

    /// Extract the init value from a for-loop init block (which is the current block).
    fn extract_init_value(&mut self, init_bid: BlockId) -> ReactiveValue {
        // The init block is the block containing the For terminal.
        // Its instructions are the init expressions.
        self.extract_block_value(init_bid)
    }

    /// Extract all instructions from a block as a ReactiveValue (Sequence or single).
    fn extract_block_value(&mut self, bid: BlockId) -> ReactiveValue {
        self.emitted.insert(bid);
        if let Some(block) = self.hir.body.blocks.get(&bid) {
            let instrs: Vec<ReactiveInstruction> = block.instructions.iter().map(|i| {
                ReactiveInstruction {
                    id: i.id,
                    lvalue: Some(i.lvalue.clone()),
                    value: ReactiveValue::Instruction(i.value.clone()),
                    effects: None,
                    loc: i.lvalue.loc.clone(),
                }
            }).collect();
            if instrs.is_empty() {
                return ReactiveValue::Instruction(InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc: SourceLocation::Generated,
                });
            }
            if instrs.len() == 1 {
                return instrs.into_iter().next().unwrap().value;
            }
            let last = instrs.last().unwrap().clone();
            return ReactiveValue::Sequence(ReactiveSequenceValue {
                instructions: instrs[..instrs.len()-1].to_vec(),
                id: last.id,
                value: Box::new(last.value),
                loc: SourceLocation::Generated,
            });
        }
        ReactiveValue::Instruction(InstructionValue::Primitive {
            value: PrimitiveValue::Undefined,
            loc: SourceLocation::Generated,
        })
    }
}
