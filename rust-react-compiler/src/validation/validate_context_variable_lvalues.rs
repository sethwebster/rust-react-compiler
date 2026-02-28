use crate::hir::hir::HIRFunction;
use crate::error::Result;

/// Validates that context variable lvalues are used correctly.
///
/// Phase 1: stub — full check requires shape information from the environment.
pub fn run(_hir: &HIRFunction) -> Result<()> {
    Ok(())
}
