use std::collections::HashSet;

use crate::hir::environment::Environment;
use crate::hir::hir::{HIRFunction, IdentifierId};

/// Remove scope declarations that are only used within the scope's instruction range.
///
/// A declaration can be removed (converted to a local variable) if the identifier's
/// mutable range ends at or before the scope's instruction range ends — meaning
/// the identifier is never read after the scope body completes and doesn't need
/// to be cached in a memo slot.
///
/// The primary case this handles: early-return scopes where a variable is computed
/// and used only within the scope body (e.g., `let x = []; if (cond) { return x; }`).
/// After PropagateEarlyReturns transforms the early return to use a sentinel variable,
/// the original variable `x` doesn't need its own cache slot — it becomes a local `const`.
///
/// This mirrors the TS compiler's behavior of not caching variables that don't escape
/// the scope's instruction range.
pub fn run(_hir: &HIRFunction, env: &mut Environment) {
    if env.scopes.is_empty() {
        return;
    }

    // Collect declaration IDs where mutable_range.end <= scope.range.end.
    // These declarations are only used within the scope body and don't need cache slots.
    let mut to_remove: HashSet<IdentifierId> = HashSet::new();

    for scope in env.scopes.values() {
        let scope_end = scope.range.end.0;
        if scope_end == 0 {
            continue; // Skip degenerate scopes.
        }
        for &decl_id in scope.declarations.keys() {
            if let Some(ident) = env.identifiers.get(&decl_id) {
                let mutable_end = ident.mutable_range.end.0;
                // mutable_range is [start, end) — end is exclusive.
                // If mutable_end <= scope_end, the identifier's last use is within
                // the scope range [scope.range.start, scope.range.end).
                // Skip uninitialized ranges (mutable_end == 0).
                if mutable_end > 0 && mutable_end <= scope_end {
                    to_remove.insert(decl_id);
                }
            }
        }
    }

    if to_remove.is_empty() {
        return;
    }

    if std::env::var("RC_DEBUG_PRUNE_LOCAL").is_ok() {
        for &id in &to_remove {
            let name = env.identifiers.get(&id)
                .and_then(|i| i.name.as_ref())
                .map(|n| n.value().to_string())
                .unwrap_or_else(|| format!("id={}", id.0));
            eprintln!("[prune_local_decls] removing {:?} from scope declarations", name);
        }
    }

    // Remove from scope.declarations.
    for scope in env.scopes.values_mut() {
        scope.declarations.retain(|id, _| !to_remove.contains(id));
    }

    // Clear ident.scope for removed declarations so other passes don't
    // treat them as scope outputs.
    for &id in &to_remove {
        if let Some(ident) = env.identifiers.get_mut(&id) {
            ident.scope = None;
        }
    }
}
