#![allow(unused_imports, unused_variables, dead_code)]
use oxc_ast::ast::*;
use oxc_index::Idx;
use oxc_semantic::Semantic;
use crate::hir::hir::*;
use crate::error::{CompilerError, Result};
use super::{LoweringContext, Scope};

// ---------------------------------------------------------------------------
// lower_while
// ---------------------------------------------------------------------------

/// CFG shape:
///   current → WhileTerminal{test, loop_, fall}
///   test:    eval test → Branch{loop_, fall, fall}
///   loop_:   body → Goto(test, Continue)
///   fall:    (continuation)
pub fn lower_while<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &WhileStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    // Reserve successor blocks.
    let test_id = ctx.reserve(BlockKind::Loop);
    let loop_id = ctx.reserve(BlockKind::Loop);
    let fall_id = ctx.reserve(BlockKind::Block);

    // Seal current block with the While terminal.
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::While {
        test: test_id,
        loop_: loop_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- test block ----
    ctx.switch_to(test_id, BlockKind::Loop);
    let test_place = lower_expr(&stmt.test, ctx)?;
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Branch {
        test: test_place,
        consequent: loop_id,
        alternate: fall_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- loop body block ----
    ctx.switch_to(loop_id, BlockKind::Loop);
    ctx.push_scope(Scope::Loop {
        label: None,
        continue_block: test_id,
        break_block: fall_id,
    });
    lower_stmt(&stmt.body, ctx)?;
    ctx.pop_scope();

    // Back-edge to test (emitted only if the body didn't already terminate).
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: test_id,
        variant: GotoVariant::Continue,
        id,
        loc: loc.clone(),
    });

    // ---- fallthrough block ----
    ctx.switch_to(fall_id, BlockKind::Block);
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_do_while
// ---------------------------------------------------------------------------

/// CFG shape:
///   current → DoWhileTerminal{loop_, test, fall}
///   loop_:   body → Goto(test, Continue)
///   test:    eval test → Branch{loop_, fall, fall}
///   fall:    (continuation)
pub fn lower_do_while<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &DoWhileStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    let loop_id = ctx.reserve(BlockKind::Loop);
    let test_id = ctx.reserve(BlockKind::Loop);
    let fall_id = ctx.reserve(BlockKind::Block);

    // Seal current block with the DoWhile terminal.
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::DoWhile {
        loop_: loop_id,
        test: test_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- loop body block ----
    ctx.switch_to(loop_id, BlockKind::Loop);
    ctx.push_scope(Scope::Loop {
        label: None,
        continue_block: test_id,
        break_block: fall_id,
    });
    lower_stmt(&stmt.body, ctx)?;
    ctx.pop_scope();

    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: test_id,
        variant: GotoVariant::Continue,
        id,
        loc: loc.clone(),
    });

    // ---- test block ----
    ctx.switch_to(test_id, BlockKind::Loop);
    let test_place = lower_expr(&stmt.test, ctx)?;
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Branch {
        test: test_place,
        consequent: loop_id,
        alternate: fall_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- fallthrough block ----
    ctx.switch_to(fall_id, BlockKind::Block);
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_for
// ---------------------------------------------------------------------------

/// CFG shape:
///   current[init] → ForTerminal{init=current, test, update?, loop_, fall}
///   test:    lower test → Branch{loop_, fall, fall}   (or Goto(loop_) if no test)
///   loop_:   body → Goto(update or test, Continue)
///   update:  lower update → Goto(test, Continue)      (only if stmt.update is Some)
///   fall:    (continuation)
pub fn lower_for<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &ForStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    // Lower the init in the current block (before the ForTerminal).
    if let Some(init) = &stmt.init {
        match init {
            ForStatementInit::VariableDeclaration(decl) => {
                lower_var_decl_for_init(ctx, decl, lower_expr)?;
            }
            // UsingDeclaration is not in oxc 0.69 ForStatementInit; expression fallthrough handles it.
            init_expr => {
                // ForStatementInit also covers expression forms via the
                // Expression variant — lower it for side effects.
                if let Some(expr) = init_expr.as_expression() {
                    lower_expr(expr, ctx)?;
                }
            }
        }
    }

    let init_block_id = ctx.current_block_id();

    // Reserve remaining blocks.
    let test_id = ctx.reserve(BlockKind::Loop);
    let update_id = if stmt.update.is_some() {
        Some(ctx.reserve(BlockKind::Loop))
    } else {
        None
    };
    let loop_id = ctx.reserve(BlockKind::Loop);
    let fall_id = ctx.reserve(BlockKind::Block);

    // Determine continue target: update_id if present, else test_id.
    let continue_target = update_id.unwrap_or(test_id);

    // Seal current (init) block with the For terminal.
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::For {
        init: init_block_id,
        test: test_id,
        update: update_id,
        loop_: loop_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- test block ----
    ctx.switch_to(test_id, BlockKind::Loop);
    if let Some(test_expr) = &stmt.test {
        let test_place = lower_expr(test_expr, ctx)?;
        let id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Branch {
            test: test_place,
            consequent: loop_id,
            alternate: fall_id,
            fallthrough: fall_id,
            id,
            loc: loc.clone(),
        });
    } else {
        // No test → unconditional jump into loop body.
        let id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Goto {
            block: loop_id,
            variant: GotoVariant::Continue,
            id,
            loc: loc.clone(),
        });
    }

    // ---- loop body block ----
    ctx.switch_to(loop_id, BlockKind::Loop);
    ctx.push_scope(Scope::Loop {
        label: None,
        continue_block: continue_target,
        break_block: fall_id,
    });
    lower_stmt(&stmt.body, ctx)?;
    ctx.pop_scope();

    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: continue_target,
        variant: GotoVariant::Continue,
        id,
        loc: loc.clone(),
    });

    // ---- update block (optional) ----
    if let (Some(uid), Some(update_expr)) = (update_id, &stmt.update) {
        ctx.switch_to(uid, BlockKind::Loop);
        lower_expr(update_expr, ctx)?;
        let id = ctx.next_instruction_id();
        ctx.terminate(Terminal::Goto {
            block: test_id,
            variant: GotoVariant::Continue,
            id,
            loc: loc.clone(),
        });
    }

    // ---- fallthrough block ----
    ctx.switch_to(fall_id, BlockKind::Block);
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_for_of
// ---------------------------------------------------------------------------

