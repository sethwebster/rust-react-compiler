/// Mutable-range inference with simple alias tracking.
///
/// `Identifier.mutable_range` semantics:
///   - SSA temps (unnamed):  [def_iid, last_use_iid + 1)  (liveness range)
///   - Named variables:      [def_iid, last_mutation_iid + 1)
///     where "mutation" means: a StoreLocal/StoreContext that writes to the var,
///     or a MethodCall / PropertyStore whose receiver/object is an alias of the var.
///     Pure reads (LoadLocal) do NOT extend the mutable range of named variables.
///
/// Non-mutating methods: for known non-mutating Array/String/Object methods (e.g. map,
/// filter, join, slice, etc.), we do NOT extend the receiver's mutable range. This
/// allows the receiver to be memoized in a separate scope from the call result.
use std::collections::{HashMap, HashSet};

use crate::hir::environment::Environment;
use crate::hir::hir::{HIRFunction, IdentifierId, InstructionId, InstructionValue, MutableRange, Param, Place};
use crate::hir::visitors::{each_instruction_value_operand, each_terminal_operand};

/// Build a map from FunctionExpression SSA temp id → list of MUTATED context variable ids.
/// A context variable is "mutated" if the closure body contains an assignment, property
/// store, method call, or other mutation pattern that writes through it.
/// Only non-outlined closures are tracked.
///
/// Since inner function bodies are lowered as stubs (not fully lowered into HIR),
/// we use source text analysis to detect mutations.
fn build_closure_context_map(hir: &HIRFunction, env: &Environment) -> HashMap<IdentifierId, Vec<IdentifierId>> {
    let mut map: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::FunctionExpression { lowered_func, name_hint, .. } = &instr.value {
                // Only track non-outlined closures.
                if name_hint.is_none() {
                    let ctx_ids = find_mutated_context_vars_from_source(
                        &lowered_func.func.context,
                        &lowered_func.func.original_source,
                        env,
                    );
                    if std::env::var("RC_DEBUG2").is_ok() {
                        let ctx_all: Vec<u32> = lowered_func.func.context.iter().map(|p| p.identifier.0).collect();
                        eprintln!("[closure_ctx] fn_temp={} ctx={:?} mutated={:?} src_len={}",
                            instr.lvalue.identifier.0, ctx_all,
                            ctx_ids.iter().map(|id| id.0).collect::<Vec<_>>(),
                            lowered_func.func.original_source.len());
                    }
                    if !ctx_ids.is_empty() {
                        map.insert(instr.lvalue.identifier, ctx_ids);
                    }
                }
            }
        }
    }
    map
}

/// Determine which context variables of a closure are actually mutated in its body.
/// A context var is mutated if:
/// 1. There's a PropertyStore/ComputedStore whose `object` aliases a context var, OR
/// 2. There's a StoreContext that writes to a context var directly, OR
/// 3. There's a non-hook CallExpression/MethodCall whose arg or receiver aliases a context var.
///
/// We do a simple linear scan of the closure body to find these patterns.
fn find_mutated_context_vars(func: &HIRFunction) -> Vec<IdentifierId> {
    use crate::hir::hir::InstructionValue;

    // Build alias map inside closure: temp → context_place_id
    let mut inner_aliases: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    let ctx_ids: HashSet<IdentifierId> = func.context.iter().map(|p| p.identifier).collect();
    let mut mutated: HashSet<IdentifierId> = HashSet::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                // LoadContext: alias temp → context var
                InstructionValue::LoadContext { place, .. } => {
                    if ctx_ids.contains(&place.identifier) {
                        inner_aliases.insert(instr.lvalue.identifier, place.identifier);
                    }
                }
                // StoreContext: directly writes to context var
                InstructionValue::StoreContext { lvalue, .. } => {
                    if ctx_ids.contains(&lvalue.place.identifier) {
                        mutated.insert(lvalue.place.identifier);
                    }
                }
                // PropertyStore/ComputedStore: check if object aliases a context var
                InstructionValue::PropertyStore { object, .. }
                | InstructionValue::ComputedStore { object, .. } => {
                    if let Some(&src) = inner_aliases.get(&object.identifier) {
                        mutated.insert(src);
                    }
                }
                // MethodCall on a context var alias (potentially mutating)
                InstructionValue::MethodCall { receiver, property, .. } => {
                    let prop_name = match &instr.value {
                        InstructionValue::MethodCall { property, .. } => {
                            // property is a Place, we need to look up its name
                            // We don't have property name map here, so conservatively
                            // treat all method calls as potentially mutating.
                            let _ = property;
                            None::<&str>
                        }
                        _ => unreachable!(),
                    };
                    let is_non_mutating = prop_name.map_or(false, is_non_mutating_method);
                    if !is_non_mutating {
                        if let Some(&src) = inner_aliases.get(&receiver.identifier) {
                            mutated.insert(src);
                        }
                    }
                }
                // For array/object captures: if a context var is stored into an
                // array element that later gets mutated, it's indirectly mutated.
                // We don't track this level of depth — simple linear scan only.
                _ => {}
            }
            // Propagate aliases through LoadLocal (for re-aliasing inside closures)
            if let InstructionValue::LoadLocal { place, .. } = &instr.value {
                if let Some(&ctx_src) = inner_aliases.get(&place.identifier) {
                    inner_aliases.insert(instr.lvalue.identifier, ctx_src);
                }
            }
        }
    }

    mutated.into_iter().collect()
}

