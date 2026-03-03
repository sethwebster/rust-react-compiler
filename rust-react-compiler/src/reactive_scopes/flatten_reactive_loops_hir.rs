use crate::hir::hir::HIRFunction;
use crate::hir::environment::Environment;
pub fn run(_hir: &mut HIRFunction) {}
pub fn run_with_env(_hir: &mut HIRFunction, _env: &mut Environment) {
    // Deferred: requires scope terminals (Terminal::Scope) which we don't have yet.
    // The TS compiler prunes scopes that are inside loop bodies, but our scope
    // representation (Environment.scopes with InstructionId ranges) doesn't
    // distinguish "scope inside loop" from "scope wrapping loop" reliably enough.
}