/// CFG shape:
///   current[GetIterator(right)] → ForOfTerminal{init=current, test, loop_, fall}
///   test:    IteratorNext → Branch{loop_, fall, fall}
///   loop_:   bind left pattern → body → Goto(test, Continue)
///   fall:    (continuation)
pub fn lower_for_of<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &ForOfStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    // `for await` loops are not yet supported.
    if stmt.r#await {
        return Err(crate::error::CompilerError::todo(
            "for-await-of loops are not yet supported",
        ));
    }

    // Lower the iterable (right-hand side).
    let collection_place = lower_expr(&stmt.right, ctx)?;

    // Emit GetIterator in the current (init) block.
    let iterator_place = ctx.push(
        InstructionValue::GetIterator {
            collection: collection_place.clone(),
            loc: loc.clone(),
        },
        loc.clone(),
    );

    let init_block_id = ctx.current_block_id();

    let test_id = ctx.reserve(BlockKind::Loop);
    let loop_id = ctx.reserve(BlockKind::Loop);
    let fall_id = ctx.reserve(BlockKind::Block);

    // Seal the init block.
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::ForOf {
        init: init_block_id,
        test: test_id,
        loop_: loop_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- test block ----
    ctx.switch_to(test_id, BlockKind::Loop);
    let next_place = ctx.push(
        InstructionValue::IteratorNext {
            iterator: iterator_place,
            collection: collection_place,
            loc: loc.clone(),
        },
        loc.clone(),
    );
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Branch {
        test: next_place.clone(),
        consequent: loop_id,
        alternate: fall_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- loop body block ----
    ctx.switch_to(loop_id, BlockKind::Loop);
    // Bind the loop variable(s) to the current iterator value.
    lower_for_of_left(ctx, semantic, &stmt.left, next_place, &loc, lower_expr)?;

    ctx.push_scope(Scope::Loop {
        label: None,
        continue_block: test_id,
        break_block: fall_id,
    });
    lower_stmt(&stmt.body, ctx)?;
    ctx.pop_scope();

    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: test_id,
        variant: GotoVariant::Continue,
        id,
        loc: loc.clone(),
    });

    // ---- fallthrough block ----
    ctx.switch_to(fall_id, BlockKind::Block);
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_for_in
// ---------------------------------------------------------------------------

