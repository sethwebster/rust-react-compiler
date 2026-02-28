use crate::hir::hir::HIRFunction;

pub struct InferMutationAliasingRangesOptions {
    pub is_function_expression: bool,
}

pub fn infer_mutation_aliasing_ranges(
    _hir: &mut HIRFunction,
    _options: InferMutationAliasingRangesOptions,
) {
    // TODO: implement
}
