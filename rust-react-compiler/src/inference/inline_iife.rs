use crate::hir::hir::HIRFunction;

pub fn inline_immediately_invoked_function_expressions(_hir: &mut HIRFunction) {
    // Inner function bodies are stubs (original_source passthrough), so CFG-level
    // IIFE inlining is deferred until full lowering of nested functions is implemented.
    // Instead, codegen handles the pattern via extract_iife_return_expr.
}