/// CFG shape:
///   current[NextPropertyOf(right)] → ForInTerminal{init=current, loop_, fall}
///   loop_:   bind left pattern → body → Goto(loop_, Continue)
///   fall:    (continuation)
///
/// Note: Terminal::ForIn has no `test` field; the "done" check is
/// represented by the terminal itself branching to loop_ vs fall.
pub fn lower_for_in<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &ForInStatement<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);

    // Lower the object (right-hand side).
    let obj_place = lower_expr(&stmt.right, ctx)?;

    // Emit NextPropertyOf in the init block to get the first key.
    let next_place = ctx.push(
        InstructionValue::NextPropertyOf {
            value: obj_place,
            loc: loc.clone(),
        },
        loc.clone(),
    );

    let init_block_id = ctx.current_block_id();

    let loop_id = ctx.reserve(BlockKind::Loop);
    let fall_id = ctx.reserve(BlockKind::Block);

    // Seal the init block.
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::ForIn {
        init: init_block_id,
        loop_: loop_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    // ---- loop body block ----
    ctx.switch_to(loop_id, BlockKind::Loop);
    // Bind the loop variable to the current property key.
    lower_for_in_left(ctx, semantic, &stmt.left, next_place, &loc, lower_expr)?;

    ctx.push_scope(Scope::Loop {
        label: None,
        continue_block: loop_id,
        break_block: fall_id,
    });
    lower_stmt(&stmt.body, ctx)?;
    ctx.pop_scope();

    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: loop_id,
        variant: GotoVariant::Continue,
        id,
        loc: loc.clone(),
    });

    // ---- fallthrough block ----
    ctx.switch_to(fall_id, BlockKind::Block);
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_break
// ---------------------------------------------------------------------------

