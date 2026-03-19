#![allow(unused_imports, unused_variables, dead_code)]

use std::collections::{HashSet, HashMap};
use crate::hir::hir::{BlockId, DeclarationId, HIRFunction, IdentifierId, InstructionKind, InstructionValue};
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
pub fn rewrite_instruction_kinds_based_on_reassignment(hir: &mut HIRFunction, env: &mut Environment) {
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
        // Only visit reachable blocks: dead blocks (e.g. the continuation after a
        // return inside a for-loop body) should not contribute reassignment markers.
        // This allows loop-init variables like `let i = 0` to be promoted to `const`
        // when the loop update (e.g. `i++`) is unreachable.
        let mut reachable: HashSet<BlockId> = HashSet::new();
        let mut queue = vec![func.body.entry];
        while let Some(id) = queue.pop() {
            if reachable.insert(id) {
                if let Some(block) = func.body.blocks.get(&id) {
                    for succ in block.terminal.real_successors() {
                        queue.push(succ);
                    }
                }
            }
        }
        for block in func.body.blocks.values() {
            if !reachable.contains(&block.id) {
                continue;
            }
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

    // --- Detect forward-captured Let declarations ---
    // A variable is "forward-captured" if a FunctionExpression/ObjectMethod at
    // instruction index i captures a context var whose Let declaration is at
    // index j > i in the same block. These must stay `let` (not `const`) because
    // the TS compiler preserves them as `let` for TDZ correctness.
    let mut forward_captured_decls: HashSet<DeclarationId> = HashSet::new();
    for block in hir.body.blocks.values() {
        // Map declaration_id → instruction index in this block for Let-kind decls.
        let mut let_decl_pos: HashMap<DeclarationId, usize> = HashMap::new();
        for (idx, instr) in block.instructions.iter().enumerate() {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Let {
                        let d = env.get_identifier(lvalue.place.identifier)
                            .map(|i| i.declaration_id)
                            .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
                        let_decl_pos.insert(d, idx);
                    }
                }
                _ => {}
            }
        }
        // Check each FunctionExpression/ObjectMethod: context vars declared
        // later in the same block are forward-captured.
        for (idx, instr) in block.instructions.iter().enumerate() {
            let lowered_func = match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => Some(lowered_func),
                _ => None,
            };
            if let Some(lf) = lowered_func {
                for ctx_place in &lf.func.context {
                    let d = env.get_identifier(ctx_place.identifier)
                        .map(|i| i.declaration_id)
                        .unwrap_or_else(|| DeclarationId(ctx_place.identifier.0));
                    if let Some(&decl_idx) = let_decl_pos.get(&d) {
                        if decl_idx > idx {
                            forward_captured_decls.insert(d);
                        }
                    }
                }
            }
        }

        // Second sub-pass: any `let` variable declared in this block AFTER a
        // `let`-assigned FunctionExpression (i.e. `let foo = () => {...}`) stays `let`.
        // This catches same-scope forward refs: when foo is declared with `let` and later
        // bar is declared with `let`, foo's body may reference bar even if bar is not in
        // foo's context (stub body). Only track FEs whose own StoreLocal has kind=Let,
        // because `const fn` declarations cannot cause forward-capture issues.
        //
        // Pattern: FE(foo)→T1, StoreLocal(let,foo,T1), FE(bar)→T2, StoreLocal(let,bar,T2)
        // For bar's StoreLocal at 3: there's a prior let-FE at 0 (foo), and 0 < 3-1=2
        // → bar is forward-captured and stays `let`.
        let mut last_let_fe_pos: Option<usize> = None;
        for (idx, instr) in block.instructions.iter().enumerate() {
            match &instr.value {
                InstructionValue::FunctionExpression { .. }
                | InstructionValue::ObjectMethod { .. } => {
                    // Track this FE only if its immediately-following StoreLocal has kind=Let.
                    let fe_lvalue_id = instr.lvalue.identifier;
                    let next_is_let_own = block.instructions.get(idx + 1).map_or(false, |next| {
                        if let InstructionValue::StoreLocal { value, lvalue, .. } = &next.value {
                            value.identifier == fe_lvalue_id && lvalue.kind == InstructionKind::Let
                        } else {
                            false
                        }
                    });
                    if next_is_let_own && last_let_fe_pos.is_none() {
                        // This is the first `let`-FE in this block.
                        last_let_fe_pos = Some(idx);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value: sv, .. } => {
                    if lvalue.kind == InstructionKind::Let {
                        if let Some(fe_pos) = last_let_fe_pos {
                            // Check if this StoreLocal directly follows its own FE (at idx-1).
                            let own_fe_at_prev = idx > 0 && block.instructions.get(idx - 1).map_or(false, |prev| {
                                matches!(&prev.value,
                                    InstructionValue::FunctionExpression { .. }
                                    | InstructionValue::ObjectMethod { .. }
                                ) && prev.lvalue.identifier == sv.identifier
                            });
                            // If there's a let-FE from before idx-1 (not this var's own FE),
                            // this let var may be forward-captured by that earlier let-FE.
                            if !own_fe_at_prev || fe_pos < idx.saturating_sub(1) {
                                let d = env.get_identifier(lvalue.place.identifier)
                                    .map(|i| i.declaration_id)
                                    .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
                                forward_captured_decls.insert(d);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    // Persist newly detected forward-captured decls across pipeline calls.
    for d in &forward_captured_decls {
        env.forward_captured_let_decls.insert(*d);
    }
    // Also include previously persisted ones from earlier pipeline calls.
    for d in &env.forward_captured_let_decls {
        forward_captured_decls.insert(*d);
    }

    if std::env::var("RC_DEBUG_REWRITE").is_ok() {
        eprintln!("[rewrite_kinds] reassigned_decls count={}", reassigned_decls.len());
        eprintln!("[rewrite_kinds] forward_captured_decls count={}", forward_captured_decls.len());
        for (id, ident) in &env.identifiers {
            if ident.name.is_some() && forward_captured_decls.contains(&ident.declaration_id) {
                eprintln!("[rewrite_kinds] forward_captured: {:?} (id={})",
                    ident.name.as_ref().map(|n| n.value()), id.0);
            }
        }
        // Print named identifiers in reassigned_decls
        for (id, ident) in &env.identifiers {
            if ident.name.is_some() && reassigned_decls.contains(&ident.declaration_id) {
                eprintln!("[rewrite_kinds] reassigned: {:?} (id={})",
                    ident.name.as_ref().map(|n| n.value()), id.0);
            }
        }
    }

    // --- Pass 2: tighten declaration kinds for non-reassigned variables ---
    for block in hir.body.blocks.values_mut() {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. } => {
                    let was_let = lvalue.kind == InstructionKind::Let;
                    tighten_instruction_kind(lvalue, &reassigned_decls, &forward_captured_decls, env);
                    // Record if we promoted Let→Const (used later by rewrite_scope_decls_as_let).
                    if was_let && lvalue.kind == InstructionKind::Const {
                        let decl = env.get_identifier(lvalue.place.identifier)
                            .map(|i| i.declaration_id)
                            .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
                        env.originally_let_decls.insert(decl);
                    }
                }
                _ => {}
            }
        }
    }
}

/// After reactive scope inference and merging, revert `Const` → `Let` for
/// variables that need the named-var pattern in codegen. Two cases:
/// 1. Scope declarations that were originally `let` in the source — these emit
///    as `let x = expr` inside the scope body (preserving the original keyword).
/// 2. Named aliases of FunctionExpression scope outputs that were originally `let`
///    (`const y = tN` → `let y = tN`) — these need `is_let_kind=true` in
///    analyze_scope so skip_set is empty and intra-scope stores stay in body_lines.
pub fn rewrite_scope_decls_as_let(hir: &mut HIRFunction, env: &Environment) {
    if std::env::var("RC_DEBUG_REWRITE").is_ok() {
        eprintln!("[rewrite_scope_decls_as_let] scopes={} originally_let_decls={}",
            env.scopes.len(), env.originally_let_decls.len());
    }
    if env.scopes.is_empty() || env.originally_let_decls.is_empty() {
        return;
    }

    // Build a set of IdentifierIds produced by FunctionExpression/ObjectMethod instructions.
    let mut fn_expr_output_ids: HashSet<u32> = HashSet::new();
    for block in hir.body.blocks.values() {
        for instr in &block.instructions {
            if matches!(&instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            ) {
                fn_expr_output_ids.insert(instr.lvalue.identifier.0);
            }
        }
    }

    for block in hir.body.blocks.values_mut() {
        for instr in &mut block.instructions {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &mut instr.value {
                if lvalue.kind != InstructionKind::Const {
                    continue;
                }
                let lvalue_decl = env.get_identifier(lvalue.place.identifier)
                    .map(|i| i.declaration_id)
                    .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
                let was_originally_let = env.originally_let_decls.contains(&lvalue_decl);
                if !was_originally_let {
                    continue; // Never change variables that were originally const.
                }

                // Revert `const y = tN` alias where tN is a FunctionExpression scope
                // output. Revert to Let so analyze_scope treats y as a named var
                // (skip_set empty → no intra-scope stores spill to post_scope_lines).
                let lvalue_is_named = env.get_identifier(lvalue.place.identifier)
                    .and_then(|i| i.name.as_ref())
                    .is_some();
                // Only revert `const y = fn_temp` → `let y` when y is also in
                // scope.reassignments for the same scope. This ensures we only
                // apply the named-var fix for variables the TS compiler also
                // treats as reassigned (e.g. `let y; y = fn` pattern), not for
                // simple single-assignment fn aliases like `const x = function(){}`.
                let value_is_fn_scope_output = lvalue_is_named
                    && fn_expr_output_ids.contains(&value.identifier.0)
                    && env.scopes.values().any(|s|
                        s.declarations.contains_key(&value.identifier)
                        && s.reassignments.contains(&lvalue.place.identifier)
                    );

                if value_is_fn_scope_output {
                    if std::env::var("RC_DEBUG_REWRITE").is_ok() {
                        let name = env.get_identifier(lvalue.place.identifier)
                            .and_then(|i| i.name.as_ref())
                            .map(|n| n.value().to_string())
                            .unwrap_or_else(|| format!("id={}", lvalue.place.identifier.0));
                        eprintln!("[rewrite_scope_decls_as_let] reverting const→let for {:?} (id={})",
                            name, lvalue.place.identifier.0);
                    }
                    lvalue.kind = InstructionKind::Let;
                }
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
    forward_captured_decls: &HashSet<DeclarationId>,
    env: &Environment,
) {
    let decl = env.get_identifier(lvalue.place.identifier)
        .map(|i| i.declaration_id)
        .unwrap_or_else(|| DeclarationId(lvalue.place.identifier.0));
    if reassigned_decls.contains(&decl) || forward_captured_decls.contains(&decl) {
        return;
    }
    lvalue.kind = match lvalue.kind {
        InstructionKind::Let => InstructionKind::Const,
        // Do NOT promote HoistedLet → HoistedConst: the TS compiler preserves
        // hoisted variables as `let` since they're hoisted to function scope.
        other => other,
    };
}
