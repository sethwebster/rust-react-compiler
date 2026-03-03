use std::collections::HashMap;
use crate::hir::hir::{
    CallArg, HIRFunction, IdentifierId, InstructionValue, NonLocalBinding,
};

/// Identifies whether an IdentifierId refers to `useMemo` or `useCallback`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManualMemoKind {
    UseMemo,
    UseCallback,
}

fn is_memo_name(name: &str) -> Option<ManualMemoKind> {
    match name {
        "useMemo" => Some(ManualMemoKind::UseMemo),
        "useCallback" => Some(ManualMemoKind::UseCallback),
        _ => None,
    }
}

/// Drop manual memoization: replace `useMemo(fn, deps)` with `fn()` and
/// `useCallback(fn, deps)` with just `fn`. The compiler will then re-memoize
/// these expressions through its own reactive scope analysis.
pub fn drop_manual_memoization(hir: &mut HIRFunction) {
    // Phase 1: Scan all blocks to find identifiers that are `useMemo`/`useCallback`.
    // These come from LoadGlobal or from a property load on React (React.useMemo).
    let mut memo_ids: HashMap<IdentifierId, ManualMemoKind> = HashMap::new();

    // Track identifiers loaded via LoadGlobal with a known memo binding.
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                let name = match binding {
                    NonLocalBinding::ImportSpecifier { name, module, .. }
                        if module == "react" || module == "React" =>
                    {
                        Some(name.as_str())
                    }
                    NonLocalBinding::ImportDefault { name, .. } => {
                        // `import useMemo from 'react'` — unlikely but handle
                        Some(name.as_str())
                    }
                    NonLocalBinding::Global { name } => Some(name.as_str()),
                    NonLocalBinding::ModuleLocal { name } => Some(name.as_str()),
                    _ => None,
                };
                if let Some(n) = name {
                    if let Some(kind) = is_memo_name(n) {
                        memo_ids.insert(instr.lvalue.identifier, kind);
                    }
                }
            }
        }
    }

    // Phase 2: Replace CallExpression/MethodCall to useMemo/useCallback.
    for (_, block) in &mut hir.body.blocks {
        for instr in &mut block.instructions {
            let replacement = match &instr.value {
                InstructionValue::CallExpression { callee, args, loc } => {
                    if let Some(&kind) = memo_ids.get(&callee.identifier) {
                        build_replacement(kind, args, loc.clone())
                    } else {
                        None
                    }
                }
                // React.useMemo(fn, deps) — MethodCall where property is useMemo
                InstructionValue::MethodCall { property, args, loc, .. } => {
                    if let Some(&kind) = memo_ids.get(&property.identifier) {
                        build_replacement(kind, args, loc.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(new_value) = replacement {
                instr.value = new_value;
            }
        }
    }

    // Also process nested functions (components inside components, etc.)
    for (_, block) in &mut hir.body.blocks {
        for instr in &mut block.instructions {
            if let InstructionValue::FunctionExpression { lowered_func, .. }
            | InstructionValue::ObjectMethod { lowered_func, .. } = &mut instr.value
            {
                drop_manual_memoization(&mut lowered_func.func);
            }
        }
    }
}

fn build_replacement(
    kind: ManualMemoKind,
    args: &[CallArg],
    loc: crate::hir::hir::SourceLocation,
) -> Option<InstructionValue> {
    // Extract the first argument (the function).
    let fn_place = match args.first() {
        Some(CallArg::Place(p)) => p.clone(),
        _ => return None,
    };

    match kind {
        ManualMemoKind::UseMemo => {
            // useMemo(fn, deps) → CallExpression(fn, [])
            // i.e. call the function immediately with no arguments
            Some(InstructionValue::CallExpression {
                callee: fn_place,
                args: vec![],
                loc,
            })
        }
        ManualMemoKind::UseCallback => {
            // useCallback(fn, deps) → LoadLocal(fn)
            // i.e. just use the function directly
            Some(InstructionValue::LoadLocal {
                place: fn_place,
                loc,
            })
        }
    }
}
