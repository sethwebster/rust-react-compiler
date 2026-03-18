#![allow(unused_imports, unused_variables, dead_code)]

use std::collections::HashSet;
use crate::hir::hir::{DeclarationId, HIRFunction, IdentifierId, InstructionKind, InstructionValue};
use crate::hir::environment::Environment;
use crate::hir::hir::Place;

/// Rewrite `Let` declarations to `Const` for identifiers that are never
/// reassigned anywhere in the function.
///
/// A variable is considered "reassigned" if any `StoreLocal` instruction uses
/// `InstructionKind::Reassign` for that identifier's lvalue, OR if it appears
/// as the lvalue of a `PrefixUpdate`/`PostfixUpdate`. After SSA, the named
/// variable identifiers in `StoreLocal.lvalue.place` retain their pre-SSA ids,
/// while `PostfixUpdate.lvalue` holds the SSA-renamed id. We use
/// `declaration_id` (preserved across SSA) to match them correctly.
///
/// For nested function expressions whose bodies are stubs (original_source
/// passthrough), we use source-text analysis to detect reassignments.
pub fn rewrite_instruction_kinds_based_on_reassignment(hir: &mut HIRFunction, env: &Environment) {
    let mut reassigned_decls: HashSet<DeclarationId> = HashSet::new();

    let decl_id = |id: IdentifierId| -> DeclarationId {
        env.get_identifier(id)
            .map(|i| i.declaration_id)
            .unwrap_or_else(|| DeclarationId(id.0))
    };

    // Recursively collect reassigned decls from a function and all nested functions.
    fn collect_reassigned_decls(
        func: &crate::hir::hir::HIRFunction,
        env: &Environment,
        reassigned: &mut HashSet<DeclarationId>,
    ) {
        let decl_id = |id: IdentifierId| -> DeclarationId {
            env.get_identifier(id)
                .map(|i| i.declaration_id)
                .unwrap_or_else(|| DeclarationId(id.0))
        };
        for block in func.body.blocks.values() {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. } => {
                        if lvalue.kind == InstructionKind::Reassign {
                            reassigned.insert(decl_id(lvalue.place.identifier));
                        }
                    }
                    InstructionValue::StoreContext { lvalue, .. } => {
                        if lvalue.kind == crate::hir::hir::ContextStoreKind::Reassign {
                            reassigned.insert(decl_id(lvalue.place.identifier));
                        }
                    }
                    InstructionValue::PrefixUpdate { lvalue, .. }
                    | InstructionValue::PostfixUpdate { lvalue, .. } => {
                        reassigned.insert(decl_id(lvalue.identifier));
                    }
                    // Recurse into nested function expressions.
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        collect_reassigned_decls(&lowered_func.func, env, reassigned);
                        // Also detect direct reassignments via source-text analysis
                        // for stub function bodies (original_source passthrough).
                        // Only checks for `name = ...` / `name++` / `++name` patterns,
                        // NOT property stores like `name.prop = ...`.
                        if !lowered_func.func.original_source.is_empty() {
                            let reassigned_ids = find_reassigned_context_vars_from_source(
                                &lowered_func.func.context,
                                &lowered_func.func.original_source,
                                env,
                            );
                            for rid in reassigned_ids {
                                reassigned.insert(decl_id(rid));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    collect_reassigned_decls(hir, env, &mut reassigned_decls);

    // --- Pass 2: tighten declaration kinds for non-reassigned variables ---
    for block in hir.body.blocks.values_mut() {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. } => {
                    tighten_instruction_kind(lvalue, &reassigned_decls, env);
                }
                _ => {}
            }
        }
    }
}

