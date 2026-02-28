use crate::hir::hir::ReactiveFunction;
use crate::error::Result;

#[derive(Debug)]
pub struct CodegenOutput {
    pub js: String,
}

pub fn codegen_reactive_function(_func: &ReactiveFunction) -> Result<CodegenOutput> {
    // TODO: implement full codegen
    Ok(CodegenOutput {
        js: "// TODO: codegen not yet implemented".into(),
    })
}
