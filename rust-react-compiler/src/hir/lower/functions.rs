#![allow(dead_code, unused_variables, unused_imports)]

use oxc_ast::ast::*;
use oxc_index::Idx;
use oxc_semantic::Semantic;

use crate::hir::hir::*;
use crate::error::Result;
use super::LoweringContext;

// ---------------------------------------------------------------------------
// Internal helper: build a minimal stub HIRFunction body with a single block
// that returns `undefined`.
// ---------------------------------------------------------------------------

fn make_stub_hir_body(
    ctx: &mut LoweringContext,
    loc: &SourceLocation,
    is_async: bool,
    is_generator: bool,
    id: Option<String>,
) -> HIRFunction {
    // Allocate IDs up-front via the shared environment so all IDs stay globally
    // unique across the parent and child.
    let entry_id = ctx.env.new_block_id();
    let undef_id  = ctx.env.new_temporary(SourceLocation::Generated);
    let ret_id    = ctx.env.new_temporary(SourceLocation::Generated);
    let instr_id  = ctx.env.new_instruction_id();
    let term_id   = ctx.env.new_instruction_id();

    let undef_place = Place::new(undef_id, SourceLocation::Generated);
    let ret_place   = Place::new(ret_id,   SourceLocation::Generated);

    // Single entry block: `$undef = undefined; return $undef`
    let entry_block = BasicBlock {
        kind: BlockKind::Block,
        id: entry_id,
        instructions: vec![Instruction {
            id: instr_id,
            lvalue: undef_place.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Undefined,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        }],
        terminal: Terminal::Return {
            value: undef_place,
            return_variant: ReturnVariant::Void,
            id: term_id,
            loc: SourceLocation::Generated,
            effects: None,
        },
        preds: std::collections::HashSet::new(),
        phis: vec![],
    };

    let mut hir_body = HIR::new(entry_id);
    hir_body.blocks.insert(entry_id, entry_block);

    HIRFunction {
        loc: loc.clone(),
        id,
        name_hint: None,
        fn_type: ReactFunctionType::Other,
        params: vec![],
        return_type_annotation: None,
        returns: ret_place,
        context: vec![],
        body: hir_body,
        generator: is_generator,
        async_: is_async,
        directives: vec![],
        aliasing_effects: None,
    }
}

// ---------------------------------------------------------------------------
// lower_function_expr
//
// Lowers a named or anonymous `function` expression to an
// `InstructionValue::FunctionExpression` with `fn_type: Expression`.
//
// A full recursive lowering of the body is deferred; for now a stub HIR body
// is emitted.  core.rs will wire up real body lowering once all agent modules
// are merged and circular-dependency concerns are resolved.
// ---------------------------------------------------------------------------

pub fn lower_function_expr<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    func: &Function<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(func.span.start, func.span.end);
    let name = func.id.as_ref().map(|id| id.name.to_string());

    let lowered_fn = make_stub_hir_body(
        ctx,
        &loc,
        func.r#async,
        func.generator,
        name.clone(),
    );

    let result = ctx.push(
        InstructionValue::FunctionExpression {
            name,
            name_hint: None,
            lowered_func: LoweredFunction { func: Box::new(lowered_fn) },
            fn_type: FunctionExpressionType::Expression,
            loc: loc.clone(),
        },
        loc,
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// lower_arrow
//
// Lowers an arrow function expression to an
// `InstructionValue::FunctionExpression` with `fn_type: Arrow`.
//
// Arrow functions are always anonymous and never generators.
// ---------------------------------------------------------------------------

pub fn lower_arrow<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    expr: &ArrowFunctionExpression<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<Place> {
    let loc = SourceLocation::source(expr.span.start, expr.span.end);

    let lowered_fn = make_stub_hir_body(
        ctx,
        &loc,
        expr.r#async,
        false, // arrows are never generators
        None,  // arrows are always anonymous
    );

    let result = ctx.push(
        InstructionValue::FunctionExpression {
            name: None,
            name_hint: None,
            lowered_func: LoweredFunction { func: Box::new(lowered_fn) },
            fn_type: FunctionExpressionType::Arrow,
            loc: loc.clone(),
        },
        loc,
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// lower_function_declaration
//
// Lowers a function declaration statement.  Unlike an expression, a
// declaration binds the function value to the declared name in the current
// scope via a `StoreLocal` instruction with `InstructionKind::Function`.
//
// Steps:
//   1. Lower the function itself (reusing lower_function_expr).
//   2. Resolve the binding identifier via oxc's semantic symbol table.
//   3. Emit StoreLocal { lvalue: LValue { place, kind: Function }, value }.
// ---------------------------------------------------------------------------

pub fn lower_function_declaration<'a>(
    ctx: &mut LoweringContext,
    semantic: &Semantic<'a>,
    func: &Function<'a>,
    lower_expr: &mut dyn FnMut(&Expression<'a>, &mut LoweringContext) -> Result<Place>,
) -> Result<()> {
    let loc = SourceLocation::source(func.span.start, func.span.end);

    // 1. Build the FunctionExpression instruction (stub body).
    let func_place = lower_function_expr(ctx, semantic, func, lower_expr)?;

    // 2. Resolve or create the HIR identifier for the declaration name.
    //    Function declarations always have an `id`; if somehow missing we
    //    just skip the StoreLocal and return the value unreferenced.
    let Some(func_id) = func.id.as_ref() else {
        return Ok(());
    };

    // Look up the oxc SymbolId for this binding.  If the identifier has no
    // symbol (possible in pathological/error-recovery parses) we skip.
    let Some(symbol_id) = func_id.symbol_id.get() else {
        return Ok(());
    };

    let ident_id = ctx.get_or_create_symbol(
        symbol_id.index() as u32,
        Some(func_id.name.as_str()),
        loc.clone(),
    );
    let lvalue_place = Place::new(ident_id, loc.clone());

    // 3. Emit StoreLocal binding the function value to the declared name.
    ctx.push_with_lvalue(
        lvalue_place.clone(),
        InstructionValue::StoreLocal {
            lvalue: LValue {
                place: lvalue_place,
                kind: InstructionKind::Function,
            },
            value: func_place,
            type_annotation: None,
            loc: loc.clone(),
        },
        loc,
    );

    Ok(())
}
