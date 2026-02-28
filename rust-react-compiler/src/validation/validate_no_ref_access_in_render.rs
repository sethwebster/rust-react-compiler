use crate::hir::hir::HIRFunction;
use crate::error::Result;

/// Validates that `.current` on a ref object is not read or written during
/// the render phase (outside of effects and event handlers).
///
/// Phase 1: stub — requires shape-based type recognition to identify ref
/// objects from `useRef` return values.
pub fn run(_hir: &HIRFunction) -> Result<()> {
    Ok(())
}
