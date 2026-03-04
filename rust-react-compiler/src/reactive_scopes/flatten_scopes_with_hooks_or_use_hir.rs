/// Remove reactive scopes that contain hook calls.
///
/// Hooks must be called unconditionally on every render. If a scope wraps a hook
/// call (e.g., `if ($[0] === sentinel) { const ref = useRef(null); ... }`), the
/// hook only runs on the first render — violating the Rules of Hooks.
///
/// This pass removes any scope whose instruction range contains a hook call.
/// The hook call and its result are emitted without memoization (inline).
///
/// This mirrors the TS compiler's `flattenScopesWithHooksOrUse` pass.
use std::collections::HashSet;

use crate::hir::environment::Environment;
use crate::hir::hir::{HIRFunction, IdentifierId, InstructionValue, NonLocalBinding, ScopeId};

pub fn run(_hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    if env.scopes.is_empty() {
        return;
    }

    // Build map: identifiers → names (from LoadGlobal and PropertyLoad).
    let mut global_names: std::collections::HashMap<IdentifierId, String> = std::collections::HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = match binding {
                        NonLocalBinding::Global { name } => name.clone(),
                        NonLocalBinding::ModuleLocal { name } => name.clone(),
                        NonLocalBinding::ImportDefault { name, .. }
                        | NonLocalBinding::ImportNamespace { name, .. }
                        | NonLocalBinding::ImportSpecifier { name, .. } => name.clone(),
                    };
                    global_names.insert(instr.lvalue.identifier, name);
                }
                // Track property names for method calls like React.useState
                InstructionValue::PropertyLoad { property, .. } => {
                    global_names.insert(instr.lvalue.identifier, property.clone());
                }
                _ => {}
            }
        }
    }

    let is_hook_name = |name: &str| -> bool {
        name.starts_with("use") && name[3..].chars().next().map_or(false, |c| c.is_uppercase())
    };

    // Find scopes that contain hook calls.
    let mut scopes_with_hooks: HashSet<ScopeId> = HashSet::new();

    for (&sid, scope) in &env.scopes {
        let range_start = scope.range.start;
        let range_end = scope.range.end;

        'instr: for (_, block) in &hir.body.blocks {
            for instr in &block.instructions {
                if instr.id < range_start || instr.id >= range_end {
                    continue;
                }

                // Check for hook calls: CallExpression or MethodCall where callee is a hook.
                match &instr.value {
                    InstructionValue::CallExpression { callee, .. } => {
                        if let Some(name) = global_names.get(&callee.identifier) {
                            if is_hook_name(name) {
                                scopes_with_hooks.insert(sid);
                                break 'instr;
                            }
                        }
                    }
                    InstructionValue::MethodCall { property, .. } => {
                        // Method calls like React.useState(), obj.useHook()
                        if let Some(name) = global_names.get(&property.identifier) {
                            if is_hook_name(name) {
                                scopes_with_hooks.insert(sid);
                                break 'instr;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if scopes_with_hooks.is_empty() {
        return;
    }

    // Remove scopes that contain hooks.
    for &sid in &scopes_with_hooks {
        env.scopes.remove(&sid);
    }

    // Clear ident.scope for identifiers that were in removed scopes.
    for ident in env.identifiers.values_mut() {
        if let Some(sid) = ident.scope {
            if scopes_with_hooks.contains(&sid) {
                ident.scope = None;
            }
        }
    }
}
