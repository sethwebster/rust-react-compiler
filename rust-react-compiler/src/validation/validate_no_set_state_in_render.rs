use crate::hir::hir::HIRFunction;
use crate::error::Result;

/// Validates that `setState` (and similar state-setter dispatch) is not called
/// unconditionally during the render phase.
///
/// Phase 1: stub — requires shape-based callee recognition to identify
/// state-setter functions returned by `useState`.
pub fn run(_hir: &HIRFunction) -> Result<()> {
    Ok(())
}
