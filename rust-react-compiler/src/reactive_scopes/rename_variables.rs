use crate::hir::hir::HIRFunction;
use crate::hir::environment::Environment;

pub fn run(_hir: &mut HIRFunction) {}

/// Rename variables to ensure unique names.
///
/// Currently deferred: the pass needs careful coordination with codegen's
/// scope output temp naming (t0, t1, etc.) and shadowing resolution
/// (name_overrides). Implementing it standalone causes collisions.
///
/// The TS compiler runs this on ReactiveFunction (a tree with block scoping),
/// which naturally prevents collisions between nested scopes. Our flat HIR
/// doesn't have this structure yet.
pub fn run_with_env(_hir: &mut HIRFunction, _env: &mut Environment) {
    // Stub: rename_variables requires ReactiveFunction tree for correct
    // block-scoping semantics. Without it, flat renaming creates collisions
    // with codegen's scope output temps and shadowing resolution.
}
