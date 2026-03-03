/// AST → HIR lowering entry point.
///
/// This module re-exports the primary lowering function from the `lower::core`
/// submodule. Phase 1 was a stub; Phase 2+ delegates to the full implementation.
use crate::error::Result;
use crate::hir::environment::Environment;
use crate::hir::hir::HIRFunction;

/// Lower an oxc `Program` (containing one top-level function) into HIR.
pub fn lower_program(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
) -> Result<HIRFunction> {
    crate::hir::lower::core::lower_program(source, source_type, env)
}

/// Like `lower_program` but skips the first `n` compilable function-like
/// top-level statements and compiles the (n+1)th.
pub fn lower_program_nth(
    source: &str,
    source_type: oxc_span::SourceType,
    env: &mut Environment,
    n: usize,
) -> Result<HIRFunction> {
    crate::hir::lower::core::lower_program_nth(source, source_type, env, n)
}