pub fn lower_break(ctx: &mut LoweringContext, label: Option<&str>) -> Result<()> {
    let target = ctx
        .find_break_target(label)
        .ok_or_else(|| CompilerError::invariant("break: no matching break target in scope stack"))?;
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: target,
        variant: GotoVariant::Break,
        id,
        loc: SourceLocation::Generated,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_continue
// ---------------------------------------------------------------------------

pub fn lower_continue(ctx: &mut LoweringContext, label: Option<&str>) -> Result<()> {
    let target = ctx
        .find_continue_target(label)
        .ok_or_else(|| CompilerError::invariant("continue: no matching loop in scope stack"))?;
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: target,
        variant: GotoVariant::Continue,
        id,
        loc: SourceLocation::Generated,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// lower_labeled
// ---------------------------------------------------------------------------

/// CFG shape:
///   current → LabelTerminal{body_id, fall_id}
///   body_id: lower body → Goto(fall_id, Break)
///   fall_id: (continuation)
pub fn lower_labeled<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    stmt: &LabeledStatement<'a>,
    lower_stmt: &mut dyn FnMut(&Statement<'a>, &mut LoweringContext) -> Result<()>,
) -> Result<()> {
    let loc = SourceLocation::source(stmt.span.start, stmt.span.end);
    let label = stmt.label.name.to_string();

    let fall_id = ctx.reserve(BlockKind::Block);
    let body_id = ctx.reserve(BlockKind::Block);

    // Push label scope before emitting the terminal so that the body can
    // break to fall_id with the label name.
    ctx.push_scope(Scope::Label {
        label: label.clone(),
        break_block: fall_id,
    });

    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Label {
        block: body_id,
        fallthrough: fall_id,
        id,
        loc: loc.clone(),
    });

    ctx.switch_to(body_id, BlockKind::Block);
    lower_stmt(&stmt.body, ctx)?;
    ctx.pop_scope();

    // Connect the end of the body to the fallthrough.
    let id = ctx.next_instruction_id();
    ctx.terminate(Terminal::Goto {
        block: fall_id,
        variant: GotoVariant::Break,
        id,
        loc: loc.clone(),
    });

    ctx.switch_to(fall_id, BlockKind::Block);
    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Lower a variable declaration appearing as a `for` / `for-of` / `for-in`
/// init clause.  Each declarator whose init is Some gets lowered as a StoreLocal;
/// declarators without an init get a DeclareLocal (undefined-initialized).
fn lower_var_decl_for_init<'a>(
    ctx: &mut LoweringContext,
    decl: &VariableDeclaration<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let kind = match decl.kind {
        VariableDeclarationKind::Const => InstructionKind::Const,
        VariableDeclarationKind::Let => InstructionKind::Let,
        VariableDeclarationKind::Var => InstructionKind::Let,
        VariableDeclarationKind::Using => InstructionKind::Let,
        VariableDeclarationKind::AwaitUsing => InstructionKind::Let,
    };

    for declarator in &decl.declarations {
        let loc = SourceLocation::source(declarator.span.start, declarator.span.end);

        // Build a place for the binding identifier (simple case).
        // Pattern bindings (destructuring) emit an UnsupportedNode for now.
        let lvalue_place = match &declarator.id.kind {
            BindingPatternKind::BindingIdentifier(ident) => {
                let sym_id = ident
                    .symbol_id
                    .get()
                    .map(|s| s.index() as u32)
                    .unwrap_or(u32::MAX);
                let id = ctx.get_or_create_symbol(sym_id, Some(ident.name.as_str()), loc.clone());
                Place::new(id, loc.clone())
            }
            _ => {
                // Destructuring in for-init — emit unsupported and skip.
                ctx.push(
                    InstructionValue::UnsupportedNode { loc: loc.clone() },
                    loc.clone(),
                );
                continue;
            }
        };

        if let Some(init_expr) = &declarator.init {
            let value_place = lower_expr(init_expr, ctx)?;
            ctx.push_with_lvalue(
                lvalue_place.clone(),
                InstructionValue::StoreLocal {
                    lvalue: LValue { place: lvalue_place, kind },
                    value: value_place,
                    type_annotation: None,
                    loc: loc.clone(),
                },
                loc,
            );
        } else {
            ctx.push_with_lvalue(
                lvalue_place.clone(),
                InstructionValue::DeclareLocal {
                    lvalue: LValue { place: lvalue_place, kind },
                    type_annotation: None,
                    loc: loc.clone(),
                },
                loc,
            );
        }
    }
    Ok(())
}

/// Bind the for-of left-hand side to `value_place`.
fn lower_for_of_left<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    left: &ForStatementLeft<'a>,
    value_place: Place,
    loc: &SourceLocation,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    match left {
        ForStatementLeft::VariableDeclaration(decl) => {
            let kind = match decl.kind {
                VariableDeclarationKind::Const => InstructionKind::Const,
                VariableDeclarationKind::Let => InstructionKind::Let,
                VariableDeclarationKind::Var => InstructionKind::Let,
                VariableDeclarationKind::Using => InstructionKind::Let,
                VariableDeclarationKind::AwaitUsing => InstructionKind::Let,
            };
            for declarator in &decl.declarations {
                let bind_loc =
                    SourceLocation::source(declarator.span.start, declarator.span.end);
                match &declarator.id.kind {
                    BindingPatternKind::BindingIdentifier(ident) => {
                        let sym_id = ident
                            .symbol_id
                            .get()
                            .map(|s| s.index() as u32)
                            .unwrap_or(u32::MAX);
                        let id = ctx.get_or_create_symbol(
                            sym_id,
                            Some(ident.name.as_str()),
                            bind_loc.clone(),
                        );
                        let lvalue_place = Place::new(id, bind_loc.clone());
                        ctx.push_with_lvalue(
                            lvalue_place.clone(),
                            InstructionValue::StoreLocal {
                                lvalue: LValue { place: lvalue_place, kind },
                                value: value_place.clone(),
                                type_annotation: None,
                                loc: bind_loc.clone(),
                            },
                            bind_loc,
                        );
                    }
                    _ => {
                        // Destructuring pattern (ObjectPattern, ArrayPattern, etc.)
                        super::patterns::lower_binding_pattern(
                            ctx, semantic, &declarator.id, value_place.clone(), kind, lower_expr,
                        )?;
                    }
                }
            }
        }
        // All non-VariableDeclaration / non-UsingDeclaration variants of
        // ForStatementLeft are inherited from AssignmentTarget via oxc's
        // inherit_variants! macro. as_assignment_target() returns Some for these.
        left_non_decl => {
            if let Some(target) = left_non_decl.as_assignment_target() {
                match target {
                    AssignmentTarget::AssignmentTargetIdentifier(ident) => {
                        let sym_id = ident
                            .reference_id
                            .get()
                            .map(|r| r.index() as u32)
                            .unwrap_or(u32::MAX);
                        let id = ctx.get_or_create_symbol(
                            sym_id,
                            Some(ident.name.as_str()),
                            loc.clone(),
                        );
                        let lvalue_place = Place::new(id, loc.clone());
                        ctx.push_with_lvalue(
                            lvalue_place.clone(),
                            InstructionValue::StoreLocal {
                                lvalue: LValue {
                                    place: lvalue_place,
                                    kind: InstructionKind::Reassign,
                                },
                                value: value_place,
                                type_annotation: None,
                                loc: loc.clone(),
                            },
                            loc.clone(),
                        );
                    }
                    _ => {
                        // Member expressions, destructuring assignment targets.
                        ctx.push(
                            InstructionValue::UnsupportedNode { loc: loc.clone() },
                            loc.clone(),
                        );
                    }
                }
            } else {
                // UsingDeclaration or truly unknown.
                ctx.push(
                    InstructionValue::UnsupportedNode { loc: loc.clone() },
                    loc.clone(),
                );
            }
        }
    }
    Ok(())
}

/// Bind the for-in left-hand side to `value_place`.
/// Identical logic to for-of left binding.
fn lower_for_in_left<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    left: &ForStatementLeft<'a>,
    value_place: Place,
    loc: &SourceLocation,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    lower_for_of_left(ctx, semantic, left, value_place, loc, lower_expr)
}
