use crate::hir::environment::Environment;
use crate::hir::hir::HIRFunction;

/// Remove non-reactive dependencies from all reactive scopes.
///
/// `propagate_scope_dependencies_hir` intentionally adds always-invalidating
/// deps (Object/Array/Function/JSX) even when `reactive=false`, so that
/// `merge_reactive_scopes_that_invalidate_together` can perform Case 2b
/// merges.  After merging, those non-reactive deps must be pruned so that
/// scopes with no genuinely-reactive deps become sentinel (dep-free) scopes.
pub fn run(_hir: &mut HIRFunction, env: &mut Environment) {
    for scope in env.scopes.values_mut() {
        scope.dependencies.retain(|dep| dep.place.reactive);
    }
}