/// Detect which context variables are mutated inside a closure by analyzing
/// the function's original source text. This is needed because inner function
/// bodies are lowered as stubs (not fully lowered into HIR).
///
/// A context variable `name` is considered mutated if the source contains:
/// - `name = ...` (direct assignment, but not `== name` or `=== name`)
/// - `name +=`, `name -=`, `name++`, `name--`, etc. (compound assignment/update)
/// - `name.prop = ...` (property store)
/// - `name.method(...)` where method is potentially mutating (conservative)
fn find_mutated_context_vars_from_source(
    context: &[Place],
    source: &str,
    env: &Environment,
) -> Vec<IdentifierId> {
    let is_id_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let bytes = source.as_bytes();
    let slen = bytes.len();
    let mut mutated = Vec::new();

    for ctx_place in context {
        let name = match env.get_identifier(ctx_place.identifier).and_then(|i| i.name.as_ref()) {
            Some(n) => n.value().to_string(),
            None => continue,
        };
        if name.is_empty() { continue; }
        let name_bytes = name.as_bytes();
        let nlen = name.len();
        let mut is_mutated = false;

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

            // Check for identifier match with word boundaries.
            if bytes[i..].starts_with(name_bytes) {
                let before_ok = i == 0 || !is_id_char(bytes[i - 1]);
                let after_pos = i + nlen;
                let after_ok = after_pos >= slen || !is_id_char(bytes[after_pos]);
                if before_ok && after_ok {
                    // Found `name` at position i. Check what follows.
                    let mut j = after_pos;
                    // Skip whitespace.
                    while j < slen && bytes[j].is_ascii_whitespace() { j += 1; }

                    if j < slen {
                        // Check for assignment: name = (but not == or ===)
                        if bytes[j] == b'=' && (j + 1 >= slen || bytes[j + 1] != b'=') {
                            is_mutated = true;
                            break;
                        }
                        // Compound assignments: +=, -=, *=, /=, %=, &=, |=, ^=, <<=, >>=, >>>=, **=, ??=, ||=, &&=
                        if j + 1 < slen && bytes[j + 1] == b'=' {
                            match bytes[j] {
                                b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^' => {
                                    is_mutated = true;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        // ++, --
                        if j + 1 < slen && ((bytes[j] == b'+' && bytes[j + 1] == b'+') || (bytes[j] == b'-' && bytes[j + 1] == b'-')) {
                            is_mutated = true;
                            break;
                        }
                    }

                    // Check for prefix ++ / -- before the identifier.
                    if i >= 2 {
                        let mut k = i - 1;
                        while k > 0 && bytes[k].is_ascii_whitespace() { k -= 1; }
                        if k > 0 && ((bytes[k] == b'+' && bytes[k - 1] == b'+') || (bytes[k] == b'-' && bytes[k - 1] == b'-')) {
                            is_mutated = true;
                            break;
                        }
                    }

                    // Check for property store: name.prop = or name[expr] =
                    if j < slen && (bytes[j] == b'.' || bytes[j] == b'[') {
                        // Scan forward past the property access chain to see if it ends with =
                        let mut depth = 0i32;
                        let mut k = j;
                        loop {
                            if k >= slen { break; }
                            match bytes[k] {
                                b'[' => { depth += 1; k += 1; }
                                b']' => { depth -= 1; k += 1; }
                                b'(' => break, // method call — handled separately
                                b'=' if depth == 0 => {
                                    if k + 1 >= slen || bytes[k + 1] != b'=' {
                                        is_mutated = true;
                                    }
                                    break;
                                }
                                _ if bytes[k].is_ascii_whitespace() => { k += 1; }
                                _ if is_id_char(bytes[k]) || bytes[k] == b'.' => { k += 1; }
                                _ => break,
                            }
                        }
                        if is_mutated { break; }

                        // Check for mutating method call: name.push(...), name.splice(...), etc.
                        if j < slen && bytes[j] == b'.' {
                            let mut m = j + 1;
                            let method_start = m;
                            while m < slen && is_id_char(bytes[m]) { m += 1; }
                            let method_name = std::str::from_utf8(&bytes[method_start..m]).unwrap_or("");
                            // Skip whitespace.
                            while m < slen && bytes[m].is_ascii_whitespace() { m += 1; }
                            if m < slen && bytes[m] == b'(' && !is_non_mutating_method(method_name) && is_known_mutating_method(method_name) {
                                is_mutated = true;
                                break;
                            }
                        }
                    }

                    i = after_pos;
                    continue;
                }
            }
            i += 1;
        }

        if is_mutated {
            mutated.push(ctx_place.identifier);
        }
    }

    mutated
}

/// Returns true for methods that are known to mutate their receiver.
fn is_known_mutating_method(name: &str) -> bool {
    matches!(
        name,
        "push" | "pop" | "shift" | "unshift" | "splice" | "sort" | "reverse"
            | "fill" | "copyWithin" | "set" | "delete" | "clear" | "add"
    )
}

/// Returns true if a method name is known to NOT mutate its receiver.
/// Array methods: map, filter, find, reduce, etc. are non-mutating.
/// Mutating methods: push, pop, splice, sort, reverse, fill are mutating.
fn is_non_mutating_method(name: &str) -> bool {
    matches!(
        name,
        // Array non-mutating
        "map" | "filter" | "find" | "findIndex" | "findLast" | "findLastIndex"
            | "every" | "some" | "reduce" | "reduceRight" | "forEach"
            | "join" | "keys" | "values" | "entries" | "indexOf" | "lastIndexOf"
            | "includes" | "at" | "flat" | "flatMap" | "slice" | "concat"
            | "toString" | "toLocaleString" | "toReversed" | "toSorted" | "toSpliced"
            // String non-mutating (strings are immutable by nature)
            | "trim" | "trimStart" | "trimEnd" | "split" | "replace" | "replaceAll"
            | "match" | "matchAll" | "search" | "startsWith" | "endsWith"
            | "substring" | "substr" | "charAt" | "charCodeAt" | "codePointAt"
            | "normalize" | "repeat" | "padStart" | "padEnd" | "toLowerCase"
            | "toUpperCase" | "toLocaleLowerCase" | "toLocaleUpperCase"
            // Object/generic non-mutating
            | "hasOwnProperty" | "hasOwn" | "getOwnPropertyNames"
            | "getOwnPropertyDescriptor" | "getPrototypeOf" | "isPrototypeOf"
            | "propertyIsEnumerable" | "valueOf"
    )
}

pub struct InferMutationAliasingRangesOptions {
    pub is_function_expression: bool,
}

pub fn infer_mutation_aliasing_ranges(
    hir: &mut HIRFunction,
    env: &mut Environment,
    _options: InferMutationAliasingRangesOptions,
) {
    let zero = InstructionId(0);

    // Pre-build closure context map: FunctionExpression temp id → context var ids.
    // Used below to extend context vars' mutable ranges when the closure is called.
    let closure_context_map = build_closure_context_map(hir, env);

    // SSA temp liveness: def → last-use (for all types of use).
    let mut defs: HashMap<IdentifierId, InstructionId> = HashMap::new();
    let mut last_uses: HashMap<IdentifierId, InstructionId> = HashMap::new();

    // Named variable mutation tracking: var_id → last mutation instruction.
    // A "mutation" is a write (StoreLocal/StoreContext) or a mutating call through alias.
    let mut named_defs: HashMap<IdentifierId, InstructionId> = HashMap::new();
    let mut named_mut_uses: HashMap<IdentifierId, InstructionId> = HashMap::new();

    // Tracks the first StoreLocal/StoreContext iid for each named variable.
    // Used to distinguish "initial assignment" from "post-definition mutation":
    // if named_mut_uses[v] == named_first_stores[v], v was only ever assigned once
    // (the initial store) — subsequent captures into arrays/objects should NOT extend
    // v's mutable range. Only if named_mut_uses[v] > named_first_stores[v] (i.e., v
    // was mutated via a method call, second store, or call arg AFTER its first store)
    // should capture into an array/object extend the range.
    let mut named_first_stores: HashMap<IdentifierId, InstructionId> = HashMap::new();

    // Maps named variable id → the SSA temp that was stored into it on its FIRST StoreLocal.
    // Used in Phase 3: if the named variable was later mutated, the SSA temp's liveness
    // range must be extended to cover that mutation so scope-grouping sees them as
    // co-mutable. Example: `let x = []; x.push(a)` → store_values[x] = t0 (the array
    // literal). After Phase 2 finds named_mut_uses[x] > named_first_stores[x], Phase 3
    // extends last_uses[t0] to named_mut_uses[x] so t0 and x land in the same scope.
    let mut store_values: HashMap<IdentifierId, IdentifierId> = HashMap::new();

    // Tracks only "real" mutations (store, method-call receiver, call arg, property store).
    // Array/Object/JSX capture-induced extensions do NOT update this map.
    // Used in Phase 2.5 capture propagation so that capture-induced extensions
    // of receiver X don't transitively extend captured arg A's range.
    let mut named_real_muts: HashMap<IdentifierId, InstructionId> = HashMap::new();

    // Alias map: SSA temp id → source named variable id.
    let mut aliases: HashMap<IdentifierId, IdentifierId> = HashMap::new();

    // Property name map: identifier id (result of PropertyLoad) → property name string.
    // Used in Phase 2 to identify non-mutating MethodCall receivers.
    let mut property_names: HashMap<IdentifierId, String> = HashMap::new();

    // Hook detection: map LoadGlobal result id → global name string.
    // Used in Phase 2 to skip mutable-range extension for hook call arguments.
    // Hook calls (useEffect, useState, etc.) freeze their arguments rather than
    // mutating them — React's rules of hooks guarantee args are not mutated by the hook.
    let mut global_name_of: HashMap<IdentifierId, String> = HashMap::new();

    // Capture map: when arg is passed to MethodCall/CallExpression, track that
    // the argument may be captured into the receiver/container.
    // captures[receiver_source] = Vec<(capture_iid, arg_source)>
    let mut captures: HashMap<IdentifierId, Vec<(InstructionId, IdentifierId)>> = HashMap::new();

    // Closure var contexts: named_var_id → Vec<context_ids>.
    // When StoreLocal(f0, t_fn) where t_fn is a FunctionExpression temp, record
    // that f0 holds a closure whose context is closure_context_map[t_fn].
    // When f0 is later called, extend all context vars' mutable ranges to the call iid.
    let mut closure_var_contexts: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();

    // Iterator element tracking: IteratorNext SSA temp id → collection identifier id.
    // For `for (const x of items)`, records that the IteratorNext result temp aliases
    // an element of `items`. When x is mutated (e.g., `x.a = ...`), items' range extends.
    let mut iter_next_results: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    // named variable id → collection identifier id.
    // If `x` is the for-of loop var assigned from IteratorNext(collection: items),
    // element_of[x.id] = items.id. Mutations to x also extend items' mutable range.
    let mut element_of: HashMap<IdentifierId, IdentifierId> = HashMap::new();

    // Pure-read lvalue set: SSA temps produced by instructions that only READ
    // data (PropertyLoad, BinaryExpression, etc.) and never mutate anything.
    // These temps should NOT have their mutable_range extended to last-use in the
    // writeback pass — their range stays at [def, def+1].  This prevents them from
    // being co-located (UnionFind'd) into the same reactive scope as their
    // *consumers* in infer_reactive_scope_variables.
    //
    // NOTE: Phase 3 may still extend a pure-read temp's last_uses entry if the temp
    // was stored into a named variable that later gets mutated — that extension is
    // intentional and is handled separately via the phase3_extended set below.
    let mut pure_read_lvalues: HashSet<IdentifierId> = HashSet::new();
    let mut phase3_extended: HashSet<IdentifierId> = HashSet::new();

    // Params are defined at instruction 0.
    for param in &hir.params {
        match param {
            Param::Place(p) => { defs.entry(p.identifier).or_insert(zero); }
            Param::Spread(s) => { defs.entry(s.place.identifier).or_insert(zero); }
        }
    }

    // Walk all blocks.
    for (_, block) in &hir.body.blocks {
        let block_start = block.instructions.first().map(|i| i.id).unwrap_or(zero);

        // Phi nodes.
        for phi in &block.phis {
            let pid = phi.place.identifier;
            defs.entry(pid).or_insert(block_start);
            for op in phi.operands.values() {
                use_at(&mut last_uses, op.identifier, block_start);
            }
        }

        // Instructions.
        for instr in &block.instructions {
            let iid = instr.id;
            let lv = instr.lvalue.identifier;
            defs.entry(lv).or_insert(iid);

            match &instr.value {
                InstructionValue::DeclareLocal { .. } => {
                    // Do NOT set named_defs for DeclareLocal: `let z;` just declares
                    // an uninitialized variable. The mutable_range should start at the
                    // first actual write (StoreLocal), not the declaration. This matches
                    // TS behavior where scope ranges cover the write, not the declaration.
                }
                InstructionValue::DeclareContext { lvalue, .. } => {
                    let var_id = lvalue.place.identifier;
                    named_defs.entry(var_id).or_insert(iid);
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let var_id = lvalue.place.identifier;
                    named_defs.entry(var_id).or_insert(iid);
                    // Track the first store separately (the "initial assignment").
                    // On the first store, also record which SSA temp was stored (for Phase 3).
                    let is_first = *named_first_stores.entry(var_id).or_insert(iid) == iid;
                    if is_first {
                        store_values.insert(var_id, value.identifier);
                        // If the stored value is a FunctionExpression temp, record that this
                        // named variable holds a closure whose context needs range extension
                        // when the closure is called.
                        if let Some(ctx_ids) = closure_context_map.get(&value.identifier) {
                            closure_var_contexts.insert(var_id, ctx_ids.clone());
                        }
                        // If the stored value is from IteratorNext, record that this
                        // named variable holds an element of the iterated collection.
                        // When the named variable is later mutated (e.g., x.a = ...),
                        // the collection's mutable range must also extend.
                        // Resolve through aliases: IteratorNext.collection may be a temp
                        // (e.g., t_items from LoadLocal { place: items }) — we need the
                        // underlying named variable identifier.
                        if let Some(&raw_coll_id) = iter_next_results.get(&value.identifier) {
                            let coll_named_id = aliases.get(&raw_coll_id).copied().unwrap_or(raw_coll_id);
                            // Record for both the original named variable id and the SSA
                            // lvalue id (instr.lvalue.identifier). Subsequent LoadLocal
                            // instructions reference the SSA id, so we need element_of
                            // keyed by BOTH to ensure Phase 2 finds it via aliases lookup.
                            element_of.insert(var_id, coll_named_id);
                            element_of.insert(lv, coll_named_id);
                        }
                    }
                    // A store is a mutation of the named variable.
                    use_at(&mut named_mut_uses, var_id, iid);
                    use_at(&mut named_real_muts, var_id, iid);
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    let var_id = lvalue.place.identifier;
                    named_defs.entry(var_id).or_insert(iid);
                    let is_first = *named_first_stores.entry(var_id).or_insert(iid) == iid;
                    if is_first {
                        store_values.insert(var_id, value.identifier);
                    }
                    use_at(&mut named_mut_uses, var_id, iid);
                    use_at(&mut named_real_muts, var_id, iid);
                }
                // LoadLocal/LoadContext: record alias temp → source variable.
                // These are READ operations — do NOT extend named variable mutation range.
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    aliases.insert(lv, place.identifier);
                }
                // PropertyLoad: record the property name so Phase 2 can identify
                // non-mutating method calls (e.g., x.map → property_names[lv] = "map").
                // Also propagate the alias chain: if the object is an alias of a named var,
                // the property load result also aliases that named var (for mutation tracking:
                // mutate(x.y.z) should extend x's mutable range).
                // ALSO: extend object's liveness range to this iid — mirrors TS
                // `state.capture(index, object, lvalue)` which does
                // `from.mutableRange.end = max(from.mutableRange.end, index+1)`.
                // This ensures x is mutable at the PropertyLoad iid, keeping x and the
                // result in the same scope group (e.g., x = {}; y = x.a; store(y)).
                InstructionValue::PropertyLoad { object, property, .. } => {
                    property_names.insert(lv, property.clone());
                    if let Some(&source) = aliases.get(&object.identifier) {
                        aliases.insert(lv, source);
                    }
                    use_at(&mut last_uses, object.identifier, iid);
                }
                // ComputedLoad: same semantics as PropertyLoad — extend object's liveness.
                InstructionValue::ComputedLoad { object, .. } => {
                    if let Some(&source) = aliases.get(&object.identifier) {
                        aliases.insert(lv, source);
                    }
                    use_at(&mut last_uses, object.identifier, iid);
                }
                // IteratorNext: the result holds an element of the collection.
                // Record this so that when the result is stored into a named variable
                // (via StoreLocal) and that variable is later mutated, the collection's
                // mutable range is also extended.
                InstructionValue::IteratorNext { collection, .. } => {
                    iter_next_results.insert(lv, collection.identifier);
                }
                // LoadGlobal: record the global name for hook detection in Phase 2.
                InstructionValue::LoadGlobal { binding, .. } => {
                    use crate::hir::hir::NonLocalBinding;
                    let name = match binding {
                        NonLocalBinding::Global { name } => Some(name.clone()),
                        NonLocalBinding::ModuleLocal { name } => Some(name.clone()),
                        NonLocalBinding::ImportDefault { name, .. }
                        | NonLocalBinding::ImportNamespace { name, .. }
                        | NonLocalBinding::ImportSpecifier { name, .. } => Some(name.clone()),
                    };
                    if let Some(n) = name {
                        global_name_of.insert(lv, n);
                    }
                }
                _ => {}
            }

            // Mark pure-read lvalues: instructions whose result's mutable_range must NOT
            // be extended to its last general use.  This mirrors the TS compiler's
            // semantics where ranges are only extended via explicit Assign/Alias/Capture/
            // Mutate effects — never just because a value is "used" as an operand.
            //
            // Two categories:
            //  A) Pure reads: PropertyLoad, BinaryExpression, etc. — never allocate or mutate.
            //  B) Allocating values: ObjectExpression, ArrayExpression, FunctionExpression, etc.
            //     In the TS compiler, allocating values only get their ranges extended when
            //     the CONTAINER they're captured into is TRANSITIVELY MUTATED after the
            //     capture.  We approximate this via Phase 3 (named-var mutation propagation).
            //     General liveness extension (operand use) must be suppressed here so that
            //     e.g. `const x = [{}, [], props.value]` does NOT co-group {} and [] with
            //     the array — they belong in separate sentinel scopes.
            if matches!(
                &instr.value,
                // Category A: pure reads (no allocation, no mutation)
                InstructionValue::PropertyLoad { .. }
                    | InstructionValue::ComputedLoad { .. }
                    | InstructionValue::LoadGlobal { .. }
                    | InstructionValue::LoadLocal { .. }
                    | InstructionValue::LoadContext { .. }
                    | InstructionValue::BinaryExpression { .. }
                    | InstructionValue::UnaryExpression { .. }
                    | InstructionValue::TypeCastExpression { .. }
                    | InstructionValue::Primitive { .. }
                    | InstructionValue::TemplateLiteral { .. }
                    | InstructionValue::RegExpLiteral { .. }
                    | InstructionValue::JsxText { .. }
                    | InstructionValue::MetaProperty { .. }
                    // Category B: allocating values — range only via Phase 3 mutation propagation.
                    // In the TS compiler, allocating values only get their ranges extended when
                    // the CONTAINER they're captured into is TRANSITIVELY MUTATED after the
                    // capture.  We approximate this via Phase 3 (named-var mutation propagation).
                    // General liveness extension (operand use) must be suppressed here so that
                    // e.g. `const x = [{}, [], props.value]` does NOT co-group {} and [] with
                    // the array — they belong in separate sentinel scopes.
                    | InstructionValue::ObjectExpression { .. }
                    | InstructionValue::ArrayExpression { .. }
                    | InstructionValue::NewExpression { .. }
                    | InstructionValue::FunctionExpression { .. }
                    | InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. }
            ) {
                pure_read_lvalues.insert(lv);
            }

            // Track all operand uses for SSA temp liveness.
            // EXCEPTION 1: for StoreLocal/StoreContext, the VALUE operand is just being
            // copied into the target variable (pure read). We do NOT extend the value
            // temp's liveness range here — doing so would cause scope 1 to extend to
            // cover the StoreLocal instruction, merging it with the scope that uses the
            // variable. Example: `let a = someObj()` → t0 = Call, a = StoreLocal(t0).
            // t0's liveness should end at instruction 1 (the Call), not extend to 2
            // (the StoreLocal). This ensures scope 1 ends before the StoreLocal, creating
            // a gap instruction that the merge pass detects as unsafe (Let StoreLocal).
            //
            // EXCEPTION 2: for hook CallExpressions (useEffect, useState, etc.), the
            // argument operands are frozen (not mutated) by the hook. We do NOT extend
            // the arg SSA temps' liveness ranges to include the hook call instruction.
            // This prevents the hook call from being pulled inside the preceding scope's
            // range, which would block merge_reactive_scopes_that_invalidate_together.
            // The callee operand itself IS tracked (it's just a function reference read).
            let store_value_id: Option<crate::hir::hir::IdentifierId> = match &instr.value {
                InstructionValue::StoreLocal { value, .. } => Some(value.identifier),
                InstructionValue::StoreContext { value, .. } => Some(value.identifier),
                _ => None,
            };
            // Collect hook arg ids to skip (only for hook CallExpression).
            let hook_arg_ids: Option<std::collections::HashSet<crate::hir::hir::IdentifierId>> =
                if let InstructionValue::CallExpression { callee, args, .. } = &instr.value {
                    let is_hook = global_name_of
                        .get(&callee.identifier)
                        .map(|n| is_hook_name(n))
                        .unwrap_or(false);
                    if is_hook {
                        let ids: std::collections::HashSet<_> = args.iter().map(|arg| match arg {
                            crate::hir::hir::CallArg::Place(p) => p.identifier,
                            crate::hir::hir::CallArg::Spread(s) => s.place.identifier,
                        }).collect();
                        Some(ids)
                    } else {
                        None
                    }
                } else {
                    None
                };
            // EXCEPTION 3: ArrayExpression, ObjectExpression, JsxExpression, JsxFragment
            // capture their operands IMMUTABLY (ImmutableCapture in TS semantics).
            // They do NOT extend operand mutable ranges — only Capture/MutateTransitive
            // effects do. Skipping liveness extension here prevents e.g. `t0=foo()` from
            // having its range extended to the containing `[t0]` ArrayExpression instruction,
            // which would cause the two scopes to appear overlapping and get incorrectly merged.
            let is_immutable_capture_instr = matches!(&instr.value,
                InstructionValue::ArrayExpression { .. }
                | InstructionValue::ObjectExpression { .. }
                | InstructionValue::JsxExpression { .. }
                | InstructionValue::JsxFragment { .. }
            );
            for op in each_instruction_value_operand(&instr.value) {
                if store_value_id == Some(op.identifier) {
                    // Skip: this is a pure read of the value being stored.
                    continue;
                }
                if is_immutable_capture_instr {
                    // Skip: operands of immutable-capture instructions (Array/Object/JSX)
                    // are captured by value, not mutated. Don't extend their liveness.
                    continue;
                }
                if hook_arg_ids.as_ref().map(|s| s.contains(&op.identifier)).unwrap_or(false) {
                    // Skip: hook arg is frozen, not mutated — don't extend its liveness.
                    continue;
                }
                if pure_read_lvalues.contains(&op.identifier) {
                    // Skip: pure-read SSA temps must not have their liveness extended to
                    // consumers — their mutable_range stays at [def, def+1].  This
                    // prevents them from being co-grouped into the same reactive scope
                    // as their consumers in infer_reactive_scope_variables.
                    continue;
                }
                use_at(&mut last_uses, op.identifier, iid);
            }
        }

        // Terminal operands (for SSA liveness).
        let tid = block.terminal.id();
        for op in each_terminal_operand(&block.terminal) {
            use_at(&mut last_uses, op.identifier, tid);
        }
    }

    // Phase 2: alias mutation extension.
    // For MethodCall / PropertyStore / ComputedStore:
    // if the receiver/object is an alias of a named variable V, V is mutated here.
    // Also: if a named variable V is stored as a VALUE into an object property,
    // V's mutable range extends to that store (the object now captures V).
    // For MethodCall args: if arg is alias of named var A passed to receiver X,
    // track (capture_iid, A) for X. If X is later mutated, A's range extends too.
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            let iid = instr.id;
            match &instr.value {
                InstructionValue::CallExpression { callee, args, .. } => {
                    // Hook calls (useEffect, useState, etc.) freeze their arguments —
                    // they do NOT mutate them. Skip mutable-range extension for hook args.
                    // This prevents hook call instructions from being pulled inside the
                    // preceding scope's mutable range, which would block scope merging.
                    let is_hook = global_name_of
                        .get(&callee.identifier)
                        .map(|n| is_hook_name(n))
                        .unwrap_or(false);
                    if !is_hook {
                        // Function calls may mutate their arguments (conservative).
                        for arg in args {
                            let arg_id = match arg {
                                crate::hir::hir::CallArg::Place(p) => p.identifier,
                                crate::hir::hir::CallArg::Spread(s) => s.place.identifier,
                            };
                            if let Some(&source_var) = aliases.get(&arg_id) {
                                use_at(&mut named_mut_uses, source_var, iid);
                                use_at(&mut named_real_muts, source_var, iid);
                            }
                        }
                        // Closure call: when the callee is an alias of a named var that
                        // holds a FunctionExpression, the closure's context variables may
                        // be mutated at the call site. Extend their mutable ranges.
                        // This handles patterns like:
                        //   const x = {foo};
                        //   const f0 = function() { x.z = bar; };
                        //   f0();  ← x is mutated here through the closure
                        if let Some(&callee_source) = aliases.get(&callee.identifier) {
                            if std::env::var("RC_DEBUG2").is_ok() {
                                eprintln!("[closure_call] callee={} source={} has_ctx={}", callee.identifier.0, callee_source.0, closure_var_contexts.contains_key(&callee_source));
                            }
                            if let Some(ctx_ids) = closure_var_contexts.get(&callee_source) {
                                for &ctx_id in ctx_ids {
                                    // ctx_id may be either a named var id or an SSA temp id.
                                    // If it's a named var, extend named_mut_uses.
                                    // If it's an SSA temp, extend last_uses.
                                    // We conservatively extend both — whichever one gets
                                    // written back will have the correct range.
                                    use_at(&mut named_mut_uses, ctx_id, iid);
                                    use_at(&mut named_real_muts, ctx_id, iid);
                                    use_at(&mut last_uses, ctx_id, iid);
                                }
                            }
                        }
                    }
                }
                InstructionValue::NewExpression { args, .. } => {
                    // Constructor calls may mutate their arguments (conservative).
                    for arg in args {
                        let arg_id = match arg {
                            crate::hir::hir::CallArg::Place(p) => p.identifier,
                            crate::hir::hir::CallArg::Spread(s) => s.place.identifier,
                        };
                        if let Some(&source_var) = aliases.get(&arg_id) {
                            use_at(&mut named_mut_uses, source_var, iid);
                            use_at(&mut named_real_muts, source_var, iid);
                        }
                    }
                }
                InstructionValue::MethodCall { receiver, property, args, .. } => {
                    // Check if this is a known non-mutating method (e.g., map, filter, join).
                    // For non-mutating methods, the receiver is NOT mutated, so we should
                    // NOT extend the receiver's mutable range. This allows the receiver to
                    // be memoized in a separate scope from the call result.
                    let method_is_non_mutating = property_names
                        .get(&property.identifier)
                        .map_or(false, |name| is_non_mutating_method(name));

                    if let Some(&source_var) = aliases.get(&receiver.identifier) {
                        if !method_is_non_mutating {
                            use_at(&mut named_mut_uses, source_var, iid);
                            use_at(&mut named_real_muts, source_var, iid);
                        }
                        // Track args captured into receiver's source variable (for Phase 2.5).
                        // We do NOT extend the arg's mutable range here — args are only READ
                        // by the method call, not mutated. The Phase 2.5 propagation below
                        // handles the case where the receiver X is further mutated after
                        // capturing arg A (in that case, A's range should extend too).
                        if !method_is_non_mutating {
                            for arg in args {
                                let arg_id = match arg {
                                    crate::hir::hir::CallArg::Place(p) => p.identifier,
                                    crate::hir::hir::CallArg::Spread(s) => s.place.identifier,
                                };
                                if let Some(&arg_source) = aliases.get(&arg_id) {
                                    if arg_source != source_var {
                                        captures.entry(source_var).or_default().push((iid, arg_source));
                                    }
                                }
                            }
                        }
                    }
                }
                InstructionValue::PropertyStore { object, value, .. } => {
                    // Mutation of object's aliased source.
                    if let Some(&source_var) = aliases.get(&object.identifier) {
                        use_at(&mut named_mut_uses, source_var, iid);
                        use_at(&mut named_real_muts, source_var, iid);
                        // If source_var is a for-of loop variable (element of a collection),
                        // also extend the collection's mutable range. Mutating an element
                        // of `items` (through the loop variable) is a mutation of `items`.
                        if let Some(&coll_id) = element_of.get(&source_var) {
                            use_at(&mut named_mut_uses, coll_id, iid);
                            use_at(&mut named_real_muts, coll_id, iid);
                        }
                    }
                    // Also: the stored value (if it's an alias of a named var) is captured here.
                    // Extend its mutable range to this store.
                    if let Some(&val_source) = aliases.get(&value.identifier) {
                        use_at(&mut named_mut_uses, val_source, iid);
                        // val_source is captured (not directly mutated), so no real_muts update.
                    }
                }
                InstructionValue::ComputedStore { object, value, .. } => {
                    if let Some(&source_var) = aliases.get(&object.identifier) {
                        use_at(&mut named_mut_uses, source_var, iid);
                        use_at(&mut named_real_muts, source_var, iid);
                        // Same for-of element tracking as PropertyStore.
                        if let Some(&coll_id) = element_of.get(&source_var) {
                            use_at(&mut named_mut_uses, coll_id, iid);
                            use_at(&mut named_real_muts, coll_id, iid);
                        }
                    }
                    if let Some(&val_source) = aliases.get(&value.identifier) {
                        use_at(&mut named_mut_uses, val_source, iid);
                    }
                }
                // PropertyDelete / ComputedDelete: `delete x.b` or `delete x[key]`
                // counts as a mutation of the object's source variable, extending its
                // mutable range so the delete falls within the scope that owns `x`.
                InstructionValue::PropertyDelete { object, .. }
                | InstructionValue::ComputedDelete { object, .. } => {
                    if let Some(&source_var) = aliases.get(&object.identifier) {
                        use_at(&mut named_mut_uses, source_var, iid);
                        use_at(&mut named_real_muts, source_var, iid);
                    }
                }
                _ => {}
            }
        }
    }

    // Phase 2.5: propagate captures.
    // If variable A was captured into X at instruction iid, and X was REALLY mutated
    // BEYOND iid (via a StoreLocal, MethodCall, or call arg — not via a capture-induced
    // array/object extension), then A must also be considered mutated at that same point.
    // We use named_real_muts (not named_mut_uses) for the receiver's end so that
    // capture-induced extensions of X (e.g., X used in an array) don't transitively
    // extend A's range when X wasn't actually mutated after the capture.
    for (receiver_source, capture_list) in &captures {
        let receiver_last_real_mut = named_real_muts.get(receiver_source).copied()
            .or_else(|| named_defs.get(receiver_source).copied());
        if let Some(receiver_end) = receiver_last_real_mut {
            for &(capture_iid, arg_source) in capture_list {
                // Only propagate if X was REALLY mutated AFTER the capture point.
                if receiver_end > capture_iid {
                    use_at(&mut named_mut_uses, arg_source, receiver_end);
                    // Also update named_real_muts so Phase 3 can extend the SSA temp
                    // (t0 in `let t0 = expr(); a = t0`) to cover a's capture-induced mutation.
                    use_at(&mut named_real_muts, arg_source, receiver_end);
                }
            }
        }
    }

    // Phase 3: SSA temp alias mutation extension.
    // For each StoreLocal/StoreContext(x, t0): if x had a REAL mutation after its first
    // store (named_real_muts[x] > named_first_stores[x]), extend t0's liveness range to
    // cover x's last real mutation. This groups the SSA temp with the named variable in
    // the same reactive scope when the variable is later mutated.
    //
    // We use named_real_muts (not named_mut_uses) to avoid false triggers from
    // capture-induced extensions (e.g., `x` used in `[x, a]` extends named_mut_uses[x]
    // but that's not a real mutation — we shouldn't extend t0's range for that).
    //
    // Example: `let x = []; x.push(a)`
    //   t0=[] (iid 1), x=StoreLocal(t0) (iid 2), x.push(a) → MethodCall (iid 5)
    //   named_real_muts[x]=5, named_first_stores[x]=2 → 5>2 → extend last_uses[t0] to 5
    //   → t0.mutableRange=[1,6), isMutable(iid2, t0)=TRUE → t0 grouped with x ✓
    //
    // Counter-example: `let a = someObj(); return [x, a]`
    //   a never really mutated → named_real_muts[a]=2=named_first_stores[a] → 2>2 FALSE ✓
    for (&x_id, &t0_id) in &store_values {
        if let (Some(&x_last_real_mut), Some(&x_first_store)) =
            (named_real_muts.get(&x_id), named_first_stores.get(&x_id))
        {
            if x_last_real_mut > x_first_store {
                use_at(&mut last_uses, t0_id, x_last_real_mut);
                // Record that t0 was explicitly extended by Phase 3 (named var mutation).
                // This allows the writeback to honor this extension even if t0 is a
                // pure-read SSA temp (e.g., a PropertyLoad result stored into a named
                // variable that was later mutated via x.push(...)).
                phase3_extended.insert(t0_id);
            }
        }
    }

    // Write back ranges for SSA temps (liveness range).
    for (&id, &start) in &defs {
        // Skip if this is a named variable (handled separately below).
        if named_defs.contains_key(&id) {
            continue;
        }
        // Pure-read SSA temps (PropertyLoad, BinaryExpression, etc.) must NOT have
        // their mutable_range extended to their last general use.  Their range stays
        // at [def, def+1] UNLESS Phase 3 explicitly extended them (i.e., they were
        // stored into a named variable that later got mutated).
        let end_last_use = if pure_read_lvalues.contains(&id) && !phase3_extended.contains(&id) {
            start // Keep range at [def, def+1] — not extended to last use
        } else {
            last_uses.get(&id).copied().unwrap_or(start)
        };
        let end = InstructionId(end_last_use.0 + 1);
        let range = MutableRange {
            start,
            end: if end > start { end } else { InstructionId(start.0 + 1) },
        };
        if let Some(ident) = env.get_identifier_mut(id) {
            ident.mutable_range = range;
        }
    }

    // Write back ranges for named variables (mutation range).
    for (&var_id, &def_iid) in &named_defs {
        let last_mut = named_mut_uses.get(&var_id).copied().unwrap_or(def_iid);
        let end = InstructionId(last_mut.0 + 1);
        let range = MutableRange {
            start: def_iid,
            end: if end > def_iid { end } else { InstructionId(def_iid.0 + 1) },
        };
        if let Some(ident) = env.get_identifier_mut(var_id) {
            ident.mutable_range = range;
        }
    }
}

fn use_at(map: &mut HashMap<IdentifierId, InstructionId>, id: IdentifierId, iid: InstructionId) {
    map.entry(id).and_modify(|e| { if iid > *e { *e = iid; } }).or_insert(iid);
}

/// Returns true if `name` matches the React hook naming convention:
/// starts with "use" followed by an uppercase letter.
fn is_hook_name(name: &str) -> bool {
    name.starts_with("use")
        && name.len() > 3
        && name[3..].starts_with(|c: char| c.is_uppercase())
}
