use crate::hir::hir::HIRFunction;
use crate::error::Result;

/// Validates that local variables that escape into closures passed to hooks
/// are not reassigned after the render phase completes.
///
/// Phase 1: stub — requires escape analysis output from the aliasing/mutation
/// inference passes to identify which locals are captured by hook closures.
pub fn run(_hir: &HIRFunction) -> Result<()> {
    Ok(())
}