/// Detect context variables that are directly reassigned in source text.
/// Only checks for `name = ...` (not `==`/`===`), `name++`, `++name`,
/// `name += ...` etc. Does NOT count property stores like `name.prop = ...`.
pub fn find_reassigned_context_vars_from_source(
    context: &[Place],
    source: &str,
    env: &Environment,
) -> Vec<IdentifierId> {
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let bytes = source.as_bytes();
    let slen = bytes.len();
    let mut reassigned = Vec::new();

    for ctx_place in context {
        let name = match env.get_identifier(ctx_place.identifier).and_then(|i| i.name.as_ref()) {
            Some(n) => n.value().to_string(),
            None => continue,
        };
        if name.is_empty() { continue; }
        let name_bytes = name.as_bytes();
        let nlen = name.len();
        let mut is_reassigned = false;

        let mut i = 0;
        while i + nlen <= slen {
            // Skip string literals.
            if bytes[i] == b'\'' || bytes[i] == b'"' || bytes[i] == b'`' {
                let q = bytes[i];
                i += 1;
                while i < slen && bytes[i] != q {
                    if bytes[i] == b'\\' { i += 1; }
                    i += 1;
                }
                if i < slen { i += 1; }
                continue;
            }
            // Skip comments.
            if i + 1 < slen && bytes[i] == b'/' && bytes[i + 1] == b'/' {
                while i < slen && bytes[i] != b'\n' { i += 1; }
                continue;
            }
            if i + 1 < slen && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < slen && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; }
                if i + 1 < slen { i += 2; }
                continue;
            }

            if bytes[i..].starts_with(name_bytes) {
                let before_ok = i == 0 || !is_id_char(bytes[i - 1]);
                // Exclude property access: `.name` is not a standalone reference
                let not_prop_access = i == 0 || bytes[i - 1] != b'.';
                let after_pos = i + nlen;
                let after_ok = after_pos >= slen || !is_id_char(bytes[after_pos]);
                if before_ok && not_prop_access && after_ok {
                    let mut j = after_pos;
                    while j < slen && bytes[j].is_ascii_whitespace() { j += 1; }

                    if j < slen {
                        // Direct assignment: name = (not == or ===)
                        if bytes[j] == b'=' && (j + 1 >= slen || bytes[j + 1] != b'=') {
                            // Make sure it's not preceded by . (property of something else)
                            is_reassigned = true;
                            break;
                        }
                        // Compound: +=, -=, *=, /=, %=, &=, |=, ^=
                        if j + 1 < slen && bytes[j + 1] == b'=' {
                            match bytes[j] {
                                b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' => {
                                    is_reassigned = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        // Postfix ++, --
                        if j + 1 < slen && ((bytes[j] == b'+' && bytes[j + 1] == b'+') || (bytes[j] == b'-' && bytes[j + 1] == b'-')) {
                            is_reassigned = true;
                            break;
                        }
                    }

                    // Prefix ++ / --
                    if i >= 2 {
                        let mut k = i - 1;
                        while k > 0 && bytes[k].is_ascii_whitespace() { k -= 1; }
                        if k > 0 && ((bytes[k] == b'+' && bytes[k - 1] == b'+') || (bytes[k] == b'-' && bytes[k - 1] == b'-')) {
                            is_reassigned = true;
                            break;
                        }
                    }

                    i = after_pos;
                    continue;
                }
            }
            i += 1;
        }

        if is_reassigned {
            reassigned.push(ctx_place.identifier);
        }
    }

    reassigned
}

/// Upgrade a `Let` → `Const` or `HoistedLet` → `HoistedConst` when the
/// identifier is never reassigned (matched by declaration_id).
fn tighten_instruction_kind(
    lvalue: &mut crate::hir::hir::LValue,
    reassigned_decls: &HashSet<DeclarationId>,
    env: &Environment,
) {
    let decl = env.get_identifier(lvalue.place.identifier)
        .map(|i| i.declaration_id)
        .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
    if reassigned_decls.contains(&decl) {
        return;
    }
    lvalue.kind = match lvalue.kind {
        InstructionKind::Let => InstructionKind::Const,
        // Do NOT promote HoistedLet → HoistedConst: the TS compiler preserves
        // hoisted variables as `let` since they're hoisted to function scope.
        other => other,
    };
}
