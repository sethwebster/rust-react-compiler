use crate::hir::hir::HIRFunction;
use crate::error::Result;

/// Validates that `useMemo` / `useCallback` calls are well-formed:
/// - called with exactly two arguments
/// - second argument is an array literal (dependency array)
///
/// Phase 1: stub — detection is straightforward once callee name resolution
/// via the environment is wired up.
pub fn run(_hir: &HIRFunction) -> Result<()> {
    Ok(())
}
