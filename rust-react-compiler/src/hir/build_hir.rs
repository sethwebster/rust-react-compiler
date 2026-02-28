/// AST → HIR lowering.
///
/// This is the main lowering pass that converts an oxc AST function into a
/// HIR control-flow graph. Phase 1 implements a stub that produces a minimal
/// HIR with one block containing a single return of undefined.
///
/// Full implementation follows BuildHIR.ts (~4555 lines) in phases 2+.
use oxc_allocator::Allocator;
use oxc_semantic::SemanticBuilder;

use crate::error::{CompilerError, Result};
use crate::hir::environment::Environment;
use crate::hir::hir::*;

pub struct LoweringContext<'a> {
    pub env: &'a mut Environment,
    /// Map from oxc SymbolId to our IdentifierId
    pub symbol_map: rustc_hash::FxHashMap<u32, IdentifierId>,
}

impl<'a> LoweringContext<'a> {
    pub fn new(env: &'a mut Environment) -> Self {
        LoweringContext {
            env,
            symbol_map: rustc_hash::FxHashMap::default(),
        }
    }
}

/// Lower an oxc `Program` (containing one top-level function) into HIR.
///
/// For Phase 1, we produce a minimal valid HIR. Full lowering is Phase 2+.
pub fn lower_program(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
) -> Result<HIRFunction> {
    let allocator = Allocator::default();
    let parser_return = oxc_parser::Parser::new(&allocator, source, source_type).parse();

    if !parser_return.errors.is_empty() {
        let msgs: Vec<_> = parser_return.errors.iter().map(|e| e.to_string()).collect();
        return Err(CompilerError::invalid_js(format!(
            "Parse errors:\n{}",
            msgs.join("\n")
        )));
    }

    let program = parser_return.program;
    let _semantic = SemanticBuilder::new()
        .build(&program);

    // For Phase 1: produce a stub HIRFunction that represents the parsed program.
    // We'll populate it with a single block containing a return of undefined.
    let mut ctx = LoweringContext::new(env);
    let hir = build_stub_hir(&mut ctx);
    Ok(hir)
}

/// Minimal HIR that passes validation: entry block with `return undefined`
fn build_stub_hir(ctx: &mut LoweringContext) -> HIRFunction {
    let env = &mut ctx.env;
    let loc = SourceLocation::Generated;

    let entry_id = env.new_block_id();
    let instr_id = env.new_instruction_id();
    let ret_instr_id = env.new_instruction_id();

    // lvalue for the undefined temporary
    let undef_id = env.new_temporary(loc.clone());
    let undef_place = Place::new(undef_id, loc.clone());

    // return place
    let ret_id = env.new_temporary(loc.clone());
    let ret_place = Place::new(ret_id, loc.clone());

    // Instruction: $t0 = undefined
    let undef_instr = Instruction {
        id: instr_id,
        lvalue: undef_place.clone(),
        value: InstructionValue::Primitive {
            value: PrimitiveValue::Undefined,
            loc: loc.clone(),
        },
        loc: loc.clone(),
        effects: None,
    };

    // Terminal: return $t0
    let terminal = Terminal::Return {
        value: undef_place,
        return_variant: ReturnVariant::Void,
        id: ret_instr_id,
        loc: loc.clone(),
        effects: None,
    };

    let entry_block = BasicBlock {
        kind: BlockKind::Block,
        id: entry_id,
        instructions: vec![undef_instr],
        terminal,
        preds: std::collections::HashSet::new(),
        phis: vec![],
    };

    let mut hir_body = HIR::new(entry_id);
    hir_body.blocks.insert(entry_id, entry_block);

    HIRFunction {
        loc,
        id: None,
        name_hint: None,
        fn_type: ReactFunctionType::Component,
        params: vec![],
        return_type_annotation: None,
        returns: ret_place,
        context: vec![],
        body: hir_body,
        generator: false,
        async_: false,
        directives: vec![],
        aliasing_effects: None,
    }
}
