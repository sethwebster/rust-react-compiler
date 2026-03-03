/// Promote unnamed function parameters to named variables.
///
/// Assigns sequential `tN` names (t0, t1, ...) to unnamed function parameters.
/// This is needed for destructured props: `function Component({ a })` gets
/// the props object renamed to `t0`, so the output is `function Component(t0)`.
///
/// The counter starts AFTER the number of scopes (cache slots) to avoid naming
/// conflicts with scope result temps (which use t{slot} names in codegen).
///
/// This mirrors the TS compiler's `promoteUsedTemporaries` pass (params-only portion).
use crate::hir::environment::Environment;
use crate::hir::hir::{HIRFunction, IdentifierName, Param};

pub fn run(_hir: &mut HIRFunction) {}

pub fn run_with_env(hir: &mut HIRFunction, env: &mut Environment) {
    // Assign tN names to unnamed function parameters, starting from 0.
    // In codegen, scope result temps will start from (num_promoted_params)
    // to avoid name collisions (e.g., param t0 + scope result t0).
    let mut counter = 0u32;
    for param in &hir.params {
        match param {
            Param::Place(p) => {
                let id = p.identifier;
                if env.get_identifier(id).and_then(|i| i.name.as_ref()).is_none() {
                    let name = format!("t{}", counter);
                    counter += 1;
                    if let Some(ident) = env.get_identifier_mut(id) {
                        ident.name = Some(IdentifierName::Promoted(name));
                    }
                }
            }
            Param::Spread(s) => {
                let id = s.place.identifier;
                if env.get_identifier(id).and_then(|i| i.name.as_ref()).is_none() {
                    let name = format!("t{}", counter);
                    counter += 1;
                    if let Some(ident) = env.get_identifier_mut(id) {
                        ident.name = Some(IdentifierName::Promoted(name));
                    }
                }
            }
        }
    }
}
