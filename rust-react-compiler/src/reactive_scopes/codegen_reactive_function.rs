use crate::hir::hir::ReactiveFunction;
use crate::error::Result;

#[derive(Debug)]
pub struct CodegenOutput {
    pub js: String,
    /// Outlined helper functions (e.g., `function _temp(...) {...}`) extracted from
    /// the component body. Callers should append these at module scope after all
    /// other declarations, not inline with the component.
    pub outlines: Vec<String>,
}

pub fn codegen_reactive_function(_func: &ReactiveFunction) -> Result<CodegenOutput> {
    // TODO: implement full codegen
    Ok(CodegenOutput {
        js: "// TODO: codegen not yet implemented".into(),
        outlines: Vec::new(),
    })
}
