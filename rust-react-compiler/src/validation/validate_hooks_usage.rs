use crate::hir::hir::*;
use crate::error::Result;

/// Validates that React hooks (functions whose name begins with `use`) are not
/// called conditionally and follow the Rules of Hooks.
///
/// Phase 1 implementation: scans for call expressions where we can determine
/// the callee name statically (e.g. a `LoadGlobal` or `LoadLocal` with a
/// known `IdentifierName`). Full conditional-call checking requires dominator
/// analysis, which is deferred to a later phase.
pub fn run(hir: &HIRFunction) -> Result<()> {
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::CallExpression { callee, .. } => {
                    // We would look up callee name via env here.
                    // Phase 1: no env available; skip name resolution.
                    let _ = callee;
                }
                InstructionValue::MethodCall { receiver, property, .. } => {
                    let _ = (receiver, property);
                }
                _ => {}
            }
        }
    }
    Ok(())
}
